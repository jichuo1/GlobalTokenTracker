"""Durability, ownership and version tests for the actual Rust SQLite owner."""
import copy
import hashlib
import json
from pathlib import Path
import queue
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

from test_scheduler_kernel import EXE
from gtt_router.models import Task, WorkerFailure, now_ms
from gtt_router.rust_kernel import RustKernel
from gtt_router.store import StateStore


class NativeStateTests(unittest.TestCase):
    def setUp(self):
        if not EXE.is_file():
            self.skipTest("build the Rust kernel")
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.resources = []

    def tearDown(self):
        for store, kernel in reversed(self.resources):
            store.close()
            if kernel:
                kernel.close()
        self.temp.cleanup()

    def native(self, path=None):
        kernel = RustKernel(str(EXE.resolve()))
        try:
            # Prove the selected backend never opens a Python SQLite writer.
            with patch("gtt_router.store.sqlite3.connect", side_effect=AssertionError("Python writer opened")):
                store = StateStore(path or self.root / "state.sqlite", kernel)
        except Exception:
            kernel.close()
            raise
        self.resources.append((store, kernel))
        return store, kernel

    @staticmethod
    def create_run(store, kernel, body=""):
        store.conn.execute("CREATE TABLE IF NOT EXISTS visual_runs(id TEXT PRIMARY KEY,updated_at INTEGER NOT NULL,payload TEXT NOT NULL)")
        value = {"id": "r", "state": "running", "definition": {"nodes": [{"id": "n", "node_type": "model"}], "edges": []},
            "nodes": {"n": {"state": "pending"}}, "tokens_used": 0, "body": body}
        kernel.write_record("run", "r", value)
        return kernel.read_record("run", "r")

    def test_single_owner_is_enforced_across_rust_and_python(self):
        store, kernel = self.native()
        with self.assertRaisesRegex(ValueError, "state_owner_busy"):
            StateStore(store.path)
        with self.assertRaisesRegex(sqlite3.OperationalError, "state_owner_busy"):
            self.native(store.path)
        store.close()
        kernel.close()
        self.resources.remove((store, kernel))
        python = StateStore(store.path)
        self.resources.append((python, None))
        with self.assertRaisesRegex(sqlite3.OperationalError, "state_owner_busy"):
            self.native(store.path)

    def test_run_revision_rejects_stale_writes_without_overwriting_new_state(self):
        store, kernel = self.native()
        first = self.create_run(store, kernel)
        stale = kernel.read_record("run", "r")
        first["nodes"]["n"]["state"] = "succeeded"
        kernel.write_record("run", "r", first)
        stale["nodes"]["n"]["state"] = "running"
        with self.assertRaisesRegex(ValueError, "runtime_revision_conflict"):
            kernel.write_record("run", "r", stale)
        self.assertEqual(kernel.read_record("run", "r")["nodes"]["n"]["state"], "succeeded")
        self.assertNotIn("revision", json.loads(json.dumps(first)))
        self.assertFalse(kernel.broken)

    def test_acknowledged_views_are_isolated_and_sql_changes_invalidate_them(self):
        store, kernel = self.native()
        self.create_run(store, kernel)
        before = kernel.transport_stats["calls"]
        for _ in range(10):
            view = kernel.read_record("run", "r")
            view["nodes"]["n"]["state"] = "failed"
        self.assertEqual(kernel.transport_stats["calls"], before)
        self.assertEqual(kernel.read_record("run", "r")["nodes"]["n"]["state"], "pending")
        with store.conn:
            store.conn.execute("UPDATE visual_runs SET payload=json_set(payload,'$.tokens_used',?) WHERE id=?", (77, "r"))
        self.assertEqual(kernel.read_record("run", "r")["tokens_used"], 77)

    def test_job_and_usage_rollback_together_on_commit_failure(self):
        store, kernel = self.native()
        store.conn.executescript("PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); "
            "CREATE TABLE child(parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);")
        detail = {"attempts": [{"state": "succeeded", "source": "fixture", "model": "m", "usage": {"total_tokens": 20}}], "results": []}
        events = [{"workflow_id": "w", "event_id": "state-j", "payload": "{}", "now_ms": now_ms(), "ttl_s": 21600}]
        with self.assertRaises(sqlite3.IntegrityError):
            with store.conn:
                store.conn.execute("INSERT INTO child VALUES(1)")
                store.save_job("j", "succeeded", Task("extract", "fixture"), detail, events)
        self.assertEqual(store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)
        self.assertEqual(store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], 0)
        self.assertEqual(store.conn.execute("SELECT COUNT(*) FROM coordination_events").fetchone()[0], 0)
        self.assertIsNone(kernel.read_record("job", "j"))
        # A rolled-back runtime version cannot poison the next valid write.
        store.save_job("j", "succeeded", Task("extract", "fixture"), detail, events)
        self.assertEqual(store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], 1)
        self.assertEqual(store.conn.execute("SELECT COUNT(*) FROM coordination_events").fetchone()[0], 1)

    def test_lost_commit_reply_recovers_unknown_once_without_losing_usage(self):
        store, kernel = self.native()
        detail = {"attempts": [{"state": "running", "token_measurement": "reported", "source": "fixture", "model": "m",
            "usage": {"total_tokens": 100}, "tokens_charged": 100, "started_at": now_ms()}], "results": [],
            "tokens_used": 100, "tokens_estimated": 0, "tokens_reserved": 0}
        # Prime the read, so the dropped response is the durable write reply.
        self.assertIsNone(kernel.read_record("job", "j"))
        original = kernel.responses.get
        def lose(*args, **kwargs):
            response = original(*args, **kwargs)
            if isinstance(response, dict) and "revision" in response.get("result", {}) and "created_at" in response["result"]:
                raise queue.Empty
            return response
        with patch.object(kernel.responses, "get", side_effect=lose), self.assertRaises(WorkerFailure):
            store.save_job("j", "running", Task("extract", "fixture"), detail)
        kernel.process.wait(timeout=5)
        path = store.path
        store.close()
        kernel.close()
        self.resources.remove((store, kernel))
        restored, _ = self.native(path)
        self.assertEqual(restored.job("j")["state"], "interrupted")
        self.assertEqual(restored.job("j")["tokens_used"], 100)
        self.assertEqual(restored.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], 1)

    def test_large_history_is_chunked_and_updates_send_only_changes(self):
        store, kernel = self.native()
        body = "历史🙂" * 2200000  # over the 16 MiB physical frame limit
        expected = hashlib.sha256(body.encode()).hexdigest()
        run = self.create_run(store, kernel, body)
        before = dict(kernel.transport_stats)
        run["tokens_used"] = 123
        run["nodes"]["n"]["state"] = "succeeded"
        kernel.write_record("run", "r", run)
        unchanged = kernel.read_record("run", "r")
        sent = kernel.transport_stats["request_bytes"] - before["request_bytes"]
        received = kernel.transport_stats["response_bytes"] - before["response_bytes"]
        self.assertLess(sent + received, 4096)
        self.assertEqual(unchanged["tokens_used"], 123)
        # Force a cold response, including the large history, through chunks.
        kernel.invalidate_views()
        cold = kernel.read_record("run", "r")
        self.assertEqual(hashlib.sha256(cold["body"].encode()).hexdigest(), expected)

    def test_legacy_nonfinite_metadata_becomes_unknown_without_changing_text(self):
        store, _ = self.native()
        detail = '{"results":[{"usage":{"total_tokens":NaN},"content":{"summary":"NaN and Infinity are text"}}],"attempts":[]}'
        with store.conn:
            store.conn.execute("INSERT INTO jobs VALUES(?,?,?,?,?,?)", ("old", "succeeded", 1, 1, "{}", detail))
        value = store.job("old")
        self.assertIsNone(value["results"][0]["usage"]["total_tokens"])
        self.assertEqual(value["results"][0]["content"]["summary"], "NaN and Infinity are text")

    def test_sql_compatibility_preserves_row_access_blobs_and_rollback(self):
        store, _ = self.native()
        store.conn.execute("CREATE TABLE fixture(id INTEGER PRIMARY KEY,body BLOB)")
        with self.assertRaisesRegex(ValueError, "abort"):
            with store.conn:
                store.conn.execute("INSERT INTO fixture VALUES(?,?)", (1, b"\0\xff"))
                row = store.conn.execute("SELECT id,body FROM fixture").fetchone()
                self.assertEqual(tuple(row), (1, b"\0\xff"))
                self.assertEqual(dict(row), {"id": 1, "body": b"\0\xff"})
                raise ValueError("abort")
        self.assertEqual(store.conn.execute("SELECT COUNT(*) FROM fixture").fetchone()[0], 0)

    def test_native_state_survives_an_explicit_python_backend_reopen(self):
        store, kernel = self.native()
        store.save_job("j", "succeeded", Task("extract", "fixture"), {"results": [], "attempts": [], "tokens_used": 19})
        store.close()
        kernel.close()
        self.resources.remove((store, kernel))
        python = StateStore(store.path)
        self.resources.append((python, None))
        self.assertEqual(python.job("j")["tokens_used"], 19)
        self.assertEqual(python.job("j")["state"], "succeeded")
