import json
import threading
import time
import unittest
from unittest.mock import patch

from gtt_router.group_contract import GroupRequest, parse_leader_decision
from gtt_router.groups import GroupJob
from gtt_router.models import Task, Route, WorkerFailure, MAX_OUTPUT_TOKENS
from gtt_router.slicing import SliceQueue, digest, output_budget, requested_output
from tests import test_groups as fixtures


def template(**extra):
    return {"role": "analyst", "kind": "logic_check", "prompt": "Inspect this material", **extra}


class SliceContractTests(unittest.TestCase):
    def request(self, text, **controls):
        return GroupRequest.parse({"topic": "Check supplied evidence", "context": text, "needs_web": False,
                                   "slice_target_chars": 2000, "slice_min_chars": 500,
                                   "slice_max_chars": 4000, **controls})

    def test_unicode_ranges_cover_material_once_and_grow_after_success(self):
        text = ("甲乙🙂 line\n\n" * 3000)
        queue = SliceQueue(self.request(text), [template()])
        ranges = []
        while (record := queue.take()) is not None:
            ranges.append((record["start"], record["end"]))
            self.assertEqual(record["content_hash"], digest(text[record["start"]:record["end"]]))
            queue.finish(record, {"ok": True, "worker": len(ranges)})
        self.assertEqual("".join(text[a:b] for a, b in ranges), text)
        self.assertFalse(queue.incomplete())
        self.assertGreater(queue.state["target_chars"], 2000)
        self.assertEqual(len({r["id"] for r in queue.state["records"]}), len(ranges))

    def test_completed_truncation_splits_only_failed_range(self):
        queue = SliceQueue(self.request("x" * 6000), [template()])
        first = queue.take()
        queue.finish(first, {"ok": False, "error": {"code": "output_truncated"}})
        self.assertEqual(first["state"], "split")
        children = queue.state["records"][1:]
        self.assertEqual([(r["start"], r["end"]) for r in children], [(0, 1000), (1000, 2000)])
        self.assertTrue(all(r["parent"] == first["id"] for r in children))
        self.assertEqual(queue.state["target_chars"], 1000)

    def test_unknown_timeout_is_never_automatically_split_or_replayed(self):
        queue = SliceQueue(self.request("x" * 6000), [template()])
        first = queue.take()
        queue.finish(first, {"ok": False, "error": {"code": "timeout"}})
        self.assertEqual(first["state"], "unknown")
        self.assertEqual(len(queue.state["records"]), 1)
        self.assertNotEqual(queue.take()["id"], first["id"])
        self.assertTrue(queue.incomplete())

    def test_slice_limit_cannot_hide_unassigned_source(self):
        queue = SliceQueue(self.request("x" * 20000, max_slices=2), [template()])
        for i in range(2):
            queue.finish(queue.take(), {"ok": True, "worker": i})
        self.assertIsNone(queue.take())
        self.assertTrue(queue.incomplete())

    def test_scope_and_template_hash_are_part_of_provenance(self):
        req = self.request("x" * 6000)
        one = SliceQueue(req, [template()])
        two = SliceQueue(req, [template(prompt="Different analysis")])
        self.assertNotEqual(one.take()["id"], two.take()["id"])
        changed = self.request("y" * 6000)
        with self.assertRaises(ValueError):
            SliceQueue(changed, [template()], one.state)

    def test_semantic_followup_uses_only_requested_range_or_report(self):
        req = self.request("x" * 12000)
        queue = SliceQueue(req, [template(context_range=[500, 1500]), template(evidence_workers=[3])])
        first = queue.take()
        self.assertEqual((first["start"], first["end"]), (500, 1500))
        queue.finish(first, {"ok": True, "worker": 1})
        second = queue.take()
        self.assertEqual((second["start"], second["end"]), (0, 0))
        with self.assertRaises(ValueError):
            SliceQueue(req, [template(context_range=[0, 20000])]).take()

    def test_512k_native_and_explicit_output_and_shared_context_headroom(self):
        route = Route("fixture", "p", "big", output_limit=512000)
        task = Task.parse({"kind": "logic_check", "prompt": "check", "max_output_tokens": MAX_OUTPUT_TOKENS})
        self.assertGreater(task.token_budget, 512000)
        self.assertEqual(output_budget(task, route)["effective_output_tokens"], 512000)
        native = Task.parse({"kind": "logic_check", "prompt": "check", "output_mode": "native"})
        self.assertEqual(output_budget(native, route)["effective_output_tokens"], 512000)
        route.context_window = 4096
        self.assertEqual(output_budget(task, route, 2000)["effective_output_tokens"], 3096)
        fixed = Task.parse({"kind": "logic_check", "prompt": "x" * 20000, "max_output_tokens": 512})
        self.assertEqual(output_budget(fixed, route)["effective_output_tokens"], -5904)
        adaptive = Task.parse({"kind": "logic_check", "prompt": "x" * 32000})
        self.assertGreater(requested_output(adaptive), 32768)

    def test_expanded_queue_contract_does_not_expand_role_seats(self):
        decision = parse_leader_decision({"action": "continue_research", "next_tasks": [template(prompt=str(i)) for i in range(24)]}, max_tasks=64)
        self.assertEqual(len(decision["next_tasks"]), 24)
        for fields in ({"slice_min_chars": 3000, "slice_target_chars": 2000}, {"max_parallel": True},
                       {"exploration_budget_ratio": .4}, {"max_slices": 257}):
            with self.assertRaises(ValueError):
                GroupRequest.parse({"topic": "bounded", **fields})
        with self.assertRaises(ValueError):
            parse_leader_decision({"action": "continue_research", "next_tasks": [template(context_range=[True, 10])]}, max_tasks=64)

    def test_support_queue_does_not_skip_unassigned_primary_tasks(self):
        queue = SliceQueue(self.request("x" * 1000), [template(prompt="first"), template(prompt="second")])
        first = queue.take()
        result = {"ok": True, "worker": 1, "information_requests": [
            {"question": "Need definition", "reason": "Missing context", "context_range": [10, 100]}]}
        queue.finish(first, result)
        queue.support(first, result)
        support = queue.take()
        self.assertEqual(support["support_depth"], 1)
        queue.finish(support, {"ok": True, "worker": 2})
        second = queue.take()
        self.assertEqual(second["template"], 1)
        queue.finish(second, {"ok": True, "worker": 3})
        self.assertFalse(queue.incomplete())
        self.assertIsNone(queue.take())

    def test_recursive_support_is_bounded_to_two_hops(self):
        queue = SliceQueue(self.request("x" * 1000), [template(context_range=[0, 100])])
        for i in range(3):
            record = queue.take()
            result = {"ok": True, "worker": i, "information_requests": [
                {"question": "Need another definition", "reason": "Missing dependency", "context_range": [200 + i * 100, 250 + i * 100]}]}
            queue.finish(record, result)
            queue.support(record, result)
        self.assertIsNone(queue.take())
        self.assertEqual(len(queue.state["records"]), 3)

    def test_split_minimum_and_support_depth_survive_paragraph_boundaries(self):
        queue = SliceQueue(self.request("x" * 300 + "\n" + "x" * 1699, slice_target_chars=1000), [template(context_range=[0, 1000])])
        record = queue.take()
        record["support_depth"] = 2
        queue.finish(record, {"ok": False, "error": {"code": "output_truncated"}})
        children = queue.state["records"][1:]
        self.assertEqual([r["end"] - r["start"] for r in children], [500, 500])
        self.assertTrue(all(r["support_depth"] == 2 for r in children))

    def test_native_default_budget_can_reserve_a_reported_512k_route(self):
        task = Task.parse({"kind": "logic_check", "prompt": "check", "output_mode": "native"})
        self.assertGreater(task.token_budget, 512000)
        self.assertLessEqual(task.token_budget, 4000000)

    def test_explicit_large_range_is_partitioned_without_skipping_following_task(self):
        req = self.request("x" * 20000)
        queue = SliceQueue(req, [template(context_range=[3000, 12000]), template(context_range=[15000, 16000])])
        ranges = []
        while (record := queue.take()) is not None:
            ranges.append((record["start"], record["end"]))
            self.assertLessEqual(record["end"] - record["start"], req.slice_max_chars)
            queue.finish(record, {"ok": True, "worker": len(ranges)})
        self.assertEqual(ranges[0][0], 3000)
        self.assertEqual(ranges[-2][1], 12000)
        self.assertEqual(ranges[-1], (15000, 16000))
        self.assertEqual(sum(b - a for a, b in ranges), 10000)
        self.assertFalse(queue.incomplete())


class DynamicSchedulingTests(unittest.TestCase):
    setUp = fixtures.GroupTests.setUp
    tearDown = fixtures.GroupTests.tearDown
    service = fixtures.GroupTests.service
    submit = fixtures.GroupTests.submit
    wait = fixtures.GroupTests.wait

    def test_large_work_is_partitioned_and_completed_without_full_context_replication(self):
        seen = []
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            data = json.loads(task.context)
            seen.append(data)
            return fixtures.reply()
        service = self.service(adapter)
        text = "资料abc\n\n" * 6000
        group = self.submit(service, context=text, member_count=3, slice_target_chars=2000,
                            slice_min_chars=500, slice_max_chars=4000, max_slices=128, max_parallel=3)
        state = self.wait(service, group)
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertGreater(len(seen), 3)
        ordered = sorted(seen, key=lambda d: d["slice"]["start"])
        self.assertEqual("".join(d["supplied_context"] for d in ordered), text)
        self.assertTrue(all(len(d["supplied_context"]) <= 4000 for d in seen))
        self.assertEqual(state["tokens_reserved"], 0)

    def test_completing_one_slice_immediately_refills_while_other_slice_is_slow(self):
        slow = threading.Event()
        refilled = threading.Event()
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            slot = json.loads(task.context)["slice"]["slot"]
            if slot == 1:
                slow.wait(2)
            if slot == 4:
                refilled.set()
                slow.set()
            return fixtures.reply()
        service = self.service(adapter)
        group = self.submit(service, context="x" * 20000, member_count=3, max_parallel=3,
                            slice_target_chars=2000, slice_min_chars=500, slice_max_chars=3000)
        self.assertTrue(refilled.wait(1.5))
        self.assertEqual(self.wait(service, group)["pending_action"]["type"], "submit_codex")

    def test_empty_dispatch_queue_does_not_rescan_routes_while_waiting(self):
        gate, entered = threading.Event(), threading.Event()
        count = [0]
        lock = threading.Lock()
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            with lock:
                count[0] += 1
                if count[0] == 2:
                    entered.set()
            gate.wait(2)
            return fixtures.reply()
        service = self.service(adapter)
        with patch.object(service.groups, "_candidates", wraps=service.groups._candidates) as candidates:
            group = self.submit(service, max_parallel=3)
            self.assertTrue(entered.wait(1))
            before = candidates.call_count
            time.sleep(.3)
            self.assertEqual(candidates.call_count, before)
            gate.set()
            self.wait(service, group)

    def test_logical_slice_retains_reasoning_wait_headroom(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "context": "x" * 32000})
        job = GroupJob(req)
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        queue = SliceQueue(req, [template()])
        record = queue.take()
        task = service.groups._member_task(job, template(), 1, record)
        self.assertGreaterEqual(task.ttft_s, 600)
        self.assertLessEqual(task.ttft_s, 3600)

    def test_explicit_material_range_is_respected_when_automatic_slicing_is_off(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "context": "x" * 8000, "slice_mode": "off"})
        job = GroupJob(req)
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        task = service.groups._member_task(job, template(context_range=[500, 1000]), 1)
        context = json.loads(task.context)
        self.assertEqual(len(context["supplied_context"]), 500)
        self.assertEqual(context["material_range"]["start"], 500)

    def test_truncation_is_metered_and_subranges_replace_parent(self):
        failed = []
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            record = json.loads(task.context)["slice"]
            if record["start"] == 0 and record["parent"] is None and not failed:
                failed.append(record["id"])
                raise WorkerFailure("output_truncated", usage={"totalTokens": 23}, retry_after_s=0)
            return fixtures.reply()
        service = self.service(adapter)
        group = self.submit(service, context="x" * 10000, slice_target_chars=2000,
                            slice_min_chars=500, slice_max_chars=4000)
        state = self.wait(service, group)
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertEqual(state["slicing"]["counts"]["split"], 1)
        attempts = service.store.job(group)["attempts"]
        self.assertEqual(sum(a["tokens_charged"] for a in attempts), state["tokens_used"])
        self.assertTrue(any(a["tokens_charged"] == 23 for a in attempts))

    def test_budget_too_large_on_first_route_tries_a_smaller_qualified_route(self):
        candidates = fixtures.routes()[::2]
        candidates[0].output_limit = 512000
        candidates[1].output_limit = 256
        seen = []
        service = self.service(lambda r, p, t, c: seen.append(t.max_output_tokens) or fixtures.reply(), candidates)
        job = service.submit({"kind": "logic_check", "prompt": "small", "max_output_tokens": 524288, "token_budget": 5000})
        deadline = time.monotonic() + 3
        while service.status(job["job_id"])["state"] == "queued" or service.status(job["job_id"])["state"] == "running":
            self.assertLess(time.monotonic(), deadline)
            time.sleep(.01)
        self.assertEqual(service.status(job["job_id"])["state"], "succeeded")
        self.assertEqual(seen, [256])

    def test_information_request_receives_targeted_support_before_acceptance(self):
        requested = []
        supported = []
        def adapter(route, prompt, task, ctx):
            data = json.loads(task.context)
            if fixtures.is_leader(task):
                if fixtures.initial(task):
                    return fixtures.reply({"action": "continue_research"})
                if data["round"] == 1:
                    worker = data["information_requests"][0]["worker"]
                    requested.append(worker)
                    if data["information_requests"][0]["support_workers"]:
                        return fixtures.reply({"action": "submit_codex"})
                    return fixtures.reply({"action": "continue_research", "next_tasks": [template(
                        context_range=[5000, 5500], evidence_workers=[worker], prompt="Resolve missing definition")]})
                return fixtures.reply({"action": "submit_codex"})
            if data["requested_evidence"]:
                supported.append(data)
                return fixtures.reply()
            response = fixtures.reply()
            if data["slice"]["slot"] == 1:
                content = json.loads(response["text"])
                content["information_requests"] = [{"question": "Where is the definition?", "reason": "Cannot infer its contract",
                                                     "context_range": [5000, 5500]}]
                response["text"] = json.dumps(content)
            return response
        service = self.service(adapter)
        group = self.submit(service, context="x" * 8000)
        state = self.wait(service, group)
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertEqual(len(supported), 1)
        self.assertEqual(len(supported[0]["supplied_context"]), 500)
        self.assertEqual(supported[0]["requested_evidence"][0]["worker"], requested[0])
        events, cursor = [], 0
        for _ in range(10):
            page = service.coordination.events("default", cursor, 64)
            events.extend(page["events"])
            if page["next_cursor"] == cursor:
                break
            cursor = page["next_cursor"]
        self.assertTrue(any(e["type"] == "information_request" for e in events))

    def test_unresolved_request_blocks_unsupported_submit(self):
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            response = fixtures.reply()
            content = json.loads(response["text"])
            content["information_requests"] = [{"question": "Missing input", "reason": "Need evidence"}]
            response["text"] = json.dumps(content)
            return response
        service = self.service(adapter)
        state = self.wait(service, self.submit(service))
        self.assertEqual(state["pending_action"]["type"], "unresolved_information_requests")

    def test_support_starts_while_another_member_is_still_waiting(self):
        support = threading.Event()
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            data = json.loads(task.context)
            if data["slice"]["slot"] == 2:
                support.wait(2)
            if data["requested_evidence"]:
                support.set()
                return fixtures.reply()
            response = fixtures.reply()
            if data["slice"]["slot"] == 1:
                content = json.loads(response["text"])
                content["information_requests"] = [{"question": "Need definition", "reason": "Missing dependency", "context_range": [3000, 3500]}]
                response["text"] = json.dumps(content)
            return response
        service = self.service(adapter)
        group = self.submit(service, context="x" * 8000, max_parallel=3)
        self.assertTrue(support.wait(1))
        self.assertEqual(self.wait(service, group)["pending_action"]["type"], "submit_codex")

    def test_deadline_retains_new_support_for_resume_without_replaying_success(self):
        entered = threading.Event()
        expired = threading.Event()
        calls = []
        service = None
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            data = json.loads(task.context)
            slot = data["slice"]["slot"]
            calls.append(slot)
            if slot == 2:
                entered.set()
                expired.wait(2)
            response = fixtures.reply()
            if slot == 1:
                entered.wait(2)
                job = next(iter(service.groups.jobs.values()))
                job.deadline = time.monotonic() - .01
                expired.set()
                content = json.loads(response["text"])
                content["information_requests"] = [{"question": "Need supporting range", "reason": "Missing dependency", "context_range": [100, 200]}]
                response["text"] = json.dumps(content)
            return response
        service = self.service(adapter)
        group = self.submit(service, context="x" * 1000, max_parallel=3)
        paused = self.wait(service, group)
        self.assertEqual(paused["pending_action"]["type"], "incomplete_slice_evidence")
        self.assertEqual(paused["phase"], "research_round")
        self.assertEqual(paused["slicing"]["counts"]["pending"], 1)
        service.groups.resume(group, "Continue only queued supporting evidence")
        after = self.wait(service, group)
        self.assertEqual(after["pending_action"]["type"], "submit_codex")
        self.assertEqual(sorted(calls), [1, 2, 3])

    def test_exploration_off_is_enforced_even_for_initial_plan(self):
        calls = []
        lead = {"question": "Explore blind spot", "rationale": "Potential failure", "priority": "normal", "expected_value": "Resolve uncertainty"}
        def adapter(route, prompt, task, ctx):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research", "exploration": True, "research_leads": [lead], "next_tasks": [template()]})
            calls.append(task)
            return fixtures.reply()
        service = self.service(adapter)
        state = self.wait(service, self.submit(service, exploration_mode="off"))
        self.assertEqual(state["pending_action"]["type"], "exploration_limit")
        self.assertEqual(calls, [])

    def test_exploration_reservations_cannot_exceed_its_budget_fraction(self):
        lead = {"question": "Explore blind spot", "rationale": "Potential failure", "priority": "normal", "expected_value": "Resolve uncertainty"}
        calls = []
        def adapter(route, prompt, task, ctx):
            calls.append(task.kind)
            if fixtures.is_leader(task):
                if fixtures.initial(task):
                    return fixtures.reply({"action": "continue_research"})
                return fixtures.reply({"action": "continue_research", "exploration": True,
                                       "research_leads": [lead], "next_tasks": [template()]})
            return fixtures.reply()
        candidates = fixtures.routes()
        for route in candidates:
            route.output_limit = 4096
        service = self.service(adapter, candidates)
        state = self.wait(service, self.submit(service, token_budget=60000, exploration_budget_ratio=.01))
        self.assertEqual(state["pending_action"]["type"], "exploration_limit")
        self.assertEqual(len(calls), 6)  # leader reviews remain reachable on the main budget
        self.assertEqual(state["exploration_accounting"]["tokens_used"], 0)
        self.assertEqual(state["tokens_reserved"], 0)

    def test_guided_exploration_runs_bounded_followup_and_reports_lead(self):
        def adapter(route, prompt, task, ctx):
            if not fixtures.is_leader(task):
                return fixtures.reply()
            data = json.loads(task.context)
            if fixtures.initial(task):
                return fixtures.reply({"action": "continue_research"})
            lead = {"question": "Check boundary failure", "rationale": "Untested assumption", "priority": "high",
                    "expected_value": "Find a counterexample", "evidence_refs": []}
            if data["round"] == 1:
                return fixtures.reply({"action": "continue_research", "exploration": True, "research_leads": [lead],
                                       "next_tasks": [template(prompt="Check the boundary counterexample")]})
            return fixtures.reply({"action": "submit_codex"})
        service = self.service(adapter)
        state = self.wait(service, self.submit(service))
        self.assertEqual(state["exploration_rounds"], 1)
        self.assertEqual(state["round"], 2)
        self.assertEqual(state["research_leads"][0]["priority"], "high")
        self.assertEqual(state["pending_action"]["type"], "submit_codex")

    def test_restart_retains_completed_slices_and_requires_opt_in_for_unknown(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "context": "x" * 24000, "needs_web": False})
        job = GroupJob(req)
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        job.round, job.phase, job.state = 1, "research_round", "interrupted"
        job.next_tasks = [template()]
        queue = SliceQueue(req, job.next_tasks)
        one = queue.take()
        queue.finish(one, {"ok": True, "worker": 0})
        unknown = queue.take()
        unknown["state"] = "running"
        job.slice_state = queue.state
        job.calls = [{"worker": 1, "slot": 2, "round": 1, "role": "analyst"}]
        job.attempts = [{"worker": 1, "state": "running", "decision": {"reserved_token_estimate": 200}}]
        job.results = [{"worker": 0, "slot": 1, "round": 1, "role": "analyst", "ok": True}]
        service._save(job)
        restored = service.groups._restore(job.id)
        self.assertEqual(restored.slice_state["records"][0]["state"], "succeeded")
        self.assertEqual(restored.slice_state["records"][1]["state"], "unknown")
        with self.assertRaisesRegex(ValueError, "Unknown paid"):
            service.groups.resume(job.id, "Inspect pending source")

    def test_caller_handoff_updates_slice_state_without_duplicate_metering(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False})
        job = GroupJob(req)
        job.state, job.round, job.phase = "needs_codex", 1, "leader_review"
        queue = SliceQueue(req, [template()])
        record = queue.take()
        record["state"] = "failed"
        job.slice_state = queue.state
        job.results = [{"worker": 5, "slot": record["slot"], "slice": dict(record), "round": 1,
                        "phase": "research_round", "role": "analyst", "ok": False,
                        "codex_fallback": {"recommended_model": "gpt-6-luna"}}]
        service.groups.jobs[job.id] = job
        service._save(job)
        content = json.loads(fixtures.reply()["text"])
        service.complete_codex(job.id, 5, "gpt-6-luna", content, {"total_tokens": 23})
        self.assertEqual(service.groups._missing_slots(job), [])
        self.assertEqual(job.tokens_used, 23)
        with self.assertRaises(ValueError):
            service.complete_codex(job.id, 5, "gpt-6-luna", content, {"total_tokens": 23})
        self.assertEqual(job.tokens_used, 23)


if __name__ == "__main__":
    unittest.main()
