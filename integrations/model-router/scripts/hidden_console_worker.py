"""Launch a self-restarting CLI in one hidden console, retaining raw stdio.

This standalone bridge also works with a resident coordinator that predates
the launch-policy fix. Closing its Windows job kills only its own CLI tree.
"""
import ctypes
from ctypes import wintypes as w
import os
from pathlib import Path
import subprocess
import sys
import uuid


def run(command):
    if not command or not Path(command[0]).is_absolute() or not Path(command[0]).is_file():
        raise ValueError("A valid absolute CLI executable is required")
    if os.name != "nt":
        os.execv(command[0], command)
    # Coordinator workers are non-interactive and use a checksum-pinned CLI.
    # The CLI's automatic updater can self-relaunch with a fresh console.
    os.environ["AGY_CLI_DISABLE_AUTO_UPDATE"] = "true"
    os.environ["AGY_CLI_NONINTERACTIVE_HEADLESS"] = "true"

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    user = ctypes.WinDLL("user32", use_last_error=True)

    class Startup(ctypes.Structure):
        _fields_ = [("cb", w.DWORD), ("reserved", w.LPWSTR), ("desktop", w.LPWSTR), ("title", w.LPWSTR),
                    ("x", w.DWORD), ("y", w.DWORD), ("x_size", w.DWORD), ("y_size", w.DWORD),
                    ("x_chars", w.DWORD), ("y_chars", w.DWORD), ("fill", w.DWORD), ("flags", w.DWORD),
                    ("show", w.WORD), ("reserved_size", w.WORD), ("reserved_bytes", ctypes.c_void_p),
                    ("stdin", w.HANDLE), ("stdout", w.HANDLE), ("stderr", w.HANDLE)]

    class Process(ctypes.Structure):
        _fields_ = [("process", w.HANDLE), ("thread", w.HANDLE), ("pid", w.DWORD), ("tid", w.DWORD)]

    class BasicLimits(ctypes.Structure):
        _fields_ = [("process_time", ctypes.c_int64), ("job_time", ctypes.c_int64), ("flags", w.DWORD),
                    ("min_working_set", ctypes.c_size_t), ("max_working_set", ctypes.c_size_t),
                    ("active_processes", w.DWORD), ("affinity", ctypes.c_size_t),
                    ("priority", w.DWORD), ("scheduling", w.DWORD)]

    class IoCounters(ctypes.Structure):
        _fields_ = [(name, ctypes.c_uint64) for name in
                    ["read_ops", "write_ops", "other_ops", "read_bytes", "write_bytes", "other_bytes"]]

    class Limits(ctypes.Structure):
        _fields_ = [("basic", BasicLimits), ("io", IoCounters), ("process_memory", ctypes.c_size_t),
                    ("job_memory", ctypes.c_size_t), ("peak_process_memory", ctypes.c_size_t),
                    ("peak_job_memory", ctypes.c_size_t)]

    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, w.LPCWSTR]
    kernel.CreateJobObjectW.restype = w.HANDLE
    kernel.SetInformationJobObject.argtypes = [w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD]
    kernel.CreateProcessW.argtypes = [w.LPCWSTR, w.LPWSTR, ctypes.c_void_p, ctypes.c_void_p, w.BOOL,
                                     w.DWORD, ctypes.c_void_p, w.LPCWSTR,
                                     ctypes.POINTER(Startup), ctypes.POINTER(Process)]
    kernel.AssignProcessToJobObject.argtypes = [w.HANDLE, w.HANDLE]
    kernel.GetStdHandle.argtypes = [w.DWORD]
    kernel.GetStdHandle.restype = w.HANDLE
    kernel.ResumeThread.argtypes = [w.HANDLE]
    kernel.ResumeThread.restype = w.DWORD
    kernel.WaitForSingleObject.argtypes = [w.HANDLE, w.DWORD]
    kernel.WaitForSingleObject.restype = w.DWORD
    kernel.GetExitCodeProcess.argtypes = [w.HANDLE, ctypes.POINTER(w.DWORD)]
    kernel.TerminateProcess.argtypes = [w.HANDLE, w.UINT]
    kernel.CloseHandle.argtypes = [w.HANDLE]
    user.GetProcessWindowStation.restype = w.HANDLE
    user.GetUserObjectInformationW.argtypes = [w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD, ctypes.POINTER(w.DWORD)]
    user.CreateDesktopW.argtypes = [w.LPCWSTR, w.LPCWSTR, ctypes.c_void_p, w.DWORD, w.DWORD, ctypes.c_void_p]
    user.CreateDesktopW.restype = w.HANDLE
    user.CloseDesktop.argtypes = [w.HANDLE]

    job = kernel.CreateJobObjectW(None, None)
    if not job:
        raise ctypes.WinError(ctypes.get_last_error())
    process = Process()
    assigned = False
    desktop = None
    try:
        limits = Limits()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not kernel.SetInformationJobObject(job, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            raise ctypes.WinError(ctypes.get_last_error())
        startup = Startup()
        startup.cb = ctypes.sizeof(startup)
        # Some CLI self-relaunch paths detach and allocate a fresh console.
        # Keep descendants on a private desktop that is never made the input
        # desktop. Creating it within the current station needs no elevation.
        station_name = ctypes.create_unicode_buffer(256)
        size = w.DWORD()
        if not user.GetUserObjectInformationW(user.GetProcessWindowStation(), 2, station_name,
                                             ctypes.sizeof(station_name), ctypes.byref(size)):
            raise ctypes.WinError(ctypes.get_last_error())
        desktop_name = "GttWorker_" + uuid.uuid4().hex
        desktop = user.CreateDesktopW(desktop_name, None, None, 0, 0x1FF, None)
        if not desktop:
            raise ctypes.WinError(ctypes.get_last_error())
        startup.desktop = station_name.value + "\\" + desktop_name
        startup.flags = 0x100 | 1  # STARTF_USESTDHANDLES | STARTF_USESHOWWINDOW
        startup.show = 0  # SW_HIDE
        startup.stdin = kernel.GetStdHandle(w.DWORD(-10))
        startup.stdout = kernel.GetStdHandle(w.DWORD(-11))
        startup.stderr = kernel.GetStdHandle(w.DWORD(-12))
        arguments = ctypes.create_unicode_buffer(subprocess.list2cmdline(command))
        # Suspend until job membership is established, so no descendant can
        # escape cleanup between CreateProcess and AssignProcessToJobObject.
        if not kernel.CreateProcessW(command[0], arguments, None, None, True,
                                     0x10 | 0x4, None, None, ctypes.byref(startup), ctypes.byref(process)):
            raise ctypes.WinError(ctypes.get_last_error())
        if not kernel.AssignProcessToJobObject(job, process.process):
            raise ctypes.WinError(ctypes.get_last_error())
        assigned = True
        if kernel.ResumeThread(process.thread) == 0xFFFFFFFF:
            raise ctypes.WinError(ctypes.get_last_error())
        if kernel.WaitForSingleObject(process.process, 0xFFFFFFFF) == 0xFFFFFFFF:
            raise ctypes.WinError(ctypes.get_last_error())
        code = w.DWORD()
        if not kernel.GetExitCodeProcess(process.process, ctypes.byref(code)):
            raise ctypes.WinError(ctypes.get_last_error())
        return code.value
    finally:
        if process.process and not assigned:
            kernel.TerminateProcess(process.process, 1)
        kernel.CloseHandle(job)
        if process.thread:
            kernel.CloseHandle(process.thread)
        if process.process:
            kernel.CloseHandle(process.process)
        if desktop:
            user.CloseDesktop(desktop)


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] != "--":
        raise ValueError("Usage: hidden_console_worker.py -- absolute-executable [arguments]")
    sys.exit(run(sys.argv[2:]))
