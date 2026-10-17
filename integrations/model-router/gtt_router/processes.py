"""Windows background launches: hide GUI helpers as well as console programs."""
import os
from pathlib import Path
import subprocess
import sys


def background_options(detached=False, hidden_console=False):
    if os.name != "nt":
        return {}
    startup = subprocess.STARTUPINFO()
    startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
    startup.wShowWindow = subprocess.SW_HIDE
    # CREATE_NO_WINDOW is ignored for GUI images and when combined with
    # DETACHED_PROCESS. Keep the resident service detached, and explicitly
    # hide the initial window for either process class.
    if detached and hidden_console:
        raise ValueError("Detached and hidden-console modes are mutually exclusive")
    # A self-relaunching console CLI needs a console for its descendants to
    # inherit. CREATE_NO_WINDOW only controls the initial process; a child
    # launched without its own flag may allocate a new, visible terminal.
    flags = (subprocess.DETACHED_PROCESS if detached else
             subprocess.CREATE_NEW_CONSOLE if hidden_console else subprocess.CREATE_NO_WINDOW)
    return {"creationflags": flags, "startupinfo": startup}


def hidden_console_command(command):
    """Compatible command bridge for old resident services and new clients."""
    if os.name != "nt":
        return list(command)
    runner = Path(__file__).resolve().parent.parent / "scripts" / "hidden_console_worker.py"
    if (len(command) >= 3 and command[2] == "--" and
            os.path.normcase(os.path.abspath(command[1])) == os.path.normcase(str(runner))):
        return list(command)
    if not runner.is_file():
        raise FileNotFoundError("Hidden console worker bridge is missing")
    return [sys.executable, str(runner), "--", *command]


def owned_process_command(command):
    if os.name != "nt":
        return list(command)
    runner = Path(__file__).resolve().parent.parent / "scripts" / "owned_process_worker.py"
    if not runner.is_file():
        raise FileNotFoundError("Owned native worker bridge is missing")
    return [sys.executable, str(runner), "--", *command]
