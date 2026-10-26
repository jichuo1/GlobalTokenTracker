import json
import threading
import time
import unittest
from unittest.mock import patch

import test_router as fixtures
from gtt_router.api import dispatch
from gtt_router.models import WorkerFailure, now_ms
from gtt_router.operator import WorkspaceBusy, _workspace_lease, _path_lock


class ControllerWorkTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service

    def create(self, adapter=None, count=1, budget=6000):
        route = fixtures.route()
        route.max_parallel, route.output_limit = 2, 8192
        self.workspace = self.root / "controller-workspace"
        self.workspace.mkdir()
        self.workspace = self.workspace.resolve()
        for name in ("a.txt", "b.txt", "notes.txt"):
            (self.workspace / name).write_text("before", encoding="utf-8")
        self.calls = []
        self.service_instance = self.service([route], adapter or self.respond)
        def node(ident, kind):
            return {"id": ident, "title": ident, "node_type": kind, "route_id": route.id if kind != "codex" else "",
                "effort": "low" if kind != "codex" else "", "task": {"kind": "extract", "prompt": ident,
                    "max_output_tokens": 512, "token_budget": budget, "deadline_s": 60, "hard_deadline_s": 120,
                    "max_attempts": 1, "allow_fallback": False}}
        names = ["first", "second"][:count]
        definition = {"id": "controller", "name": "controller", "luna_supervision": False, "max_parallel": 2,
            "token_budget": 30000, "max_rounds": 3, "nodes": [node("mother", "codex"), *[node(n, "operator") for n in names]],
            "edges": [edge for n in names for edge in [{"from": "mother", "to": n}, {"from": n, "to": "mother", "kind": "report"}]]}
        self.receipt = dispatch(self.service_instance, "router_dispatch", {"action": "submit", "request_id": "controller-test",
            "request": {"kind": "workflow", "definition": definition, "execution": {"root": str(self.workspace),
                "writable_paths": ["a.txt", "b.txt", "notes.txt"]}}})
        return self.service_instance

    def respond(self, route, prompt, task, context):
        assignment = json.loads(task.context)["assignment"]
        self.calls.append(assignment["node_id"])
        file = assignment["files"][0]
        content = fixtures.content("implemented")
        content["operations"] = [{"id": "edit", "op": "replace_text", "path": file["path"],
            "base_sha256": file["sha256"], "old_text": file["content"],
            "new_text": "corrected" if "corrected" in assignment["instruction"] else "after"}]
        return {"text": json.dumps(content), "usage": {"total_tokens": 100}}

    def status(self):
        return dispatch(self.service_instance, "router_dispatch", {"action": "status", "dispatch_id": self.receipt["dispatch_id"]})["detail"]

    def topology(self):
        return dispatch(self.service_instance, "router_dispatch", {"action": "topology", "dispatch_id": self.receipt["dispatch_id"]})

    def decide(self, decision, identity=None):
        revision = self.status()["control_revision"]
        return dispatch(self.service_instance, "router_dispatch", {"action": "coordinate", "dispatch_id": self.receipt["dispatch_id"],
            "expected_revision": revision, "event_id": identity or "decision-" + str(revision), "decision": decision})

    def work(self, op, **fields):
        return self.decide({"type": "controller_work", "op": op, **fields})

    def assign(self, count=1):
        return self.decide({"type": "assign", "assignments": [{"node_id": n, "paths": [p], "instruction": "Implement minimal change"}
            for n, p in list(zip(("first", "second"), ("a.txt", "b.txt")))[:count]]})

    def wait_for(self, predicate, timeout=6):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            value = self.status()
            if predicate(value):
                return value
            self.assertNotEqual(value["state"], "interrupted", value)
            time.sleep(.02)
        self.fail("Controller workflow timed out: " + str(self.status()))

    def enqueue(self, ident="research", **extra):
        return self.work("enqueue", items=[{"id": ident, "kind": "research", "instruction": "Check independent evidence", **extra}])

    def test_controller_research_and_review_continue_during_native_execution(self):
        release = threading.Event()
        def adapter(*args):
            self.assertTrue(release.wait(5))
            return self.respond(*args)
        self.create(adapter)
        try:
            self.enqueue()
            self.enqueue("verify", kind="verification", paths=["a.txt"], depends_on=["first"], access="current")
            self.assign()
            self.wait_for(lambda r: r["nodes"]["first"]["state"] == "running")
            active = self.work("start", item_id="research", lease_s=120)
            self.assertEqual(active["nodes"]["mother"]["state"], "running")
            self.assertEqual(active["nodes"]["mother"]["dependency_state"], "succeeded")
            self.assertFalse(active["controller_work"]["should_wait"])
            self.assertEqual(active["controller_work"]["phase"], "research")
            self.assertEqual(self.calls, [])
            with self.assertRaisesRegex(ValueError, "current controller"):
                self.work("start", item_id="verify")
            self.work("finish", item_id="research", summary="Independent source invariant checked")
            release.set()
            run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
            self.assertIn("verify", run["controller_work"]["ready_items"])
            self.assertFalse(any(a["type"] == "accept" for a in run["pending_actions"]))
            report = run["controller_work"]["reports_ready"][0]
            with self.assertRaisesRegex(ValueError, "exact report"):
                self.work("ack_reports", reports=[{"node_id": "first", "fingerprint": "stale"}])
            with self.assertRaisesRegex(ValueError, "exact report"):
                self.work("ack_reports", reports=[{"node_id": "missing", "fingerprint": None}])
            self.work("ack_reports", reports=[{"node_id": "first", "fingerprint": report["fingerprint"]}])
            read = self.work("start", item_id="verify")
            self.assertEqual(read["controller_work"]["active_item"]["sources"][0]["content"], "after")
            self.work("finish", item_id="verify", summary="Actual current file is after")
            run = self.wait_for(lambda r: any(a["type"] == "accept" for a in r["pending_actions"]))
            accepted = self.decide({"type": "accept", "evidence_digest": run["evidence_digest"]})
            self.assertEqual(accepted["state"], "succeeded")
            self.assertEqual(self.calls, ["first"])
        finally:
            release.set()

    def test_current_read_claim_only_blocks_the_overlapping_operator(self):
        self.create(count=2)
        self.enqueue(paths=["a.txt"], access="current")
        self.work("start", item_id="research")
        self.assign(2)
        run = self.wait_for(lambda r: r["nodes"]["second"]["state"] == "succeeded")
        self.assertEqual(run["nodes"]["first"]["waiting_reason"], "controller_current_read")
        self.assertEqual(self.calls, ["second"])
        self.work("finish", item_id="research", summary="Read of unchanged baseline complete")
        run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
        self.assertEqual(self.calls, ["second", "first"])
        self.assertEqual(run["controller_work"]["items"][0]["status"], "stale")
        self.assertFalse(any(a["type"] == "accept" for a in run["pending_actions"]))
        self.work("start", item_id="research")
        self.work("finish", item_id="research", summary="Actual changed source rechecked")

    def test_snapshot_research_remains_immutable_while_operator_changes_file(self):
        self.create()
        self.enqueue(paths=["a.txt"], access="snapshot")
        self.work("start", item_id="research")
        self.assign()
        run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
        self.assertEqual(run["controller_work"]["active_item"]["sources"][0]["content"], "before")
        self.assertEqual((self.workspace / "a.txt").read_text(), "after")
        self.work("finish", item_id="research", summary="Research is bound to the frozen baseline")

    def test_live_current_read_waits_for_writer_but_independent_work_is_ready(self):
        release = threading.Event()
        def adapter(*args):
            self.assertTrue(release.wait(5))
            return self.respond(*args)
        self.create(adapter)
        try:
            self.assign()
            self.wait_for(lambda r: r["nodes"]["first"]["state"] == "running")
            self.enqueue("current", paths=["a.txt"], access="current")
            self.enqueue("independent")
            with self.assertRaisesRegex(ValueError, "active_operator:first"):
                self.work("start", item_id="current")
            board = self.status()["controller_work"]
            self.assertIn("independent", board["ready_items"])
            self.assertFalse(board["should_wait"])
            self.work("start", item_id="independent")
            self.work("finish", item_id="independent", summary="Unrelated evidence checked")
        finally:
            release.set()

    def test_expired_claim_releases_resources_but_cannot_report_completion(self):
        self.create()
        clock = [now_ms()]
        with patch("gtt_router.controller_work.now_ms", side_effect=lambda: clock[0]):
            self.enqueue(paths=["a.txt"], access="current")
            self.work("start", item_id="research", lease_s=15)
            self.assign()
            self.wait_for(lambda r: r["nodes"]["first"].get("waiting_reason") == "controller_current_read")
            clock[0] += 16000
            self.service_instance.store.wake()
            self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
            with self.assertRaisesRegex(ValueError, "expired"):
                self.work("finish", item_id="research", summary="Cannot claim unobserved work")
            with self.assertRaisesRegex(ValueError, "unfinished controller"):
                self.decide({"type": "accept", "evidence_digest": self.topology()["evidence_digest"]})
            self.work("start", item_id="research")
            self.work("finish", item_id="research", summary="Fresh source checked after reacquiring")

    def test_current_read_external_change_requires_refresh(self):
        self.create()
        self.enqueue(paths=["a.txt"], access="current")
        self.work("start", item_id="research")
        (self.workspace / "a.txt").write_text("external edit")
        run = self.work("finish", item_id="research", summary="Claim must not endorse a changed source")
        self.assertEqual(run["controller_work"]["items"][0]["status"], "stale")
        self.work("start", item_id="research")
        self.work("finish", item_id="research", summary="Updated source reviewed")

    def test_caller_verification_can_create_exact_scoped_correction_without_external_review(self):
        self.create()
        self.assign()
        run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
        self.enqueue("check", kind="verification", paths=["a.txt"], depends_on=["first"], access="current")
        self.work("start", item_id="check")
        report = run["controller_work"]["reports_ready"][0]
        run = self.work("finish", item_id="check", summary="Exact file does not meet its specified value",
                        verdict="needs_revision", reports=[{"node_id": "first", "fingerprint": report["fingerprint"]}],
                        issues=["a.txt should contain corrected, not after"])
        proposal = run["review_proposals"][0]
        self.assertEqual(proposal["kind"], "caller_review")
        self.assertFalse(any(a["type"] == "accept" for a in run["pending_actions"]))
        self.decide({"type": "revise", "proposal_digest": proposal["digest"],
            "assignment": {"node_id": "first", "paths": ["a.txt"], "instruction": "Set exact corrected behavior"}})
        run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded" and r["nodes"]["first"]["work_revision"] == 2)
        self.assertEqual((self.workspace / "a.txt").read_text(), "corrected")
        self.assertEqual(run["nodes"]["first"]["generation_overrides"]["token_budget"], 5900)
        self.assertEqual(run["controller_work"]["items"][0]["status"], "stale")
        self.work("start", item_id="check")
        self.work("finish", item_id="check", summary="Corrected source now satisfies the contract", verdict="accepted")

    def test_controller_decisions_alone_never_release_model_dependencies(self):
        self.create()
        self.enqueue()
        self.work("start", item_id="research")
        self.work("finish", item_id="research", summary="Planning can precede explicit dispatch")
        self.assertEqual(self.calls, [])
        self.assertEqual(self.status()["nodes"]["first"]["state"], "pending")

    def test_native_independent_same_file_edits_preserve_raw_report_and_apply_once(self):
        def adapter(route, prompt, task, context):
            file = json.loads(task.context)["assignment"]["files"][0]
            body = fixtures.content("two independent fixes")
            body["operations"] = [{"id": key, "op": "replace_text", "path": file["path"], "base_sha256": file["sha256"],
                "old_text": old, "new_text": new} for key, old, new in [("a", "alpha=1", "alpha=10"), ("b", "beta=2", "beta=20")]]
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        self.create(adapter)
        (self.workspace / "a.txt").write_text("alpha=1\nbeta=2\n")
        self.assign()
        run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
        result = run["nodes"]["first"]["operation_result"]
        self.assertEqual(len(result["receipts"]), 1)
        self.assertEqual(result["operation_bundles"][0]["source_operation_ids"], ["a", "b"])
        raw = self.service_instance.store.job(run["nodes"]["first"]["job_id"])["results"][0]
        self.assertEqual(len(raw["content"]["operations"]), 2)
        self.assertEqual((self.workspace / "a.txt").read_text(), "alpha=10\nbeta=20\n")

    def test_invalid_scope_lease_and_item_identity_do_not_expand_authority(self):
        self.create()
        with self.assertRaisesRegex(ValueError, "scope"):
            self.enqueue(paths=["../secret"])
        self.enqueue()
        with self.assertRaisesRegex(ValueError, "identity"):
            self.enqueue()
        with self.assertRaises(ValueError):
            self.work("start", item_id="research", lease_s=True)
        result = self.work("start", item_id="research")
        self.assertFalse(result["controller_work"]["identity_verified"])
        self.assertEqual(self.calls, [])

    def test_busy_workspace_assignment_is_queued_without_blocking_controller(self):
        self.create()
        entered, release = threading.Event(), threading.Event()
        def command_owner():
            with _workspace_lease(self.workspace, exclusive=True):
                entered.set()
                release.wait(5)
        thread = threading.Thread(target=command_owner)
        thread.start()
        try:
            self.assertTrue(entered.wait(1))
            started = time.monotonic()
            self.assign()
            self.assertLess(time.monotonic() - started, 1)
            self.assertTrue(self.topology()["assignments"]["first"]["snapshot_pending"])
            self.enqueue()
            self.work("start", item_id="research")
            self.work("finish", item_id="research", summary="Independent reasoning does not wait for the workspace")
            self.assertEqual(self.calls, [])
        finally:
            release.set()
            thread.join(2)
            self.service_instance.store.wake()
        self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")

    def test_nonblocking_snapshot_does_not_leak_a_workspace_read_lease(self):
        service = self.create()
        grant = service.operator.validate_grant({"root": str(self.workspace), "writable_paths": ["a.txt"]})
        entered, release = threading.Event(), threading.Event()
        def owner():
            with _path_lock(self.workspace / "a.txt"):
                entered.set()
                release.wait(5)
        thread = threading.Thread(target=owner)
        thread.start()
        try:
            self.assertTrue(entered.wait(1))
            with self.assertRaises(WorkspaceBusy):
                service.operator.snapshot(grant, ["a.txt"], blocking=False)
            with _workspace_lease(self.workspace, exclusive=True, blocking=False):
                pass
        finally:
            release.set()
            thread.join(2)

    def test_read_claim_during_admission_defers_commit_without_replaying_generation(self):
        service = self.create()
        submitted, release = threading.Event(), threading.Event()
        original = service.submit
        def delayed(*args, **kwargs):
            result = original(*args, **kwargs)
            submitted.set()
            self.assertTrue(release.wait(5))
            return result
        try:
            with patch.object(service, "submit", side_effect=delayed):
                self.assign()
                self.assertTrue(submitted.wait(2))
                self.enqueue(paths=["a.txt"], access="current")
                self.work("start", item_id="research")
                release.set()
                self.wait_for(lambda r: r["nodes"]["first"].get("waiting_reason") == "controller_current_read")
                self.assertEqual((self.workspace / "a.txt").read_text(), "before")
                self.work("finish", item_id="research", summary="Current baseline read completed")
                self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
                self.assertEqual(self.calls, ["first"])
        finally:
            release.set()

    def test_truncated_branch_can_be_revised_while_independent_branch_runs(self):
        release = threading.Event()
        attempts = {"first": 0, "second": 0}
        def adapter(route, prompt, task, context):
            ident = json.loads(task.context)["assignment"]["node_id"]
            attempts[ident] += 1
            if ident == "second":
                self.assertTrue(release.wait(5))
            elif attempts[ident] == 1:
                raise WorkerFailure("output_truncated", usage={"total_tokens": 300}, retry_after_s=0,
                                    details={"visible_output_chars": 0})
            return self.respond(route, prompt, task, context)
        self.create(adapter, count=2)
        try:
            self.assign(2)
            run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "failed" and r["nodes"]["second"]["state"] == "running")
            proposal = run["review_proposals"][0]
            stale_report = run["controller_work"]["reports_ready"][0]
            self.assertEqual(proposal["kind"], "generation_recovery")
            self.assertFalse(any(a["type"] == "accept" for a in run["pending_actions"]))
            old_second = run["nodes"]["second"]["job_id"]
            assignment = {"node_id": "first", "paths": ["a.txt"], "instruction": "Implement minimal change"}
            with self.assertRaisesRegex(ValueError, "cannot change"):
                self.decide({"type": "revise", "proposal_digest": proposal["digest"], "assignment": assignment,
                             "generation": {"token_budget": 500000}})
            with self.assertRaisesRegex(ValueError, "headroom"):
                self.decide({"type": "revise", "proposal_digest": proposal["digest"], "assignment": assignment})
            self.decide({"type": "revise", "proposal_digest": proposal["digest"], "assignment": assignment,
                         "generation": {"output_mode": "adaptive", "max_output_tokens": 2048}})
            run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded")
            self.assertEqual(run["nodes"]["second"]["job_id"], old_second)
            self.assertEqual(run["nodes"]["second"]["state"], "running")
            job = self.service_instance.store.job(run["nodes"]["first"]["job_id"])
            self.assertEqual(job["task"]["token_budget"], 5700)
            self.assertEqual(job["deadline_at"], proposal["deadline_at"])
            with self.assertRaisesRegex(ValueError, "exact report"):
                self.work("ack_reports", reports=[{"node_id": stale_report["node_id"], "fingerprint": stale_report["fingerprint"]}])
            self.assertEqual(attempts, {"first": 2, "second": 1})
            self.enqueue("verify-first", kind="verification", paths=["a.txt"], depends_on=["first"], access="current")
            self.work("start", item_id="verify-first")
            current = self.status()
            report = next(r for r in current["controller_work"]["reports_ready"] if r["node_id"] == "first")
            reviewed = self.work("finish", item_id="verify-first", summary="One additional boundary needs correction",
                verdict="needs_revision", reports=[{"node_id": "first", "fingerprint": report["fingerprint"]}], issues=["Use corrected behavior"])
            correction = next(p for p in reviewed["review_proposals"] if p.get("kind") == "caller_review")
            self.decide({"type": "revise", "proposal_digest": correction["digest"],
                         "assignment": {"node_id": "first", "paths": ["a.txt"], "instruction": "Use corrected behavior"}})
            run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "succeeded" and r["nodes"]["first"]["work_revision"] == 3)
            job = self.service_instance.store.job(run["nodes"]["first"]["job_id"])
            self.assertEqual(job["task"]["token_budget"], 5600)
            self.assertEqual(job["task"]["max_output_tokens"], 2048)
            self.assertEqual(job["deadline_at"], proposal["deadline_at"])
            self.assertNotIn("error", run["nodes"]["first"])
        finally:
            release.set()

    def test_unknown_paid_result_never_gets_generation_recovery(self):
        def adapter(*args):
            raise WorkerFailure("output_truncated", usage={"total_tokens": 300}, metering={"unknown_paid_outcome": True})
        self.create(adapter)
        self.assign()
        run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "unknown")
        self.assertFalse(run.get("review_proposals"))
        self.assertFalse(any(a["type"] == "accept" for a in run["pending_actions"]))

    def test_old_resident_rejects_new_control_protocol_before_sending(self):
        from gtt_router.daemon import LocalClient
        client = object.__new__(LocalClient)
        for decision, feature in [({"type": "controller_work", "op": "start"}, "controller_work_v1"),
                                  ({"type": "revise", "generation": {}}, "operator_recovery_v1")]:
            with patch.object(client, "runtime_info", return_value={"features": ["dispatch_v1"]}), \
                    patch.object(client, "_request") as request:
                with self.assertRaisesRegex(ValueError, feature):
                    client.call("router_dispatch", {"action": "coordinate", "decision": decision})
                request.assert_not_called()

    def test_exhausted_node_budget_or_deadline_stops_recovery_without_new_calls(self):
        for exhausted_budget in (True, False):
            def adapter(*args):
                raise WorkerFailure("output_truncated", usage={"total_tokens": 6000 if exhausted_budget else 300}, retry_after_s=0)
            if exhausted_budget:
                self.create(adapter)
            else:
                self.service_instance.adapters["fake"] = adapter
                self.receipt = dispatch(self.service_instance, "router_dispatch", {"action": "submit", "request_id": "deadline-case",
                    "request": {"kind": "workflow", "definition": self.topology()["definition"],
                        "execution": {"root": str(self.root / "controller-workspace"), "writable_paths": ["a.txt"]}}})
            self.assign()
            run = self.wait_for(lambda r: r["nodes"]["first"]["state"] == "failed")
            proposal = run["review_proposals"][0]
            clock = proposal["deadline_at"] + 1 if not exhausted_budget else now_ms()
            with patch("gtt_router.operator_workflow.now_ms", return_value=clock):
                with self.assertRaisesRegex(ValueError, "recovery_(budget|deadline)_exhausted"):
                    self.decide({"type": "revise", "proposal_digest": proposal["digest"],
                        "assignment": {"node_id": "first", "paths": ["a.txt"], "instruction": "New plan"},
                        "generation": {"output_mode": "adaptive", "max_output_tokens": 2048}})
