"""Owned subprocesses for console bridge and forced-termination verification."""
import json
import os
import subprocess
import sys
import time

if sys.argv[1] == "exit":
    payload = sys.stdin.buffer.read()
    sys.stdout.buffer.write(json.dumps({"argument": sys.argv[2], "input": payload.decode("utf-8")}).encode("utf-8"))
    sys.stderr.buffer.write(b"fixture stderr\n")
    sys.exit(7)
elif sys.argv[1] == "hold":
    child = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(60)"],
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    sys.stdout.buffer.write(json.dumps({"root": os.getpid(), "child": child.pid}).encode() + b"\n")
    sys.stdout.buffer.flush()
    time.sleep(60)
