import os
import json
from pathlib import Path
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

from gtt_router.processes import background_options, hidden_console_command


class ProcessVisibilityTests(unittest.TestCase):
    @unittest.skipUnless(os.name=="nt","Windows startup flags")
    def test_console_and_gui_helpers_have_an_explicit_hidden_show_state(self):
        options=background_options()
        self.assertEqual(options["creationflags"],subprocess.CREATE_NO_WINDOW)
        self.assertTrue(options["startupinfo"].dwFlags & subprocess.STARTF_USESHOWWINDOW)
        self.assertEqual(options["startupinfo"].wShowWindow,subprocess.SW_HIDE)

    @unittest.skipUnless(os.name=="nt","Windows startup flags")
    def test_resident_service_remains_detached_without_an_ignored_flag_combination(self):
        options=background_options(detached=True)
        self.assertEqual(options["creationflags"],subprocess.DETACHED_PROCESS)
        self.assertFalse(options["creationflags"] & subprocess.CREATE_NO_WINDOW)
        self.assertEqual(options["startupinfo"].wShowWindow,subprocess.SW_HIDE)

    def test_other_platforms_keep_their_native_session_behavior(self):
        with patch("gtt_router.processes.os.name","posix"):
            self.assertEqual(background_options(),{})

    @unittest.skipUnless(os.name=="nt","Windows process visibility")
    def test_real_worker_and_resident_start_without_a_console(self):
        code = "import ctypes,json; print(json.dumps({'console':bool(ctypes.windll.kernel32.GetConsoleWindow())}))"
        for detached in [False,True]:
            with self.subTest(detached=detached):
                result=subprocess.run([sys.executable,"-c",code],stdin=subprocess.DEVNULL,
                                      capture_output=True,text=True,timeout=10,
                                      **background_options(detached=detached))
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertFalse(json.loads(result.stdout)["console"])

    @unittest.skipUnless(os.name=="nt","Windows hidden console inheritance")
    def test_self_relaunching_worker_inherits_one_invisible_console_and_raw_pipes(self):
        fixture=Path(__file__).with_name("console_chain_fixture.py")
        payload='{"message":"tab\t中文 stream"}'.encode("utf-8")
        result=subprocess.run([sys.executable,str(fixture),"2"],input=payload,
                              capture_output=True,timeout=15,**background_options(hidden_console=True))
        self.assertEqual(result.returncode,0,result.stderr.decode())
        report=json.loads(result.stdout)
        for depth in range(3):
            self.assertTrue(report["console"])
            self.assertFalse(report["visible"])
            self.assertFalse(report["stdin_tty"])
            self.assertFalse(report["stdout_tty"])
            self.assertEqual(report["input"],payload.decode("utf-8"))
            self.assertGreaterEqual(report["console_clients"],depth+1)
            if depth<2:report=report["child"]
        self.assertEqual(result.stderr.replace(b"\r\n",b"\n"),b"fixture stderr\n")
        with self.assertRaises(ValueError):
            background_options(detached=True,hidden_console=True)

    @unittest.skipUnless(os.name=="nt","Windows console command bridge")
    def test_bridge_preserves_raw_pipes_arguments_exit_status_and_is_idempotent(self):
        fixture=Path(__file__).with_name("hidden_worker_fixture.py")
        command=hidden_console_command([sys.executable,str(fixture),"exit","arg with spaces"])
        self.assertEqual(hidden_console_command(command),command)
        payload="tabs\t中文\n".encode("utf-8")
        result=subprocess.run(command,input=payload,capture_output=True,timeout=10,**background_options())
        self.assertEqual(result.returncode,7)
        self.assertEqual(json.loads(result.stdout),{"argument":"arg with spaces","input":payload.decode("utf-8")})
        self.assertEqual(result.stderr,b"fixture stderr\n")

    @unittest.skipUnless(os.name=="nt","Windows job cleanup")
    def test_forced_bridge_exit_kills_only_its_owned_descendants(self):
        import ctypes
        from ctypes import wintypes
        fixture=Path(__file__).with_name("hidden_worker_fixture.py")
        command=hidden_console_command([sys.executable,str(fixture),"hold"])
        kernel=ctypes.WinDLL("kernel32",use_last_error=True)
        kernel.OpenProcess.argtypes=[wintypes.DWORD,wintypes.BOOL,wintypes.DWORD]
        kernel.OpenProcess.restype=wintypes.HANDLE
        kernel.GetExitCodeProcess.argtypes=[wintypes.HANDLE,ctypes.POINTER(wintypes.DWORD)]
        kernel.CloseHandle.argtypes=[wintypes.HANDLE]
        def active(pid):
            handle=kernel.OpenProcess(0x1000,False,pid)
            if not handle:return False
            try:
                code=wintypes.DWORD()
                return bool(kernel.GetExitCodeProcess(handle,ctypes.byref(code))) and code.value==259
            finally:kernel.CloseHandle(handle)
        with subprocess.Popen(command,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE,**background_options()) as process:
            owned=json.loads(process.stdout.readline())
            self.assertTrue(all(active(pid) for pid in owned.values()))
            process.kill()
            process.communicate(timeout=5)
        until=time.monotonic()+3
        while any(active(pid) for pid in owned.values()) and time.monotonic()<until:time.sleep(.05)
        self.assertFalse(any(active(pid) for pid in owned.values()))

    @unittest.skipUnless(os.name=="nt","Windows private desktop")
    def test_explicit_new_consoles_remain_on_the_workers_private_desktop(self):
        fixture=Path(__file__).with_name("console_chain_fixture.py")
        result=subprocess.run(hidden_console_command([sys.executable,str(fixture),"2","fresh"]),
                              input=b"private-desktop",capture_output=True,timeout=15,**background_options())
        self.assertEqual(result.returncode,0,result.stderr.decode())
        report=json.loads(result.stdout)
        desktop=report["desktop"]
        self.assertTrue(desktop.startswith("GttWorker_"))
        for depth in range(3):
            self.assertEqual(report["desktop"],desktop)
            self.assertEqual(report["input"],"private-desktop")
            if depth<2:report=report["child"]


if __name__=="__main__":
    unittest.main()
