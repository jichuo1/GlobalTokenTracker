"""Native-parent fixture; descendants inherit pipes but never show a console."""
import ctypes
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from gtt_router.processes import background_options

if sys.argv[1] == "echo":
    value = json.loads(sys.stdin.buffer.readline().decode("utf-8"))
    console = ctypes.windll.kernel32.GetConsoleWindow() if os.name == "nt" else 0
    print(json.dumps({"chars": len(value["text"]), "argument": sys.argv[2],
                      "env": os.environ.get("GTT_FIXTURE_VALUE"), "console": console}), flush=True)
else:
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"],
                             stdin=subprocess.DEVNULL, stdout=sys.stdout, stderr=sys.stderr,
                             **background_options())
    print(json.dumps({"child_pid": child.pid}), flush=True)
    if sys.argv[1] == "wait":
        time.sleep(30)
