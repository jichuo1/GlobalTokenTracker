"""Synthetic transport fixture; never accesses the real ledger or credentials."""
import sys
import time

case = sys.argv[1]
if case == "overflow":
    sys.stdout.write("x" * 600000)
elif case == "timeout":
    time.sleep(10)
elif case == "error":
    sys.stderr.write("diagnostic-must-not-be-exported")
    sys.exit(2)
else:
    sys.stdout.write('{"ok":true}')
