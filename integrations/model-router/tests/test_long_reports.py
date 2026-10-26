import json
import unittest
import threading
import time
from unittest.mock import patch

from gtt_router.adapters import validate_result
from gtt_router.models import Task, WorkerFailure, MAX_REPORT_CHARS
from gtt_router.mcp import TASK_SCHEMA
from gtt_router.profiles import ABS_LIMITS
from gtt_router.daemon import LocalClient, RUNTIME_CONTRACT_VERSION
from gtt_router.service import Job, make_prompt
from tests import test_groups as fixtures


class LongReportTests(unittest.TestCase):
    setUp = fixtures.GroupTests.setUp
    tearDown = fixtures.GroupTests.tearDown
    service = fixtures.GroupTests.service

    def test_large_complete_report_requires_explicit_capacity_and_keeps_strict_json(self):
        body = {"summary": "long evidence", "findings": [{"payload": "x" * 140000}],
                "sources": [], "uncertainties": [], "complete": True}
        text = json.dumps(body)
        with self.assertRaises(WorkerFailure) as failed:
            validate_result({"text": text}, False)
        self.assertEqual(failed.exception.code, "output_too_large")
        self.assertEqual(validate_result({"text": text}, False, 180000)["content"], body)
        with self.assertRaises(WorkerFailure):
            validate_result({"text": text[:-1]}, False, 180000)

    def test_report_capacity_schema_bounds_and_prompt_are_consistent(self):
        self.assertEqual(TASK_SCHEMA["properties"]["report_max_chars"]["maximum"], ABS_LIMITS["max_report_chars"])
        task = Task.parse({"kind": "extract", "prompt": "long evidence", "report_max_chars": 180000})
        self.assertIn("under 180000 characters", make_prompt(task))
        self.assertIn("at most 4 findings", make_prompt(task))
        self.assertIn("Do not omit requested records", make_prompt(task))
        task._research_group = True
        self.assertIn("at most 16 findings", make_prompt(task))
        for value in [True, 1200.5, 999, MAX_REPORT_CHARS + 1]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                Task.parse({"kind": "extract", "prompt": "x", "report_max_chars": value})

    def test_large_report_survives_worker_storage_and_bounded_collection(self):
        body = {"summary": "long evidence", "findings": [{"payload": "x" * 140000}],
                "sources": [], "uncertainties": [], "complete": True}
        def adapter(route, prompt, task, context):
            text = json.dumps(body)
            context.report_content("output", "fixture", text)
            return {"text": text, "usage": {"total_tokens": 100}, "observed_urls": []}
        service = self.service(adapter)
        task = Task.parse({"kind": "extract", "prompt": "long", "report_max_chars": 180000,
                           "max_output_tokens": 32768, "token_budget": 50000})
        job = Job(task)
        result = service._worker(job, task, 0, job.deadline)
        self.assertTrue(result["ok"])
        self.assertEqual(job.progress["0"]["activity"]["output_chars"], len(json.dumps(body)))
        job.results.append(result)
        job.state = "succeeded"
        service._save(job)
        self.assertEqual(service.store.job(job.id)["results"][0]["content"], body)
        collected = service.collect(job.id, max_chars=2000)
        self.assertTrue(collected["results"][0]["truncated"])
        self.assertTrue(collected["results"][0]["content"]["complete"])
        self.assertEqual((job.tokens_used, job.tokens_reserved), (100, 0))

    def test_final_snapshot_includes_output_arriving_inside_progress_throttle(self):
        def adapter(route, prompt, task, context):
            context.report_content("reasoning", "fixture", "reasoning")
            response = fixtures.reply()
            context.report_content("output", "fixture", response["text"])
            return response
        service = self.service(adapter)
        task = Task.parse({"kind": "extract", "prompt": "x"})
        job = Job(task)
        self.assertTrue(service._worker(job, task, 0, job.deadline)["ok"])
        self.assertGreater(job.progress["0"]["activity"]["output_chars"], 0)
        self.assertEqual(job.progress["0"]["activity"]["reasoning_chars"], 9)

    def test_additive_feature_does_not_break_old_tasks_or_dispatch_to_old_runtime(self):
        client = object.__new__(LocalClient)
        old = {"runtime_contract_version": RUNTIME_CONTRACT_VERSION}
        with patch.object(client, "_request", side_effect=[old, {"candidates": []}]) as request:
            self.assertEqual(client.call("router_plan", {"task": {"kind": "extract", "prompt": "x"}}), {"candidates": []})
            self.assertEqual(request.call_count, 2)
        with patch.object(client, "_request", return_value=old) as request:
            with self.assertRaisesRegex(ValueError, "resident_feature_missing"):
                client.call("router_submit", {"task": {"report_max_chars": 180000}})
            self.assertEqual(request.call_count, 1)

    def test_explicit_queue_wait_survives_legacy_short_grace(self):
        service = self.service()
        service.limits["external_queue_grace_s"] = .02
        service.total_inflight = service.limits.get("global_parallel", 4)
        task = Task.parse({"kind": "extract", "prompt": "x", "queue_wait_s": .2})
        job = Job(task)
        def release_capacity():
            with service.capacity:
                service.total_inflight = 0
                service.capacity.notify_all()
        timer = threading.Timer(.06, release_capacity)
        timer.start()
        try:
            started = time.monotonic()
            route, decision = service._reserve(job, task, set(), job.deadline, 200)
            self.assertGreaterEqual(time.monotonic() - started, .04)
            service._release(route, job, decision["reserved_token_estimate"])
            self.assertEqual(job.tokens_reserved, 0)
        finally:
            timer.join()

    def test_stop_with_incomplete_json_is_not_misreported_as_native_length(self):
        text = '{"summary":"long","findings":[{"records":[1,2,'
        with self.assertRaises(WorkerFailure) as failed:
            validate_result({"text": text, "finish_reason": "stop"}, False)
        self.assertEqual(failed.exception.code, "incomplete_output")
        self.assertEqual(failed.exception.public()["details"]["contract_reason"], "incomplete_json")
        self.assertEqual(failed.exception.public()["details"]["finish_reason"], "stop")


if __name__ == "__main__":
    unittest.main()
