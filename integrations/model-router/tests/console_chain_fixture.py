"""A self-relaunching console worker with no hiding flags on its descendants."""
import ctypes
from ctypes import wintypes
import json
from pathlib import Path
import subprocess
import sys

kernel = ctypes.WinDLL("kernel32", use_last_error=True)
kernel.GetConsoleWindow.restype = wintypes.HWND
user = ctypes.WinDLL("user32", use_last_error=True)
user.IsWindowVisible.argtypes = [wintypes.HWND]
user.GetThreadDesktop.argtypes = [wintypes.DWORD]
user.GetThreadDesktop.restype = wintypes.HANDLE
user.GetUserObjectInformationW.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD,
                                         ctypes.POINTER(wintypes.DWORD)]
desktop = ctypes.create_unicode_buffer(256)
size = wintypes.DWORD()
if not user.GetUserObjectInformationW(user.GetThreadDesktop(kernel.GetCurrentThreadId()), 2,
                                     desktop, ctypes.sizeof(desktop), ctypes.byref(size)):
    raise ctypes.WinError(ctypes.get_last_error())
kernel.GetConsoleProcessList.argtypes = [ctypes.POINTER(wintypes.DWORD), wintypes.DWORD]
clients = (wintypes.DWORD * 32)()
window = kernel.GetConsoleWindow()
report = {"console": bool(window), "visible": bool(user.IsWindowVisible(window)),
          "console_clients": kernel.GetConsoleProcessList(clients, 32),
          "stdin_tty": sys.stdin.isatty(), "stdout_tty": sys.stdout.isatty(), "desktop": desktop.value}
depth = int(sys.argv[1])
payload = sys.stdin.buffer.read()
report["input"] = payload.decode("utf-8")
if depth:
    fresh = len(sys.argv) > 2 and sys.argv[2] == "fresh"
    startup = subprocess.STARTUPINFO()
    startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    startup.wShowWindow = subprocess.SW_HIDE
    child = subprocess.run([sys.executable, str(Path(__file__).resolve()), str(depth - 1),
                            "fresh" if fresh else "inherit"],
                           input=payload, capture_output=True, timeout=10,
                           creationflags=subprocess.CREATE_NEW_CONSOLE if fresh else 0,
                           startupinfo=startup if fresh else None)
    if child.returncode:
        raise RuntimeError("fixture descendant failed")
    report["child"] = json.loads(child.stdout)
sys.stdout.buffer.write(json.dumps(report).encode("utf-8") + b"\n")
sys.stderr.buffer.write(b"fixture stderr\n")
