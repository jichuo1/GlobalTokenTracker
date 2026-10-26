import copy
import json
from pathlib import Path
import threading
import time
import unittest
from unittest.mock import patch

import test_router as fixtures
from gtt_router.api import dispatch, dispatch_management
from gtt_router.scheduling import graph


class OperatorWorkflowTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service

    def definition(self, route):
        def node(ident, kind):
            return {"id": ident, "title": ident, "node_type": kind,
                "route_id": route.id if kind != "codex" else "", "effort": route.effort or "low", "role": ident,
                "task": {"kind": "extract", "prompt": ident, "max_output_tokens": 1000,
                         "token_budget": 5000, "deadline_s": 5, "max_attempts": 1}}
        return {"id": "operators", "name": "Codex operator review", "luna_supervision": False,
            "max_parallel": 2, "max_rounds": 3, "token_budget": 30000,
            "nodes": [node("mother", "codex"), node("first", "operator"), node("second", "operator"), node("review", "reviewer")],
            "edges": [{"from": "mother", "to": "first"}, {"from": "first", "to": "second"},
                      {"from": "first", "to": "review", "kind": "report"}, {"from": "second", "to": "review", "kind": "report"},
                      {"from": "review", "to": "mother", "kind": "report"}]}

    def call(self, service, action, **params):
        return dispatch(service, "router_dispatch", {"action": action, **params})

    def wait_for(self, service, ident, predicate, timeout=10):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            run = self.call(service, "status", dispatch_id=ident)["detail"]
            if predicate(run):
                return run
            if run["state"] == "interrupted":
                self.fail("Unexpected interrupted workflow: " + str(run))
            time.sleep(.03)
        self.fail("Operator workflow did not reach expected state: " + str(run))

    def test_two_level_real_files_review_requires_codex_correction_and_final_acceptance(self):
        workspace = self.root / "workspace"
        workspace.mkdir()
        for name in ("a.txt", "b.txt"):
            (workspace / name).write_text("before", encoding="utf-8")
        calls = []
        def adapter(route, prompt, task, context):
            payload = json.loads(task.context)
            assignment = payload["assignment"]
            body = fixtures.content()
            if assignment:
                ident = assignment["node_id"]
                calls.append(ident)
                self.assertIn("fast operator", prompt)
                target = "a.txt" if ident == "first" else "b.txt"
                snapshot = next(f for f in assignment["files"] if f["path"] == target)
                content = "correct" if "correct" in assignment["instruction"] else "bad" if ident == "first" else "ready"
                body["operations"] = [{"id": "write", "op": "write_text", "path": target,
                    "base_sha256": snapshot["sha256"], "content": content}]
            else:
                calls.append("review")
                self.assertTrue(all(r["operation_result"]["state"] == "applied" for r in payload["upstream"]))
                actual = {}
                for source in sorted(payload["upstream"], key=lambda s: s["operation_result"]["completed_order"]):
                    self.assertNotIn("operations", source["reports"][0])
                    self.assertTrue(all(not f["excerpt_only"] for f in source["after_files"]))
                    actual.update({f["path"]: f["content"] for f in source["after_files"]})
                # The real native reviewer has no filesystem tools. This fake
                # must make the same decision solely from broker evidence.
                body["review"] = {"accepted": actual["a.txt"] == "correct", "issues": ["a.txt requires correction"]}
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        route = fixtures.route()
        service = self.service([route], adapter)
        receipt = self.call(service, "submit", request_id="controlled", request={"kind": "workflow", "definition": self.definition(route),
            "input_text": "first stage and second stage material", "execution": {"root": str(workspace), "writable_paths": ["a.txt", "b.txt"]}})
        ident = receipt["dispatch_id"]
        topology = self.call(service, "topology", dispatch_id=ident)
        self.assertEqual(len(topology["definition"]["nodes"]), 4)
        self.assertEqual(topology["levels"]["second"], 2)
        self.assertFalse(topology["identity_verified"])
        self.assertEqual(calls, [])
        assignments = [{"node_id": "first", "instruction": "implement first stage", "paths": ["a.txt", "b.txt"], "input_range": [0, 11]},
                       {"node_id": "second", "instruction": "implement second stage", "paths": ["b.txt"], "input_range": [12, 37]}]
        decision = {"type": "assign", "assignments": assignments}
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=0, decision=decision, event_id="plan")
        again = self.call(service, "coordinate", dispatch_id=ident, expected_revision=0, decision=decision, event_id="plan")
        self.assertTrue(again["deduplicated"])
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete") and r.get("review_proposals"))
        self.assertEqual((workspace / "a.txt").read_text(), "bad")
        self.assertEqual((workspace / "b.txt").read_text(), "ready")
        self.assertEqual(calls.count("second"), 1)
        with self.assertRaisesRegex(ValueError, "exact current reviewer proposal"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="unapproved", decision={"type": "revise", "proposal_digest": "invented", "assignment": assignments[0]})
        with self.assertRaisesRegex(ValueError, "Cannot accept"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="early", decision={"type": "accept", "evidence_digest": self.call(service, "topology", dispatch_id=ident)["evidence_digest"]})
        proposal = run["review_proposals"][0]
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="approved-correction", decision={"type": "revise",
            "proposal_digest": proposal["digest"], "assignment": assignments[0] | {"instruction": "correct the first file"}})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete") and not r.get("review_proposals"))
        self.assertEqual((workspace / "a.txt").read_text(), "correct")
        self.assertEqual(calls.count("first"), 2)
        self.assertEqual(calls.count("second"), 1)
        self.assertEqual(calls.count("review"), 2)
        self.assertEqual(run["tokens_used"], 500)
        accept = next(a for a in run["pending_actions"] if a["type"] == "accept")
        (workspace / "a.txt").write_text("external edit after review", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "workflow_workspace_changed"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=2, event_id="stale-accept",
                decision={"type": "accept", "evidence_digest": accept["evidence_digest"]})
        self.assertEqual((workspace / "a.txt").read_text(), "external edit after review")
        (workspace / "a.txt").write_text("correct", encoding="utf-8")
        final = self.call(service, "coordinate", dispatch_id=ident, expected_revision=2, event_id="accept", decision={"type": "accept", "evidence_digest": accept["evidence_digest"]})
        self.assertEqual(final["state"], "succeeded")
        events = self.call(service, "events", dispatch_id=ident, limit=64)["events"]
        self.assertTrue(any(e["kind"] == "progress" and e["target_node"] == "mother" for e in events))
        self.assertTrue(any(e["kind"] == "review" and e["target_node"] == "mother" for e in events))
        self.assertFalse(any(e.get("authority") == "approve" for e in events))

    def test_correcting_one_independent_reviewer_does_not_clear_another_rejection(self):
        workspace = self.root / "parallel-workspace"
        workspace.mkdir()
        route = fixtures.route()
        definition = self.definition(route)
        reviewer = copy.deepcopy(definition["nodes"][-1])
        reviewer.update(id="review-b", title="review-b", role="review-b")
        definition["nodes"].append(reviewer)
        definition["edges"] = [{"from": "mother", "to": "first"}, {"from": "mother", "to": "second"},
            {"from": "first", "to": "review", "kind": "report"}, {"from": "second", "to": "review-b", "kind": "report"},
            {"from": "review", "to": "mother", "kind": "report"}, {"from": "review-b", "to": "mother", "kind": "report"}]
        def adapter(route, prompt, task, context):
            payload = json.loads(task.context)
            assignment = payload["assignment"]
            body = fixtures.content("same report text")
            if assignment:
                file = assignment["files"][0]
                body["operations"] = [{"id": "write", "op": "write_text", "path": file["path"],
                    "base_sha256": file["sha256"], "content": "fixed" if "fix" in assignment["instruction"] else "bad"}]
            else:
                after = payload["upstream"][0]["after_files"][0]
                body["review"] = {"accepted": after["content"] == "fixed", "issues": []}
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        service = self.service([route], adapter)
        receipt = self.call(service, "submit", request_id="independent-reviews", request={"kind": "workflow", "definition": definition,
            "execution": {"root": str(workspace), "writable_paths": ["a.txt", "b.txt"]}})
        ident = receipt["dispatch_id"]
        assignments = [{"node_id": "first", "instruction": "first", "paths": ["a.txt"]},
                       {"node_id": "second", "instruction": "second", "paths": ["b.txt"]}]
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=0, event_id="plan",
            decision={"type": "assign", "assignments": assignments})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete") and len(r.get("review_proposals", [])) == 2)
        proposals = {p["node_id"]: p for p in run["review_proposals"]}
        self.assertNotEqual(proposals["review"]["digest"], proposals["review-b"]["digest"])
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="fix-first", decision={"type": "revise",
            "proposal_digest": proposals["review"]["digest"], "assignment": assignments[0] | {"instruction": "fix first"}})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete") and r["nodes"]["review"].get("review", {}).get("accepted"))
        self.assertEqual([p["digest"] for p in run["review_proposals"]], [proposals["review-b"]["digest"]])
        self.assertFalse(any(a["type"] == "accept" for a in run["pending_actions"]))
        accept = self.call(service, "topology", dispatch_id=ident)
        with self.assertRaisesRegex(ValueError, "Cannot accept"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=2, event_id="incomplete-accept",
                decision={"type": "accept", "evidence_digest": accept["evidence_digest"]})
        # Defense in depth: even a missing proposal index cannot turn a known
        # explicit negative review into acceptance.
        with service.scheduling.lock:
            full = service.scheduling.status(run["id"], full=True)
            full["review_proposals"] = []
            service.scheduling._save(full)
        with self.assertRaisesRegex(ValueError, "rejected or incomplete review"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=2, event_id="missing-index-accept",
                decision={"type": "accept", "evidence_digest": accept["evidence_digest"]})

    def test_revise_cannot_expand_a_child_operator_beyond_its_parent_grant(self):
        workspace = self.root / "scope-workspace"
        workspace.mkdir()
        route = fixtures.route()
        def adapter(route, prompt, task, context):
            assignment = json.loads(task.context)["assignment"]
            body = fixtures.content()
            if assignment:
                file = assignment["files"][0]
                body["operations"] = [{"id": "write", "op": "write_text", "path": file["path"],
                    "base_sha256": file["sha256"], "content": assignment["node_id"]}]
            else:
                body["review"] = {"accepted": False, "issues": []}
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        service = self.service([route], adapter)
        receipt = self.call(service, "submit", request_id="child-grants", request={"kind": "workflow", "definition": self.definition(route),
            "execution": {"root": str(workspace), "writable_paths": ["a.txt", "b.txt"]}})
        ident = receipt["dispatch_id"]
        assignments = [{"node_id": name, "instruction": name, "paths": ["a.txt"]} for name in ("first", "second")]
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=0, event_id="plan", decision={"type": "assign", "assignments": assignments})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete") and r.get("review_proposals"))
        with self.assertRaisesRegex(ValueError, "within its parent"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="expand-child", decision={"type": "revise",
                "proposal_digest": run["review_proposals"][0]["digest"], "assignment": assignments[1] | {"paths": ["a.txt", "b.txt"]}})
        self.assertFalse((workspace / "b.txt").exists())
        self.assertEqual(self.call(service, "status", dispatch_id=ident)["detail"]["control_revision"], 1)

    def test_parallel_operators_must_not_claim_the_same_writable_file(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: self.fail("overlap must be rejected before generation"))
        workspace = self.root / "overlap-workspace"
        workspace.mkdir()
        definition = self.definition(route)
        definition["edges"] = [e for e in definition["edges"] if not (e["from"] == "first" and e["to"] == "second")]
        definition["edges"].append({"from": "mother", "to": "second"})
        receipt = self.call(service, "submit", request_id="overlap", request={"kind": "workflow", "definition": definition,
            "execution": {"root": str(workspace), "writable_paths": ["a.txt"]}})
        with self.assertRaisesRegex(ValueError, "overlap writable paths"):
            self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=0, event_id="overlap-plan",
                decision={"type": "assign", "assignments": [{"node_id": name, "instruction": name, "paths": ["a.txt"]} for name in ("first", "second")]})
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)

    def test_positive_review_with_unresolved_information_cannot_be_accepted(self):
        route = fixtures.route()
        definition = self.definition(route)
        definition["nodes"] = [definition["nodes"][0], definition["nodes"][-1]]
        definition["edges"] = [{"from": "mother", "to": "review"}]
        def adapter(*args):
            body = fixtures.content()
            body["review"] = {"accepted": True, "issues": []}
            body["information_requests"] = [{"question": "Missing source boundary?", "reason": "Not supplied"}]
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        service = self.service([route], adapter)
        receipt = self.call(service, "submit", request_id="unresolved-positive-review", request={"kind": "workflow", "definition": definition})
        ident = receipt["dispatch_id"]
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=0, event_id="plan", decision={"type": "assign", "assignments": [
            {"node_id": "review", "instruction": "review supplied material", "paths": []}]})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete"))
        self.assertTrue(run["nodes"]["review"]["review"]["accepted"])
        self.assertEqual(run["nodes"]["review"]["information_request_count"], 1)
        accept = next(a for a in run["pending_actions"] if a["type"] == "accept")
        with self.assertRaisesRegex(ValueError, "unresolved_information_requests"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="accept", decision={"type": "accept", "evidence_digest": accept["evidence_digest"]})

    def test_information_support_uses_exact_upstream_ids_and_closes_gate(self):
        route = fixtures.route()
        definition = self.definition(route)
        definition["nodes"] = definition["nodes"][:3]
        for node in definition["nodes"][1:]:
            node["node_type"] = "model"
        gate = copy.deepcopy(definition["nodes"][1])
        gate.update(id="gate", node_type="gate", route_id="")
        definition["nodes"].append(gate)
        definition["edges"] = [{"from": "mother", "to": "first"}, {"from": "first", "to": "second"},
                               {"from": "second", "to": "gate"}]
        ids = []
        def adapter(route, prompt, task, context):
            body = fixtures.content()
            payload = json.loads(task.context)
            if payload["assignment"]["node_id"] == "first":
                body["information_requests"] = [{"question": "Where is the evidence?", "reason": "Need source boundary"}]
            else:
                requests = payload["unresolved_information_requests"]
                self.assertEqual(len(requests), 1)
                ids.append(requests[0]["id"])
                body["information_support"] = [{"request_id": requests[0]["id"], "summary": "Bounded answer", "evidence": "Exact supplied source anchor"}]
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        service = self.service([route], adapter)
        receipt = self.call(service, "submit", request_id="information-support", request={"kind": "workflow", "definition": definition})
        ident = receipt["dispatch_id"]
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=0, event_id="plan", decision={"type": "assign", "assignments": [
            {"node_id": name, "instruction": name, "paths": []} for name in ("first", "second")]})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete"))
        self.assertEqual(run["nodes"]["gate"]["state"], "succeeded")
        self.assertEqual(run["nodes"]["second"]["information_resolved_ids"], ids)
        accept = next(a for a in run["pending_actions"] if a["type"] == "accept")
        final = self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="accept", decision={"type": "accept", "evidence_digest": accept["evidence_digest"]})
        self.assertEqual(final["state"], "succeeded")

    def test_typed_edge_planes_and_caller_scope_are_validated(self):
        route = fixtures.route()
        definition = self.definition(route)
        self.assertEqual(len(graph(definition)["edges"]), 5)
        cycle = copy.deepcopy(definition)
        cycle["edges"].append({"from": "second", "to": "first"})
        with self.assertRaisesRegex(ValueError, "cycle in task"):
            graph(cycle)
        cycle = copy.deepcopy(definition)
        cycle["edges"].append({"from": "mother", "to": "first", "kind": "report"})
        with self.assertRaisesRegex(ValueError, "cycle in report"):
            graph(cycle)
        wrong = copy.deepcopy(definition)
        wrong["edges"].append({"from": "second", "to": "mother"})
        with self.assertRaisesRegex(ValueError, "accept no task"):
            graph(wrong)

    def test_bidirectional_operator_feedback_never_blocks_initial_task_dispatch(self):
        workspace = self.root / "duplex"
        workspace.mkdir()
        calls = []
        def adapter(route, prompt, task, context):
            assignment = json.loads(task.context)["assignment"]
            ident = assignment["node_id"]
            calls.append(ident)
            name = "a.txt" if ident == "first" else "b.txt"
            file = next(f for f in assignment["files"] if f["path"] == name)
            body = fixtures.content()
            body["operations"] = [{"id": "create", "op": "write_text", "path": name, "base_sha256": file["sha256"], "content": ident}]
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        route = fixtures.route()
        service = self.service([route], adapter)
        definition = self.definition(route)
        definition["nodes"] = [n for n in definition["nodes"] if n["id"] != "review"]
        definition["edges"] = [{"from": "mother", "to": "first"}, {"from": "first", "to": "second"},
            {"from": "second", "to": "first", "kind": "report"}, {"from": "first", "to": "mother", "kind": "report"}]
        receipt = self.call(service, "submit", request_id="duplex", request={"kind": "workflow", "definition": definition,
            "execution": {"root": str(workspace), "writable_paths": ["a.txt", "b.txt"]}})
        self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=0, event_id="assign", decision={"type": "assign", "assignments": [
            {"node_id": "first", "instruction": "first file", "paths": ["a.txt", "b.txt"]},
            {"node_id": "second", "instruction": "second file", "paths": ["b.txt"]}]})
        run = self.wait_for(service, receipt["dispatch_id"], lambda r: r.get("execution_complete"))
        self.assertEqual(calls, ["first", "second"])
        self.assertTrue(all(n["state"] == "succeeded" for n in run["nodes"].values()))
        self.assertEqual((workspace / "b.txt").read_text(), "second")
        events = self.call(service, "events", dispatch_id=receipt["dispatch_id"], limit=64)["events"]
        reports = [e for e in events if e["kind"] == "operation_result" and e["source_node"] == "second"]
        self.assertEqual({e["target_node"] for e in reports}, {"first", "mother"})
        mother = next(e for e in reports if e["target_node"] == "mother")
        self.assertEqual(mother["payload"]["result"]["state"], "applied")
        self.assertEqual(mother["payload"]["result"]["after_files"][0]["content"], "second")

    def test_review_startup_deadlock_is_rejected_before_any_model_generation(self):
        definition = self.definition(fixtures.route())
        definition["nodes"] = [n for n in definition["nodes"] if n["id"] != "second"]
        definition["edges"] = [{"from": "mother", "to": "review"}, {"from": "review", "to": "first"},
            {"from": "first", "to": "review", "kind": "report"}]
        with self.assertRaisesRegex(ValueError, "review dependencies conflict"):
            graph(definition)

    def test_line_slices_preserve_the_rest_of_file_and_reject_fragment_replacement_as_full_file(self):
        for malicious in (False, True):
            with self.subTest(fragment_as_full_file=malicious):
                workspace = self.root / ("slice-bad" if malicious else "slice-good")
                workspace.mkdir()
                original = "prefix\nneedle = old\nsuffix\n"
                (workspace / "a.txt").write_text(original, encoding="utf-8", newline="\n")
                def adapter(route, prompt, task, context):
                    payload = json.loads(task.context)
                    body = fixtures.content()
                    if payload["assignment"]:
                        file = payload["assignment"]["files"][0]
                        self.assertEqual(file["content"], "needle = old\n")
                        self.assertTrue(file["excerpt_only"])
                        operation = {"id": "slice", "path": "a.txt", "base_sha256": file["sha256"]}
                        operation.update({"op": "write_text", "content": "needle = new\n"} if malicious else
                            {"op": "replace_text", "old_text": "needle = old", "new_text": "needle = new"})
                        body["operations"] = [operation]
                        body["sources"] = ["a.txt"]
                    else:
                        after = payload["upstream"][0]["after_files"][0]
                        self.assertEqual(after["content"], "needle = new\n")
                        self.assertTrue(after["excerpt_only"])
                        body["review"] = {"accepted": True, "issues": []}
                    return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
                route = fixtures.route()
                service = self.service([route], adapter)
                definition = self.definition(route)
                definition["nodes"] = [n for n in definition["nodes"] if n["id"] != "second"]
                definition["edges"] = [{"from": "mother", "to": "first"}, {"from": "first", "to": "review", "kind": "report"},
                    {"from": "review", "to": "mother", "kind": "report"}]
                receipt = self.call(service, "submit", request_id="slice" + str(malicious), request={"kind": "workflow", "definition": definition,
                    "execution": {"root": str(workspace), "writable_paths": ["a.txt"]}})
                self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=0, event_id="slice-plan", decision={"type": "assign", "assignments": [
                    {"node_id": "first", "instruction": "replace only line 2", "paths": ["a.txt"], "file_ranges": {"a.txt": [2, 2]}}]})
                run = self.wait_for(service, receipt["dispatch_id"], lambda r: r.get("execution_complete"))
                if malicious:
                    self.assertEqual((workspace / "a.txt").read_text(), original)
                    self.assertEqual(run["nodes"]["first"]["reason"], "sliced_file_requires_unique_replace_within_assigned_lines")
                else:
                    self.assertEqual((workspace / "a.txt").read_text(), "prefix\nneedle = new\nsuffix\n")
                    self.assertEqual(run["nodes"]["review"]["state"], "succeeded")

    def test_child_line_scope_and_large_unsliced_files_require_a_new_bounded_plan(self):
        workspace = self.root / "line-scope"
        workspace.mkdir()
        (workspace / "a.txt").write_text("one\ntwo\nthree\n", encoding="utf-8")
        (workspace / "b.txt").write_text("z" * 49000, encoding="utf-8")
        route = fixtures.route()
        service = self.service([route], lambda *a: self.fail("invalid plan must not issue generation"))
        receipt = self.call(service, "submit", request_id="line-scope", request={"kind": "workflow", "definition": self.definition(route),
            "execution": {"root": str(workspace), "writable_paths": ["a.txt", "b.txt"]}})
        with self.assertRaisesRegex(ValueError, "line range cannot expand"):
            self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=0, event_id="expanded", decision={"type": "assign", "assignments": [
                {"node_id": "first", "instruction": "parent", "paths": ["a.txt"], "file_ranges": {"a.txt": [1, 1]}},
                {"node_id": "second", "instruction": "child", "paths": ["a.txt"], "file_ranges": {"a.txt": [2, 2]}}]})
        with self.assertRaisesRegex(ValueError, "supply file_ranges or split"):
            self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=0, event_id="large", decision={"type": "assign", "assignments": [
                {"node_id": "first", "instruction": "too large", "paths": ["b.txt"]}]})
        self.assertEqual(self.call(service, "topology", dispatch_id=receipt["dispatch_id"])["control_revision"], 0)

    def test_missing_operations_requires_bound_codex_revision_before_generation(self):
        workspace = self.root / "missing-operations"
        workspace.mkdir()
        (workspace / "a.txt").write_text("before", encoding="utf-8")
        generations = []
        def adapter(route, prompt, task, context):
            generations.append(True)
            body = fixtures.content()
            if len(generations) > 1:
                file = json.loads(task.context)["assignment"]["files"][0]
                body["operations"] = [{"id": "replace", "op": "replace_text", "path": "a.txt",
                    "base_sha256": file["sha256"], "old_text": "before", "new_text": "after"}]
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        route = fixtures.route()
        service = self.service([route], adapter)
        definition = self.definition(route)
        definition["nodes"] = [n for n in definition["nodes"] if n["id"] in {"mother", "first"}]
        definition["edges"] = [{"from": "mother", "to": "first"}, {"from": "first", "to": "mother", "kind": "report"}]
        receipt = self.call(service, "submit", request_id="missing-operations", request={"kind": "workflow", "definition": definition,
            "execution": {"root": str(workspace), "writable_paths": ["a.txt"]}})
        ident = receipt["dispatch_id"]
        assignment = {"node_id": "first", "instruction": "replace before with after", "paths": ["a.txt"]}
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=0, event_id="plan",
            decision={"type": "assign", "assignments": [assignment]})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete"))
        node = run["nodes"]["first"]
        self.assertEqual(node["state"], "blocked")
        self.assertEqual(node["reason"], "operator_report_requires_operations")
        self.assertEqual((workspace / "a.txt").read_text(), "before")
        self.assertEqual(generations, [True])
        proposal = run["review_proposals"][0]
        self.assertEqual(proposal["digest"], node["report_digest"])
        self.assertEqual(proposal["job_id"], node["job_id"])
        self.assertEqual(proposal["operator_nodes"], ["first"])
        self.assertTrue(any(action["type"] == "review" for action in run["pending_actions"]))
        with self.assertRaisesRegex(ValueError, "exact current reviewer proposal"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="unbound",
                decision={"type": "revise", "proposal_digest": "invented", "assignment": assignment})
        with self.assertRaisesRegex(ValueError, "already validated complete model report"):
            self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="no-operations-to-replay",
                decision={"type": "retry_operations", "node_id": "first", "proposal_digest": proposal["digest"]})
        self.assertEqual(generations, [True])
        self.call(service, "coordinate", dispatch_id=ident, expected_revision=1, event_id="approved-correction",
            decision={"type": "revise", "proposal_digest": proposal["digest"], "assignment": assignment})
        run = self.wait_for(service, ident, lambda r: r.get("execution_complete") and r["nodes"]["first"]["state"] == "succeeded")
        self.assertEqual((workspace / "a.txt").read_text(), "after")
        self.assertEqual(generations, [True, True])
        self.assertEqual(run["tokens_used"], 200)
        self.assertEqual(run["review_proposals"], [])
        accept = next(a for a in run["pending_actions"] if a["type"] == "accept")
        final = self.call(service, "coordinate", dispatch_id=ident, expected_revision=2, event_id="accept",
            decision={"type": "accept", "evidence_digest": accept["evidence_digest"]})
        self.assertEqual(final["state"], "succeeded")


    def test_known_blocked_operations_can_reuse_validated_report_only_after_codex_confirmation(self):
        workspace = self.root / "cached-operations"
        workspace.mkdir()
        (workspace / "a.txt").write_text("before", encoding="utf-8")
        generations = []
        def adapter(route, prompt, task, context):
            generations.append(True)
            file = json.loads(task.context)["assignment"]["files"][0]
            body = fixtures.content()
            body["operations"] = [{"id": "replace", "op": "replace_text", "path": "a.txt", "base_sha256": file["sha256"],
                                   "old_text": "before", "new_text": "after"}]
            return {"text": json.dumps(body), "usage": {"total_tokens": 100}}
        route = fixtures.route()
        service = self.service([route], adapter)
        definition = self.definition(route)
        definition["nodes"] = [n for n in definition["nodes"] if n["id"] in {"mother", "first"}]
        definition["edges"] = [{"from": "mother", "to": "first"}, {"from": "first", "to": "mother", "kind": "report"}]
        receipt = self.call(service, "submit", request_id="reuse", request={"kind": "workflow", "definition": definition,
            "execution": {"root": str(workspace), "writable_paths": ["a.txt"]}})
        with patch.object(service.operator, "execute", return_value={"state": "failed", "receipts": [], "reason": "test_gate_closed"}):
            self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=0, event_id="plan", decision={"type": "assign", "assignments": [
                {"node_id": "first", "instruction": "replace one string", "paths": ["a.txt"]}]})
            run = self.wait_for(service, receipt["dispatch_id"], lambda r: r.get("execution_complete"))
        self.assertEqual((workspace / "a.txt").read_text(), "before")
        self.assertEqual(generations, [True])
        proposal = run["review_proposals"][0]["digest"]
        service.scheduling.shutdown()
        from gtt_router.scheduling import Scheduling
        from gtt_router.operator_workflow import OperatorWorkflow
        service.scheduling = Scheduling(service)
        service.scheduling.operators = OperatorWorkflow(service.scheduling)
        self.assertEqual(self.call(service, "status", dispatch_id=receipt["dispatch_id"])["state"], "interrupted")
        (workspace / "a.txt").write_text("someone else changed", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "every original file baseline unchanged"):
            self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=1, event_id="conflicting", decision={
                "type": "retry_operations", "node_id": "first", "proposal_digest": proposal})
        (workspace / "a.txt").write_text("before", encoding="utf-8")
        self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=1, event_id="approved-reuse", decision={
            "type": "retry_operations", "node_id": "first", "proposal_digest": proposal})
        run = self.wait_for(service, receipt["dispatch_id"], lambda r: r.get("execution_complete") and r["nodes"]["first"]["state"] == "succeeded")
        self.assertEqual((workspace / "a.txt").read_text(), "after")
        self.assertEqual(generations, [True])
        self.assertEqual(run["tokens_used"], 100)
        self.assertEqual(len(run["job_ids"]), 1)
        events = self.call(service, "events", dispatch_id=receipt["dispatch_id"], limit=64)["events"]
        operation_states = {e["payload"]["state"] for e in events if e["kind"] == "operation_result"}
        self.assertEqual(operation_states, {"blocked", "succeeded"})
        accept = next(a for a in run["pending_actions"] if a["type"] == "accept")
        service.scheduling.shutdown()
        service.scheduling = Scheduling(service)
        service.scheduling.operators = OperatorWorkflow(service.scheduling)
        final = self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=2, event_id="accept-reused", decision={
            "type": "accept", "evidence_digest": accept["evidence_digest"]})
        self.assertEqual(final["state"], "succeeded")

    def test_operator_requires_explicit_grant_and_codex_does_not_self_spawn(self):
        route = fixtures.route()
        service = self.service([route], lambda *a: self.fail("no Codex plan was supplied"))
        with self.assertRaisesRegex(ValueError, "operator_execution_grant_required"):
            self.call(service, "submit", request_id="no-grant", request={"kind": "workflow", "definition": self.definition(route)})
        workspace = self.root / "workspace"
        workspace.mkdir()
        receipt = self.call(service, "submit", request_id="waiting", request={"kind": "workflow", "definition": self.definition(route),
            "execution": {"root": str(workspace), "writable_paths": ["a.txt", "b.txt"]}})
        self.assertEqual(receipt["state"], "awaiting_codex")
        with self.assertRaisesRegex(ValueError, "cannot expand"):
            self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=0, event_id="scope", decision={"type": "assign", "assignments": [
                {"node_id": "first", "instruction": "wrong scope", "paths": ["outside.txt"]}]})
        with self.assertRaisesRegex(ValueError, "revision_conflict"):
            self.call(service, "coordinate", dispatch_id=receipt["dispatch_id"], expected_revision=1, event_id="stale", decision={"type": "assign", "assignments": []})
        public = self.call(service, "status", dispatch_id=receipt["dispatch_id"])
        self.assertNotIn("execution", public["detail"])
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)
        self.call(service, "cancel", dispatch_id=receipt["dispatch_id"])
        self.wait_for(service, receipt["dispatch_id"], lambda r: r["state"] == "cancelled")

    def test_estimated_usage_is_a_subset_and_does_not_double_reduce_remaining_budget(self):
        first_entered, release = threading.Event(), threading.Event()
        def adapter(route, prompt, task, context):
            if task.prompt.startswith("职责：\nfirst"):
                first_entered.set()
                release.wait(4)
                result = fixtures.response()
                result.pop("usage")
                return result
            return fixtures.response()
        route = fixtures.route()
        service = self.service([route], adapter)
        definition = {"id": "budget", "name": "estimated subset", "token_budget": 10000, "max_parallel": 1,
            "nodes": [{"id": label, "title": label, "route_id": route.id, "effort": route.effort or "low",
                       "task": {"kind": "extract", "prompt": label, "max_output_tokens": 1000, "token_budget": 5000, "deadline_s": 5}} for label in ("first", "second")], "edges": []}
        receipt = service.scheduling.submit(definition)
        self.assertTrue(first_entered.wait(2))
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            run = service.scheduling.status(receipt["run_id"], full=True)
            if run["nodes"]["first"]["job_id"]:
                break
            time.sleep(.01)
        first = service.store.job(run["nodes"]["first"]["job_id"])
        estimated = first["attempts"][0]["decision"]["reserved_token_estimate"]
        with service.scheduling.lock:
            run = service.scheduling.status(receipt["run_id"], full=True)
            run["definition"]["token_budget"] = estimated + 5000
            service.scheduling._save(run)
        release.set()
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            result = service.scheduling.status(receipt["run_id"])
            if result["state"] != "running":
                break
            time.sleep(.02)
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual(result["tokens_estimated"], estimated)
        self.assertEqual(result["tokens_used"], estimated + 100)


if __name__ == "__main__":
    unittest.main()
