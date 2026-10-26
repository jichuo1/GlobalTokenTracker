import json
import unittest

from gtt_router.adapters import validate_result
from gtt_router.models import WorkerFailure


class OperatorReportTests(unittest.TestCase):
    @staticmethod
    def report(sources, **extra):
        return {"text": json.dumps({"summary": "one local edit", "findings": [], "sources": sources,
            "uncertainties": [], "complete": True, "operations": [], **extra})}

    def test_local_file_reference_requires_the_exact_operator_assignment_snapshot(self):
        snapshot = [{"path": "example.py", "sha256": "a" * 64}]
        result = validate_result(self.report(["example.py"]), False, local_files=snapshot)["content"]
        self.assertEqual(result["sources"], [])
        self.assertEqual(result["operator_file_sources"][0]["sha256"], "a" * 64)
        self.assertFalse(result["operator_file_sources"][0]["verified_by_tool"])
        for sources in [["outside.py"], [{"path": "example.py", "sha256": "b" * 64}]]:
            with self.assertRaises(WorkerFailure):
                validate_result(self.report(sources), False, local_files=snapshot)

    def test_ordinary_and_web_reports_cannot_use_operator_file_provenance(self):
        with self.assertRaises(WorkerFailure):
            validate_result(self.report(["example.py"]), False)
        with self.assertRaises(WorkerFailure):
            validate_result(self.report(["example.py"]), True, local_files=[{"path": "example.py", "sha256": None}])
        result = validate_result(self.report([], operator_file_sources=[{"path": "forged.py"}]), False, local_files=[])
        self.assertNotIn("operator_file_sources", result["content"])
