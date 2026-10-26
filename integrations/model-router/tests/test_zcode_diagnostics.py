from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

from gtt_router.adapters import ExecutionContext, zcode
from gtt_router.models import Route, Task, WorkerFailure


class ZCodeDiagnosticsTests(unittest.TestCase):
    def test_configured_payload_and_canonical_selection_preserve_identity_and_usage(self):
        for scenario in ["messages", "canonical", "wrong"]:
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as directory:
                root=Path(directory)
                route=Route("fixture","p","m",effort="xhigh",runtime={"command":[sys.executable,str(Path(__file__).with_name("zcode_rpc_fixture.py")),scenario],
                    "builtin_config":str(root/"unused"),"provider_config":str(root/"unused"),
                    "gtt_call_options":{"selection_validation":"requested_options","zcode_payload":"messages" if scenario=="messages" else "prompt"}})
                context=ExecutionContext(threading.Event(),time.monotonic()+5,root)
                if scenario=="wrong":
                    with self.assertRaises(WorkerFailure) as caught:zcode(route,"bounded",Task("extract","fixture",max_output_tokens=1024),context)
                    self.assertEqual(caught.exception.code,"model_selection_mismatch")
                else:
                    result=zcode(route,"bounded",Task("extract","fixture",max_output_tokens=1024),context)
                    self.assertEqual(result["usage"]["inputTokens"],10)
                    self.assertEqual(result["usage"]["outputTokens"],5)
                    self.assertEqual(result["usage"]["reasoningTokens"],1)
                    self.assertNotIn("vendor_private",str(result["actual_selection"]))

    def test_length_keeps_requested_ceiling_reasoning_and_visible_size(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            route = Route("fixture", "p", "m", effort="xhigh", runtime={
                "command": [sys.executable, str(Path(__file__).with_name("zcode_rpc_fixture.py"))],
                "builtin_config": str(root / "unused-builtins.json"), "provider_config": str(root / "unused-providers.json")})
            task = Task("logic_check", "fixture", max_output_tokens=1024)
            context = ExecutionContext(threading.Event(), time.monotonic() + 5, root)
            with self.assertRaises(WorkerFailure) as caught:
                zcode(route, "fixture", task, context)
            failure = caught.exception
            self.assertEqual(failure.code, "output_truncated")
            self.assertEqual(failure.usage["totalTokens"], 1034)
            self.assertEqual(failure.public()["details"], {
                "finish_reason": "length", "requested_output_tokens": 1024,
                "reported_output_tokens": 1024, "reported_reasoning_tokens": 1024, "visible_output_chars": 0})

    def test_public_details_exclude_vendor_content_secrets_and_invalid_numbers(self):
        failure = WorkerFailure("output_truncated", details={"api_key": "private", "text": "private", "reasoning": "private",
            "visible_output_chars": -1, "reported_reasoning_tokens": True, "finish_reason": "Bearer private", "requested_output_tokens": 1024})
        self.assertEqual(failure.public()["details"], {"requested_output_tokens": 1024})
        self.assertNotIn("details", WorkerFailure("output_truncated", details={"finish_reason": {"text": "private"}}).public())
