import unittest

from gtt_router.mcp import TASK_SCHEMA
from gtt_router.models import Task, Route, MAX_OUTPUT_TOKENS, MAX_TASK_TOKENS, DEFAULT_TASK_TOKENS
from gtt_router.catalog import zcode_output_limit
from gtt_router.profiles import ABS_LIMITS


class OutputBudgetTests(unittest.TestCase):
    def test_zcode_inherited_option_range_and_personal_override(self):
        builtin={"modelConfigRules":{"modelRules":[{"modelMatch":".*","config":{"optionSpecs":{"maxOutputTokens":{"max":32000}}}}],
            "providerSiteRules":[{"modelMatch":".*","baseUrlMatch":"https://example.com","config":{"optionSpecs":{"maxOutputTokens":{"max":64000}}}}]}}
        provider={"config":{"api":{"type":"anthropic-messages","baseUrl":"https://elsewhere.example/"}}}
        self.assertEqual(zcode_output_limit(builtin,{},"p",provider,"m"),32000)
        provider["config"]["api"]["baseUrl"]="https://example.com/"
        self.assertEqual(zcode_output_limit(builtin,{},"p",provider,"m"),64000)
        personal={"modelConfigRules":{"providerModelRules":[{"providerId":"p","modelId":"m","config":{"optionSpecs":{"maxOutputTokens":{"max":100000}}}}]}}
        self.assertEqual(zcode_output_limit(builtin,personal,"p",provider,"m"),100000)
        personal["modelConfigRules"]["manualProviderModelRules"]=[{"providerId":"p","modelId":"m","config":{}}]
        self.assertIsNone(zcode_output_limit(builtin,personal,"p",provider,"m"))

    def test_large_ceiling_matches_mcp_schema_and_rejects_overflow(self):
        self.assertEqual(TASK_SCHEMA["properties"]["max_output_tokens"]["maximum"], ABS_LIMITS["max_output_tokens"])
        t = Task.parse({"kind": "logic_check", "prompt": "bounded", "max_output_tokens": 65536})
        self.assertEqual(t.max_output_tokens, 65536)
        self.assertGreater(t.token_budget, 65536)
        with self.assertRaises(ValueError):
            Task.parse({"kind": "logic_check", "prompt": "bounded", "max_output_tokens": MAX_OUTPUT_TOKENS + 1})

    def test_logical_defaults_leave_reasoning_and_time_headroom(self):
        for kind in ("code_review", "logic_check", "cross_check"):
            t = Task.parse({"kind": kind, "prompt": "bounded"})
            self.assertEqual((t.max_output_tokens, t.deadline_s), (32768, 600))
            self.assertGreater(t.token_budget, t.max_output_tokens)

    def test_simple_tasks_and_explicit_limits_keep_existing_behavior(self):
        t = Task.parse({"kind": "extract", "prompt": "bounded"})
        self.assertEqual((t.max_output_tokens, t.deadline_s, t.token_budget), (8192, 180, 24000))
        t = Task.parse({"kind": "logic_check", "prompt": "bounded", "max_output_tokens": 512,
                        "deadline_s": 20, "token_budget": 3000})
        self.assertEqual((t.max_output_tokens, t.deadline_s, t.token_budget), (512, 20, 3000))

    def test_default_budget_covers_workers_and_verifier_and_stays_bounded(self):
        t = Task.parse({"kind": "code_review", "prompt": "bounded", "fanout": 2})
        self.assertTrue(t.verify)
        self.assertGreater(t.token_budget, 3 * t.max_output_tokens)
        self.assertLessEqual(t.token_budget, MAX_TASK_TOKENS)
        t = Task.parse({"kind": "code_review", "prompt": "x" * 64000, "fanout": 4, "max_output_tokens": 65536})
        self.assertEqual(t.token_budget, DEFAULT_TASK_TOKENS)

    def test_native_limit_is_used_without_mutating_requested_budget(self):
        r = Route("fixture", "p", "m", output_limit=4096)
        t = Task.parse({"kind": "code_review", "prompt": "bounded"})
        self.assertEqual(r.output_ceiling(t.max_output_tokens), 4096)
        self.assertEqual(r.output_ceiling(512), 512)
        self.assertEqual(t.max_output_tokens, 32768)
        self.assertEqual(r.public()["output_limit"], 4096)
        for invalid in (None, 0, -1, True, "4096"):
            r.output_limit = invalid
            self.assertEqual(r.output_ceiling(65536), 65536)
