import json
import unittest

from gtt_router.adapters import truncation_failure
from gtt_router.models import Task, WorkerFailure
from gtt_router.service import Job, routing_warnings
from gtt_router.group_contract import GroupRequest
from gtt_router.groups import GroupJob
from gtt_router.slicing import SliceQueue
from tests import test_groups as fixtures


class TruncationRecoveryTests(unittest.TestCase):
    setUp = fixtures.GroupTests.setUp
    tearDown = fixtures.GroupTests.tearDown
    service = fixtures.GroupTests.service
    wait = fixtures.GroupTests.wait
    submit = fixtures.GroupTests.submit

    def test_eof_json_with_native_stop_returns_to_slice_queue_without_paid_replay(self):
        calls = []
        def adapter(route, prompt, task, context):
            calls.append(route.id)
            return {"text": '{"summary":"partial","findings":[', "usage": {"totalTokens": 123}, "finish_reason": "stop"}
        service = self.service(adapter)
        req = GroupRequest.parse({"topic": "fixture", "context": "x" * 8000, "needs_web": False,
            "slice_min_chars": 2000, "slice_target_chars": 8000})
        job = GroupJob(req)
        job.state, job.round = "running", 1
        job.roles = service.groups.plan({"topic": "fixture", "needs_web": False})["roles"]
        assignment = {"role": "analyst", "kind": "logic_check", "prompt": "Check assigned range"}
        queue = SliceQueue(req, [assignment])
        record = queue.take()
        task = service.groups._member_task(job, assignment, 1, record)
        result = service._worker(job, task, 1, job.deadline)
        self.assertEqual(result["error"]["code"], "incomplete_output")
        self.assertEqual(result["error"]["details"]["finish_reason"], "stop")
        self.assertEqual((len(calls), job.tokens_used, job.tokens_reserved), (1, 123, 0))
        queue.finish(record, result)
        self.assertEqual(record["state"], "split")
        children = queue.state["records"][1:]
        self.assertEqual([(r["start"], r["end"]) for r in children], [(0, 4000), (4000, 8000)])
        untouched = SliceQueue(req, [assignment])
        bad = untouched.take()
        untouched.finish(bad, {"ok": False, "error": {"code": "incomplete_output", "details": {"contract_reason": "missing_complete"}}})
        self.assertEqual(bad["state"], "failed")
        self.assertEqual(len(untouched.state["records"]), 1)

    def test_adaptive_retry_grows_once_on_known_capable_route_and_bills_both(self):
        seen = []
        def adapter(route, prompt, task, context):
            seen.append(task.max_output_tokens)
            if len(seen) == 1:
                raise truncation_failure(task.max_output_tokens, "", {"totalTokens": 8192})
            return fixtures.reply()
        candidates = fixtures.routes()[::2][:1]
        service = self.service(adapter, candidates)
        task = Task.parse({"kind": "extract", "prompt": "short", "output_mode": "adaptive", "token_budget": 40000})
        job = Job(task)
        result = service._worker(job, task, 0, job.deadline)
        self.assertTrue(result["ok"])
        self.assertEqual(seen, [8192, 16384])
        self.assertEqual((job.tokens_used, job.tokens_reserved), (8202, 0))
        self.assertEqual(task.max_output_tokens, 8192)
        self.assertEqual(len(job.attempts), 2)

    def test_fixed_explicit_ceiling_stops_without_repeating_that_ceiling(self):
        seen = []
        def adapter(route, prompt, task, context):
            seen.append(task.max_output_tokens)
            raise truncation_failure(task.max_output_tokens, "", {"totalTokens": 8192})
        service = self.service(adapter)
        task = Task.parse({"kind": "code_review", "prompt": "short", "max_output_tokens": 8192, "token_budget": 80000})
        job = Job(task)
        result = service._worker(job, task, 0, job.deadline)
        self.assertFalse(result["ok"])
        self.assertEqual(result["error"]["code"], "output_truncated")
        self.assertEqual(seen, [8192])
        self.assertEqual(job.tokens_used, 8192)
        self.assertIn("logical_fixed_output_below_reasoning_default", routing_warnings(task))

    def test_native_limit_switches_to_a_route_with_more_headroom(self):
        candidates = fixtures.routes()[::2]
        candidates[0].output_limit = 8192
        seen = []
        def adapter(route, prompt, task, context):
            seen.append((route.id, task.max_output_tokens))
            if route.id == candidates[0].id:
                raise truncation_failure(task.max_output_tokens, "", {"totalTokens": 8192})
            return fixtures.reply()
        service = self.service(adapter, candidates)
        task = Task.parse({"kind": "logic_check", "prompt": "short", "output_mode": "adaptive", "token_budget": 100000,
                           "preferred_route": candidates[0].id})
        job = Job(task)
        self.assertTrue(service._worker(job, task, 0, job.deadline)["ok"])
        self.assertEqual(seen[0], (candidates[0].id, 8192))
        self.assertEqual(seen[1][0], candidates[1].id)
        self.assertGreater(seen[1][1], 8192)

    def test_growth_is_clamped_to_remaining_total_budget_above_failed_floor(self):
        seen = []
        def adapter(route, prompt, task, context):
            seen.append(task.max_output_tokens)
            if len(seen) == 1:
                raise truncation_failure(task.max_output_tokens, "", {"totalTokens": 8192})
            return fixtures.reply()
        service = self.service(adapter, fixtures.routes()[::2][:1])
        task = Task.parse({"kind": "extract", "prompt": "short", "output_mode": "adaptive", "token_budget": 20000})
        job = Job(task)
        self.assertTrue(service._worker(job, task, 0, job.deadline)["ok"])
        self.assertGreater(seen[1], 8192)
        self.assertLess(seen[1], 16384)
        self.assertLessEqual(job.tokens_used, job.task.token_budget)

    def test_reasoning_only_failure_does_not_create_smaller_source_fragments(self):
        req = GroupRequest.parse({"topic": "fixture", "context": "x" * 30000, "needs_web": False})
        queue = SliceQueue(req, [{"role": "analyst", "kind": "logic_check", "prompt": "review"}])
        record = queue.take()
        error = truncation_failure(32000, "", {"totalTokens": 33000})
        queue.finish(record, {"ok": False, "error": error.public()})
        self.assertEqual(record["state"], "failed")
        self.assertEqual(len(queue.state["records"]), 1)

    def test_group_can_recover_zero_body_without_splitting_successful_scope(self):
        failed = []
        def adapter(route, prompt, task, context):
            if fixtures.is_leader(task):
                return fixtures.reply({"action": "continue_research" if fixtures.initial(task) else "submit_codex"})
            data = json.loads(task.context)
            if data["slice"]["slot"] == 1 and not failed:
                failed.append(task.max_output_tokens)
                raise truncation_failure(task.max_output_tokens, "", {"totalTokens": 1000})
            return fixtures.reply()
        service = self.service(adapter)
        state = self.wait(service, self.submit(service, context="x" * 1000))
        self.assertEqual(state["pending_action"]["type"], "submit_codex")
        self.assertEqual(state["slicing"]["counts"]["split"], 0)
        self.assertTrue(failed)


if __name__ == "__main__":
    unittest.main()
