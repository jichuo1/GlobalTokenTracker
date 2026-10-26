import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

from gtt_router.models import Task, Route
from gtt_router.store import StateStore
from gtt_router.usage import clean_usage, normalize, total_tokens


class UsageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / "state.sqlite"
        self.store = StateStore(self.path)

    def tearDown(self):
        self.store.close()
        self.temp.cleanup()

    def attempt(self, **changes):
        return {"worker": 0, "source": "zcode", "provider": "fixture-provider", "model": "fixture-model",
                "route_id": "fixture-route", "started_at": 1000, "state": "failed",
                "tokens_charged": 23, "token_measurement": "reported", **changes}

    def save(self, attempt, results=None):
        self.store.save_job("fixture-job", "failed", Task("extract", "private prompt"),
                            {"attempts": [attempt], "results": results or []})

    def records(self):
        return [(r[0], json.loads(r[1])) for r in self.store.conn.execute("SELECT seq,payload FROM usage_records ORDER BY seq")]

    def test_camel_cache_is_subset_of_input_and_reasoning_not_added_again(self):
        u = {"inputTokens": 100, "outputTokens": 20, "totalTokens": 120, "cacheReadTokens": 50,
             "cacheWriteTokens": 10, "reasoningTokens": 15}
        n = normalize(u)
        self.assertEqual((n["input_tokens"], n["cache_read_tokens"], n["cache_write_5m_tokens"], n["output_tokens"]), (40, 50, 10, 20))
        self.assertEqual(total_tokens(u), 120)

    def test_snake_uncached_input_and_incomplete_totals(self):
        self.assertEqual(total_tokens({"input_tokens": 100, "output_tokens": 20, "cache_read_tokens": 50, "total_tokens": 120}), 170)
        self.assertEqual(normalize({"total_tokens": 23})["unclassified_tokens"], 23)
        self.assertEqual(total_tokens({"input_tokens": 10, "output_tokens": 2}), 12)

    def test_non_numbers_secrets_and_unrecognized_fields_are_not_exported(self):
        self.assertEqual(clean_usage({"input_tokens": True, "outputTokens": -1, "total_tokens": float("inf"), "apiKey": "private", "prompt": "private"}), {})
        self.assertIsNone(total_tokens({"reasoning_tokens": 7}))
        self.assertIsNone(total_tokens({"totalTokens": 10**30}))

    def test_failed_usage_is_durable_and_replay_is_idempotent(self):
        self.save(self.attempt())
        first = self.records()
        self.save(self.attempt())
        self.assertEqual(self.records(), first)
        self.store.close()
        self.store = StateStore(self.path)
        self.assertEqual(self.records(), first)
        self.assertEqual(first[0][1]["unclassified_tokens"], 23)
        self.assertNotIn("private prompt", json.dumps(first))

    def test_upgrade_keeps_record_key_and_advances_watermark(self):
        self.save(self.attempt())
        first = self.records()[0]
        self.save(self.attempt(usage={"inputTokens": 20, "outputTokens": 3, "totalTokens": 23}))
        last = self.records()[0]
        self.assertGreater(last[0], first[0])
        self.assertEqual(last[1]["record_key"], first[1]["record_key"])
        self.assertEqual(last[1]["unclassified_tokens"], 0)

    def test_job_and_metering_failure_roll_back_together(self):
        self.store.conn.execute("CREATE TRIGGER reject_metering BEFORE INSERT ON usage_records BEGIN SELECT RAISE(ABORT,'fixture'); END")
        with self.assertRaises(sqlite3.IntegrityError): self.save(self.attempt())
        self.assertEqual(self.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)

    def test_legacy_success_recovers_dimensions_but_estimates_remain_estimates(self):
        result = {"worker": 0, "route": {"route_id": "fixture-route"}, "usage": {"inputTokens": 20, "outputTokens": 3}}
        self.save(self.attempt(state="succeeded"), [result])
        self.assertEqual(self.records()[0][1]["input_tokens"], 20)
        legacy = self.attempt(state="succeeded")
        legacy.pop("tokens_charged"); legacy.pop("token_measurement")
        self.save(legacy, [result])
        self.assertEqual(self.records()[0][1]["measurement"], "reported")
        self.assertEqual(self.records()[0][1]["input_tokens"], 20)
        self.save(self.attempt(token_measurement="estimated"))
        self.assertEqual(self.records()[0][1]["measurement"], "estimated")

    def test_native_identity_and_reported_probe_are_preserved(self):
        self.save(self.attempt(source="opencode", native_dedup_key="opencode:msg:msg_fixture", native_backed=True))
        self.assertEqual(self.records()[0][1]["native_dedup_key"], "opencode:msg:msg_fixture")
        self.store.record_probe(Route("zcode", "p", "m"), True, "generation_probe_ok", 4,
                                {"token_measurement": "reported", "started_at": 1000, "duration_ms": 10,
                                 "usage": {"input_tokens": 3, "output_tokens": 1}})
        self.assertTrue(self.records()[-1][1]["record_key"].startswith("probe:"))
        self.assertEqual(self.records()[-1][1]["input_tokens"], 3)
        self.assertEqual((self.records()[-1][1]["ts_start"], self.records()[-1][1]["duration_ms"]), (1000, 10))
