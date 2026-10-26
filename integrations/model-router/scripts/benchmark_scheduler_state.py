"""Compare Python, phase-one Rust and Rust state ownership without real models."""
import argparse
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time
import uuid


def summary(values):
    return {"n": len(values), "mean_ms": statistics.mean(values), "p50_ms": statistics.median(values),
        "p95_ms": sorted(values)[math.ceil(len(values) * .95) - 1]}


class CountIO:
    def __init__(self, wrapped):
        self.wrapped, self.bytes = wrapped, 0
    def __getattr__(self, name):
        return getattr(self.wrapped, name)
    def write(self, data):
        count = self.wrapped.write(data)
        self.bytes += count or 0
        return count
    def read(self, count):
        value = self.wrapped.read(count)
        self.bytes += len(value)
        return value


def observe(kernel, action):
    if kernel is None:
        action()
        return {"calls": 0, "request_bytes": 0, "response_bytes": 0}
    original_in, original_out, original_call = kernel.process.stdin, kernel.process.stdout, kernel.call
    writer, reader = CountIO(original_in), CountIO(original_out)
    kernel.process.stdin, kernel.process.stdout = writer, reader
    # Let the reader's previously blocked read finish before counting.
    kernel.call("health", {})
    writer.bytes = reader.bytes = 0
    calls = 0
    def call(*args, **kwargs):
        nonlocal calls
        calls += 1
        return original_call(*args, **kwargs)
    kernel.call = call
    try:
        action()
        return {"calls": calls, "request_bytes": writer.bytes, "response_bytes": reader.bytes}
    finally:
        kernel.call = original_call
        kernel.process.stdin, kernel.process.stdout = original_in, original_out


def measure(action, count, kernel):
    for _ in range(3):
        action()
    values = []
    for _ in range(count):
        start = time.perf_counter_ns()
        action()
        values.append((time.perf_counter_ns() - start) / 1e6)
    return summary(values) | {"ipc": observe(kernel, action)}


def worker(args):
    sys.path.insert(0, str(Path(args.product).resolve()))
    from gtt_router.models import Route, Task, now_ms
    from gtt_router.service import RouterService, Job
    from gtt_router.policy import rank_routes
    class Catalog:
        def __init__(self, routes):
            self.routes, self.ledger, self.refreshed_at = routes, {}, now_ms()
        def refresh(self, force=False):
            self.refreshed_at = now_ms()
            return self.public()
        def public(self):
            return {"refreshed_at": self.refreshed_at, "routes": [r.public() for r in self.routes], "ledger": self.ledger, "discovery_errors": []}
    report = {"summary": "synthetic", "findings": [], "sources": [], "uncertainties": [], "complete": True}
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        routes = [Route("fixture", "p", "m" + str(i), auth_hint=True, adapter="fake", max_parallel=4,
            profile={"quality_prior": 50, "latency_prior_s": .1}) for i in range(147)]
        config = {"state_dir": str(root), "limits": {"global_parallel": 4, "combined_parallel": 6, "max_active_jobs": 4}, "recovery": {"enabled": False}}
        if args.mode != "python":
            config["scheduler_kernel"] = {"backend": "rust", "executable": str(Path(args.kernel).resolve())}
        adapter_calls = 0
        def adapter(*args):
            nonlocal adapter_calls
            adapter_calls += 1
            return {"text": json.dumps(report), "usage": {"total_tokens": 100}}
        service = RouterService(config, Catalog(routes), {"fake": adapter}, interface_paths=[root / "switch.json"])
        try:
            service.sync_codex_agents("fixture", [{"id": "fixture", "model": "gpt-6-luna", "role": "supervisor", "status": "running", "workflow_id": "default"}], lease_s=600)
            task = Task.parse({"kind": "extract", "prompt": "material🙂", "max_output_tokens": 1000, "token_budget": 5000})
            kernel = service.scheduler_kernel
            def rank():
                chosen, rejected = rank_routes(task, routes, {}, service.store)
                assert len(chosen) == 147 and not rejected
            def admit():
                job = Job(task)
                route, choice = service._reserve(job, task, set(), time.monotonic() + 20, 256)
                service._release(route, job, choice["reserved_token_estimate"])
                assert job.tokens_reserved == service.total_inflight == 0
            result = {"mode": args.mode, "native_generations": 0, "rank_147": measure(rank, args.samples, kernel),
                "admit_147": measure(admit, args.samples, kernel)}
            # Use a one-route chain so selection scale does not hide state cost.
            service.catalog.routes = [routes[0]]
            definition = {"id": "chain", "name": "chain", "token_budget": 30000, "max_parallel": 2, "luna_supervision": False,
                "nodes": [{"id": str(i), "title": str(i), "route_id": routes[0].id, "effort": "low", "node_type": "model",
                    "task": {"kind": "extract", "prompt": "fixture", "max_output_tokens": 1000, "token_budget": 5000, "max_attempts": 1}} for i in range(4)],
                "edges": [{"from": str(i), "to": str(i + 1)} for i in range(3)]}
            def chain():
                before_calls = adapter_calls
                receipt = service.scheduling.submit(definition, request_id=uuid.uuid4().hex)
                assert not receipt.get("deduplicated")
                ident = receipt["run_id"]
                deadline = time.monotonic() + 12
                while time.monotonic() < deadline:
                    run = service.scheduling.status(ident)
                    if run["state"] == "succeeded":
                        assert run["tokens_used"] == 400
                        assert adapter_calls - before_calls == 4
                        return
                    assert run["state"] not in {"failed", "interrupted"}, run["state"]
                    time.sleep(.002)
                raise AssertionError("chain deadline")
            result["chain_4"] = measure(chain, 6, kernel)
            body = {"results": [], "attempts": [], "tokens_used": 0, "history": "x" * 1048576}
            def update():
                body["tokens_used"] += 1
                service.store.save_job("history", "succeeded", task, body)
            result["state_update_1mib"] = measure(update, args.samples, kernel)
            result["synthetic_adapter_calls"] = adapter_calls
        finally:
            service.shutdown()
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--kernel", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--product", default=str(Path(__file__).resolve().parents[1]))
    parser.add_argument("--baseline-product")
    parser.add_argument("--baseline-kernel")
    parser.add_argument("--mode", choices=("python", "rust_phase1", "rust_state"))
    args = parser.parse_args()
    if args.mode:
        result = worker(args)
    else:
        result = {"native_generations": 0, "runs": []}
        choices = [("python", args.product, args.kernel)]
        if args.baseline_product and args.baseline_kernel:
            choices.append(("rust_phase1", args.baseline_product, args.baseline_kernel))
        choices.append(("rust_state", args.product, args.kernel))
        for mode, product, kernel in choices:
            output = str(Path(args.output).with_suffix("." + mode + ".json"))
            command = [sys.executable, str(Path(__file__).resolve()), "--mode", mode, "--product", product,
                "--kernel", kernel, "--samples", str(args.samples), "--output", output]
            subprocess.run(command, check=True, env=os.environ | {"PYTHONDONTWRITEBYTECODE": "1"}, timeout=180)
            result["runs"].append(json.loads(Path(output).read_text()))
    Path(args.output).write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps(result, indent=2), flush=True)


if __name__ == "__main__":
    main()
