import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from gtt_router.adapters import ExecutionContext, classify_error, command_json, validate_result, native_slot
from gtt_router.benchmarks import load_profiles, refresh_aa
from gtt_router.bridge import account_key, quota_for, snapshot
from gtt_router.catalog import merge, zcode_routes
from gtt_router.daemon import load_config
from gtt_router.hook_guard import decide, public_url, excerpt
from gtt_router.models import Route, Task, WorkerFailure, now_ms
from gtt_router.policy import rank_routes
from gtt_router.service import RouterService
from gtt_router.store import StateStore


def content(summary="bounded result"):
    return {"summary": summary, "findings": [], "sources": [], "uncertainties": [], "complete": True}


def response(summary="bounded result"):
    return {"text": json.dumps(content(summary)), "usage": {"total_tokens": 100}, "tool_calls": [], "observed_urls": []}


def route(source="fast", provider="p", model="m", quality=41, latency=1):
    return Route(source=source, provider=provider, model=model, auth_hint=True, adapter="fake", max_parallel=1,
                 profile={"quality_prior": quality, "latency_prior_s": latency, "preferred_kinds": ["code_review", "research"]})


class FakeCatalog:
    def __init__(self, routes, ledger=None):
        self.routes = routes
        self.ledger = ledger or {"sources": [], "models": [], "quotas": []}
        self.refreshed_at = now_ms()

    def refresh(self, force=False):
        self.refreshed_at = now_ms()
        return self.public()

    def public(self):
        return {"refreshed_at": self.refreshed_at, "routes": [r.public() for r in self.routes],
                "ledger": self.ledger, "discovery_errors": [], "availability_note": "test fixture"}


class RouterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.store = StateStore(self.root / "unit.sqlite")
        self.services = []

    def tearDown(self):
        for service in self.services:
            service.shutdown()
        self.store.close()
        self.temp.cleanup()

    def service(self, routes, adapter, **overrides):
        config = {"state_dir": str(self.root / str(len(self.services))), "limits": {
            "global_parallel": 2, "combined_parallel": 4, "max_active_jobs": 3,
            "external_queue_grace_s": .05, "max_context_chars": 64000}, "allow_codex_fallback": True,
                  **overrides}
        service = RouterService(config, FakeCatalog(routes), {"fake": adapter})
        self.services.append(service)
        return service

    def wait(self, service, job_id, timeout=4):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            state = service.status(job_id)["state"]
            if state in {"succeeded", "partial", "failed", "cancelled", "needs_codex"}:
                return service.store.job(job_id)
            time.sleep(.01)
        self.fail("job did not reach a terminal state")

    def test_task_rejects_credentials_and_unbounded_context(self):
        for text in ["Bearer " + "abc123" * 10, "sk-" + "x" * 32, "-----BEGIN PRIVATE KEY-----"]:
            with self.assertRaises(ValueError):
                Task.parse({"kind": "code_review", "prompt": text})
        with self.assertRaises(ValueError):
            Task.parse({"kind": "research", "prompt": "x" * 64001})

    def test_fanout_defaults_to_discrepancy_verifier(self):
        self.assertTrue(Task.parse({"kind": "research", "prompt": "bounded", "fanout": 2}).verify)

    def test_mutating_task_kinds_are_not_supported(self):
        for kind in ["implementation", "deploy", "shell", "merge"]:
            with self.assertRaises(ValueError):
                Task.parse({"kind": kind, "prompt": "bounded"})

    def test_public_route_does_not_export_runtime_credentials(self):
        r = route()
        r.runtime = {"apiKey": "very-private", "provider_config": "credential-file"}
        self.assertNotIn("very-private", json.dumps(r.public()))
        self.assertNotIn("credential-file", json.dumps(r.public()))

    def test_static_catalog_preserves_disabled_providers(self):
        path = self.root / "providers.json"
        path.write_text(json.dumps({"config": {"providerConfigRules": {"providerRules": [
            {"providerId": "off", "enabled": False, "config": {"access": {"type": "api-key", "apiKey": "secret"}, "personalModelIds": ["m"]}},
            {"providerId": "on", "config": {"access": {"type": "api-key", "apiKey": "secret"}, "personalModelIds": ["m"]}},
        ]}, "modelConfigRules": {"providerModelRules": [{"providerId": "on", "modelId": "m", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "max"]}}}}]}}}))
        routes = zcode_routes({"id": "z", "provider_config": str(path)})
        self.assertEqual([(r.provider, r.effort) for r in routes], [("on", "max")])
        self.assertNotIn("secret", json.dumps([r.public() for r in routes]))

    def test_readonly_gtt_snapshot_excludes_raw_data(self):
        db = self.root / "ledger.sqlite"
        with sqlite3.connect(db) as c:
            c.executescript("CREATE TABLE usage_events(app,provider_id,model,ts_start,duration_ms,ttft_ms);"
                            "CREATE TABLE sources(source,enabled,last_synced_at,files_seen,rows_ingested);"
                            "CREATE TABLE quota_snapshots(id INTEGER PRIMARY KEY,app,account,captured_at,window_kind,used,limit_value,used_percent,resets_at,raw_json);")
            c.execute("INSERT INTO usage_events VALUES('zcode','p','m',?,100,10)", (now_ms(),))
            c.execute("INSERT INTO quota_snapshots VALUES(1,'zcode',NULL,?,'daily',1,10,10,NULL,'credential-bearing-raw')", (now_ms(),))
        c.close()
        before = hashlib.sha256(db.read_bytes()).digest()
        s = snapshot(db)
        self.assertTrue(s["available"])
        self.assertEqual(s["quotas"][0]["remaining_percent"], 90)
        self.assertNotIn("credential-bearing-raw", json.dumps(s))
        self.assertEqual(hashlib.sha256(db.read_bytes()).digest(), before)

    def test_absent_ledger_is_unknown(self):
        self.assertFalse(snapshot(self.root / "missing.db")["available"])
        self.assertFalse(quota_for(route(), {"quotas": []})["known"])

    def test_wrong_account_quota_does_not_block_route(self):
        r = route()
        r.quota_app = "zcode"
        ledger = {"quotas": [{"app": "zcode", "account_key": account_key("other-account"), "remaining_percent": 0}]}
        self.assertFalse(quota_for(r, ledger)["known"])

    def test_exhausted_fresh_quota_blocks_and_unknown_is_not_zero(self):
        r = route()
        r.quota_app = "zcode"
        task = Task("code_review", "bounded")
        candidates, rejected = rank_routes(task, [r], {"quotas": [{"app": "zcode", "account_key": None, "remaining_percent": 0}]}, self.store)
        self.assertFalse(candidates)
        self.assertIn("fresh_quota_exhausted", rejected[0]["reasons"])
        self.assertTrue(rank_routes(task, [r], {"quotas": []}, self.store)[0])

    def test_provider_auth_circuit_blocks_sibling_models(self):
        a, b, c = route(model="a"), route(model="b"), route(provider="other", model="c")
        self.store.failure("provider:" + a.failure_domain, "auth_failed", 60)
        candidates, _ = rank_routes(Task("code_review", "bounded"), [a, b, c], {"quotas": []}, self.store)
        self.assertEqual([c["route"].model for c in candidates], ["c"])

    def test_benchmark_versions_and_unmatched_aliases_remain_distinct(self):
        path = self.root / "bench.json"
        path.write_text(json.dumps({"schema_version": 1, "models": [{"aliases": ["exact-v1"], "intelligence": 41, "benchmark_version": "Terminal-Bench 4.0"}]}))
        profiles = load_profiles(path)
        self.assertNotIn("exact", profiles)
        self.assertNotIn("exact-v2", profiles)
        self.assertEqual(profiles["exact-v1"]["benchmark_version"], "Terminal-Bench 4.0")

    def test_deadline_rejects_slow_high_quality_route(self):
        fast, slow = route(model="fast", latency=1), route(model="slow", quality=90, latency=200)
        candidates, rejected = rank_routes(Task("logic_check", "bounded", deadline_s=10), [slow, fast], {"quotas": []}, self.store)
        self.assertEqual(candidates[0]["route"].model, "fast")
        self.assertIn("unlikely_to_meet_deadline", rejected[0]["reasons"])

    def test_web_requirement_never_falls_back_to_text_only(self):
        candidates, rejected = rank_routes(Task("research", "bounded", needs_web=True), [route()], {"quotas": []}, self.store)
        self.assertFalse(candidates)
        self.assertIn("web_not_supported", rejected[0]["reasons"])

    def test_guard_denies_shell_writes_agents_and_private_urls(self):
        for tool in ["run_command", "write_to_file", "invoke_subagent", "call_mcp_tool", "view_file"]:
            self.assertEqual(decide({"toolCall": {"name": tool, "args": {}}}, True)[0], "deny")
        for url in ["file:///secret", "http://127.0.0.1/test", "http://192.168.1.2/test", "https://localhost/test", "https://user:password@example.com/test"]:
            self.assertFalse(public_url(url))
        self.assertEqual(decide({"toolCall": {"name": "read_url_content", "args": {"Url": "https://example.com/doc"}}}, True)[0], "allow")
        self.assertEqual(decide({"toolCall": {"name": "search_web", "args": {"query": "public docs"}}}, False)[0], "deny")

    def test_urls_in_model_text_are_not_web_evidence(self):
        body = content()
        body["sources"] = [{"url": "https://example.com/doc", "title": "doc", "claim": "a"}]
        with self.assertRaises(WorkerFailure):
            validate_result({"text": json.dumps(body), "observed_urls": []}, True)
        verified = validate_result({"text": json.dumps(body), "observed_urls": ["https://example.com/doc/"]}, True)
        self.assertTrue(verified["content"]["sources"][0]["verified_by_tool"])

    def test_output_contract_rejects_partial_and_non_json_reports(self):
        for text in ["helpful prose", json.dumps({**content(), "complete": False}), json.dumps({"summary": "x", "complete": True})]:
            with self.assertRaises(WorkerFailure):
                validate_result({"text": text}, False)

    def test_error_taxonomy_scopes_auth_and_rate_limits(self):
        self.assertEqual(classify_error({"data": {"code": "provider_not_configured"}}).scope, "provider")
        self.assertEqual(classify_error("HTTP 429").code, "rate_limited")
        self.assertEqual(classify_error("model_not_found").scope, "route")

    def test_async_submit_and_provider_failover(self):
        first, sibling, fallback = route(model="bad", quality=50), route(model="sibling", quality=49), route(source="other", provider="other", model="good")
        calls = []
        def adapter(r, prompt, task, ctx):
            calls.append(r.model)
            if r.provider == "p":
                raise WorkerFailure("auth_failed", scope="provider", retry_after_s=30)
            return response()
        service = self.service([first, sibling, fallback], adapter)
        started = time.monotonic()
        job = service.submit({"kind": "code_review", "prompt": "bounded"})
        self.assertLess(time.monotonic() - started, .5)
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual(calls, ["bad", "good"])
        self.assertEqual(len(result["attempts"]), 2)

    def test_capacity_overflow_uses_other_external_source_before_codex(self):
        entered, release = threading.Event(), threading.Event()
        def adapter(r, prompt, task, ctx):
            if r.source == "preferred":
                entered.set()
                while not release.wait(.01):
                    ctx.check()
            return response(r.source)
        preferred, alternate = route(source="preferred", quality=50), route(source="alternate", provider="q")
        service = self.service([preferred, alternate], adapter)
        first = service.submit({"kind": "code_review", "prompt": "one", "verify": False})
        self.assertTrue(entered.wait(1))
        second = service.submit({"kind": "code_review", "prompt": "two", "verify": False})
        result = self.wait(service, second["job_id"])
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual(result["results"][0]["route"]["source"], "alternate")
        release.set()
        self.wait(service, first["job_id"])

    def test_full_external_capacity_returns_explicit_luna_handoff(self):
        entered, release = threading.Event(), threading.Event()
        def adapter(r, prompt, task, ctx):
            entered.set()
            while not release.wait(.01):
                ctx.check()
            return response()
        service = self.service([route()], adapter)
        first = service.submit({"kind": "extract", "prompt": "one"})
        self.assertTrue(entered.wait(1))
        second = service.submit({"kind": "extract", "prompt": "two"})
        result = self.wait(service, second["job_id"])
        self.assertEqual(result["state"], "needs_codex")
        self.assertEqual(result["results"][0]["codex_fallback"]["recommended_model"], "gpt-6-luna")
        completed = service.complete_codex(second["job_id"], 0, "gpt-6-luna", content())
        self.assertEqual(completed["state"], "succeeded")
        release.set()
        self.wait(service, first["job_id"])

    def test_codex_work_claim_avoids_duplicate_external_generation(self):
        calls = []
        service = self.service([route()], lambda *args: calls.append(1) or response())
        service.sync_codex_agents("root", [{"id": "local-agent", "model": "gpt-6-luna", "workflow_id": "workflow", "work_key": "path:review", "status": "running"}])
        result = service.submit({"kind": "code_review", "prompt": "bounded", "workflow_id": "workflow", "work_key": "path:review"})
        self.assertEqual(result["state"], "covered")
        self.assertFalse(calls)
        # Implementation and review are intentionally different purposes.
        job = service.submit({"kind": "code_review", "prompt": "bounded", "workflow_id": "workflow", "work_key": "path:implementation"})
        self.assertEqual(self.wait(service, job["job_id"])["state"], "succeeded")

    def test_cancel_stops_running_attempt_and_prevents_fallback(self):
        entered = threading.Event()
        def adapter(r, prompt, task, ctx):
            entered.set()
            while True:
                ctx.check()
                time.sleep(.01)
        service = self.service([route()], adapter)
        job = service.submit({"kind": "logic_check", "prompt": "bounded"})
        self.assertTrue(entered.wait(1))
        service.cancel(job["job_id"])
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "cancelled")
        self.assertFalse(any(r.get("codex_fallback") for r in result["results"]))
        self.assertEqual(service.total_inflight, 0)

    def test_fanout_collects_early_then_crosschecks(self):
        r = route()
        r.max_parallel = 2
        def adapter(route, prompt, task, ctx):
            if "counterexamples" in prompt and task.kind != "cross_check":
                time.sleep(.1)
            return response("verifier" if task.kind == "cross_check" else "worker")
        service = self.service([r], adapter)
        job = service.submit({"kind": "research", "prompt": "provided evidence", "fanout": 2})
        early = service.collect(job["job_id"], wait_ms=500)
        self.assertTrue(early["results"])
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual(len(result["results"]), 3)
        self.assertTrue(result["results"][-1]["verifier"])

    def test_attempt_budget_is_shared_across_workers(self):
        r = route()
        r.max_parallel = 2
        service = self.service([r, route(provider="q", model="q")], lambda *a: (_ for _ in ()).throw(WorkerFailure("temporary")))
        job = service.submit({"kind": "extract", "prompt": "bounded", "fanout": 2, "verify": False, "max_attempts": 2})
        result = self.wait(service, job["job_id"])
        self.assertLessEqual(result["attempt_count"], 2)

    def test_token_reservations_prevent_parallel_budget_overcommit(self):
        r = route()
        r.max_parallel = 2
        def adapter(*args):
            time.sleep(.05)
            return response()
        service = self.service([r], adapter)
        job = service.submit({"kind": "extract", "prompt": "bounded", "fanout": 2, "verify": False,
                              "max_output_tokens": 2048, "token_budget": 4000})
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "partial")
        self.assertEqual(result["attempt_count"], 1)
        self.assertEqual(result["tokens_reserved"], 0)

    def test_duplicate_work_key_reuses_job(self):
        service = self.service([route()], lambda *a: (time.sleep(.1), response())[1])
        task = {"kind": "extract", "prompt": "bounded", "work_key": "lookup:contract"}
        one, two = service.submit(task), service.submit(task)
        self.assertEqual(one["job_id"], two["job_id"])
        self.assertTrue(two["deduplicated"])

    def test_restart_marks_pending_work_interrupted_without_replaying(self):
        path = self.root / "recover.sqlite"
        s = StateStore(path)
        task = Task("extract", "bounded")
        s.save_job("pending", "running", task, {"results": [], "attempts": []})
        s.close()
        s = StateStore(path)
        self.assertEqual(s.job("pending")["state"], "interrupted")
        s.close()

    def test_generic_adapter_is_a_real_no_shell_process_protocol(self):
        fixture = Path(__file__).with_name("worker_fixture.py")
        r = route(model="fixture-model")
        r.runtime = {"command": [sys.executable, str(fixture)]}
        ctx = ExecutionContext(threading.Event(), time.monotonic() + 5, self.root / "process")
        result = command_json(r, "quote ' & $(not-a-shell-command)", Task("extract", "bounded"), ctx)
        self.assertEqual(result["actual_selection"]["model"], "fixture-model")
        self.assertEqual(validate_result(result, False)["content"]["summary"], "fixture response")

    def test_missing_usage_is_charged_as_estimate(self):
        service = self.service([route()], lambda *a: {"text": json.dumps(content()), "usage": {}, "observed_urls": []})
        job = service.submit({"kind": "extract", "prompt": "bounded", "max_output_tokens": 2048})
        result = self.wait(service, job["job_id"])
        self.assertGreater(result["tokens_used"], 2048)
        self.assertEqual(result["tokens_used"], result["tokens_estimated"])
        self.assertEqual(result["attempts"][0]["token_measurement"], "estimated")

    def test_verifier_reports_are_included_in_reservation(self):
        r = route()
        r.max_parallel = 2
        def adapter(route, prompt, task, ctx):
            return response("x" * 7000)
        service = self.service([r], adapter, allow_codex_fallback=False)
        job = service.submit({"kind": "research", "prompt": "bounded", "fanout": 2,
                              "max_output_tokens": 1024, "token_budget": 6000})
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "partial")
        self.assertEqual(result["attempt_count"], 2)
        self.assertEqual(result["results"][-1]["error"]["code"], "token_budget_exhausted")

    def test_no_fallback_when_pinned_route_is_unavailable(self):
        service = self.service([route()], lambda *a: response())
        job = service.submit({"kind": "extract", "prompt": "bounded", "preferred_route": "missing", "allow_fallback": False})
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "failed")
        self.assertFalse(result["results"][0].get("codex_fallback"))

    def test_cancelled_handoff_cannot_be_completed(self):
        service = self.service([], lambda *a: response())
        job = service.submit({"kind": "extract", "prompt": "bounded"})
        self.assertEqual(self.wait(service, job["job_id"])["state"], "needs_codex")
        service.cancel(job["job_id"])
        with self.assertRaises(ValueError):
            service.complete_codex(job["job_id"], 0, "gpt-6-luna", content(), {"total_tokens": 100})
        self.assertEqual(service.status(job["job_id"])["state"], "cancelled")

    def test_codex_handoff_usage_is_accounted(self):
        service = self.service([], lambda *a: response())
        job = service.submit({"kind": "extract", "prompt": "bounded"})
        self.wait(service, job["job_id"])
        service.complete_codex(job["job_id"], 0, "gpt-6-luna", content(), {"total_tokens": 234})
        self.assertEqual(service.store.job(job["job_id"])["tokens_used"], 234)

    def test_caller_handoff_over_budget_is_visible(self):
        service = self.service([], lambda *a: response())
        job = service.submit({"kind": "extract", "prompt": "bounded", "token_budget": 12000})
        self.wait(service, job["job_id"])
        result = service.complete_codex(job["job_id"], 0, "gpt-6-luna", content(), {"total_tokens": 13000})
        self.assertEqual(result["state"], "partial")
        self.assertEqual(result["error"]["code"], "caller_handoff_exceeded_token_budget")

    def test_only_this_successful_web_cache_can_be_read(self):
        cache = self.root / "content.md"
        other = self.root / "credentials.json"
        audit = self.root / "audit.jsonl"
        audit.write_text(json.dumps({"phase": "post", "tool": "read_url_content", "success": True, "content_file": str(cache)}) + "\n")
        self.assertEqual(decide({"toolCall": {"name": "view_file", "args": {"AbsolutePath": str(cache), "StartLine": 1, "EndLine": 100}}}, True, audit)[0], "allow")
        self.assertEqual(decide({"toolCall": {"name": "view_file", "args": {"AbsolutePath": str(other)}}}, True, audit)[0], "deny")

    def test_codex_claim_lease_expires(self):
        service = self.service([route()], lambda *a: response())
        service.sync_codex_agents("root", [{"id": "agent", "workflow_id": "w", "work_key": "k", "status": "running"}])
        service.codex_agents["root"]["expires_at"] = now_ms() - 1
        job = service.submit({"kind": "extract", "prompt": "bounded", "workflow_id": "w", "work_key": "k"})
        self.assertEqual(self.wait(service, job["job_id"])["state"], "succeeded")

    def test_truncated_generation_usage_is_not_lost(self):
        def adapter(*a):
            raise WorkerFailure("output_truncated", usage={"totalTokens": 2048}, retry_after_s=0)
        service = self.service([route()], adapter, allow_codex_fallback=False)
        job = service.submit({"kind": "extract", "prompt": "bounded", "max_output_tokens": 2048})
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["tokens_used"], 2048)

    def test_cancel_wins_while_handoff_validation_is_in_progress(self):
        service = self.service([], lambda *a: response())
        job = service.submit({"kind": "extract", "prompt": "bounded"})
        self.wait(service, job["job_id"])
        entered, release = threading.Event(), threading.Event()
        outcomes = []
        def delayed(result, web):
            entered.set()
            release.wait(1)
            return validate_result(result, web)
        def complete():
            try:
                service.complete_codex(job["job_id"], 0, "gpt-6-luna", content())
                outcomes.append("accepted")
            except ValueError:
                outcomes.append("rejected")
        with patch("gtt_router.service.validate_result", delayed):
            thread = threading.Thread(target=complete)
            thread.start()
            self.assertTrue(entered.wait(1))
            service.cancel(job["job_id"])
            release.set()
            thread.join(1)
        self.assertEqual(outcomes, ["rejected"])
        self.assertEqual(service.status(job["job_id"])["state"], "cancelled")

    def test_handoff_does_not_reuse_verifier_that_saw_only_one_report(self):
        entered, release = threading.Event(), threading.Event()
        def adapter(r, prompt, task, ctx):
            entered.set()
            release.wait(.2)
            return response()
        service = self.service([route()], adapter)
        job = service.submit({"kind": "extract", "prompt": "bounded", "fanout": 2, "verify": True})
        self.assertTrue(entered.wait(1))
        time.sleep(.1)
        release.set()
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "needs_codex")
        self.assertFalse(any(r["verifier"] for r in result["results"]))
        fallback = next(r for r in result["results"] if r.get("codex_fallback"))
        completed = service.complete_codex(job["job_id"], fallback["worker"], "gpt-6-luna", content(), {"total_tokens": 100})
        self.assertEqual(completed["state"], "needs_codex")
        self.assertTrue(completed["results"][-1]["verifier"])

    def test_invalid_reported_token_types_are_not_zero(self):
        for invalid in [False, -1, float("nan"), "100"]:
            service = self.service([route()], lambda *a, value=invalid: {"text": json.dumps(content()), "usage": {"total_tokens": value}, "observed_urls": []})
            job = service.submit({"kind": "extract", "prompt": "bounded"})
            result = self.wait(service, job["job_id"])
            self.assertGreater(result["tokens_estimated"], 0)

    def test_feedback_is_idempotent_and_can_be_corrected(self):
        service = self.service([route()], lambda *a: response())
        job = service.submit({"kind": "extract", "prompt": "bounded"})
        self.wait(service, job["job_id"])
        service.feedback(job["job_id"], 0, True)
        twice = service.feedback(job["job_id"], 0, True)
        self.assertEqual(twice["samples"], 1)
        corrected = service.feedback(job["job_id"], 0, False)
        self.assertEqual(corrected, {"samples": 1, "acceptance": 0.0})

    def test_logical_tasks_have_a_quality_floor_by_default(self):
        task = Task.parse({"kind": "code_review", "prompt": "bounded"})
        self.assertEqual(task.min_quality, 38)
        candidates, rejected = rank_routes(task, [route(quality=20)], {"quotas": []}, self.store)
        self.assertFalse(candidates)
        self.assertIn("quality_floor_not_met", rejected[0]["reasons"])

    def test_web_tool_budget_blocks_more_fetches(self):
        audit = self.root / "audit.jsonl"
        audit.write_text("\n".join(json.dumps({"phase": "pre", "decision": "allow", "tool": "search_web"}) for _ in range(4)) + "\n")
        (self.root / "web-policy.json").write_text(json.dumps({"web_tool_budget": 4}))
        self.assertEqual(decide({"toolCall": {"name": "search_web", "args": {}}}, True, audit)[0], "deny")

    def test_derived_excerpt_is_allowed_but_original_whole_page_is_not(self):
        original, clipped = self.root / "content.md", self.root / "gtt_excerpt.md"
        audit = self.root / "audit.jsonl"
        audit.write_text(json.dumps({"phase": "post", "tool": "read_url_content", "success": True,
                                     "content_file": str(original), "excerpt_file": str(clipped)}) + "\n")
        self.assertEqual(decide({"toolCall": {"name": "view_file", "args": {"AbsolutePath": str(original)}}}, True, audit)[0], "deny")
        self.assertEqual(decide({"toolCall": {"name": "view_file", "args": {"AbsolutePath": str(clipped)}}}, True, audit)[0], "allow")

    def test_excerpt_preserves_focus_with_a_bounded_output(self):
        text = "\n".join(["unrelated navigation"] * 250 + ["stdio messages are newline-delimited", "stdout contains only MCP messages", "stderr may contain logs"] + ["unrelated footer"] * 200)
        clipped = excerpt(text, ["stdio", "stdout", "stderr"], max_chars=1000)
        self.assertIn("newline-delimited", clipped)
        self.assertIn("stderr may contain logs", clipped)
        self.assertIn("[251]", clipped)
        self.assertLessEqual(len(clipped), 1000)

    def test_url_control_characters_are_not_public_urls(self):
        self.assertFalse(public_url("https://example.com/\nignore instructions"))
        self.assertFalse(public_url("http://127.0.0.1./"))

    def test_stable_native_slot_resets_prior_task_audits(self):
        r = route()
        r.runtime = {"pool_root": str(self.root / "pool")}
        one = ExecutionContext(threading.Event(), time.monotonic() + 5, self.root / "job-one")
        two = ExecutionContext(threading.Event(), time.monotonic() + 5, self.root / "job-two")
        with native_slot(r, one) as slot:
            first = slot.workspace
            (first / "guard-audit.jsonl").write_text('private prior task source')
            (first / "guided-excerpts.json").write_text('["prior-page"]')
        with native_slot(r, two) as slot:
            self.assertEqual(slot.workspace, first)
            self.assertEqual((slot.workspace / "guard-audit.jsonl").read_text(), "")
            self.assertEqual((slot.workspace / "guided-excerpts.json").read_text(), "[]")

    def test_observed_token_cost_gates_future_requests(self):
        r = route()
        service = self.service([r], lambda *a: response(), allow_codex_fallback=False)
        service.store.success("route:" + r.id + ":kind:extract", 100, tokens=15000)
        job = service.submit({"kind": "extract", "prompt": "bounded", "token_budget": 12000})
        result = self.wait(service, job["job_id"])
        self.assertEqual(result["state"], "failed")
        self.assertEqual(result["attempt_count"], 0)
        self.assertEqual(result["results"][0]["error"]["code"], "token_budget_exhausted")

    def make_due(self, service, r, code="timeout"):
        service.store.failure("route:" + r.id, code, 0)
        with service.store.lock:
            service.store.conn.execute("UPDATE health SET next_probe_at=? WHERE key=?", (now_ms() - 1, "route:" + r.id))
            service.store.conn.commit()

    def test_repeated_failures_demote_the_route(self):
        r = route(quality=50)
        initial = self.store.availability("route:" + r.id)["weight"]
        for _ in range(3):
            self.store.failure("route:" + r.id, "timeout", 0)
        degraded = self.store.availability("route:" + r.id)
        self.assertLess(degraded["weight"], initial)
        self.assertEqual(degraded["failure_streak"], 3)
        self.assertGreater(degraded["next_probe_at"], now_ms())

    def test_old_failures_decay_instead_of_permanent_suppression(self):
        r = route(quality=50)
        self.store.failure("route:" + r.id, "timeout", 0)
        initial = self.store.availability("route:" + r.id)["weight"]
        future = now_ms() + 4 * 1800 * 1000
        with patch("gtt_router.store.now_ms", return_value=future):
            healed = self.store.availability("route:" + r.id)["weight"]
        self.assertGreater(healed, initial)
        self.assertGreater(healed, .98)

    def test_successful_generation_probe_restores_availability_only(self):
        r = route()
        service = self.service([r], lambda *a: response("GTT_RECOVERY_OK"))
        service.store.record_feedback(r.id, "code_review", False)
        self.make_due(service, r)
        result = service.recovery.tick()
        self.assertTrue(result["success"])
        self.assertTrue(result["generation_verified"])
        self.assertEqual(service.store.availability("route:" + r.id)["weight"], 1)
        self.assertEqual(service.store.quality(r.id, "code_review")["acceptance"], 0)

    def test_failed_recovery_probe_backs_off(self):
        r = route()
        def adapter(*a):
            raise WorkerFailure("auth_failed", scope="provider", retry_after_s=0)
        service = self.service([r], adapter)
        service.recovery.settings.update({"initial_delay_s": 2, "max_backoff_s": 8, "max_probes_per_hour": 20})
        self.make_due(service, r, "auth_failed")
        first = service.recovery.tick()
        self.assertEqual(first["retry_after_s"], 2)
        self.make_due(service, r, "auth_failed")
        second = service.recovery.tick()
        self.assertEqual(second["retry_after_s"], 4)

    def test_recovery_yields_to_foreground_capacity(self):
        r = route()
        service = self.service([r], lambda *a: response("GTT_RECOVERY_OK"))
        self.make_due(service, r)
        service.total_inflight = 1
        self.assertEqual(service.recovery.tick()["reason"], "foreground_busy")
        service.total_inflight = 0

    def test_recovery_honors_hourly_request_budget(self):
        r = route()
        service = self.service([r], lambda *a: response("GTT_RECOVERY_OK"))
        service.recovery.settings["max_probes_per_hour"] = 1
        service.store.record_probe(r, True, "fixture", 100)
        self.make_due(service, r)
        self.assertEqual(service.recovery.tick()["reason"], "recovery_hourly_budget")

    def test_manual_recheck_is_not_limited_by_the_hourly_probe_budget(self):
        r = route()
        service = self.service([r], lambda *a: response("GTT_RECOVERY_OK"))
        service.recovery.settings["max_probes_per_hour"] = 1
        service.recovery.settings["max_tokens_per_hour"] = 100
        service.store.record_probe(r, True, "fixture", 100)
        self.make_due(service, r)
        # Both hourly limits are spent; an explicit check of one route
        # (the node menu / Sources page button) still runs. The background
        # tick keeps the budget (test_recovery_honors_hourly_request_budget).
        result = service.recovery.recheck(r.id, supervision_required=False)
        self.assertTrue(result["checked"], result)
        self.assertTrue(result["success"], result)

    def test_model_list_or_wrong_probe_answer_does_not_restore_route(self):
        r = route()
        service = self.service([r], lambda *a: response("available models: m"))
        self.make_due(service, r)
        result = service.recovery.tick()
        self.assertFalse(result["success"])
        self.assertLess(service.store.availability("route:" + r.id)["weight"], 1)

    def test_one_provider_auth_failure_does_not_demote_entire_software(self):
        r = route()
        service = self.service([r], lambda *a: response())
        service.record_failure(r, WorkerFailure("auth_failed", scope="provider"))
        self.assertLess(service.store.availability("provider:" + r.failure_domain)["weight"], 1)
        self.assertEqual(service.store.availability("source:" + r.source)["weight"], 1)

    def test_repeated_bridge_failures_demote_software_source(self):
        r = route()
        service = self.service([r], lambda *a: response())
        for _ in range(3):
            service.record_failure(r, WorkerFailure("adapter_error"))
        self.assertLess(service.store.availability("source:" + r.source)["weight"], .9)

    def test_cancelled_recovery_is_counted_but_not_demoted(self):
        r = route()
        entered = threading.Event()
        def adapter(route, prompt, task, ctx):
            entered.set()
            while True:
                ctx.check()
                time.sleep(.01)
        service = self.service([r], adapter)
        self.make_due(service, r)
        outcomes = []
        thread = threading.Thread(target=lambda: outcomes.append(service.recovery.tick()))
        thread.start()
        self.assertTrue(entered.wait(1))
        service.recovery.yield_to_foreground()
        thread.join(1)
        self.assertEqual(outcomes[0]["reason"], "yielded_to_foreground")
        self.assertEqual(service.store.health("route:" + r.id)["failure_streak"], 1)
        self.assertEqual(service.store.probe_budget(now_ms() - 3600000)["count"], 1)

    def test_disabled_source_capacity_is_not_bypassed_by_probe(self):
        r = route()
        service = self.service([r], lambda *a: response("GTT_RECOVERY_OK"))
        self.make_due(service, r)
        service.limits["source_parallel"] = {r.source: 0}
        self.assertEqual(service.recovery.tick()["reason"], "no_due_route")


if __name__ == "__main__":
    unittest.main()
