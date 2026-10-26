import concurrent.futures
import copy
import hashlib
import json
import threading
import time
import unittest
from unittest.mock import patch

import test_router as fixtures
import test_scheduling as scheduling_fixtures
from gtt_router.api import dispatch, dispatch_management
from gtt_router.dispatch import Dispatch
from gtt_router.mcp import TOOLS, COMPAT_TOOLS
from gtt_router.models import WorkerFailure
from gtt_router.scheduling import settings


class DispatchTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service
    wait = fixtures.RouterTests.wait
    definition = scheduling_fixtures.SchedulingTests.definition
    run_done = scheduling_fixtures.SchedulingTests.run_done

    def request(self, **changes):
        return {"kind": "task", "task": {"kind": "extract", "prompt": "bounded fixture",
                "max_output_tokens": 1000, "token_budget": 5000, **changes}}

    def control(self, service, action, **params):
        return dispatch(service, "router_dispatch", {"action": action, **params})

    def test_concurrent_retries_and_completed_retry_execute_once(self):
        calls = []
        def adapter(*args):
            calls.append(True)
            return fixtures.response()
        service = self.service([fixtures.route()], adapter)
        with concurrent.futures.ThreadPoolExecutor(max_workers=12) as pool:
            receipts = list(pool.map(lambda _: self.control(service, "submit", request=self.request(), request_id="same"), range(24)))
        self.assertEqual(len({r["dispatch_id"] for r in receipts}), 1)
        self.wait(service, receipts[0]["job_id"])
        again = self.control(service, "submit", request=self.request(), request_id="same")
        self.assertEqual(again["state"], "succeeded")
        self.assertTrue(again["deduplicated"])
        self.assertEqual(calls, [True])
        with self.assertRaisesRegex(ValueError, "request_id_conflict"):
            self.control(service, "submit", request=self.request(prompt="different"), request_id="same")

    def test_all_legacy_submissions_forward_to_one_intake_and_mcp_hides_them(self):
        service = self.service([fixtures.route()], lambda *a: fixtures.response())
        receipt = dispatch(service, "router_submit", {"task": self.request()["task"]})
        self.assertIn("dispatch_id", receipt)
        self.assertEqual(receipt["replacement"], "router_dispatch")
        names = {t["name"] for t in TOOLS}
        self.assertIn("router_dispatch", names)
        for item in COMPAT_TOOLS:
            self.assertNotIn(item["name"], names)
        self.wait(service, receipt["job_id"])
        self.assertEqual(self.control(service, "list")["dispatches"][0]["job_id"], receipt["job_id"])

    def test_saved_workflow_is_resolved_once_and_requires_matching_revision(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: fixtures.response())
        prior = settings(service.store)
        service.scheduling.control("save", value=prior | {"workflows": [self.definition(route, 1)]}, expected_revision=0)
        request = {"kind": "workflow", "saved_workflow_id": "draft", "expected_revision": 1}
        with self.assertRaisesRegex(ValueError, "revision_conflict"):
            self.control(service, "submit", request=request | {"expected_revision": 0}, request_id="stale")
        receipt = self.control(service, "submit", request=request, request_id="saved")
        self.run_done(service, receipt["run_id"])
        prior = settings(service.store)
        service.scheduling.control("save", value=prior | {"workflows": []}, expected_revision=1)
        # A transport retry queries the original immutable admission even if the
        # user has subsequently removed or changed the saved graph.
        self.assertEqual(self.control(service, "submit", request=request, request_id="saved")["run_id"], receipt["run_id"])

    def test_permanently_incompatible_downstream_is_rejected_before_paid_upstream(self):
        route = fixtures.route()
        route.profile["allowed_kinds"] = ["extract"]
        service = self.service([route], lambda *a: self.fail("preflight must prevent paid upstream"))
        definition = self.definition(route)
        definition["nodes"][1]["task"]["kind"] = "logic_check"
        with self.assertRaisesRegex(ValueError, "workflow_preflight_failed: 1:task_kind_not_allowed"):
            self.control(service, "submit", request={"kind": "workflow", "definition": definition}, request_id="preflight")
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)

    def test_management_can_reconcile_and_cancel_when_external_interface_is_off(self):
        entered, release = threading.Event(), threading.Event()
        def adapter(*args):
            entered.set()
            release.wait(2)
            return fixtures.response()
        service = self.service([fixtures.route()], adapter)
        receipt = self.control(service, "submit", request=self.request(), request_id="switch")
        self.assertTrue(entered.wait(2))
        switch = self.root / "disabled.json"
        switch.write_text('{"schema_version":1,"enabled":false}', encoding="utf-8")
        service.interface_paths = [switch]
        for action in ("status", "collect", "cancel"):
            result = dispatch_management(service, "router_dispatch", {"action": action, "request_id": "switch"})
            self.assertEqual(result["dispatch_id"], receipt["dispatch_id"])
        with self.assertRaisesRegex(ValueError, "cannot admit"):
            dispatch_management(service, "router_dispatch", {"action": "submit", "request": self.request(), "request_id": "bypass"})
        with self.assertRaisesRegex(ValueError, "external_interface_disabled"):
            self.control(service, "submit", request=self.request(), request_id="blocked")
        release.set()
        self.wait(service, receipt["job_id"])

    def test_large_report_is_lossless_and_bound_to_its_dispatch(self):
        service = self.service([fixtures.route()], lambda *a: fixtures.response("测试证据" * 3000))
        receipt = self.control(service, "submit", request=self.request(), request_id="report")
        job = self.wait(service, receipt["job_id"])
        overview = self.control(service, "collect", request_id="report", max_chars=2000)
        self.assertTrue(overview["results"][0]["truncated"])
        offset, chunks, digest = 0, [], None
        while True:
            page = self.control(service, "report", request_id="report", worker=0, offset=offset, max_chars=2000)
            self.assertLessEqual(len(page["report_json"]), 2000)
            chunks.append(page["report_json"])
            digest = page["sha256"]
            offset = page["next_offset"]
            if not page["has_more"]:
                break
        body = "".join(chunks)
        self.assertEqual(json.loads(body), job["results"][0])
        self.assertEqual(hashlib.sha256(body.encode()).hexdigest(), digest)
        with self.assertRaisesRegex(ValueError, "Unknown report"):
            self.control(service, "report", request_id="report", worker=99)

    def test_workflow_collection_preserves_parallel_branch_cursor_and_report_ownership(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: fixtures.response())
        receipt = self.control(service, "submit", request={"kind": "workflow", "definition": self.definition(route)}, request_id="dag")
        run = self.run_done(service, receipt["run_id"])
        page = self.control(service, "collect", request_id="dag", max_chars=2000)
        self.assertEqual([r["node_id"] for r in page["results"]], ["0", "1"])
        report = self.control(service, "report", request_id="dag", node_id="1", worker=0)
        self.assertEqual(report["job_id"], run["nodes"]["1"]["job_id"])
        with self.assertRaisesRegex(ValueError, "No report"):
            self.control(service, "report", request_id="dag", node_id="outside", worker=0)
        # Completing a later branch must not move the cursor past an earlier
        # unfinished node, which would hide its eventual report.
        full = service.scheduling.status(run["id"], full=True)
        full["nodes"]["0"]["state"] = "running"
        service.scheduling._save(full)
        page = self.control(service, "collect", request_id="dag")
        self.assertEqual(page["next_cursor"], 0)
        self.assertTrue(page["has_more"])

    def test_restart_retains_receipt_and_ambiguous_admission_never_replays(self):
        service = self.service([fixtures.route()], lambda *a: fixtures.response())
        original = service.submit
        def ambiguous(*args, **kwargs):
            original(*args, **kwargs)
            raise OSError("transport lost after persistence")
        with patch.object(service, "submit", side_effect=ambiguous):
            with self.assertRaises(OSError):
                self.control(service, "submit", request=self.request(), request_id="unknown")
        receipt = self.control(service, "status", request_id="unknown")
        self.wait(service, receipt["job_id"])
        service.dispatcher = Dispatch(service)
        with patch.object(service, "submit", side_effect=AssertionError("replayed paid request")):
            again = self.control(service, "submit", request=self.request(), request_id="unknown")
        self.assertEqual(again["job_id"], receipt["job_id"])
        self.assertTrue(again["deduplicated"])

    def test_cancel_during_admission_does_not_wait_for_discovery(self):
        service = self.service([fixtures.route()], lambda *a: fixtures.response())
        entered, release = threading.Event(), threading.Event()
        original = service.submit
        def delayed(*args, **kwargs):
            entered.set()
            release.wait(3)
            return original(*args, **kwargs)
        with patch.object(service, "submit", side_effect=delayed), concurrent.futures.ThreadPoolExecutor() as pool:
            future = pool.submit(self.control, service, "submit", request=self.request(), request_id="slow")
            self.assertTrue(entered.wait(2))
            start = time.monotonic()
            cancelled = self.control(service, "cancel", request_id="slow")
            self.assertTrue(cancelled["cancel_requested"])
            self.assertLess(time.monotonic() - start, .5)
            release.set()
            receipt = future.result(timeout=4)
        self.assertEqual(receipt["state"], "cancelled")
        self.assertIsNone(receipt["target_id"])
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)

    def test_task_unknown_outcome_requires_acknowledgement_for_a_new_identity(self):
        service = self.service([fixtures.route()], lambda *a: (_ for _ in ()).throw(WorkerFailure("timeout")))
        request = self.request(max_attempts=1, allowed_routes=[fixtures.route().id])
        first = self.control(service, "submit", request=request, request_id="task-first")
        self.wait(service, first["job_id"])
        blocked = self.control(service, "submit", request=request, request_id="task-new")
        self.assertEqual(blocked["blocked"], "unknown_paid_outcome_requires_review")
        self.assertEqual(blocked["dispatch_id"], first["dispatch_id"])
        acknowledged = self.control(service, "submit", request=request, request_id="task-ack", acknowledge_unknown_risk=True)
        self.assertNotEqual(acknowledged["dispatch_id"], first["dispatch_id"])
        self.wait(service, acknowledged["job_id"])

    def test_persisted_interrupted_target_is_not_reported_running_after_restart(self):
        service = self.service([fixtures.route()], lambda *a: fixtures.response())
        first = self.control(service, "submit", request=self.request(), request_id="restart")
        self.wait(service, first["job_id"])
        with service.store.lock, service.store.conn:
            service.store.conn.execute("UPDATE jobs SET state='running' WHERE id=?", (first["job_id"],))
        # A restart releases the old owner before reopening the same database.
        config = dict(service.config)
        service.shutdown()
        self.services.remove(service)
        restored = self.service([fixtures.route()], lambda *a: self.fail("must not replay"), **config)
        status = self.control(restored, "status", request_id="restart")
        self.assertEqual(status["state"], "interrupted")

    def test_group_uses_the_same_receipt_and_existing_shared_execution(self):
        service = self.service([fixtures.route()], lambda *a: fixtures.response())
        request = {"kind": "group", "request": {"topic": "bounded fixture", "needs_web": False}}
        with patch.object(service.groups.pool, "submit"):
            receipt = self.control(service, "submit", request=request, request_id="group")
        self.assertEqual(receipt["group_id"], receipt["target_id"])
        self.assertEqual(receipt["detail"]["state"], "queued")
        self.control(service, "cancel", request_id="group")

    def test_invalid_envelopes_and_pool_switch_do_not_create_admissions(self):
        service = self.service([fixtures.route()], lambda *a: self.fail("no generation"))
        for request in ({"kind": "unknown"}, {"kind": "task", "task": {}, "extra": True},
                        {"kind": "workflow", "saved_workflow_id": "missing"},
                        {"kind": "workflow", "definition": {}, "saved_workflow_id": "both"}):
            with self.assertRaises((ValueError, TypeError)):
                self.control(service, "submit", request=request, request_id="invalid")
        prior = settings(service.store)
        service.scheduling.control("save", value=prior | {"enabled": False}, expected_revision=0)
        with self.assertRaisesRegex(ValueError, "scheduling_disabled"):
            self.control(service, "submit", request=self.request(), request_id="off")
        self.assertEqual(self.control(service, "list")["dispatches"], [])

    def test_request_identity_cannot_change_workflow_controls_or_escape_unknown_guard(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: (_ for _ in ()).throw(WorkerFailure("timeout", usage={"total_tokens": 12})))
        definition = self.definition(route, 1)
        receipt = self.control(service, "submit", request={"kind": "workflow", "definition": definition}, request_id="first")
        self.run_done(service, receipt["run_id"])
        changed = copy.deepcopy(definition)
        changed["luna_supervision"] = False
        with self.assertRaisesRegex(ValueError, "request_id_conflict"):
            self.control(service, "submit", request={"kind": "workflow", "definition": changed}, request_id="first")
        second = self.control(service, "submit", request={"kind": "workflow", "definition": changed}, request_id="second")
        self.assertEqual(second["blocked"], "unknown_paid_outcome_requires_review")
        self.assertEqual(second["run_id"], receipt["run_id"])


if __name__ == "__main__":
    unittest.main()
