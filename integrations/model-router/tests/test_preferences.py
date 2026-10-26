import sqlite3
import threading
import time
import unittest
from unittest.mock import patch

import test_router as fixtures
from gtt_router.api import dispatch
from gtt_router.group_contract import GroupRequest
from gtt_router.models import Task, now_ms
from gtt_router.policy import rank_routes
from gtt_router.preferences import parallel_advice
from gtt_router.store import StateStore


class PreferenceTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown
    service = fixtures.RouterTests.service
    wait = fixtures.RouterTests.wait

    def test_exact_preferences_are_scoped_and_cannot_bypass_hard_gates(self):
        strong = fixtures.route(source="other", model="strong", quality=80)
        preferred = fixtures.route(source="zcode", model="chosen", quality=41)
        service = self.service([strong, preferred], lambda *args: fixtures.response())
        scope = dispatch(service, "router_preferences", {"action": "set", "preferences": {"preferred_models": ["chosen"], "weight": 100}})["scope_id"]
        task = {"kind": "logic_check", "prompt": "bounded", "preference_scope": scope}
        plan = service.plan(task)
        self.assertEqual(plan["candidates"][0]["route"]["model"], "chosen")
        self.assertEqual(service.plan({"kind": "logic_check", "prompt": "bounded"})["candidates"][0]["route"]["model"], "strong")
        self.assertEqual(service.plan({**task, "min_quality": 70})["candidates"][0]["route"]["model"], "strong")
        preferred.enabled = False
        self.assertEqual(service.plan(task)["candidates"][0]["route"]["model"], "strong")
        preferred.enabled = True
        service.store.failure("provider:" + preferred.failure_domain, "auth_failed", 0)
        self.assertEqual(service.plan(task)["candidates"][0]["route"]["model"], "strong")

    def test_model_preference_matches_multiple_sources_and_only_exact_names(self):
        routes = [fixtures.route(source=s, model=m) for s, m in [("a", "chosen"), ("b", "chosen"), ("c", "chosen-other")]]
        ranked, _ = rank_routes(Task("extract", "bounded"), routes, {}, self.store, preferences={"preferred_models": ["chosen"]})
        matches = {c["route"].source: c["reasons"]["task_preference_boost"] for c in ranked}
        self.assertEqual(matches, {"a": 40, "b": 40, "c": 0})

    def test_merge_expiry_clear_and_persistence(self):
        scope = self.store.set_preferences({"preferred_sources": ["zcode"], "weight": 60}, ttl_s=60)
        changed = self.store.set_preferences({"concurrency_bias": -.5}, scope["scope_id"])
        self.assertEqual(changed["preferences"]["preferred_sources"], ["zcode"])
        self.assertEqual(changed["expires_at"], scope["expires_at"])
        self.assertEqual(changed["preferences"]["weight"], 60)
        self.store.close()
        self.store = StateStore(self.root / "unit.sqlite")
        self.assertEqual(self.store.preference_scope(scope["scope_id"]), changed)
        with patch("gtt_router.store.now_ms", return_value=scope["expires_at"]):
            self.assertIsNone(self.store.preference_scope(scope["scope_id"]))
            with self.assertRaises(ValueError):
                self.store.set_preferences({}, scope["scope_id"])
        self.assertTrue(self.store.clear_preferences(scope["scope_id"])["cleared"])
        self.assertFalse(self.store.clear_preferences(scope["scope_id"])["cleared"])

    def test_invalid_inputs_and_no_implicit_global_scope(self):
        for value in [{"weight": True}, {"concurrency_bias": float("nan")}, {"max_parallel": 2.5}, {"preferred_sources": "zcode"}, {"global": True}]:
            with self.assertRaises(ValueError):
                self.store.set_preferences(value)
        a, b = self.store.set_preferences({}), self.store.set_preferences({})
        self.assertNotEqual(a["scope_id"], b["scope_id"])
        self.assertEqual(parallel_advice({"concurrency_bias": -1}, 4)["effective_max_parallel"], 1)
        self.assertEqual(parallel_advice({"concurrency_bias": 1, "max_parallel": 32}, 4)["effective_max_parallel"], 4)

    def test_dynamic_cap_waits_without_cancellation_or_luna_bypass(self):
        r = fixtures.route()
        r.max_parallel = 4
        running, peak = 0, 0
        lock = threading.Lock()
        entered = threading.Event()
        release = threading.Event()
        def adapter(*args):
            nonlocal running, peak
            with lock:
                running += 1
                peak = max(peak, running)
                if running == 2:
                    entered.set()
            release.wait(3)
            with lock:
                running -= 1
            return fixtures.response()
        service = self.service([r], adapter)
        scope = service.preferences("set", preferences={"max_parallel": 2})["scope_id"]
        def submit(key):
            return service.submit({"kind": "extract", "prompt": "bounded", "preference_scope": scope, "work_key": key})["job_id"]
        jobs = [submit("first"), submit("second")]
        self.assertTrue(entered.wait(2))
        service.preferences("set", scope, {"max_parallel": 1})
        jobs.append(submit("third"))
        time.sleep(.15)
        self.assertEqual(service.status(jobs[-1])["state"], "queued")
        release.set()
        for job in jobs:
            self.assertEqual(self.wait(service, job)["state"], "succeeded")
        self.assertEqual(peak, 2)
        self.assertEqual(service.inflight["scope:" + scope], 0)

    def test_saturated_scope_does_not_block_other_scope_or_priority(self):
        service = self.service([fixtures.route()], lambda *args: fixtures.response())
        scope = service.preferences("set", preferences={"max_parallel": 1})["scope_id"]
        service.sync_codex_agents("test", [{"id": "actual-test-fixture", "status": "running", "preference_scope": scope}])
        blocked = service.submit({"kind": "extract", "prompt": "bounded", "preference_scope": scope, "priority": 100})["job_id"]
        other = service.submit({"kind": "extract", "prompt": "bounded", "priority": 1})["job_id"]
        self.assertEqual(self.wait(service, other)["state"], "succeeded")
        self.assertEqual(service.status(blocked)["state"], "queued")
        service.sync_codex_agents("test", [])
        self.assertEqual(self.wait(service, blocked)["state"], "succeeded")

    def test_group_roles_inherit_scope_without_changing_seats_or_budgets(self):
        service = self.service([fixtures.route(quality=80)], lambda *args: fixtures.response())
        scope = service.preferences("set", preferences={"concurrency_bias": -1})["scope_id"]
        req = GroupRequest.parse({"topic": "bounded", "needs_web": False, "preference_scope": scope})
        plan = service.groups.plan({"topic": "bounded", "needs_web": False, "preference_scope": scope})
        self.assertEqual(plan["member_count"], 3)
        self.assertEqual(plan["group_budget"], 50000000)
        for role, expected in [("leader", "quality"), ("collector", "speed"), ("analyst", "quality"), ("checker", "reliability")]:
            task = service.groups._task(req, kind="extract", prompt="bounded", role=role)
            self.assertEqual(task.preference_scope, scope)
            self.assertEqual(task.routing_strategy, expected)

    def test_batched_scope_jobs_cannot_hoard_coordinators_before_first_reservation(self):
        service = self.service([fixtures.route()], lambda *args: fixtures.response())
        scope = service.preferences("set", preferences={"max_parallel": 1})["scope_id"]
        entered, release = threading.Event(), threading.Event()
        original = service._coordinate
        def delayed(job):
            if job.task.preference_scope == scope:
                entered.set()
                release.wait(3)
            original(job)
        service._coordinate = delayed
        jobs = [service.submit({"kind": "extract", "prompt": "bounded", "preference_scope": scope})["job_id"] for _ in range(3)]
        self.assertTrue(entered.wait(1))
        other = service.submit({"kind": "extract", "prompt": "other round"})["job_id"]
        try:
            self.assertEqual(self.wait(service, other, timeout=1)["state"], "succeeded")
            self.assertEqual(service.scope_coordinators.get(scope), 1)
        finally:
            release.set()
        for job in jobs:
            self.assertEqual(self.wait(service, job)["state"], "succeeded")

    def test_cleared_scope_is_visible_and_reverts_to_neutral(self):
        service = self.service([fixtures.route()], lambda *args: fixtures.response())
        scope = service.preferences("set", preferences={"max_parallel": 1})["scope_id"]
        service.preferences("clear", scope)
        plan = service.plan({"kind": "extract", "prompt": "bounded", "preference_scope": scope})
        self.assertIn("preference_scope_missing_or_expired", plan["preferences"]["warnings"])
        self.assertEqual(plan["preferences"]["effective_max_parallel"], 2)


class StrategyTests(unittest.TestCase):
    setUp = fixtures.RouterTests.setUp
    tearDown = fixtures.RouterTests.tearDown

    def test_strategy_changes_ranking_after_identical_hard_gates(self):
        smart, fast = fixtures.route(model="smart", quality=90, latency=120), fixtures.route(provider="other", model="fast", quality=41, latency=1)
        self.assertEqual(rank_routes(Task("extract", "x", routing_strategy="quality"), [smart, fast], {}, self.store)[0][0]["route"].model, "smart")
        self.assertEqual(rank_routes(Task("extract", "x", routing_strategy="speed"), [smart, fast], {}, self.store)[0][0]["route"].model, "fast")
        for value in ["invalid", [], None]:
            with self.assertRaises(ValueError):
                Task.parse({"kind": "extract", "prompt": "x", "routing_strategy": value})

    def test_estimates_do_not_count_as_measured_token_cost(self):
        r = fixtures.route()
        key = "route:" + r.id + ":kind:extract"
        for _ in range(4):
            self.store.success(key, 100, tokens=1, input_chars=1000)
        task = Task("extract", "x", routing_strategy="token_efficiency")
        self.assertEqual(rank_routes(task, [r], {}, self.store)[0][0]["reasons"]["token_cost_source"], "unknown")
        for _ in range(3):
            self.store.success(key, 100, tokens=9000, input_chars=1000, measured_tokens=9000)
        ranked = rank_routes(task, [r], {}, self.store)[0][0]
        self.assertEqual(ranked["reasons"]["token_cost_source"], "reported_same_kind_history")
        self.assertGreater(ranked["reasons"]["token_efficiency_penalty"], 4)

    def test_legacy_auth_migration_and_recovered_unseen_keys(self):
        path = self.root / "legacy.sqlite"
        conn = sqlite3.connect(path)
        conn.execute("CREATE TABLE health(key TEXT PRIMARY KEY,failures INTEGER NOT NULL DEFAULT 0,successes INTEGER NOT NULL DEFAULT 0,latency_ms REAL,cooldown_until INTEGER NOT NULL DEFAULT 0,last_error TEXT)")
        conn.execute("INSERT INTO health(key,last_error) VALUES('provider:legacy','auth_failed')")
        conn.commit()
        conn.close()
        store = StateStore(path)
        self.assertTrue(store.health("provider:legacy")["verification_required"])
        store.recovered(["route:new"])
        self.assertGreater(store.health("route:new")["last_verified_at"], 0)
        store.close()


if __name__ == "__main__":
    unittest.main()
