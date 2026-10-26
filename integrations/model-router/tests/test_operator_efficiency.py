import json
import unittest

import test_router as fixtures
from gtt_router.models import Task, WorkerFailure
from gtt_router.service import make_prompt


class OperatorEfficiencyTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service
    wait = fixtures.RouterTests.wait

    def test_prompt_retains_exact_file_evidence_and_acceptance_without_double_encoding(self):
        assignment = {"id": "random-controller-id", "node_id": "node", "source": "caller_reported_codex",
            "instruction": "Implement only the assigned function", "acceptance": ["unknown must not replay"],
            "paths": ["sample.py"], "file_ranges": {"sample.py": [2, 4]},
            "files": [{"path": "sample.py", "sha256": "a" * 64, "content": '中文🙂\nvalue = "data"\n', "excerpt_only": True}]}
        context = {"input_slice": "material", "assignment": assignment, "upstream": [{"report": "evidence"}]}
        task = Task.parse({"kind": "extract", "prompt": assignment["instruction"], "context": json.dumps(context, ensure_ascii=False)})
        task._operator_assignment = assignment
        prompt = make_prompt(task)
        payload = json.loads(prompt.split("follow:\n", 1)[1])
        self.assertEqual(payload["context"]["assignment"]["files"], assignment["files"])
        self.assertEqual(payload["context"]["assignment"]["acceptance"], assignment["acceptance"])
        self.assertEqual(payload["context"]["assignment"]["file_ranges"], assignment["file_ranges"])
        self.assertEqual(payload["context"]["upstream"], context["upstream"])
        self.assertNotIn("random-controller-id", prompt)
        self.assertEqual(json.loads(task.context), context)
        self.assertIn("Never enlarge scope", prompt)

    def test_failed_paid_generation_updates_efficiency_cost_without_success_or_quality(self):
        route = fixtures.route()
        def adapter(*args):
            raise WorkerFailure("output_truncated", usage={"total_tokens": 15000})
        service = self.service([route], adapter)
        result = self.wait(service, service.submit({"kind": "extract", "prompt": "bounded", "allow_fallback": False})["job_id"])
        row = service.store.health("route:" + route.id + ":kind:extract")
        self.assertEqual(result["state"], "failed")
        self.assertEqual(row["measured_tokens_ema"], 15000)
        self.assertEqual(row["measured_samples"], 1)
        self.assertEqual(row["successes"], 0)
        self.assertEqual(row["last_verified_at"], 0)
        self.assertIsNone(row["tokens_ema"])
        self.assertEqual(service.store.quality(route.id, "extract")["samples"], 0)

    def test_unknown_estimated_cost_does_not_become_measured_efficiency(self):
        route = fixtures.route()
        def adapter(*args):
            raise WorkerFailure("provider_request_failed")
        service = self.service([route], adapter)
        result = self.wait(service, service.submit({"kind": "extract", "prompt": "bounded", "allow_fallback": False})["job_id"])
        self.assertGreater(result["tokens_estimated"], 0)
        self.assertEqual(service.store.health("route:" + route.id + ":kind:extract")["measured_samples"], 0)
