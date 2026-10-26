import concurrent.futures
import ctypes
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from gtt_router.controls import DispatchBlocked
from gtt_router.operator import WorkspaceBroker, validate_grant
from gtt_router.processes import background_options
from gtt_router.store import StateStore


class Service:
    def __init__(self, root):
        self.store = StateStore(root / "state.sqlite")
        self.blocked = False
        self.gate_checks = 0

    def require_external_interface(self):
        self.gate_checks += 1
        if self.blocked:
            raise DispatchBlocked("external_interface_disabled")

    def require_scheduling(self):
        self.gate_checks += 1
        if self.blocked:
            raise DispatchBlocked("scheduling_disabled")


def sha(value):
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def running(pid):
    if os.name != "nt":
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.OpenProcess(0x1000, False, pid)
    if not handle:
        return False
    try:
        code = wintypes.DWORD()
        return bool(kernel.GetExitCodeProcess(handle, ctypes.byref(code))) and code.value == 259
    finally:
        kernel.CloseHandle(handle)


class OperatorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.service = Service(self.root)
        self.broker = WorkspaceBroker(self.service)
        self.grant = {"root": str(self.workspace), "writable_paths": ["a.txt", "b.txt", "nested/new.txt"]}

    def tearDown(self):
        self.service.store.close()
        self.temp.cleanup()

    def write(self, path="a.txt", old=None, new="changed", ident="write"):
        return {"id": ident, "op": "write_text", "path": path, "base_sha256": sha(old) if old is not None else None,
                "content": new}

    def execute(self, operations, assignment="assignment", **kwargs):
        return self.broker.execute("run", "operator", assignment, self.grant, self.grant["writable_paths"], operations, **kwargs)

    def command(self, code, *, timeout=5, ident="check"):
        self.grant["commands"] = {ident: {"argv": [sys.executable, "-u", "-c", code], "timeout_s": timeout}}
        return {"id": "command", "op": "run_command", "command_id": ident}

    def test_grants_and_snapshots_are_exact_bounded_and_utf8(self):
        (self.workspace / "a.txt").write_bytes("中文\n".encode("utf-8"))
        normalized = self.broker.validate_grant(self.grant)
        self.assertEqual(normalized["max_file_bytes"], 1048576)
        before = self.broker.snapshot(normalized, ["a.txt", "b.txt"])
        self.assertEqual(before[0], {"path": "a.txt", "sha256": sha("中文\n"), "content": "中文\n", "exists": True})
        self.assertEqual(before[1], {"path": "b.txt", "sha256": None, "content": "", "exists": False})
        for path in ["../outside", "/absolute", "a/../b", "a\\b", "a.txt:stream", "*.txt", ".git/config", ".env",
                     "nested/credentials.json", "nested/key.pem", "CON", "a.txt "]:
            with self.subTest(path=path), self.assertRaises(ValueError):
                validate_grant({"root": str(self.workspace), "writable_paths": [path]})
        with self.assertRaisesRegex(ValueError, "outside grant"):
            self.broker.snapshot(self.grant, ["other.txt"])
        with self.assertRaisesRegex(ValueError, "byte limit"):
            self.broker.snapshot({**self.grant, "max_file_bytes": 2}, ["a.txt"])

    def test_real_creation_replacement_and_idempotence_preserve_operation_identity(self):
        path = self.workspace / "a.txt"
        path.write_bytes(b"old\n")
        replace = {"id": "replace", "op": "replace_text", "path": "a.txt", "base_sha256": sha("old\n"),
                   "old_text": "old", "new_text": "new"}
        operations = [replace, self.write("nested/new.txt", new="新文件")]
        result = self.execute(operations)
        self.assertEqual(result["state"], "applied")
        self.assertEqual(path.read_text(encoding="utf-8"), "new\n")
        stamp = path.stat().st_mtime_ns
        again = self.execute(operations)
        self.assertEqual(again["state"], "applied")
        self.assertTrue(all(r["deduplicated"] for r in again["receipts"]))
        self.assertEqual(path.stat().st_mtime_ns, stamp)
        self.assertEqual((self.workspace / "nested/new.txt").read_text(encoding="utf-8"), "新文件")
        payloads = "".join(r[0] for r in self.service.store.conn.execute("SELECT payload FROM operator_operations"))
        self.assertNotIn("新文件", payloads)
        self.assertGreaterEqual(self.service.gate_checks, 8)

    def test_whole_batch_baselines_are_checked_before_any_file_changes(self):
        (self.workspace / "a.txt").write_text("before", encoding="utf-8")
        (self.workspace / "b.txt").write_text("outside edit", encoding="utf-8")
        result = self.execute([self.write(old="before"), self.write("b.txt", old="stale", ident="write-b")])
        self.assertEqual(result["state"], "conflict")
        self.assertEqual((self.workspace / "a.txt").read_text(), "before")
        self.assertEqual(result["reason"], "operator_base_hash_mismatch")
        self.assertEqual(self.service.store.conn.execute("SELECT COUNT(*) FROM operator_operations").fetchone()[0], 0)

    def test_replacement_must_have_exactly_one_match_and_ids_cannot_change_content(self):
        (self.workspace / "a.txt").write_text("same same", encoding="utf-8")
        op = {"id": "replace", "op": "replace_text", "path": "a.txt", "base_sha256": sha("same same"),
              "old_text": "same", "new_text": "other"}
        self.assertEqual(self.execute([op])["state"], "conflict")
        original = self.write(old="same same", new="done")
        self.assertEqual(self.execute([original])["state"], "applied")
        with self.assertRaisesRegex(ValueError, "operation_id_conflict"):
            self.execute([{**original, "content": "different"}])
        (self.workspace / "a.txt").write_text("external change", encoding="utf-8")
        self.assertEqual(self.execute([original])["reason"], "applied_result_changed")
        self.assertEqual((self.workspace / "a.txt").read_text(), "external change")

    def test_concurrent_stale_assignments_conflict_instead_of_overwriting(self):
        (self.workspace / "a.txt").write_text("baseline", encoding="utf-8")
        barrier = threading.Barrier(2)
        def execute(number):
            barrier.wait()
            return self.execute([self.write(old="baseline", new=str(number))], assignment=str(number))
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(execute, [1, 2]))
        self.assertEqual(sorted(r["state"] for r in results), ["applied", "conflict"])
        self.assertIn((self.workspace / "a.txt").read_text(), {"1", "2"})

    def test_cancelled_batch_and_cancel_at_replace_boundary_do_not_write(self):
        path = self.workspace / "a.txt"
        path.write_text("baseline", encoding="utf-8")
        cancel = threading.Event()
        cancel.set()
        self.assertEqual(self.execute([self.write(old="baseline")], cancel_event=cancel)["state"], "cancelled")
        cancel.clear()
        original_fsync = os.fsync
        def before_replace(descriptor):
            original_fsync(descriptor)
            cancel.set()
        with patch("gtt_router.operator.os.fsync", side_effect=before_replace):
            result = self.execute([self.write(old="baseline")], assignment="at-boundary", cancel_event=cancel)
        self.assertEqual(result["state"], "cancelled")
        self.assertEqual(path.read_text(), "baseline")
        self.assertEqual(list(self.workspace.glob(".gtt-operator-*.tmp")), [])

    def test_closed_dispatch_stops_new_operations_and_keeps_partial_receipts(self):
        self.service.blocked = True
        result = self.execute([self.write()])
        self.assertEqual(result["state"], "failed")
        self.assertEqual(result["receipts"][0]["reason"], "external_interface_disabled")
        self.assertFalse((self.workspace / "a.txt").exists())
        self.service.blocked = False
        original = self.broker._write
        def first_then_close(*args):
            original(*args)
            self.service.blocked = True
        with patch.object(self.broker, "_write", side_effect=first_then_close):
            result = self.execute([self.write(), self.write("b.txt", ident="write-b")], assignment="partially-applied")
        self.assertEqual(result["state"], "failed")
        self.assertTrue(result["partial"])
        self.assertTrue((self.workspace / "a.txt").exists())
        self.assertFalse((self.workspace / "b.txt").exists())

    def test_unknown_write_can_be_reconciled_from_after_hash_without_rewriting(self):
        original_replace = os.replace
        operation = self.write()
        def replaced_then_lost(source, target):
            original_replace(source, target)
            raise OSError("lost after replacement")
        with patch("gtt_router.operator.os.replace", side_effect=replaced_then_lost):
            result = self.execute([operation])
        self.assertEqual(result["state"], "unknown")
        self.assertEqual((self.workspace / "a.txt").read_text(), "changed")
        self.broker = WorkspaceBroker(self.service)
        with patch("gtt_router.operator.os.replace", side_effect=AssertionError("must not repeat")):
            reconciled = self.execute([operation])
        self.assertEqual(reconciled["state"], "applied")
        self.assertTrue(reconciled["receipts"][0]["reconciled"])

    def test_unknown_command_is_never_replayed(self):
        operation = self.command("print('never used')")
        with patch.object(self.broker, "_command", return_value={"state": "unknown", "reason": "fixture"}) as command:
            first = self.execute([operation])
            second = self.execute([operation])
        self.assertEqual(first["state"], "unknown")
        self.assertEqual(second["state"], "unknown")
        self.assertTrue(second["receipts"][0]["deduplicated"])
        self.assertEqual(command.call_count, 1)

    def test_command_registry_is_fixed_and_output_is_bounded_hashed_and_redacted(self):
        text = "x" * 12000 + "\n"
        operation = self.command("import sys;print('x'*12000);sys.stderr.write('Bearer '+'s'*40)")
        result = self.execute([operation])
        self.assertEqual(result["state"], "applied")
        receipt = result["receipts"][0]
        # Windows text stdout has CRLF because this is the child process' raw
        # pipe, not Python's universal-newline text adapter.
        expected = text.replace("\n", "\r\n") if os.name == "nt" else text
        self.assertEqual(receipt["stdout"]["sha256"], sha(expected))
        self.assertTrue(receipt["stdout"]["excerpt_only"])
        self.assertLessEqual(len(receipt["stdout"]["excerpt"]), 4096)
        self.assertEqual(receipt["stderr"]["excerpt"], "Bearer [REDACTED]")
        with self.assertRaises(ValueError):
            self.execute([{**operation, "argv": ["untrusted"]}], assignment="injection")
        with self.assertRaisesRegex(ValueError, "outside caller registry"):
            self.execute([{**operation, "command_id": "missing"}], assignment="missing-command")

    def test_timeout_kills_its_process_tree_and_preserves_unrelated_process(self):
        code = ("import os,subprocess,sys,time,json;"
                "child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)']);"
                "print(json.dumps({'parent':os.getpid(),'child':child.pid}),flush=True);time.sleep(30)")
        operation = self.command(code, timeout=1)
        unrelated = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(30)"],
                                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                     **background_options())
        try:
            result = self.execute([operation])
            self.assertEqual(result["state"], "unknown")
            receipt = result["receipts"][0]
            self.assertEqual(receipt["reason"], "command_timeout_outcome_unknown")
            pids = json.loads(receipt["stdout"]["excerpt"])
            deadline = time.monotonic() + 3
            while any(running(pid) for pid in pids.values()) and time.monotonic() < deadline:
                time.sleep(.05)
            self.assertFalse(any(running(pid) for pid in pids.values()))
            self.assertIsNone(unrelated.poll())
        finally:
            unrelated.terminate()
            unrelated.wait(timeout=5)

    def test_command_cancellation_and_workspace_wait_cancellation_are_bounded(self):
        operation = self.command("from pathlib import Path;import time;Path('a.txt').write_text('started');time.sleep(30)")
        cancel = threading.Event()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            first = pool.submit(self.execute, [operation], cancel_event=cancel)
            deadline = time.monotonic() + 3
            while not (self.workspace / "a.txt").exists() and time.monotonic() < deadline:
                time.sleep(.02)
            self.assertTrue((self.workspace / "a.txt").exists())
            queued_cancel = threading.Event()
            second = pool.submit(self.execute, [self.write("b.txt")], assignment="waiting", cancel_event=queued_cancel)
            queued_cancel.set()
            self.assertEqual(second.result(timeout=1)["state"], "cancelled")
            cancel.set()
            result = first.result(timeout=3)
        self.assertEqual(result["state"], "unknown")
        self.assertEqual(result["receipts"][0]["reason"], "command_cancelled_outcome_unknown")
        self.assertFalse((self.workspace / "b.txt").exists())

    def test_progress_is_reported_without_holding_the_global_store_lock(self):
        progress, store_available = [], []
        def callback(value):
            progress.append(value)
            def other_thread():
                acquired = self.service.store.lock.acquire(timeout=.2)
                store_available.append(acquired)
                if acquired:
                    self.service.store.lock.release()
            thread = threading.Thread(target=other_thread)
            thread.start()
            thread.join(timeout=.5)
        result = self.execute([self.write()], on_progress=callback)
        self.assertEqual(result["state"], "applied")
        self.assertEqual([p["state"] for p in progress], ["prepared", "applying", "applied"])
        self.assertTrue(all(store_available))
        self.assertTrue(all(p["operation_id"] == "write" for p in progress))

    def test_symlinks_and_junction_paths_cannot_escape_the_grant(self):
        outside = self.root / "outside"
        outside.mkdir()
        (outside / "secret.txt").write_text("outside", encoding="utf-8")
        linked = self.workspace / "linked"
        try:
            linked.symlink_to(outside, target_is_directory=True)
        except OSError as error:
            if os.name != "nt":
                raise
            # Junction creation does not require the Windows symbolic-link
            # privilege and exercises the same reparse-point rejection.
            result = subprocess.run(["cmd.exe", "/d", "/c", "mklink", "/J", str(linked), str(outside)],
                                    capture_output=True, timeout=5, **background_options())
            if result.returncode:
                self.skipTest("Host cannot create symlink or junction fixture: " + type(error).__name__)
        with self.assertRaisesRegex(ValueError, "symlinks and reparse"):
            validate_grant({"root": str(self.workspace), "writable_paths": ["linked/secret.txt"]})
        self.assertEqual((outside / "secret.txt").read_text(), "outside")


if __name__ == "__main__":
    unittest.main()
