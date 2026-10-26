import json
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import patch

from gtt_router.groups import GroupJob
from gtt_router.group_contract import GroupRequest
from gtt_router.slicing import SliceQueue
from gtt_router.models import Task
from gtt_router.service import Job
from tests import test_groups as fixtures
from tests import test_coordination as coordination


def item(**fields):
    return {"role": "analyst", "kind": "logic_check", "prompt": "Check supplied evidence", **fields}


class BoundedLock:
    """Make an actual lock inversion fail instead of hanging the test runner."""
    def __init__(self):
        self.lock = threading.RLock()

    def __enter__(self):
        if not self.lock.acquire(timeout=.5):
            raise TimeoutError("Group control lock inversion")
        return self

    def __exit__(self, *args):
        self.lock.release()


class SchedulerCouplingTests(unittest.TestCase):
    setUp = fixtures.GroupTests.setUp
    tearDown = fixtures.GroupTests.tearDown
    service = fixtures.GroupTests.service
    wait = fixtures.GroupTests.wait
    submit = fixtures.GroupTests.submit
    seed = coordination.CoordinationTests.seed
    merge = coordination.CoordinationTests.merge

    def test_cached_review_can_aggregate_many_reports_without_repaying_leader(self):
        leaders = []
        def adapter(route, prompt, task, context):
            if fixtures.is_leader(task):
                leaders.append(task.prompt)
                return fixtures.reply({"action": "submit_codex"})
            data = json.loads(task.context)
            self.assertEqual(len(data["requested_evidence"]), 16)
            self.assertEqual(data["prior_evidence"], [])
            self.assertLess(len(json.dumps(data["requested_evidence"])), 19000)
            return fixtures.reply()
        service = self.service(adapter)
        req = GroupRequest.parse({"topic": "Aggregate supplied reports", "needs_web": False, "max_slices": 64})
        job = GroupJob(req)
        job.roles = service.groups.plan({"topic": req.topic, "needs_web": False})["roles"]
        job.state, job.phase, job.round = "needs_codex", "leader_review", 1
        job.expected_slots = list(range(1, 17))
        for worker in range(1, 17):
            result = {"worker": worker, "slot": worker, "round": 1, "role": "collector", "ok": True,
                "route": fixtures.routes()[1].public(), "content": {"summary": "x" * 5000}}
            if worker <= 5:
                result["content"]["information_requests"] = [{"question": "Need aggregate", "reason": "Missing synthesis"}]
                result["information_requests"] = service.groups._information_requests(job, result)
            job.results.append(result)
        decision = {"action": "continue_research", "feedback": [{"worker": 12, "accepted": True, "reason": "checked"}],
            "next_tasks": [item(role="collector", kind="extract", evidence_workers=list(range(1, 17)),
                information_request_ids=[f"{worker}:0" for worker in range(1, 6)])]}
        job.results.append({"worker": 17, "slot": 0, "round": 1, "phase": "leader_review", "role": "leader", "ok": True,
            "route": fixtures.routes()[0].public(), "content": {"summary": "saved review", "group_decision": decision}})
        job.report_worker = 18
        job.pending_action = {"type": "invalid_leader_decision"}
        service.groups.jobs[job.id] = job
        service._save(job)
        service.catalog.routes = []
        service.catalog.refreshed_at = 0
        def refresh(force=False):
            service.catalog.routes = fixtures.routes()
            service.catalog.refreshed_at = int(time.time() * 1000)
        with patch.object(service.catalog, "refresh", side_effect=refresh) as loaded:
            service.groups.resume(job.id, "Retry the validated cached decision after restart")
            state = self.wait(service, job.id)
            loaded.assert_called()
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertEqual(len(leaders), 1)
        self.assertEqual(service.groups._unresolved_requests(job), {})

    def test_last_round_resume_can_finish_already_assigned_slices(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "max_rounds": 1})
        job = GroupJob(req)
        job.state, job.phase, job.round = "needs_codex", "research_round", 1
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        job.next_tasks = [item()]
        queue = SliceQueue(req, job.next_tasks)
        queue.take()
        job.slice_state = queue.state
        job.pending_action = {"type": "incomplete_slice_evidence"}
        service.groups.jobs[job.id] = job
        service._save(job)
        service.groups.resume(job.id, "Finish existing round without adding a round")
        state = self.wait(service, job.id)
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertEqual(state["round"], 1)
        self.assertEqual(job.request.max_rounds, 1)

    def test_one_support_does_not_resolve_two_distinct_information_requests(self):
        def adapter(route, prompt, task, context):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            response = fixtures.reply()
            data = json.loads(task.context)
            if data["slice"]["slot"] == 1:
                content = json.loads(response["text"])
                content["information_requests"] = [
                    {"question": "Need supplied definition", "reason": "Missing dependency", "context_range": [100, 200]},
                    {"question": "Need independent source", "reason": "No evidence for external claim"},
                ]
                response["text"] = json.dumps(content)
            return response
        service = self.service(adapter)
        state = self.wait(service, self.submit(service, context="x" * 1000))
        self.assertEqual(state["pending_action"]["type"], "unresolved_information_requests")

    def test_exploration_budget_cannot_block_a_later_primary_call(self):
        candidates = fixtures.routes()
        for route in candidates:
            route.output_limit = 4096
        service = self.service(candidates=candidates)
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "token_budget": 100000,
                                  "exploration_budget_ratio": .001})
        job = GroupJob(req)
        job.state = "running"
        job.tokens_used, job.exploration_tokens_start = 1000, 100
        job.exploration_active = False
        task = service.groups._task(req, kind="extract", prompt="Primary evidence repair", role="collector")
        result = service._worker(job, task, 0, job.deadline)
        self.assertTrue(result["ok"])
        self.assertEqual(job.tokens_reserved, 0)

    def test_control_and_merge_follow_same_lock_order(self):
        service = self.service()
        target = self.seed(service, workflow="default")
        one, two = self.seed(service, workflow="default"), self.seed(service, workflow="default")
        service.groups.lock = BoundedLock()
        approval_entered, capacity_held = threading.Event(), threading.Event()
        original = service.groups._approved_job
        def approval(*args, **kwargs):
            approval_entered.set()
            if not service.capacity._is_owned():
                self.assertTrue(capacity_held.wait(1))
            return original(*args, **kwargs)
        def merge():
            self.assertTrue(approval_entered.wait(1))
            with service.capacity:
                capacity_held.set()
                return self.merge(service, one, two)
        with patch.object(service.groups, "_approved_job", side_effect=approval):
            with ThreadPoolExecutor(max_workers=2) as pool:
                continued = pool.submit(service.groups.resume, target.id, "Primary review")
                combined = pool.submit(merge)
                continued.result(3)
                self.assertEqual(combined.result(3)["state"], "needs_codex")

    def test_merge_cannot_seal_pending_slice_work_permanently_read_only(self):
        service = self.service()
        one, two = self.seed(service), self.seed(service)
        queue = SliceQueue(one.request, [item()])
        queue.take()
        one.slice_state = queue.state
        one.phase = "research_round"
        service._save(one)
        with self.assertRaises(ValueError):
            self.merge(service, one, two)
        self.assertNotEqual(service.groups.status(one.id)["state"], "merged")

    def test_wrapper_headroom_failure_is_immediate_and_never_handed_to_codex(self):
        routes = fixtures.routes()[::2]
        for route in routes:
            route.context_window = 600
        calls = []
        service = self.service(lambda *args: calls.append(args), routes)
        task = Task.parse({"kind": "extract", "prompt": "short"})
        job = Job(task)
        started = time.monotonic()
        result = service._worker(job, task, 0, job.deadline)
        self.assertEqual(result["error"]["code"], "context_exceeded")
        self.assertNotIn("codex_fallback", result)
        self.assertEqual((calls, job.tokens_used, job.attempt_count), ([], 0, 0))
        self.assertLess(time.monotonic() - started, .5)

    def test_affordable_busy_route_is_waited_for_after_expensive_route_is_rejected(self):
        routes = fixtures.routes()[::2]
        routes[0].output_limit, routes[1].output_limit = 100000, 256
        service = self.service(candidates=routes)
        task = Task.parse({"kind": "logic_check", "prompt": "short", "max_output_tokens": 65536, "token_budget": 5000})
        job = Job(task)
        service.inflight[routes[1].source] = routes[1].max_parallel
        service.total_inflight = 2
        with ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(service._worker, job, task, 0, job.deadline)
            time.sleep(.1)
            self.assertFalse(future.done())
            with service.capacity:
                service.inflight[routes[1].source] = 0
                service.total_inflight = 0
                service.capacity.notify_all()
            result = future.result(2)
        self.assertTrue(result["ok"])
        self.assertEqual(result["route"]["route_id"], routes[1].id)

    def test_scope_wait_expiry_keeps_capacity_reason_and_no_codex_handoff(self):
        service = self.service()
        scope = service.preferences("set", preferences={"max_parallel": 1})["scope_id"]
        service.sync_codex_agents("fixture-busy", [{"id": "fixture-busy", "status": "running", "workflow_id": "default", "preference_scope": scope}], 600)
        task = Task.parse({"kind": "extract", "prompt": "short", "preference_scope": scope})
        job = Job(task)
        result = service._worker(job, task, 0, time.monotonic() + .03)
        self.assertEqual(result["error"]["code"], "scope_capacity_full")
        self.assertNotIn("codex_fallback", result)
        self.assertEqual(job.tokens_used, 0)

    def test_permanent_budget_miss_does_not_wait_on_an_unrelated_reservation(self):
        routes = fixtures.routes()[::2]
        for route in routes:
            route.profile["latency_prior_s"] = .001
        service = self.service(candidates=routes)
        task = Task.parse({"kind": "logic_check", "prompt": "short", "token_budget": 3000})
        job = Job(task)
        job.tokens_used, job.tokens_reserved = 2800, 100
        result = service._worker(job, task, 0, time.monotonic() + .03)
        self.assertEqual(result["error"]["code"], "token_budget_exhausted")
        self.assertNotIn("codex_fallback", result)

    def test_split_support_closes_a_request_only_after_all_subranges_are_present(self):
        service = self.service()
        job = GroupJob(GroupRequest.parse({"topic": "fixture", "needs_web": False}))
        source = {"worker": 1, "ok": True, "content": {"information_requests": [{"question": "Need definition", "reason": "Missing input"}]}}
        job.results = [source]
        source["information_requests"] = service.groups._information_requests(job, source)
        support = {"id": "support-scope", "start": 100, "end": 200}
        job.results.append({"worker": 2, "ok": True, "responds_to_requests": ["1:0"], "support": support, "slice": {"start": 100, "end": 150}})
        self.assertIn("1:0", service.groups._unresolved_requests(job))
        job.results.append({"worker": 3, "ok": True, "responds_to_requests": ["1:0"], "support": support, "slice": {"start": 150, "end": 200}})
        self.assertEqual(service.groups._unresolved_requests(job), {})

    def test_malformed_advisory_communication_does_not_invalidate_a_valid_request(self):
        service = self.service()
        job = self.seed(service, workflow="default")
        report = job.results[0]
        report["content"]["communications"] = [{"to_role": "missing-role", "summary": "invalid advisory recipient"}]
        report["content"]["information_requests"] = [{"question": "Need definition", "reason": "Missing input"}]
        service.groups._publish_member_report(job, report)
        self.assertIn("communication_warning", report)
        self.assertNotIn("information_request_error", report)
        self.assertEqual(len(report["information_requests"]), 1)

    def test_exploration_usage_is_reserved_charged_and_released_once(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "token_budget": 100000})
        job = GroupJob(req)
        job.state, job.exploration_active = "running", True
        task = service.groups._task(req, kind="extract", prompt="Explore a supplied lead", role="collector")
        task._exploration = True
        result = service._worker(job, task, 0, job.deadline)
        self.assertTrue(result["ok"])
        self.assertEqual((job.tokens_used, job.exploration_tokens_used), (10, 10))
        self.assertEqual((job.tokens_reserved, job.exploration_tokens_reserved), (0, 0))
        self.assertTrue(job.attempts[0]["exploration"])

    def test_successful_initial_plan_survives_restart_without_paid_replanning(self):
        planning_calls, prompts = [], []
        def adapter(route, prompt, task, context):
            if fixtures.is_leader(task):
                if fixtures.initial(task):
                    planning_calls.append(task)
                return fixtures.reply({"action": "submit_codex"})
            prompts.append(task.prompt)
            return fixtures.reply()
        service = self.service(adapter)
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False})
        job = GroupJob(req)
        job.state, job.phase = "interrupted", "leader_plan"
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        job.leader_route = job.roles[0]["route"]["route_id"]
        content = json.loads(fixtures.reply({"action": "continue_research", "next_tasks": [item(prompt="Retain the paid plan")]})["text"])
        job.results = [{"worker": 0, "role": "leader", "round": 0, "phase": "leader_plan", "ok": True,
                        "route": job.roles[0]["route"], "content": content}]
        job.report_worker, job.tokens_used = 1, 10
        service._save(job)
        service.groups.resume(job.id, "Recover acknowledged planning result")
        state = self.wait(service, job.id)
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertEqual(planning_calls, [])
        self.assertTrue(any(p.startswith("Retain the paid plan") for p in prompts))
        self.assertEqual(state["tokens_used"], 40)

    def test_merge_preserves_source_parallel_and_exploration_constraints(self):
        service = self.service()
        one, two = self.seed(service), self.seed(service)
        one.request.max_parallel = 1
        one.request.exploration_mode = "off"
        one.request.exploration_budget_ratio = .01
        one.request.output_mode = two.request.output_mode = "native"
        one.exploration_tokens_used = 10
        two.exploration_tokens_used = 20
        merged = self.merge(service, one, two)
        combined = service.groups.jobs[merged["group_id"]]
        self.assertEqual(combined.request.max_parallel, 1)
        self.assertEqual(combined.request.exploration_mode, "off")
        self.assertEqual(combined.request.exploration_budget_ratio, .01)
        self.assertEqual(combined.request.output_mode, "native")
        self.assertEqual(combined.exploration_tokens_used, 30)

    def test_late_codex_coverage_blocks_a_queued_external_request(self):
        calls = []
        service = self.service(lambda *args: calls.append(args))
        task = Task.parse({"kind": "extract", "prompt": "short", "work_key": "same-purpose"})
        job = Job(task)
        service.sync_codex_agents("fixture-late-owner", [{"id": "fixture-late", "status": "running", "workflow_id": "default", "work_key": task.work_key}], 600)
        result = service._worker(job, task, 0, job.deadline)
        self.assertEqual(result["error"]["code"], "work_already_covered")
        self.assertEqual((calls, job.attempt_count, job.tokens_used), ([], 0, 0))

    def test_dispatcher_scope_queue_honors_explicit_wait_before_worker_start(self):
        service = self.service()
        scope = service.preferences("set", preferences={"max_parallel": 1})["scope_id"]
        service.sync_codex_agents("fixture-full-scope", [{"id": "fixture-full", "status": "running", "workflow_id": "default", "preference_scope": scope}], 600)
        submitted = service.submit({"kind": "extract", "prompt": "short", "preference_scope": scope, "queue_wait_s": .03})
        until = time.monotonic() + 1
        while time.monotonic() < until:
            status = service.status(submitted["job_id"])
            if status["state"] == "failed":
                break
            time.sleep(.01)
        self.assertEqual(status["state"], "failed")
        self.assertEqual(status["error"]["code"], "scope_capacity_full")
        self.assertEqual((status["attempt_count"], status["tokens_used"]), (0, 0))

    def test_support_task_does_not_rewrite_the_persistent_collector_seat(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "context": "x" * 1000})
        job = GroupJob(req)
        job.state, job.round = "running", 1
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        queue = SliceQueue(req, [item(context_range=[0, 500])])
        record = queue.take()
        record["slot"] = 3  # two-seat roster wraps this work item onto member-1
        task = service.groups._member_task(job, item(context_range=[0, 500]), 3, record)
        result = service.groups._call(job, task, 3, "analyst")
        self.assertTrue(result["ok"])
        self.assertEqual(job.roles[1]["role"], "collector")
        self.assertEqual(service.groups._seed_tasks(job)[0]["role"], "collector")

    def test_invalid_task_reference_returns_to_leader_without_a_stuck_running_slice(self):
        seen = []
        def adapter(route, prompt, task, context):
            if fixtures.is_leader(task):
                if fixtures.initial(task):
                    return fixtures.reply({"action": "continue_research", "next_tasks": [
                        item(prompt="Invalid reference", evidence_workers=[999]), item(prompt="Valid work")]})
                if json.loads(task.context)["round"] == 1:
                    return fixtures.reply({"action": "continue_research", "next_tasks": [item(prompt="Corrected work")]})
                return fixtures.reply({"action": "submit_codex"})
            seen.append(task.prompt)
            return fixtures.reply()
        service = self.service(adapter)
        group = self.submit(service)
        state = self.wait(service, group)
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertFalse(any(p.startswith("Invalid reference") for p in seen))
        failed = [r for r in service.store.job(group)["results"] if r.get("error", {}).get("code") == "invalid_member_task"]
        self.assertEqual(len(failed), 1)
        self.assertEqual(state["tokens_used"], 50)

    def test_outside_source_range_is_rejected_before_slice_allocation(self):
        def adapter(route, prompt, task, context):
            return fixtures.reply({"action": "continue_research", "next_tasks": [item(context_range=[0, 1001])]})
        service = self.service(adapter)
        group = self.submit(service, context="x" * 1000)
        state = self.wait(service, group)
        self.assertEqual(state["pending_action"]["type"], "invalid_leader_decision")
        self.assertIsNone(service.groups.jobs[group].slice_state)
        self.assertEqual(state["tokens_used"], 10)

    def test_precise_range_and_requested_reports_exclude_unrelated_recent_evidence(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "context": "x" * 8000})
        job = GroupJob(req)
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        job.results = [{"worker": 5, "ok": True, "content": {"summary": "Requested evidence"},
                        "slice": {"id": "completed-ref", "start": 0, "end": 8, "state": "running"}},
                       {"worker": 6, "ok": True, "role": "collector", "content": {"summary": "Unrelated outside-range case"}}]
        assignment = item(role="collector", kind="extract", context_range=[2000, 6000], evidence_workers=[5])
        record = SliceQueue(req, [assignment]).take()
        task = service.groups._member_task(job, assignment, 1, record)
        data = json.loads(task.context)
        self.assertEqual(data["supplied_context"], req.context[2000:6000])
        self.assertEqual(data["prior_evidence"], [])
        self.assertEqual([r["worker"] for r in data["requested_evidence"]], [5])
        self.assertNotIn("state", data["requested_evidence"][0]["slice"])
        self.assertEqual(job.results[0]["slice"]["state"], "running")
        self.assertTrue(data["slice"]["supplied_context_complete_for_range"])
        self.assertEqual(data["slice"]["offset_unit"], "python_characters")
        self.assertNotIn("Unrelated outside-range case", task.context)

    def test_long_summary_does_not_repeat_and_hide_findings_in_bounded_evidence(self):
        service = self.service()
        result = {"worker": 6, "content": {"summary": "Long summary " * 800,
            "findings": [{"case_id": "CASE_013", "used": 1521, "source_anchor": "[27836,36296)"}],
            "sources": [], "uncertainties": [], "complete": True}}
        excerpt = service.groups._evidence_excerpt(result, 875)
        self.assertTrue(excerpt["excerpt_only"])
        self.assertEqual(excerpt["full_report_worker"], 6)
        self.assertIn('"CASE_013"', excerpt["excerpt"])
        self.assertIn('"used": 1521', excerpt["excerpt"])
        self.assertNotIn("Long summary", excerpt["excerpt"])
        self.assertLess(len(excerpt["summary"]) + len(excerpt["excerpt"]), 875)

    def test_restore_reconciles_pending_zero_timeout_once_without_duplicate_usage(self):
        service = self.service()
        job = GroupJob(GroupRequest.parse({"topic": "fixture", "needs_web": False, "token_budget": 100000}))
        job.state, job.phase, job.round = "interrupted", "research_round", 1
        job.results = [{"worker": 1, "ok": False, "error": {"code": "timeout"}}]
        key = "opencode:msg:msg_fixture"
        job.attempts = [{"worker": 1, "source": "opencode", "state": "failed", "error": {"code": "timeout"},
            "native_dedup_key": key, "token_measurement": "reported", "tokens_charged": 0,
            "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
            "decision": {"reserved_token_estimate": 125000}}]
        service._save(job)
        first = service.groups._restore(job.id)
        self.assertEqual((first.tokens_used, first.tokens_estimated), (125000, 125000))
        self.assertEqual(first.pending_action["type"], "group_budget_exhausted")
        self.assertEqual(first.attempts[0]["usage"], {})
        self.assertEqual(first.attempts[0]["native_dedup_key"], key)
        self.assertEqual(first.attempts[0]["usage_placeholder"]["total_tokens"], 0)
        service.groups.jobs.pop(job.id)
        again = service.groups._restore(job.id)
        self.assertEqual((again.tokens_used, again.tokens_estimated), (125000, 125000))
        self.assertEqual(service.store.conn.execute("SELECT COUNT(*) FROM usage_records").fetchone()[0], 1)

    def test_journal_saturation_does_not_turn_valid_information_into_an_invalid_request(self):
        service = self.service()
        job = self.seed(service, workflow="default")
        report = job.results[0]
        report["content"]["information_requests"] = [{"question": "Need definition", "reason": "Missing input"}]
        with patch.object(service.store, "append_event", side_effect=ValueError("Workflow coordination event capacity reached")):
            service.groups._publish_member_report(job, report)
        self.assertNotIn("information_request_error", report)
        self.assertEqual(len(report["information_requests"]), 1)
        self.assertTrue(service.store.event_gap("default")["events_incomplete"])

    def test_request_id_only_task_delivers_the_exact_question_without_resending_source(self):
        service = self.service()
        req = GroupRequest.parse({"topic": "fixture", "needs_web": False, "context": "x" * 32000})
        job = GroupJob(req)
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        source = {"worker": 5, "ok": True, "content": {"summary": "long evidence" * 1000,
            "information_requests": [{"question": "Exact missing definition", "reason": "Need a contract"}]}}
        job.results = [source]
        source["information_requests"] = service.groups._information_requests(job, source)
        assignment = item(information_request_ids=["5:0"])
        queue = SliceQueue(req, [assignment])
        record = queue.take()
        task = service.groups._member_task(job, assignment, 1, record)
        context = json.loads(task.context)
        self.assertEqual(context["supplied_context"], "")
        self.assertEqual(context["support_requests"][0]["question"], "Exact missing definition")
        self.assertEqual(context["support_requests"][0]["request_id"], "5:0")
        self.assertEqual(task._responds_to_requests, ["5:0"])


if __name__ == "__main__":
    unittest.main()
