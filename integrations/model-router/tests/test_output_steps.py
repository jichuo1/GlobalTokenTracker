import json
import unittest
from unittest.mock import patch

import test_router as fixtures
from gtt_router.models import Task, WorkerFailure
from gtt_router.output_steps import ReportSteps
from gtt_router.slicing import output_budget
from gtt_router.scheduling import graph


class OutputStepsTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service
    wait = fixtures.RouterTests.wait

    @staticmethod
    def page(index, final=False, summary=None):
        body = fixtures.content(summary or f"Section {index}")
        body["findings"] = [{"section": index, "evidence": f"line {index}"}]
        body["output_step"] = {"index": index, "final": final, "next": "" if final else f"Section {index + 1}"}
        return {"text": json.dumps(body), "usage": {"total_tokens": 100}, "native_backed": True}

    def task(self, **extra):
        return {"kind": "extract", "prompt": "Read the supplied sections", "context": "Section 1\nSection 2",
                "max_output_tokens": 8192, "output_mode": "adaptive", "max_output_steps": 3,
                "max_attempts": 4, "token_budget": 24000, "allow_fallback": False, **extra}

    def test_dynamic_capacity_fits_native_context_and_remaining_budget(self):
        r = fixtures.route()
        r.output_limit = 4096
        r.context_window = 10000
        for mode in ("adaptive", "native"):
            task = Task.parse(self.task(output_mode=mode))
            self.assertEqual(output_budget(task, r, 2000, 3000)["effective_output_tokens"], 2000)
            self.assertEqual(output_budget(task, r, 18000, 20000)["effective_output_tokens"], 1000)
        fixed = Task.parse(self.task(output_mode="fixed"))
        self.assertEqual(output_budget(fixed, r, 2000, 3000)["effective_output_tokens"], 4096)

    def test_initial_adaptive_request_fits_small_total_budget(self):
        outputs = []
        def adapter(r, prompt, task, context):
            outputs.append(task.max_output_tokens)
            return fixtures.response()
        service = self.service([fixtures.route()], adapter)
        result = self.wait(service, service.submit(self.task(max_output_steps=1, token_budget=6000))["job_id"])
        self.assertEqual(result["state"], "succeeded")
        self.assertLess(outputs[0], 6000)
        self.assertEqual(result["tokens_reserved"], 0)

    def test_default_total_budget_accounts_for_pages_but_explicit_budget_wins(self):
        one = Task.parse({"kind": "extract", "prompt": "bounded"})
        pages = Task.parse({"kind": "extract", "prompt": "bounded", "max_output_steps": 4})
        self.assertGreater(pages.token_budget, one.token_budget)
        self.assertEqual(Task.parse(self.task(token_budget=6000)).token_budget, 6000)

    def test_effective_budget_includes_suffix_and_shrinks_on_later_pages(self):
        outputs = []
        def adapter(route, prompt, task, context):
            remaining = 12000 - 3000 * len(outputs)
            self.assertLessEqual((len(prompt) + 1) // 2 + task.max_output_tokens, remaining)
            outputs.append(task.max_output_tokens)
            self.assertIn("当前页的完整 JSON", prompt)
            raw = self.page(len(outputs), final=len(outputs) == 2)
            raw["usage"] = {"total_tokens": 3000}
            return raw
        service = self.service([fixtures.route()], adapter)
        result = self.wait(service, service.submit(self.task(token_budget=12000))["job_id"])
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual(result["tokens_used"], 6000)
        self.assertEqual(len(outputs), 2)
        self.assertLess(outputs[1], outputs[0])

    def test_page_loop_keeps_original_hard_deadline(self):
        deadlines = []
        def adapter(route, prompt, task, context):
            deadlines.append(context.deadline)
            return self.page(len(deadlines), final=len(deadlines) == 2)
        service = self.service([fixtures.route()], adapter)
        result = self.wait(service, service.submit(self.task())["job_id"])
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual(deadlines[0], deadlines[1])

    def test_complete_pages_share_attempts_budget_and_keep_checkpoint_provenance(self):
        calls = []
        r = fixtures.route()
        r.output_limit = 1500
        def adapter(route, prompt, task, context):
            calls.append(prompt)
            self.assertLessEqual(task.max_output_tokens, 1500)
            return self.page(len(calls), final=len(calls) == 2)
        service = self.service([r], adapter)
        result = self.wait(service, service.submit(self.task())["job_id"])
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual((result["attempt_count"], result["tokens_used"], result["tokens_reserved"]), (2, 200, 0))
        report = result["results"][0]
        self.assertTrue(report["content"]["complete"])
        self.assertEqual(len(report["content"]["findings"]), 2)
        self.assertEqual(len(report["output_steps"]), 2)
        self.assertNotIn("usage", report)
        self.assertIn('"next":"Section 2"', calls[1])
        files = list((service.state_dir / "workspaces" / result["job_id"]).glob("*-output-step-*.json"))
        self.assertEqual(len(files), 2)

    def test_exhausted_steps_preserve_partial_without_false_completion(self):
        calls = []
        def adapter(*args):
            calls.append(1)
            return self.page(len(calls))
        service = self.service([fixtures.route()], adapter)
        result = self.wait(service, service.submit(self.task(max_output_steps=2))["job_id"])
        self.assertEqual(result["state"], "failed")
        report = result["results"][0]
        self.assertFalse(report["content"]["complete"])
        self.assertTrue(report["partial_output"])
        self.assertEqual(report["error"]["code"], "output_step_limit_reached")
        self.assertEqual(len(report["content"]["findings"]), 2)

    def test_unknown_paid_second_page_never_replays_or_loses_first(self):
        calls = []
        def adapter(*args):
            calls.append(1)
            if len(calls) == 1:
                return self.page(1)
            raise WorkerFailure("timeout", usage={"total_tokens": 20}, metering={"unknown_paid_outcome": True})
        service = self.service([fixtures.route(), fixtures.route(model="second")], adapter)
        result = self.wait(service, service.submit(self.task(allow_fallback=True))["job_id"])
        self.assertEqual(len(calls), 2)
        self.assertEqual(result["tokens_used"], 120)
        self.assertTrue(result["results"][0]["unknown_paid_outcome"])
        self.assertFalse(result["results"][0]["content"]["complete"])
        self.assertEqual(len(result["results"][0]["content"]["findings"]), 1)

    def test_cancel_between_pages_never_starts_another_call(self):
        calls = []
        service = None
        def adapter(r, prompt, task, context):
            calls.append(1)
            context.cancel.set()
            return self.page(1)
        service = self.service([fixtures.route()], adapter)
        result = self.wait(service, service.submit(self.task())["job_id"])
        self.assertEqual(len(calls), 1)
        self.assertEqual(result["state"], "cancelled")

    def test_unexpected_or_invalid_metadata_cannot_claim_success(self):
        for field in (None, {"index": True, "final": True, "next": ""}, {"index": 2, "final": True, "next": ""}):
            def adapter(*args):
                raw = self.page(1, True)
                body = json.loads(raw["text"])
                body["output_step"] = field
                raw["text"] = json.dumps(body)
                return raw
            service = self.service([fixtures.route()], adapter)
            result = self.wait(service, service.submit(self.task())["job_id"])
            self.assertEqual(result["state"], "failed")
            self.assertEqual(result["attempt_count"], 1)
            self.assertEqual(result["attempts"][0]["state"], "failed")
        service = self.service([fixtures.route()], lambda *args: self.page(1, False))
        result = self.wait(service, service.submit(self.task(max_output_steps=1))["job_id"])
        self.assertEqual(result["state"], "failed")

    def test_duplicate_pages_and_conflicting_decisions_are_rejected(self):
        for conflict in (False, True):
            pages = ReportSteps(4, 12000)
            first = {"content": json.loads(self.page(1)["text"])}
            if conflict:
                first["content"]["decision"] = "reject"
            pages.accept(first)
            second = {"content": json.loads(self.page(2, True)["text"])}
            if conflict:
                second["content"]["decision"] = "accept"
            else:
                second["content"].update({k: v for k, v in first["content"].items() if k != "output_step"})
            with self.assertRaises(WorkerFailure):
                pages.accept(second)
            self.assertFalse(pages.content["complete"])

    def test_report_capacity_and_shared_attempt_ceiling_are_hard(self):
        pages = ReportSteps(2, 1000)
        raw = {"content": json.loads(self.page(1, True, "x" * 1100)["text"])}
        with self.assertRaises(WorkerFailure):
            pages.accept(raw)
        calls = []
        def adapter(*args):
            calls.append(1)
            return self.page(len(calls))
        service = self.service([fixtures.route()], adapter)
        result = self.wait(service, service.submit(self.task(max_output_steps=2, max_attempts=2))["job_id"])
        self.assertEqual(len(calls), 2)
        self.assertFalse(result["results"][0]["content"]["complete"])

    def test_output_steps_validate_and_operator_assignments_stay_explicit(self):
        for steps in (0, 17, True, 1.5):
            with self.assertRaises(ValueError):
                Task.parse(self.task(max_output_steps=steps))
        with self.assertRaises(ValueError):
            Task.parse(self.task(max_output_steps=5, max_attempts=4))
        for kind in ("operator", "reviewer"):
            with self.assertRaisesRegex(ValueError, "Codex controller"):
                graph({"id": "test", "name": "test", "nodes": [{"id": "a", "title": "a", "route_id": "route",
                       "node_type": kind, "effort": "max", "task": {"kind": "extract", "prompt": "test", "max_output_steps": 2}}]})

    def test_old_daemon_rejects_steps_before_submission(self):
        from gtt_router.daemon import LocalClient
        client = object.__new__(LocalClient)
        with patch.object(client, "runtime_info", return_value={"features": ["dispatch_v1"], "runtime_contract_version": 2}), \
                patch.object(client, "_request") as request:
            with self.assertRaisesRegex(ValueError, "output_steps_v1"):
                client.call("router_dispatch", {"action": "submit", "request": {"kind": "task", "task": self.task()}})
            request.assert_not_called()


if __name__ == "__main__":
    unittest.main()
