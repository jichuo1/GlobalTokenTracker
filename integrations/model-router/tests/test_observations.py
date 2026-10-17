from contextlib import closing
import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

from gtt_router.bridge import snapshot, quota_for, identifier
from gtt_router.models import Route
from gtt_router.models import now_ms
from gtt_router.observations import bounded_call, interface_command, load_observation, validate_snapshot


def envelope():
    now = now_ms()
    return {"protocol": "gtt.coordination", "protocol_version": 1, "operation": "snapshot", "status": "ok",
            "captured_at": now, "observation": {"consistent": True}, "warnings": [], "truncated": [],
            "sources": [{"source": "codex", "enabled": True, "last_synced_at": now, "files_seen": 1, "rows_ingested": 1}],
            "models": [{"app": "codex", "provider_id": "p", "model": "m", "samples": 1, "last_seen": now,
                        "mean_duration_ms": 100, "mean_ttft_ms": 10}],
            "quotas": [{"app": "codex", "account_key": None, "captured_at": now, "window_kind": "weekly",
                        "used": 1, "limit_value": 10, "used_percent": 10, "resets_at": None,
                        "remaining_percent": 5, "value_semantics": "allowance_percent", "state": "fresh", "scope": "app_account_window"}]}


class ObservationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="gtt-observation-")
        self.root = Path(self.temp.name)
        self.db = self.root / "ledger.db"
        self.config = {"ledger_path": str(self.db), "limits": {"quota_ttl_s": 300},
                       "gtt_interface": {"command": [sys.executable, "fixture"], "timeout_s": 2}}

    def tearDown(self):
        self.temp.cleanup()

    def seed(self):
        with closing(sqlite3.connect(self.db)) as conn, conn:
            conn.executescript("CREATE TABLE usage_events(app,provider_id,model,ts_start,duration_ms,ttft_ms);"
                               "CREATE TABLE sources(source,enabled,last_synced_at,files_seen,rows_ingested);"
                               "CREATE TABLE quota_snapshots(id INTEGER PRIMARY KEY,app,account,captured_at,window_kind,used,limit_value,used_percent,resets_at,raw_json);")
            conn.execute("INSERT INTO usage_events VALUES('codex','p','m',?,10,1)", (now_ms()-1,))

    def test_interface_preferred_and_no_implicit_refresh(self):
        with patch("gtt_router.observations.bounded_call", return_value=json.dumps(envelope())) as call:
            with patch("gtt_router.observations.snapshot") as legacy:
                result = load_observation(self.config)
        self.assertTrue(result["available"])
        self.assertEqual(result["origin"]["transport"], "gtt_cli_v1")
        self.assertEqual(result["quotas"][0]["remaining_percent"], 90)
        legacy.assert_not_called()
        args = call.call_args.args[0]
        self.assertIn("snapshot", args)
        self.assertNotIn("refresh", args)
        self.assertNotIn("--quota", args)
        self.assertEqual(args[args.index("--db")+1], str(self.db))

    def test_valid_unavailable_is_honored_without_sqlite_override(self):
        body = envelope() | {"status": "unavailable", "error": {"code": "ledger_busy", "message": "private-raw-error"}}
        with patch("gtt_router.observations.bounded_call", return_value=json.dumps(body)):
            with patch("gtt_router.observations.snapshot") as legacy:
                result = load_observation(self.config)
        legacy.assert_not_called()
        self.assertFalse(result["available"])
        self.assertEqual(result["reason"], "ledger_busy")
        self.assertNotIn("private-raw-error", json.dumps(result))

    def test_version_mismatch_falls_back_with_explicit_provenance(self):
        self.seed()
        before = hashlib.sha256(self.db.read_bytes()).digest()
        body = envelope() | {"protocol_version": 2}
        with patch("gtt_router.observations.bounded_call", return_value=json.dumps(body)):
            result = load_observation(self.config)
        self.assertTrue(result["available"])
        self.assertEqual(result["origin"]["transport"], "legacy_sqlite")
        self.assertEqual(result["origin"]["fallback_reason"], "gtt_interface_version_mismatch")
        self.assertEqual(hashlib.sha256(self.db.read_bytes()).digest(), before)

    def test_fallback_can_be_disabled(self):
        self.config["gtt_interface"]["allow_legacy_fallback"] = False
        with patch("gtt_router.observations.bounded_call", side_effect=ValueError("gtt_interface_timeout")):
            result = load_observation(self.config)
        self.assertFalse(result["available"])
        self.assertEqual(result["origin"]["transport"], "none")
        self.assertFalse(self.db.exists())

    def test_missing_cli_preserves_legacy_compatibility(self):
        self.seed()
        with patch("gtt_router.observations.interface_command", return_value=None):
            self.assertEqual(load_observation(self.config)["origin"]["fallback_reason"], "gtt_cli_missing")

    def test_expired_future_or_inconsistent_envelopes_rejected(self):
        for changes in [{"captured_at": now_ms()-60000}, {"captured_at": now_ms()+60000},
                        {"observation": {"consistent": False}}, {"status": "success"}, {"protocol_version": True}]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                validate_snapshot(json.dumps(envelope() | changes))

    def test_unknown_fields_and_account_original_not_exported(self):
        body = envelope() | {"raw_json": "private-raw", "error": "private-error"}
        body["quotas"][0]["account"] = "private-account"
        body["models"][0]["raw_ref"] = "private-file"
        self.assertNotIn("private-", json.dumps(validate_snapshot(json.dumps(body))))

    def test_raw_account_in_opaque_field_is_rejected(self):
        body = envelope()
        body["quotas"][0]["account_key"] = "private-account"
        with self.assertRaises(ValueError):
            validate_snapshot(json.dumps(body))

    def test_quota_does_not_trust_supplied_remaining_or_invalid_age_reset_semantics(self):
        for changes in [{"captured_at": now_ms()+60000}, {"captured_at": now_ms()-600000},
                        {"resets_at": now_ms()-1}, {"used_percent": -1}, {"used_percent": 101},
                        {"window_kind": "credits", "value_semantics": "remaining_balance"},
                        {"window_kind": "session_ctx", "value_semantics": "context_usage"}, {"state": "unknown"}]:
            body = envelope()
            body["quotas"][0].update(changes)
            with self.subTest(changes=changes):
                self.assertIsNone(validate_snapshot(json.dumps(body))["quotas"][0]["remaining_percent"])

    def test_bool_numbers_nonfinite_duplicate_keys_and_big_sections_rejected(self):
        bad = []
        body = envelope(); body["models"][0]["samples"] = True; bad.append(json.dumps(body))
        body = envelope(); body["sources"] *= 129; bad.append(json.dumps(body))
        body = envelope(); body["quotas"][0]["used_percent"] = float("nan"); bad.append(json.dumps(body))
        bad.append('{"protocol":"gtt.coordination","protocol":"other"}')
        for data in bad:
            with self.assertRaises(ValueError):
                validate_snapshot(data)

    def test_partial_and_additive_metadata_are_compatible_and_bounded(self):
        body = envelope() | {"status": "partial", "future_extension": {}, "warnings": ["models_schema_unavailable", "private error text"],
                             "truncated": ["models", {"raw": "private"}]}
        result = validate_snapshot(json.dumps(body))
        self.assertEqual(result["origin"]["status"], "partial")
        self.assertEqual(result["warnings"], ["models_schema_unavailable"])
        self.assertEqual(result["truncated"], ["models"])

    def test_transport_output_timeout_and_failure_are_bounded(self):
        command = [sys.executable, str(Path(__file__).with_name("observation_fixture.py"))]
        self.assertEqual(json.loads(bounded_call(command+["valid"],2)), {"ok": True})
        for case, reason in [("overflow", "output_limit"), ("timeout", "timeout"), ("error", "unsupported_or_failed")]:
            with self.subTest(case=case), self.assertRaisesRegex(ValueError, reason):
                bounded_call(command+[case],1)

    def test_commands_are_argument_arrays_with_absolute_executable(self):
        for command in ["shell -c command", [], ["relative.exe"], [sys.executable, None], [sys.executable, "\0"]]:
            with self.assertRaises(ValueError):
                interface_command({"command": command})
        self.assertEqual(interface_command({"command": [sys.executable, "literal; argument"]}), [sys.executable, "literal; argument"])

    def test_namespaced_provider_is_valid_but_absolute_paths_and_urls_are_not(self):
        self.assertTrue(identifier("provider:region"))
        self.assertFalse(identifier("C:/private/file"))
        self.assertFalse(identifier("https://private.invalid"))

    def test_legacy_quota_rejects_future_and_out_of_range_and_redacts_labels(self):
        self.seed()
        with closing(sqlite3.connect(self.db)) as conn, conn:
            for i, (captured, pct) in enumerate([(now_ms()+60000,10), (now_ms(),110)]):
                conn.execute("INSERT INTO quota_snapshots VALUES(?,'codex',NULL,?,'weekly',1,10,?,NULL,'private-raw')",(i,captured,pct))
            conn.execute("INSERT INTO usage_events VALUES('codex','https://secret.invalid/path','C:/private/config',?,1,1)",(now_ms()-1,))
        result = snapshot(self.db)
        self.assertIsNone(result["quotas"][0]["remaining_percent"])
        self.assertNotIn("private", json.dumps(result))

    def test_cached_quota_is_rechecked_when_routing(self):
        r=Route(source="s",provider="p",model="m",quota_app="codex")
        body=validate_snapshot(json.dumps(envelope()))
        self.assertTrue(quota_for(r,body)["known"])
        with patch("gtt_router.bridge.now_ms",return_value=body["captured_at"]+301000):
            self.assertFalse(quota_for(r,body)["known"])
        self.assertEqual(body["quotas"][0]["remaining_percent"],90)


if __name__ == "__main__":
    unittest.main()
