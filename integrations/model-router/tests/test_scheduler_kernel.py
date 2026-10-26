"""Differential and real-file integration tests; no native models or live state."""
import copy
import json
import os
from pathlib import Path
import random
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_router as fixtures
from gtt_router.models import Task, WorkerFailure
from gtt_router.policy import rank_routes
from gtt_router.rust_kernel import RustKernel
from gtt_router.service import Job, RouterService
from gtt_router.store import StateStore
from gtt_router.daemon import LocalClient
from gtt_router.processes import background_options

ROOT = Path(__file__).resolve().parents[3]
EXE = Path(os.environ.get("GTT_TEST_KERNEL", ROOT / "target/release" / ("gtt-scheduler-kernel.exe" if os.name == "nt" else "gtt-scheduler-kernel")))


class KernelTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not EXE.is_file():
            raise unittest.SkipTest("build gtt-scheduler-kernel and set GTT_TEST_KERNEL")
        cls.kernel = RustKernel(str(EXE.resolve()))

    @classmethod
    def tearDownClass(cls):
        cls.kernel.close()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.store = StateStore(self.root / "state.sqlite")
        self.services = []

    def tearDown(self):
        for service in self.services:
            service.shutdown()
        self.store.close()
        self.temp.cleanup()

    def service(self, backend, routes, adapter):
        state = self.root / str(len(self.services))
        config = {"state_dir": str(state), "limits": {"global_parallel": 4, "combined_parallel": 6,
            "max_active_jobs": 4, "external_queue_grace_s": 0}, "allow_codex_fallback": False}
        config["scheduler_kernel"] = {"backend": "python"}
        if backend == "rust":
            config["scheduler_kernel"] = {"backend": "rust", "executable": str(EXE.resolve())}
        service = RouterService(config, fixtures.FakeCatalog(routes), {"fake": adapter},
            interface_paths=[state / "external-interface.json"])
        service.sync_codex_agents("test", [{"id": "fixture", "model": "gpt-6-luna", "status": "running",
            "role": "supervisor", "workflow_id": "default"}], lease_s=600)
        self.services.append(service)
        return service

    def test_randomized_route_decisions_match_python_with_frozen_time(self):
        rng = random.Random(8675309)
        now = 1_800_000_000_000
        clocks = [patch("gtt_router." + module + ".now_ms", return_value=now)
            for module in ("policy", "pricing", "model_updates", "store", "bridge", "rust_kernel", "scheduling")]
        for clock in clocks:
            clock.start()
            self.addCleanup(clock.stop)
        for case in range(160):
            routes = []
            for i in range(8):
                r = fixtures.route(source="s" + str(i % 3), provider="p" + str(i % 2), model="m" + str(i),
                    quality=rng.randrange(15, 90), latency=rng.choice([1, 10, 60, 800]))
                r.context_window = rng.choice([None, 3000, 64000])
                r.output_limit = rng.choice([None, 1000, 8192, 32768])
                r.auth_hint = rng.random() > .1
                r.capabilities = rng.choice([["text"], ["text", "web"]])
                r.profile.update(captured_at=now - rng.choice([0, 40 * 86400000]), intelligence=rng.randrange(25, 90),
                    terminal_bench=rng.randrange(20, 80), allowed_kinds=rng.choice([None, ["extract"], ["code_review", "research"]]))
                r.pricing = {"input": rng.choice([0, 1, 20]), "output": rng.choice([0, 2, 40]),
                    "currency": "USD", "unit": "per_million_tokens", "captured_at": now, "source": "fixture"}
                r.catalog_metadata = {"latest_in_family": True, "distinct_versions": 2, "version": [2, 1],
                    "activity_observability": rng.choice(["final_only", "stream"])}
                if case % 3 == 0:
                    self.store.success("route:" + r.id, rng.randrange(20, 20000))
                    self.store.success("route:" + r.id + ":kind:extract", 1000, 2000, input_chars=3000, measured_tokens=2000)
                if rng.random() < .15:
                    self.store.failure("route:" + r.id, rng.choice(["timeout", "auth_failed"]), 10)
                for vote in range(rng.randrange(5)):
                    self.store.record_feedback(r.id, "extract", vote % 2 == 0)
                routes.append(r)
            task = Task.parse({"kind": rng.choice(["extract", "research", "code_review"]),
                "prompt": "中文🙂" * rng.randrange(1, 100), "max_output_tokens": rng.choice([1000, 8192, 32768]),
                "output_mode": rng.choice(["fixed", "native", "adaptive"]), "needs_web": rng.choice([True, False]),
                "routing_strategy": rng.choice(["auto", "speed", "quality", "reliability", "token_efficiency"]),
                "allowed_routes": rng.choice([None, [], [r.id for r in routes[::2]]])})
            options = {"inflight": {"s0": 2}, "diversity": {routes[0].failure_domain: 2},
                "preferences": {"preferred_sources": ["s1"], "concurrency_bias": .6, "weight": 20}}
            self.store.scheduler_kernel = None
            expected = rank_routes(task, routes, {}, self.store, **options)
            self.store.scheduler_kernel = self.kernel
            actual = rank_routes(task, routes, {}, self.store, **options)
            self.assertEqual(expected, actual, "route differential case " + str(case))

    def test_reservation_decisions_match_python_budget_capacity_and_history(self):
        r = fixtures.route()
        py = self.service("python", [r], lambda *args: self.fail("not executed"))
        rs = self.service("rust", [r], lambda *args: self.fail("not executed"))
        for case in range(40):
            results = []
            for service in (py, rs):
                task = Task.parse({"kind": "extract", "prompt": "material🙂", "max_output_tokens": 1000,
                    "token_budget": 5000, "queue_wait_s": 0, "max_attempts": 3})
                task._queue_grace_s = 0
                job = Job(task)
                job.tokens_used = [0, 1000, 4500, 0][case % 4]
                job.tokens_reserved = [0, 3500, 0, 0][case % 4]
                job.attempt_count = case % 5
                service.total_inflight = 4 if case % 7 == 0 else 0
                service.inflight = {r.source: 1} if case % 6 == 0 else {}
                service.store.success("route:" + r.id + ":kind:extract", 100, tokens=1200 + case * 30)
                try:
                    route, decision = service._reserve(job, task, set(), time.monotonic() + 20, 600)
                    results.append((route.id, decision["reserved_token_estimate"], job.tokens_reserved, job.attempt_count,
                        service.total_inflight, service.inflight.copy()))
                    service._release(route, job, decision["reserved_token_estimate"])
                except WorkerFailure as error:
                    results.append(error.code)
            self.assertEqual(*results, "reservation case " + str(case))

    def test_zero_measured_character_history_keeps_python_routing_semantics(self):
        r = fixtures.route()
        self.store.success("route:" + r.id + ":kind:extract", 1000, tokens=20000, input_chars=0, measured_tokens=20000)
        # Zero is a real observed size, while the routing denominator uses the
        # historical fallback. Both meanings must survive the language boundary.
        task = Task.parse({"kind": "extract", "prompt": "x" * 12000, "max_output_tokens": 1000,
            "routing_strategy": "token_efficiency"})
        self.store.scheduler_kernel = None
        expected = rank_routes(task, [r], {}, self.store)
        self.store.scheduler_kernel = self.kernel
        actual = rank_routes(task, [r], {}, self.store)
        self.assertEqual(expected, actual)

    @staticmethod
    def definition(r, controlled=False):
        def node(ident, kind):
            return {"id": ident, "title": ident, "node_type": kind, "effort": "low" if kind in {"model", "operator", "reviewer"} else "",
                "route_id": r.id if kind in {"model", "operator", "reviewer"} else "", "task": {"kind": "extract", "prompt": ident,
                    "max_output_tokens": 1000, "token_budget": 5000, "max_attempts": 1}}
        if controlled:
            nodes = [node("mother", "codex"), node("work", "operator"), node("review", "reviewer")]
            edges = [{"from": "mother", "to": "work"}, {"from": "work", "to": "review", "kind": "report"},
                {"from": "review", "to": "mother", "kind": "report"}]
        else:
            nodes = [node(str(i), "model") for i in range(4)]
            edges = [{"from": str(i), "to": str(i + 1)} for i in range(3)]
        return {"id": "test", "name": "test", "nodes": nodes, "edges": edges, "token_budget": 30000,
            "max_parallel": 2, "luna_supervision": False}

    def wait_run(self, service, ident, predicate):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            run = service.scheduling.status(ident)
            if predicate(run):
                return run
            if run["state"] == "interrupted":
                self.fail(str(run))
            time.sleep(.005)
        self.fail("workflow timed out: " + str(run))

    def test_chain_runs_through_python_adapters_with_event_driven_rust(self):
        outcomes = []
        for backend in ("python", "rust"):
            r = fixtures.route()
            service = self.service(backend, [r], lambda *args: fixtures.response())
            run = service.scheduling.submit(self.definition(r))["run_id"]
            result = self.wait_run(service, run, lambda value: value["state"] == "succeeded")
            outcomes.append((result["state"], result["tokens_used"], [v["state"] for v in result["nodes"].values()]))
            self.assertEqual(service.total_inflight, 0)
        self.assertEqual(*outcomes)

    def test_operator_real_unicode_file_review_and_codex_acceptance(self):
        for backend in ("python", "rust"):
            workspace = self.root / ("files-" + backend)
            workspace.mkdir()
            path = workspace / "中文.txt"
            path.write_bytes("第一行🙂\r\n第二行\r\n".encode())
            def adapter(route, prompt, task, context):
                payload = json.loads(task.context)
                report = fixtures.content()
                if payload["assignment"]:
                    snapshot = payload["assignment"]["files"][0]
                    report["operations"] = [{"id": "replace", "op": "replace_text", "path": "中文.txt",
                        "base_sha256": snapshot["sha256"], "old_text": "第二行", "new_text": "已修改"}]
                else:
                    after = payload["upstream"][0]["after_files"][0]
                    report["review"] = {"accepted": "已修改" in after["content"], "issues": []}
                return {"text": json.dumps(report), "usage": {"total_tokens": 100}}
            r = fixtures.route()
            service = self.service(backend, [r], adapter)
            run_id = service.scheduling.submit(self.definition(r, True), input_text="中文🙂材料",
                execution={"root": str(workspace), "writable_paths": ["中文.txt"]})["run_id"]
            service.scheduling.operators.coordinate(run_id, 0, {"type": "assign", "assignments": [{"node_id": "work",
                "instruction": "modify second line", "paths": ["中文.txt"], "file_ranges": {"中文.txt": [2, 2]},
                "input_range": [0, 3]}]}, "plan")
            result = self.wait_run(service, run_id, lambda r: r.get("execution_complete", False))
            self.assertEqual(path.read_bytes(), "第一行🙂\r\n已修改\r\n".encode())
            self.assertEqual(result["state"], "awaiting_codex")
            accept = next(p for p in result["pending_actions"] if p["type"] == "accept")
            accepted = service.scheduling.operators.coordinate(run_id, 1,
                {"type": "accept", "evidence_digest": accept["evidence_digest"]}, "accept")
            self.assertEqual(accepted["state"], "succeeded")
            self.assertEqual(accepted["tokens_used"], 200)
            events = service.scheduling.operators.events(run_id, limit=64)["events"]
            self.assertTrue(any(e["target_node"] == "mother" and e["kind"] == "review" for e in events))

    def test_kernel_failure_never_calls_adapter_or_restarts_worker(self):
        r = fixtures.route()
        service = self.service("rust", [r], lambda *args: self.fail("must fail closed"))
        child = service.scheduler_kernel.process
        child.kill()
        child.wait()
        # Rust now owns persistence: an unavailable store cannot acknowledge
        # admission. Inspect its durable rows through an independent reader.
        with self.assertRaisesRegex(WorkerFailure, "scheduler_kernel_unavailable"):
            service.submit({"kind": "extract", "prompt": "bounded"})
        import sqlite3
        with sqlite3.connect(f"file:{service.store.path.as_posix()}?mode=ro", uri=True) as reader:
            self.assertEqual(reader.execute("SELECT COUNT(*) FROM jobs").fetchone()[0], 0)
        reader.close()
        self.assertEqual(child.pid, service.scheduler_kernel.process.pid)

    def test_concurrent_protocol_calls_keep_responses_attributed(self):
        errors = []
        def run():
            try:
                for _ in range(30):
                    self.assertEqual(self.kernel.call("health", {})["protocol"], 1)
            except Exception as error:
                errors.append(error)
        threads = [threading.Thread(target=run) for _ in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        self.assertEqual(errors, [])

    def test_unknown_job_and_report_edge_semantics(self):
        self.assertEqual(self.kernel.job_state({"state": "failed", "results": [], "attempts": [{"unknown_paid_outcome": True}]})["state"], "unknown")
        r = fixtures.route()
        d = self.definition(r, True)
        run = {"definition": d, "mode": "operator_v1", "decisions": [1],
            "nodes": {"mother": {"state": "succeeded"}, "work": {"state": "pending"}, "review": {"state": "pending"}}}
        d["edges"].append({"from": "work", "to": "mother", "kind": "report"})
        rows = {n["node_id"]: n for n in self.kernel.workflow(run)["nodes"]}
        self.assertEqual(rows["work"]["readiness"], "ready")
        self.assertEqual(rows["review"]["readiness"], "wait")

    def test_isolated_daemon_advertises_selected_backend_without_generation(self):
        state = self.root / "isolated-daemon"
        config = {"schema_version": 2, "state_dir": str(state), "ledger_path": str(state / "missing.sqlite"),
            "benchmark_file": str(state / "missing-benchmarks.json"), "sources": [], "recovery": {"enabled": False},
            "scheduler_kernel": {"backend": "rust", "executable": str(EXE.resolve())}}
        path = self.root / "daemon-config.json"
        path.write_text(json.dumps(config), encoding="utf-8")
        product = Path(__file__).resolve().parents[1]
        child = subprocess.Popen([sys.executable, str(product / "router.py"), "daemon", "--config", str(path)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            cwd=product, **background_options())
        try:
            client = LocalClient(path, autostart=False)
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                try:
                    health = client.runtime_info()
                    break
                except (OSError, ValueError, RuntimeError):
                    self.assertIsNone(child.poll(), "isolated daemon exited")
                    time.sleep(.03)
            else:
                self.fail("isolated daemon did not start")
            self.assertEqual(health["scheduler_backend"], "rust")
            self.assertEqual(health["scheduler_kernel_protocol"], 1)
            self.assertEqual(health["runtime_contract_version"], 2)
            self.assertEqual(health["state_owner"], "rust")
            self.assertEqual(client.call("router_dispatch", {"action": "list"})["dispatches"], [])
        finally:
            child.terminate()
            child.wait(timeout=10)


if __name__ == "__main__":
    unittest.main()
