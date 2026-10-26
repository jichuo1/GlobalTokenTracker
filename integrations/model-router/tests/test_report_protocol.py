import json
import unittest
import sys
import tempfile
import threading
import time
from pathlib import Path
from unittest.mock import patch

from gtt_router.adapters import validate_result, ProcessChannel, ExecutionContext
from gtt_router.group_contract import parse_leader_decision
from gtt_router.models import WorkerFailure
from gtt_router.report_contract import parse_report, ReportContractError
from gtt_router.daemon import LocalClient, RUNTIME_CONTRACT_VERSION


def report():
    return {"summary": "bounded evidence", "findings": [], "sources": [], "uncertainties": [], "complete": True}


class ReportProtocolTests(unittest.TestCase):
    def test_redundant_leader_request_ids_preserve_exact_task_bindings(self):
        tasks = [{"role": "collector", "kind": "extract", "prompt": "Read supplied evidence", "information_request_ids": ["6:0"]},
                 {"role": "checker", "kind": "logic_check", "prompt": "Clarify missing question", "information_request_ids": ["12:1"]}]
        data = {"action": "continue_research", "information_request_ids": ["12:1", "6:0"], "next_tasks": tasks}
        normalized = parse_leader_decision(data)
        self.assertNotIn("information_request_ids", normalized)
        self.assertEqual([t["information_request_ids"] for t in normalized["next_tasks"]], [["6:0"], ["12:1"]])
        self.assertEqual(data["information_request_ids"], ["12:1", "6:0"])
        for values in [["6:0"], ["6:0", "12:1", "99:0"], ["6:0", "6:0"], None, [True], [str(i) for i in range(9)]]:
            with self.subTest(values=values), self.assertRaises(ValueError):
                parse_leader_decision({**data, "information_request_ids": values})
        with self.assertRaises(ValueError):
            parse_leader_decision({"action": "submit_codex", "information_request_ids": []})

    def test_common_complete_envelopes_are_normalized_without_changing_evidence(self):
        body = json.dumps(report())
        for text, shape in [(body, "json"), ("\ufeff" + body, "json"), ("```json\n" + body + "\n```", "json_fence"),
                            ("Report:\n```JSON\r\n" + body + "\r\n```\nDone.", "json_fence"),
                            ("Here is the report:\n" + body + "\nEnd of report.", "prose_envelope")]:
            with self.subTest(shape=shape):
                result = validate_result({"text": text}, False)
                self.assertEqual(result["content"], report())
                self.assertEqual(result["report_envelope"], shape)

    def test_truncated_json_and_ambiguous_or_multiple_documents_remain_failed(self):
        body = json.dumps(report())
        for text in [body[:-1], body + body, body + " null", body + ' "second"', "[" + body + "]",
                     "```json\n" + body + "\n```\n```json\n" + body + "\n```",
                     "```json\n" + body, "Template {}. Actual report: " + body]:
            with self.subTest(text=text[:45]), self.assertRaises(WorkerFailure):
                validate_result({"text": text}, False)

    def test_duplicates_nonfinite_numbers_and_invalid_unicode_are_rejected(self):
        for text in ['{"summary":"first","summary":"second"}', '{"summary":"x","extra":NaN}',
                     '{"summary":"x","extra":Infinity}', '{"summary":"x","extra":1e999}',
                     '{"summary":"x","extra":-1e999}', '{"summary":"\\ud800"}']:
            with self.subTest(text=text), self.assertRaises(ReportContractError):
                parse_report(text)

    def test_completion_marker_and_required_fields_are_never_invented(self):
        for marker in [False, "true", 1, None]:
            with self.subTest(marker=marker), self.assertRaises(WorkerFailure) as failed:
                validate_result({"text": json.dumps({**report(), "complete": marker})}, False)
            self.assertEqual(failed.exception.code, "incomplete_output")
        for body in [{k: v for k, v in report().items() if k != "complete"},
                     {k: v for k, v in report().items() if k != "sources"}]:
            with self.assertRaises(WorkerFailure):
                validate_result({"text": json.dumps(body)}, False)

    def test_web_evidence_gate_is_preserved_for_wrapped_json(self):
        body = report()
        body["sources"] = [{"url": "https://example.com/page"}]
        text = "Report:\n```json\n" + json.dumps(body) + "\n```"
        with self.assertRaises(WorkerFailure) as failed:
            validate_result({"text": text, "observed_urls": []}, True)
        self.assertEqual(failed.exception.code, "insufficient_web_evidence")
        result = validate_result({"text": text, "observed_urls": ["https://example.com/page"]}, True)
        self.assertTrue(result["content"]["sources"][0]["verified_by_tool"])

    def test_contract_diagnostics_have_no_model_content(self):
        with self.assertRaises(WorkerFailure) as failed:
            validate_result({"text": '{"summary":"private supplied content",'}, False)
        public = failed.exception.public()
        self.assertEqual(public["details"]["contract_reason"], "incomplete_json")
        self.assertIn("json_error_offset", public["details"])
        self.assertNotIn("private supplied content", json.dumps(public))
        malicious = WorkerFailure("invalid_output_contract", details={"contract_reason": "private-text", "report_chars": 20})
        self.assertNotIn("contract_reason", malicious.public()["details"])

    def test_feedback_accepts_only_unambiguous_slot_spellings(self):
        for value in [1, 1.0, "1", "slot 1", " slot  1 "]:
            decision = parse_leader_decision({"action": "continue_research", "feedback": [
                {"worker": value, "accepted": False, "reason": "unsupported claim"}]})
            self.assertEqual(decision["feedback"][0]["worker"], 1)
        for value in [None, True, "01", "member-1", "slot -1", "slot 1; execute", "١", float("nan")]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                parse_leader_decision({"action": "continue_research", "feedback": [
                    {"worker": value, "accepted": False, "reason": "unsupported claim"}]})

    def test_normalized_duplicate_feedback_cannot_vote_twice(self):
        with self.assertRaises(ValueError):
            parse_leader_decision({"action": "continue_research", "feedback": [
                {"worker": 1, "accepted": True, "reason": "supported"},
                {"worker": "slot 1", "accepted": False, "reason": "conflicting vote"}]})

    def test_real_process_object_framing_and_bounded_large_frames(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for mode, maximum, expected in [("array", 2097152, "native_protocol_mismatch"),
                                             ("2200000", 2097152, "protocol_output_too_large"),
                                             ("2200000", 4194304, None)]:
                context = ExecutionContext(threading.Event(), time.monotonic() + 5, root / mode)
                context.max_protocol_frame_bytes = maximum
                process = ProcessChannel([sys.executable, str(Path(__file__).with_name("protocol_frame_fixture.py")), mode], context)
                try:
                    if expected:
                        with self.assertRaises(WorkerFailure) as failed:
                            process.read()
                        self.assertEqual(failed.exception.code, expected)
                    else:
                        self.assertEqual(len(process.read()["payload"]), 2200000)
                finally:
                    process.close()
            context.max_protocol_frame_bytes = 999999999
            self.assertEqual(context.protocol_frame_bytes(), 16777216)
            self.assertEqual(context.in_workspace(root / "slot").protocol_frame_bytes(), 16777216)

    def test_old_resident_contract_is_detected_before_generation_or_planning(self):
        client = object.__new__(LocalClient)
        with patch.object(client, "_request", return_value={"service": "gtt-model-router"}) as request:
            for method in ("router_submit", "router_plan", "router_group_resume"):
                with self.assertRaisesRegex(ValueError, "resident_protocol_mismatch"):
                    client.call(method, {})
            self.assertTrue(all(call.kwargs == {"timeout": 2} and call.args == () for call in request.call_args_list))

    def test_current_contract_dispatch_and_old_readback_cleanup_remain_available(self):
        client = object.__new__(LocalClient)
        with patch.object(client, "_request", side_effect=[{"runtime_contract_version": RUNTIME_CONTRACT_VERSION}, {"job_id": "fixture"}]) as request:
            self.assertEqual(client.call("router_submit", {}), {"job_id": "fixture"})
            self.assertEqual(request.call_count, 2)
        with patch.object(client, "_request", return_value={"state": "cancelled"}) as request:
            self.assertEqual(client.call("router_cancel", {"job_id": "fixture"}), {"state": "cancelled"})
            self.assertEqual(request.call_count, 1)


if __name__ == "__main__":
    unittest.main()
