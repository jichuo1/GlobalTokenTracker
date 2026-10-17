param(
    [ValidateRange(1,7200)][int]$Seconds = 3600,
    [string]$OutputPath
)
$ErrorActionPreference = 'Stop'
$diagnosticRoot = [IO.Path]::GetFullPath((Join-Path $env:USERPROFILE '.globaltokentracker\router\diagnostics'))
[IO.Directory]::CreateDirectory($diagnosticRoot) | Out-Null
if (-not $OutputPath) {
    $OutputPath = Join-Path $diagnosticRoot ('console-events-' + [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + '.jsonl')
}
$OutputPath = [IO.Path]::GetFullPath($OutputPath)
if (-not $OutputPath.StartsWith($diagnosticRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Console diagnostics must remain inside the private diagnostic directory.'
}

# Same-desktop terminal/Electron window observation; no titles or command text.
Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.IO;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
public class GttConsoleObserver {
 [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)] struct Proc {
  public uint size,usage,pid;public IntPtr heap;public uint module,threads,parent;
  public int priority;public uint flags;
  [MarshalAs(UnmanagedType.ByValTStr,SizeConst=260)]public string name;
 }
 [StructLayout(LayoutKind.Sequential)] struct Msg {
  public IntPtr hwnd;public uint message;public UIntPtr w;public IntPtr l;
  public uint time;public int x,y;
 }
 struct Start {public long at;public uint pid;public string chain;}
 delegate void Hook(IntPtr h,uint evt,IntPtr hwnd,int obj,int child,uint thread,uint time);
 [DllImport("kernel32.dll")]static extern IntPtr CreateToolhelp32Snapshot(uint flags,uint pid);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode)]static extern bool Process32FirstW(IntPtr h,ref Proc p);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode)]static extern bool Process32NextW(IntPtr h,ref Proc p);
 [DllImport("kernel32.dll")]static extern bool CloseHandle(IntPtr h);
 [DllImport("user32.dll")]static extern IntPtr SetWinEventHook(uint min,uint max,IntPtr dll,Hook cb,uint pid,uint thread,uint flags);
 [DllImport("user32.dll")]static extern bool UnhookWinEvent(IntPtr h);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)]static extern int GetClassNameW(IntPtr hwnd,StringBuilder text,int max);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr hwnd,out uint pid);
 [DllImport("user32.dll")]static extern bool IsWindowVisible(IntPtr hwnd);
 [DllImport("user32.dll")]static extern bool PeekMessageW(out Msg msg,IntPtr hwnd,uint min,uint max,uint remove);
 [DllImport("user32.dll")]static extern bool TranslateMessage(ref Msg msg);
 [DllImport("user32.dll")]static extern IntPtr DispatchMessageW(ref Msg msg);
 static Dictionary<uint,Proc> cache=new Dictionary<uint,Proc>();
 static HashSet<uint> alive=new HashSet<uint>();
 static Queue<Start> recent=new Queue<Start>();
 static Hook callback;
 static bool initialized=false;
 static string Safe(string s){return (s??"").Replace("\\","\\\\").Replace("\"","\\\"");}
 static long Now(){return DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();}
 static void Snapshot(){
  var h=CreateToolhelp32Snapshot(2,0);
  if(h==new IntPtr(-1))return;
  var p=new Proc();p.size=(uint)Marshal.SizeOf(p);var found=new HashSet<uint>();var fresh=new List<uint>();var now=Now();
  try{if(Process32FirstW(h,ref p)){do{
   found.Add(p.pid);if(initialized&&!alive.Contains(p.pid))fresh.Add(p.pid);
   cache[p.pid]=p;
  }while(Process32NextW(h,ref p));}}finally{CloseHandle(h);}
  foreach(var id in fresh)recent.Enqueue(new Start{at=now,pid=id,chain=Chain(id)});
  alive=found;initialized=true;
  while(recent.Count>2048||(recent.Count>0&&recent.Peek().at<now-30000))recent.Dequeue();
  if(cache.Count>8192){
   var keep=new HashSet<uint>(alive);foreach(var entry in recent)keep.Add(entry.pid);
   for(int round=0;round<9;round++){foreach(var id in new List<uint>(keep)){Proc ancestor;if(cache.TryGetValue(id,out ancestor))keep.Add(ancestor.parent);}}
   foreach(var id in new List<uint>(cache.Keys))if(!keep.Contains(id))cache.Remove(id);
  }
 }
 static string Chain(uint pid){
  var result=new List<string>();var seen=new HashSet<uint>();uint cur=pid;
  for(int i=0;i<9&&cur>0&&seen.Add(cur);i++){
   Proc p;if(!cache.TryGetValue(cur,out p))break;
   result.Add("{\"pid\":"+p.pid+",\"parent\":"+p.parent+",\"name\":\""+Safe(p.name)+"\"}");cur=p.parent;
  }
  return "["+string.Join(",",result)+"]";
 }
 public static void Run(int seconds,string output){
  using(var stream=new FileStream(output,FileMode.CreateNew,FileAccess.Write,FileShare.Read))
  using(var writer=new StreamWriter(stream,new UTF8Encoding(false))){
   writer.AutoFlush=true;Snapshot();
   callback=(h,e,hwnd,obj,child,thread,time)=>{
    try{
     if(obj!=0||child!=0)return;var name=new StringBuilder(256);GetClassNameW(hwnd,name,256);var cl=name.ToString();
     if(cl!="ConsoleWindowClass"&&cl!="CASCADIA_HOSTING_WINDOW_CLASS"&&cl!="Chrome_WidgetWin_1")return;
     uint pid;GetWindowThreadProcessId(hwnd,out pid);Snapshot();
     var starts=new List<string>();var now=Now();
     foreach(var entry in recent)if(entry.at>=now-20000)starts.Add("{\"timestamp\":"+entry.at+",\"chain\":"+entry.chain+"}");
     writer.WriteLine("{\"kind\":\"window_event\",\"timestamp\":"+now+",\"event\":"+e+",\"visible\":"+(IsWindowVisible(hwnd)?"true":"false")+",\"class\":\""+Safe(cl)+"\",\"owner_chain\":"+Chain(pid)+",\"preceding_starts\":["+string.Join(",",starts)+"]}");
    }catch{ /* Keep observing after a transient window/process read error. */ }
   };
   var hook=SetWinEventHook(0x8000,0x8002,IntPtr.Zero,callback,0,0,0);
   if(hook==IntPtr.Zero)throw new Exception("WinEvent hook unavailable");
   writer.WriteLine("{\"kind\":\"watch_started\",\"timestamp\":"+Now()+",\"duration_s\":"+seconds+",\"privacy\":\"process names and IDs only\"}");
   try{
    var end=DateTime.UtcNow.AddSeconds(seconds);var next=DateTime.UtcNow;
    while(DateTime.UtcNow<end){
     Msg msg;while(PeekMessageW(out msg,IntPtr.Zero,0,0,1)){TranslateMessage(ref msg);DispatchMessageW(ref msg);}
     if(DateTime.UtcNow>=next){Snapshot();next=DateTime.UtcNow.AddMilliseconds(20);}
     Thread.Sleep(5);
    }
    writer.WriteLine("{\"kind\":\"watch_completed\",\"timestamp\":"+Now()+"}");
   }finally{UnhookWinEvent(hook);GC.KeepAlive(callback);}
  }
 }
}
"@
[GttConsoleObserver]::Run($Seconds, $OutputPath)
Write-Output $OutputPath
