import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("limits_update", SCRIPTS / "update_limits_runtime.py")
updater = importlib.util.module_from_spec(spec)
spec.loader.exec_module(updater)


class LimitsUpdateTests(unittest.TestCase):
    def test_rollback_restores_exact_bytes_and_retains_new_file(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            backup = root / "backup"
            backup.mkdir()
            old, new = root / "old.py", root / "new.py"
            old.write_bytes(b"after")
            new.write_bytes(b"addition")
            (backup / "0").write_bytes(b"before\r\n")
            rows = [{"target": str(old), "before": updater.sha(backup / "0"), "after": updater.sha(old)},
                    {"target": str(new), "before": None, "after": updater.sha(new)}]
            updater.restore_files(rows, backup, [0, 1])
            self.assertEqual(old.read_bytes(), b"before\r\n")
            self.assertEqual((backup / "new-file-1").read_bytes(), b"addition")
            self.assertFalse(new.exists())

    def test_rollback_refuses_to_overwrite_a_later_edit(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            file = root / "runtime.py"
            file.write_bytes(b"concurrent edit")
            rows = [{"target": str(file), "before": "old", "after": "expected update"}]
            with self.assertRaisesRegex(ValueError, "changed during rollback"):
                updater.restore_files(rows, root, [0])
            self.assertEqual(file.read_bytes(), b"concurrent edit")

    def test_rollback_preserves_runtime_updated_mutable_history(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            file = root / "history.json"
            file.write_bytes(b"new user save ancestry")
            rows = [{"target": str(file), "before": None, "after": "original staged ancestry"}]
            updater.restore_files(rows, root, [0], mutable_targets={file.resolve()})
            self.assertEqual(file.read_bytes(), b"new user save ancestry")
