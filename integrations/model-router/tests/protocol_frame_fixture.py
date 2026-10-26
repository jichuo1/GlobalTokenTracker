"""Synthetic stdout frames only; no real model or credentials."""
import json
import sys

mode = sys.argv[1]
if mode == "array":
    print(json.dumps([{"status": "unexpected_root"}]), flush=True)
else:
    print(json.dumps({"payload": "x" * int(mode)}), flush=True)
