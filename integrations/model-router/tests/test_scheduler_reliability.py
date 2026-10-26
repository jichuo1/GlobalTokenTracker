import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import unittest
from unittest.mock import patch

import test_router as fixtures
from gtt_router.adapters import ExecutionContext, ProcessChannel, classify_error, zcode
from gtt_router.native_agents import print_agent, dsh
from gtt_router.api import dispatch
from gtt_router.models import Task, WorkerFailure, now_ms
from gtt_router.policy import rank_routes
from gtt_router.processes import background_options
from gtt_router.service import routing_warnings
from gtt_router.store import StateStore

route = fixtures.route
response = fixtures.response


class SchedulerReliabilityTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service
    wait = fixtures.RouterTests.wait

    def test_auth_requires_verified_recovery_after_clock_and_penalty_decay(self):
        r = route()
        self.store.failure("provider:" + r.failure_domain, "auth_failed", 0)
        future = now_ms() + 24 * 3600000
        with patch("gtt_router.store.now_ms", return_value=future), patch("gtt_router.policy.now_ms", return_value=future):
            candidates, rejected = rank_routes(Task("extract", "small"), [r], {}, self.store)
            self.assertFalse(candidates)
            self.assertIn("recovery_verification_required", rejected[0]["reasons"])
        self.store.recovered(["provider:" + r.failure_domain])
        self.assertTrue(rank_routes(Task("extract", "small"), [r], {}, self.store)[0])

    def test_later_transient_error_does_not_clear_auth_quarantine(self):
        self.store.failure("route:test", "auth_failed", 0)
        self.store.failure("route:test", "timeout", 0)
        self.assertTrue(self.store.health("route:test")["verification_required"])
        self.store.success("route:test", 20)
        self.assertFalse(self.store.health("route:test")["verification_required"])

    def test_repeated_connection_failures_need_generation_recovery(self):
        r, other = route(), route(provider="healthy", model="other")
        key = "route:" + r.id
        for code in ["timeout", "provider_request_failed", "adapter_error"]:
            self.store.failure(key, code, 0)
        future = now_ms() + 24 * 3600000
        with patch("gtt_router.store.now_ms", return_value=future), patch("gtt_router.policy.now_ms", return_value=future):
            candidates, rejected = rank_routes(Task("extract", "small", preferred_route=r.id), [r, other], {}, self.store)
            self.assertEqual([c["route"].id for c in candidates], [other.id])
            self.assertIn("recovery_verification_required", rejected[0]["reasons"])
        self.store.recovered([key])
        self.assertFalse(self.store.health(key)["verification_required"])
        self.assertEqual(self.store.health(key)["failures"], 3)

    def test_success_breaks_failure_streak_and_source_timeouts_do_not_quarantine(self):
        r = route()
        service = self.service([r], lambda *a: response())
        for _ in range(2):
            service.record_failure(r, WorkerFailure("timeout", retry_after_s=0))
        service.store.success("route:" + r.id, 1)
        service.record_failure(r, WorkerFailure("timeout", retry_after_s=0))
        self.assertFalse(service.store.health("route:" + r.id)["verification_required"])
        self.assertFalse(service.store.health("source:" + r.source)["verification_required"])

    def test_source_contract_failure_blocks_other_routes_and_recovers(self):
        bad, peer, other = route(), route(provider="peer"), route(source="other")
        service = self.service([bad, peer, other], lambda *a: response("GTT_RECOVERY_OK"))
        service.record_failure(bad, WorkerFailure("native_configuration_error", scope="source", retry_after_s=0))
        candidates, rejected = rank_routes(Task("extract", "small"), [bad, peer, other], {}, service.store)
        self.assertEqual([c["route"].id for c in candidates], [other.id])
        self.assertEqual(len(rejected), 2)
        self.assertTrue(all("recovery_verification_required" in x["reasons"] for x in rejected))
        self.assertTrue(all(r["verification_required"] for r in service.inventory()["routes"] if r["source"] == bad.source))
        self.assertTrue(service.recovery.recheck(peer.id)["success"])
        self.assertFalse(service.store.health("source:" + peer.source)["verification_required"])

    def test_upgrade_quarantines_existing_failures_without_replay(self):
        for key, code, streak in [("route:legacy", "provider_request_failed", 7), ("source:shared", "timeout", 7), ("source:cli", "native_configuration_error", 1)]:
            self.store.failure(key, code, 0)
            self.store.conn.execute("UPDATE health SET failure_streak=?,verification_required=0,next_probe_at=0 WHERE key=?", (streak, key))
        self.store.conn.commit()
        path = self.root / "unit.sqlite"
        self.store.close()
        self.store = StateStore(path)
        self.assertTrue(self.store.health("route:legacy")["verification_required"])
        self.assertTrue(self.store.health("source:cli")["verification_required"])
        self.assertGreater(self.store.health("source:cli")["next_probe_at"], now_ms())
        self.assertFalse(self.store.health("source:shared")["verification_required"])
        self.assertEqual(self.store.conn.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)

    def test_human_native_errors_and_timeout_diagnostics_are_safe(self):
        for message, code in [("You've hit your limit · resets 8pm", "quota_exhausted"), ("Credit balance is too low", "quota_exhausted"),
                              ("Not logged in · Please run /login", "auth_failed"), ("error: unknown option '--fixture'", "native_configuration_error")]:
            self.assertEqual(classify_error(message).code, code)
        failure = WorkerFailure("timeout", details={"timeout_reason": "first_activity_timeout", "stderr": "private", "native_exit_code": 2})
        self.assertEqual(failure.public()["details"], {"timeout_reason": "first_activity_timeout", "native_exit_code": 2})

    def test_explicit_quota_and_rate_signals_outrank_native_auth_wrapper(self):
        for code in ["quota_exhausted", "insufficient_quota", "credit_balance_exhausted"]:
            self.assertEqual(classify_error({"code": "provider_not_configured", "data": {"reason": code}}).code, "quota_exhausted")
        self.assertEqual(classify_error({"code": "provider_not_configured", "statusCode": 429}).code, "rate_limited")
        self.assertEqual(classify_error({"statusCode": 401}).code, "auth_failed")
        self.assertEqual(classify_error({"data": {"providerId": "metadata-401", "code": "model_request_timeout"}}).code, "timeout")
        self.assertEqual(classify_error({"headers": {"x-note": "quota exhausted"}, "statusCode": 403}).code, "auth_failed")
        self.assertEqual(classify_error("authentication failed: 余额不足").code, "quota_exhausted")

    def test_large_input_is_rejected_by_scaled_deadline_before_generation(self):
        r = route()
        self.store.success("route:" + r.id + ":kind:code_review", 40000, input_chars=1000)
        short = Task("code_review", "x" * 1000, deadline_s=600, timeout_mode="hard")
        long = Task("code_review", "x" * 31000, deadline_s=600, timeout_mode="hard")
        self.assertTrue(rank_routes(short, [r], {}, self.store)[0])
        candidates, rejected = rank_routes(long, [r], {}, self.store)
        self.assertFalse(candidates)
        self.assertIn("unlikely_to_meet_deadline", rejected[0]["reasons"])
        self.assertTrue(rank_routes(Task("code_review", "x" * 31000, deadline_s=2000, timeout_mode="hard"), [r], {}, self.store)[0])

    def test_caller_constraints_are_preserved_and_explained(self):
        task = Task.parse({"kind": "code_review", "prompt": "x" * 9000, "allow_fallback": False, "max_attempts": 1})
        self.assertFalse(task.allow_fallback)
        self.assertEqual(set(routing_warnings(task)), {"failover_disabled_by_caller", "no_replacement_attempt_reserved", "large_excerpt_consider_split_or_research_group"})

    def test_long_report_history_does_not_block_a_bounded_short_extraction(self):
        r = route()
        key = "route:" + r.id + ":kind:extract"
        self.store.success(key, 260000, tokens=65000, input_chars=600, measured_tokens=65000)
        short = Task.parse({"kind": "extract", "prompt": "Extract two records", "context": "x" * 9000,
            "max_output_tokens": 8192, "hard_deadline_s": 1200})
        candidates, rejected = rank_routes(short, [r], {}, self.store)
        self.assertTrue(candidates)
        self.assertFalse(rejected)
        self.assertLess(candidates[0]["eta_s"], 1200)
        self.assertEqual(candidates[0]["reasons"]["latency_scale"], .25)
        large = Task.parse({"kind": "extract", "prompt": "x" * 9000, "max_output_tokens": 100000, "hard_deadline_s": 1200})
        self.assertFalse(rank_routes(large, [r], {}, self.store)[0])
        self.store.success("route:" + r.id + ":kind:summarize", 260000, tokens=65000, input_chars=600)
        unmeasured = Task("summarize", "x" * 9000, max_output_tokens=8192, hard_deadline_s=1200)
        self.assertFalse(rank_routes(unmeasured, [r], {}, self.store)[0])

    def test_default_retry_room_preserves_explicit_budget(self):
        normal = Task.parse({"kind": "code_review", "prompt": "small"})
        single = Task.parse({"kind": "code_review", "prompt": "small", "max_attempts": 1})
        self.assertGreater(normal.token_budget, single.token_budget)
        self.assertEqual(Task.parse({"kind": "code_review", "prompt": "small", "token_budget": 3000}).token_budget, 3000)

    def test_manual_recheck_clears_quarantine_without_resetting_lifetime_counts(self):
        r = route()
        service = self.service([r], lambda *a: response("GTT_RECOVERY_OK"))
        service.store.failure("route:" + r.id, "auth_failed", 900)
        result = dispatch(service, "router_recheck", {"route_id": r.id})
        self.assertTrue(result["success"])
        health = service.store.health("route:" + r.id)
        self.assertFalse(health["verification_required"])
        self.assertEqual(health["failures"], 1)

    def test_manual_recheck_respects_busy_disabled_but_not_automatic_probe_budget(self):
        r = route()
        service = self.service([r], lambda *a: self.fail("must not generate"))
        service.total_inflight = 1
        self.assertEqual(service.recovery.recheck(r.id)["reason"], "foreground_busy")
        service.total_inflight = 0
        r.enabled = False
        self.assertEqual(service.recovery.recheck(r.id)["reason"], "no_due_route")
        r.enabled = True
        service.recovery.settings["max_probes_per_hour"] = 0
        service.adapters["fake"] = lambda *a: response("GTT_RECOVERY_OK")
        self.assertTrue(service.recovery.recheck(r.id)["success"])
        self.assertEqual(service.store.probe_budget(0)["count"], 1)

    def test_timeout_keeps_time_and_budget_for_a_qualified_replacement(self):
        bad, good = route(model="bad", quality=90), route(provider="other", model="good", latency=.1)
        calls = []
        def adapter(r, prompt, task, context):
            calls.append(r.model)
            if r.model == "bad":
                while True:
                    context.check()
                    time.sleep(.01)
            return response()
        service = self.service([bad, good], adapter)
        job = service.submit({"kind": "extract", "prompt": "small", "deadline_s": 5, "max_attempts": 2})
        result = self.wait(service, job["job_id"], timeout=7)
        self.assertEqual(result["state"], "succeeded")
        self.assertEqual(calls, ["bad", "good"])


class ProcessOwnershipTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown

    def channel(self, mode, *args, env=None):
        fixture = Path(__file__).with_name("inherited_pipe_fixture.py")
        return ProcessChannel([sys.executable, str(fixture), mode, *args],
                              ExecutionContext(threading.Event(), time.monotonic() + 10, self.root), env)

    def test_nonreading_stdin_respects_deadline_and_cleanup(self):
        channel = self.channel("wait")
        child = channel.read()["child_pid"]
        channel.context.deadline = time.monotonic() + .15
        started = time.monotonic()
        try:
            with self.assertRaises(WorkerFailure) as failed:
                channel.send({"text": "x" * 200000})
            self.assertEqual(failed.exception.code, "timeout")
            self.assertLess(time.monotonic() - started, 1)
        finally:
            channel.close()
        self.assertFalse(channel.writer.is_alive())
        self.assertFalse(self.running(child))
        self.assertLess(time.monotonic() - started, 4)

    def test_native_adapters_cannot_bypass_input_deadline_during_cleanup(self):
        fixture = Path(__file__).with_name("inherited_pipe_fixture.py")
        for adapter in [zcode, print_agent, dsh]:
            with self.subTest(adapter=adapter.__name__):
                r = route()
                r.adapter = "claude_cli" if adapter == print_agent else "fake"
                r.runtime = {"command": [sys.executable, str(fixture), "wait"],
                             "builtin_config": "synthetic-unused", "provider_config": "synthetic-unused",
                             "profile_entries": [{"id": key, "name": "@deepseek-ai/fixture"} for key in ["agent-loop", "agent-default-model", "tools"]]}
                context = ExecutionContext(threading.Event(), time.monotonic() + .3, self.root / adapter.__name__)
                started = time.monotonic()
                with self.assertRaises(WorkerFailure) as failed:
                    adapter(r, "x" * 200000, Task("extract", "fixture"), context)
                self.assertEqual(failed.exception.code, "timeout")
                self.assertLess(time.monotonic() - started, 4)

    @staticmethod
    def running(pid):
        if os.name == "nt":
            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            kernel.OpenProcess.restype = wintypes.HANDLE
            kernel.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
            kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            handle = kernel.OpenProcess(0x100000, False, pid)
            if not handle:
                return False
            try:
                return kernel.WaitForSingleObject(handle, 0) == 0x102
            finally:
                kernel.CloseHandle(handle)
        try:
            os.kill(pid, 0)
            return True
        except ProcessLookupError:
            return False

    def test_exited_parent_cannot_leave_pipe_owning_descendant(self):
        channel = self.channel("orphan")
        try:
            child = channel.read()["child_pid"]
            started = time.monotonic()
            channel.close()
            self.assertLess(time.monotonic() - started, 3)
            self.assertFalse(self.running(child))
        finally:
            if channel.process.poll() is None:
                channel.close()

    def test_cancel_cleanup_preserves_an_unrelated_process(self):
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], **background_options())
        channel = self.channel("wait")
        try:
            child = channel.read()["child_pid"]
            channel.close()
            self.assertFalse(self.running(child))
            self.assertIsNone(unrelated.poll())
        finally:
            unrelated.terminate()
            unrelated.wait(timeout=5)
            if channel.process.poll() is None:
                channel.close()

    def test_large_unicode_input_arguments_environment_and_hidden_console(self):
        env = os.environ.copy()
        env["GTT_FIXTURE_VALUE"] = "kept"
        channel = self.channel("echo", "参数 with spaces", env=env)
        try:
            channel.send({"text": "中文" * 50000})
            result = channel.read()
            self.assertEqual(result, {"chars": 100000, "argument": "参数 with spaces", "env": "kept", "console": 0})
        finally:
            channel.close()
