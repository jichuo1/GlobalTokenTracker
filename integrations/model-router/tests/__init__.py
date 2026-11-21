"""Isolated coordinator regression fixtures and tests."""

from pathlib import Path
import sys

_TESTS_DIR = Path(__file__).resolve().parent
for _path in (str(_TESTS_DIR), str(_TESTS_DIR.parent)):
    if _path not in sys.path:
        sys.path.insert(0, _path)
