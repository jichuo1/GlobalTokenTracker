import unittest

from gtt_router.group_contract import GROUP_SCHEMA, LEADER_SCHEMA, GroupRequest, parse_leader_decision


class GroupRequestTests(unittest.TestCase):
    def test_empty_operation_fields_are_compatible_but_nonempty_actions_rejected(self):
        value=parse_leader_decision({"action":"submit_codex","next_tasks":[],"replace_roles":[],"feedback":[]})
        self.assertEqual(value["action"],"submit_codex")
        with self.assertRaises(ValueError):parse_leader_decision({"action":"submit_codex","replace_roles":["member-1"]})
    def test_defaults_public_redaction_and_integral_float(self):
        group = GroupRequest.parse({"topic": "Compare primary evidence", "context": "x" * 70000,
                                    "member_count": 4.0, "max_rounds": 3.0, "deadline_s": 3600.0,
                                    "token_budget": 1000000.0})
        self.assertEqual(group.needs_web, True)
        self.assertEqual(group.member_count, 4)
        self.assertEqual(group.max_rounds, 3)
        self.assertEqual(group.deadline_s, 3600)
        self.assertEqual(group.token_budget, 1000000)
        public = group.public()
        self.assertNotIn("topic", public)
        self.assertNotIn("context", public)
        self.assertEqual(public["payload_chars"], len("Compare primary evidence") + 70000)
        self.assertIsNone(public["max_output_tokens"])
        self.assertEqual(GROUP_SCHEMA["additionalProperties"], False)
        self.assertNotIn("max_output_tokens", GROUP_SCHEMA["properties"])

    def test_adaptive_roles_and_token_budget_boundaries(self):
        self.assertIsNone(GroupRequest.parse({"topic": "Adaptive", "token_budget": 50000000}).member_count)
        self.assertEqual(GroupRequest.parse({"topic": "Explicit", "member_count": 8, "token_budget": 1000}).member_count, 8)
        for fields in ({"member_count": 2}, {"member_count": 9}, {"token_budget": 50000001},
                       {"token_budget": 999}, {"max_rounds": 65}, {"deadline_s": 604801}):
            with self.subTest(fields=fields), self.assertRaises(ValueError):
                GroupRequest.parse({"topic": "x", **fields})

    def test_rejects_unknown_output_limit_and_unsafe_or_unbounded_input(self):
        invalid = [
            {"topic": "x", "max_output_tokens": 4096},
            {"topic": "  "},
            {"topic": "x", "needs_web": 1},
            {"topic": "x", "max_rounds": True},
            {"topic": "x", "max_rounds": 1.5},
            {"topic": "x", "deadline_s": float("nan")},
            {"topic": "x", "token_budget": 10 ** 10000},
            {"topic": "Bearer abcdefghijklmnop"},
            {"topic": "x", "context": "-----BEGIN " + "PRIVATE KEY-----"},
        ]
        for index, value in enumerate(invalid):
            with self.subTest(index=index), self.assertRaises(ValueError):
                GroupRequest.parse(value)


class LeaderDecisionTests(unittest.TestCase):
    def test_accepts_bounded_advisory_plan(self):
        decision = parse_leader_decision({
            "action": "continue_research", "rationale": "Need a primary source.",
            "summary": "Evidence is incomplete.", "uncertainties": ["Version date"],
            "next_tasks": [{"role": "collector", "kind": "research", "prompt": "Read this source.",
                            "focus_urls": ["https://example.com/docs"], "needs_web": True}],
        })
        self.assertEqual(decision["action"], "continue_research")
        self.assertEqual(decision["next_tasks"][0]["role"], "collector")
        self.assertEqual(LEADER_SCHEMA["properties"]["action"]["enum"],
                         ["blocked", "continue_research", "propose_merge", "request_extension", "request_seat_change", "submit_codex"])

    def test_rejects_arbitrary_actions_fields_secrets_and_private_urls(self):
        invalid = [
            {"action": ["run", "shell"]},
            {"action": "execute_command"},
            {"action": "continue_research", "command": "rm -rf"},
            {"action": "continue_research", "next_tasks": [{"role": "collector", "kind": "research",
                "prompt": "Bearer abcdefghijklmnop"}]},
            {"action": "continue_research", "next_tasks": [{"role": "collector", "kind": "research",
                "prompt": "fetch", "focus_urls": ["http://127.0.0.1/admin"]}]},
            {"action": "submit_codex", "next_tasks": [{"role": "checker", "kind": "logic_check", "prompt": "x"}]},
            {"action": "continue_research", "next_tasks": [{"role": "leader", "kind": "research", "prompt": "x"}]},
        ]
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(ValueError):
                parse_leader_decision(value)

    def test_limits_next_tasks_and_checks_needs_web_type(self):
        tasks = [{"role": "analyst", "kind": "extract", "prompt": "x"}] * 8
        with self.assertRaises(ValueError):
            parse_leader_decision({"action": "continue_research", "next_tasks": tasks})
        with self.assertRaises(ValueError):
            parse_leader_decision({"action": "continue_research", "next_tasks": [
                {"role": "analyst", "kind": "extract", "prompt": "x", "needs_web": "yes"}]})

    def test_replacements_feedback_and_seat_change_are_action_scoped(self):
        result = parse_leader_decision({"action": "continue_research",
            "replace_roles": ["worker-a", "worker-b"],
            "feedback": [{"worker": 1.0, "accepted": False, "reason": "Unsupported conclusion"}]})
        self.assertEqual(result["replace_roles"], ["worker-a", "worker-b"])
        self.assertEqual(result["feedback"][0]["worker"], 1)
        self.assertEqual(parse_leader_decision({"action": "request_seat_change", "member_count": 8})["member_count"], 8)
        invalid = [
            {"action": "continue_research", "member_count": 6},
            {"action": "submit_codex", "replace_roles": ["worker-a"]},
            {"action": "request_seat_change", "member_count": 2},
            {"action": "request_seat_change", "member_count": 9},
            {"action": "request_seat_change"},
            {"action": "continue_research", "replace_roles": ["a"] * 8},
            {"action": "continue_research", "replace_roles": ["x" * 101]},
            {"action": "continue_research", "feedback": [{"worker": 8, "accepted": True, "reason": "r"}]},
            {"action": "continue_research", "feedback": [{"worker": 1, "accepted": 1, "reason": "r"}]},
            {"action": "continue_research", "feedback": [{"worker": 1, "accepted": True, "reason": "r" * 1001}]},
            {"action": "continue_research", "feedback": [{"worker": 1, "accepted": True, "reason": "r"},
                {"worker": 1, "accepted": False, "reason": "conflict"}]},
            {"action": "continue_research", "feedback": [{"worker": 1, "accepted": True, "reason": "run command"}],
             "command": "arbitrary"},
        ]
        for index, value in enumerate(invalid):
            with self.subTest(index=index), self.assertRaises(ValueError):
                parse_leader_decision(value)
        self.assertIn("request_seat_change", LEADER_SCHEMA["properties"]["action"]["enum"])


if __name__ == "__main__":
    unittest.main()
