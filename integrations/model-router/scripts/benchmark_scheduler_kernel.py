"""Paired local benchmark, all state temporary and adapters synthetic.

The Rust column includes serialization, IPC, snapshots and response conversion.
The cached Python column distinguishes batching gains from language gains.
"""
import argparse
import json
import math
from pathlib import Path
import statistics
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests"))
from gtt_router.models import Task
from gtt_router.policy import rank_routes
from gtt_router.rust_kernel import RustKernel
from gtt_router.store import StateStore, default_health
from test_scheduler_kernel import KernelTests
from test_router import route, response


class SnapshotView:
    scheduler_kernel = None

    def __init__(self, store, kind):
        self.path = store.path
        self.values, self.feedback = store.routing_snapshot(kind)

    def health(self, key):
        return self.values.get(key) or default_health(key)

    def availability(self, key, half_life_s=1800):
        row = self.health(key)
        age = max(0, time.time() * 1000 - row["penalty_updated_at"]) / 1000
        penalty = row["penalty"] * math.exp(-math.log(2) * age / half_life_s)
        return {"weight": round(max(.25, 1 - penalty), 4)}

    def quality(self, ident, kind):
        return self.feedback.get(ident + ":" + kind, {"samples": 0, "acceptance": None})


def measure(fn, count):
    for _ in range(3):
        fn()
    samples = []
    for _ in range(count):
        start = time.perf_counter_ns()
        fn()
        samples.append((time.perf_counter_ns() - start) / 1e6)
    return {"n": count, "mean_ms": statistics.mean(samples), "p50_ms": statistics.median(samples),
        "p95_ms": sorted(samples)[math.ceil(count * .95) - 1]}


def paired(functions, count):
    # Rotate each sample's order to avoid attributing a period of host load to
    # just one implementation. Each measured call has the same public result.
    values = {name: [] for name, _ in functions}
    for _, fn in functions:
        for _ in range(3):
            fn()
    for index in range(count):
        offset = index % len(functions)
        for name, fn in functions[offset:] + functions[:offset]:
            start = time.perf_counter_ns()
            fn()
            values[name].append((time.perf_counter_ns() - start) / 1e6)
    return {name: {"n": count, "mean_ms": statistics.mean(samples), "p50_ms": statistics.median(samples),
        "p95_ms": sorted(samples)[math.ceil(count * .95) - 1]} for name, samples in values.items()}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--kernel", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--samples", type=int, default=40)
    args = parser.parse_args()
    result = {"native_models_called": 0, "routing": {}, "chain": {}}
    with tempfile.TemporaryDirectory() as directory:
        store = StateStore(Path(directory) / "state.sqlite")
        kernel = RustKernel(str(Path(args.kernel).resolve()))
        try:
            task = Task.parse({"kind": "extract", "prompt": "中文🙂 material", "max_output_tokens": 1000})
            for count in (32, 147, 512):
                routes = [route(model="fixture" + str(i)) for i in range(count)]
                def python_rank():
                    store.scheduler_kernel = None
                    return rank_routes(task, routes, {}, store)
                def cached_rank():
                    return rank_routes(task, routes, {}, SnapshotView(store, task.kind))
                def rust_rank():
                    store.scheduler_kernel = kernel
                    return rank_routes(task, routes, {}, store)
                expected = python_rank()
                assert expected == rust_rank() == cached_rank()
                result["routing"][str(count)] = paired([
                    ("python_original", python_rank), ("python_snapshot", cached_rank), ("rust_bridge", rust_rank)], args.samples)
        finally:
            kernel.close()
            store.close()
    # Real coordinator jobs + synthetic adapter + temporary SQLite for both
    # backends. This includes the existing Python persistence and admission.
    import test_scheduler_kernel
    test_scheduler_kernel.EXE = Path(args.kernel).resolve()
    for backend in ("python", "rust"):
        durations = []
        for _ in range(8):
            case = KernelTests()
            case.setUp()
            try:
                r = route()
                service = case.service(backend, [r], lambda *a: response())
                start = time.perf_counter_ns()
                ident = service.scheduling.submit(case.definition(r))["run_id"]
                run = case.wait_run(service, ident, lambda v: v["state"] == "succeeded")
                assert run["tokens_used"] == 400
                durations.append((time.perf_counter_ns() - start) / 1e6)
            finally:
                case.tearDown()
        result["chain"][backend] = {"n": len(durations), "mean_ms": statistics.mean(durations),
            "p50_ms": statistics.median(durations), "max_ms": max(durations), "nodes": 4}
    Path(args.output).write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
