"""Run one maintained corpus against either backend, in fixture state dirs."""
import argparse
import json
from pathlib import Path
import sys
import unittest

product = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(product))
parser = argparse.ArgumentParser()
parser.add_argument("--backend", choices=("python", "rust"), required=True)
parser.add_argument("--kernel")
parser.add_argument("--output", required=True)
parser.add_argument("--pattern", default="test_*.py")
args = parser.parse_args()
if args.backend == "rust" and (not args.kernel or not Path(args.kernel).is_file()):
    parser.error("Rust backend requires an existing --kernel")
from gtt_router.service import RouterService
original_init = RouterService.__init__

def initialize(self, config, *values, **kwargs):
    config = dict(config)
    config.setdefault("scheduler_kernel", {"backend": args.backend,
        **({"executable": str(Path(args.kernel).resolve())} if args.kernel else {})})
    original_init(self, config, *values, **kwargs)

RouterService.__init__ = initialize
suite = unittest.defaultTestLoader.discover(str(product / "tests"), pattern=args.pattern)
result = unittest.TextTestRunner(verbosity=2).run(suite)
record = {"backend": args.backend, "tests": result.testsRun,
    "failures": [{"id": test.id(), "trace": trace} for test, trace in result.failures],
    "errors": [{"id": test.id(), "trace": trace} for test, trace in result.errors],
    "skipped": [{"id": test.id(), "reason": reason} for test, reason in result.skipped]}
Path(args.output).write_text(json.dumps(record, indent=2), encoding="utf-8")
sys.exit(not result.wasSuccessful())
