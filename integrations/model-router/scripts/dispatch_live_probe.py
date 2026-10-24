"""Explicit, isolated native workload; never edits/restarts the installed router.

Uses an existing authorized adapter and its credentials in place. The new test
instance has its own durable state, exact route pool and disabled recovery. Only
the bounded dispatch implementation excerpt is sent to that selected route.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from gtt_router.daemon import LocalClient, load_config, secure_write
from gtt_router.processes import background_options
from gtt_router.scheduling import DEFAULT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True)
    parser.add_argument("--route-id", required=True)
    parser.add_argument("--source", required=True)
    parser.add_argument("--gtt-cli", type=Path)
    parser.add_argument("--resume-evidence", type=Path, help="Reuse completed reports; run only known failed/missing model nodes")
    parser.add_argument("--execute-native", action="store_true", required=True,
                        help="Explicitly authorize three bounded native model calls")
    args = parser.parse_args()
    if args.gtt_cli:
        args.gtt_cli = args.gtt_cli.resolve(strict=True)
    prior = json.loads(args.resume_evidence.resolve(strict=True).read_text(encoding="utf-8")) if args.resume_evidence else None
    if prior and (prior["route_id"] != args.route_id or any(n["state"] in {"unknown", "interrupted"} for n in prior["status"]["detail"]["nodes"].values())):
        raise ValueError("Resume cannot change routes or replay an unknown paid node")
    source_config = Path(args.config).resolve(strict=True)
    config = load_config(source_config)
    production_files = [source_config, Path(config["state_dir"]) / "scheduling.json"]
    before = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in production_files if p.exists()}
    out = ROOT / "diagnostics" / ("dispatch-live-" + time.strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:8])
    state = out / "router"
    state.mkdir(parents=True)
    config.pop("_config_path", None)
    config["state_dir"] = str(state)
    config["sources"] = [s for s in config["sources"] if s["id"] == args.source and s.get("enabled", True)]
    if len(config["sources"]) != 1:
        raise ValueError("Select exactly one existing enabled adapter")
    config["recovery"] = {"enabled": False}
    config["limits"] = {**config.get("limits", {}), "global_parallel": 2, "combined_parallel": 2, "max_active_jobs": 2}
    config["allow_codex_fallback"] = False
    secure_write(state / "config.json", config)
    secure_write(state / "scheduling.json", {**DEFAULT, "enabled": True, "mode": "directed",
        "pool": [{"route_id": args.route_id, "expires_at": int(time.time() * 1000) + 900000, "tags": ["explicit-live-test"]}]})
    client = LocalClient(state / "config.json", autostart=False)
    process = subprocess.Popen([sys.executable, str(ROOT / "router.py"), "daemon", "--config", str(state / "config.json")],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **background_options())
    run_id = None
    evidence = {"state_directory": str(state), "production_files_before": before, "route_id": args.route_id}
    try:
        for _ in range(60):
            try:
                evidence["health"] = client.runtime_info()
                break
            except (OSError, ValueError):
                time.sleep(.25)
        else:
            raise ValueError("Isolated daemon failed to start")
        inventory = client.call("router_inventory")
        if not any(r["route_id"] == args.route_id for r in inventory["routes"]):
            raise ValueError("Exact selected route was not discovered")
        source = (ROOT / "gtt_router/dispatch.py").read_text(encoding="utf-8")
        excerpt = source[source.index("    def submit("):source.index("    def _detail(")]
        def node(ident, prompt, kind="extract"):
            return {"id": ident, "title": ident, "route_id": args.route_id, "task": {"kind": kind, "prompt": prompt,
                "min_quality": 0, "max_output_tokens": 16384, "token_budget": 50000, "max_attempts": 1,
                "allow_fallback": False, "deadline_s": 180, "hard_deadline_s": 300, "ttft_s": 180,
                "queue_wait_s": 240, "report_max_chars": 12000}}
        definition = {"id": "dispatch-live-review", "name": "受理协议真实负载", "luna_supervision": False,
            "token_budget": 150000, "max_parallel": 2,
            "nodes": [node("identity", "只检查所给 Python 受理代码的请求身份、并发幂等与崩溃未知行为，给出代码锚点和具体结论；不要建议执行工具。"),
                      node("cancellation", "只检查所给 Python 受理代码的取消与异常边界，给出有证据的竞态或说明无法从此片段判定的部分；不要重复身份分析。"),
                      node("review", "结合两个上游报告与原代码，汇总并说明差异；只保留可由代码证明的问题。不得把未知结论当作通过。返回完整 JSON 报告。", "summarize")],
            "edges": [{"from": "identity", "to": "review"}, {"from": "cancellation", "to": "review"}]}
        if prior:
            imported = {}
            for spec in definition["nodes"]:
                report = prior.get("reports", {}).get(spec["id"])
                if report is not None:
                    if report.get("content", {}).get("complete") is not True or report.get("ok") is not True:
                        raise ValueError("Cannot reuse an incomplete report")
                    imported[spec["id"]] = {"job_id": prior["jobs"][spec["id"]]["job_id"], "report": report}
                    spec.update(node_type="input", route_id="")
            excerpt = json.dumps({"source": excerpt, "completed_upstream_evidence": imported}, ensure_ascii=False)
            evidence["imported_evidence"] = {"path": str(args.resume_evidence.resolve()), "report_nodes": list(imported)}
        expected_models = sum(n.get("node_type", "model") == "model" for n in definition["nodes"])
        request = {"kind": "workflow", "definition": definition, "input_text": excerpt,
                   "workflow_id": "isolated-dispatch-live-" + uuid.uuid4().hex[:12]}
        (out / "request.json").write_text(json.dumps(request, ensure_ascii=False), encoding="utf-8")
        plan = client.call("router_dispatch", {"action": "plan", "request": request})
        (out / "plan.json").write_text(json.dumps(plan, ensure_ascii=False, indent=2), encoding="utf-8")
        if any(not n["plan"].get("local_node") and not n["plan"].get("candidates") for n in plan["plan"]["nodes"]):
            raise ValueError("Exact workload preflight has no eligible route; no model request submitted")
        key = "live:" + uuid.uuid4().hex
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            receipts = list(pool.map(lambda _: client.call("router_dispatch", {"action": "submit", "request": request, "request_id": key}), range(4)))
        assert len({r["dispatch_id"] for r in receipts}) == 1
        receipt = receipts[0]
        run_id = receipt["run_id"]
        evidence.update(receipt=receipt, duplicate_submit_count=len(receipts), request_id=key)
        print(json.dumps({"phase": "submitted", "dispatch_id": receipt["dispatch_id"], "run_id": run_id, "output": str(out)}), flush=True)
        deadline = time.monotonic() + 480
        while time.monotonic() < deadline:
            status = client.call("router_dispatch", {"action": "status", "request_id": key})
            if status["state"] not in {"running", "queued", "admitting", "cancelling"}:
                break
            time.sleep(2)
        else:
            client.call("router_dispatch", {"action": "cancel", "request_id": key})
            raise ValueError("Isolated workload deadline exceeded; cancellation requested")
        evidence["status"] = status
        evidence["collect"] = client.call("router_dispatch", {"action": "collect", "request_id": key, "max_chars": 30000})
        reports = {}
        for node_id, node_state in status["detail"]["nodes"].items():
            if not node_state.get("job_id"):
                continue
            job = client.call("router_status", {"job_id": node_state["job_id"]})
            evidence.setdefault("jobs", {})[node_id] = job
            if node_state["state"] == "succeeded":
                page = client.call("router_dispatch", {"action": "report", "request_id": key, "node_id": node_id, "worker": 0, "max_chars": 30000})
                assert not page["has_more"]
                reports[node_id] = json.loads(page["report_json"])
        evidence["reports"] = reports
        if args.gtt_cli:
            cli = args.gtt_cli.resolve(strict=True)
            env = os.environ.copy()
            env["GTT_DATA_DIR"] = str(out)
            for action, options in [("status", ["--request-id", key]), ("collect", ["--request-id", key]), ("list", [])]:
                result = subprocess.run([str(cli), "scheduling", action, *options], env=env,
                    capture_output=True, text=True, encoding="utf-8", timeout=30, **background_options())
                if result.returncode:
                    raise ValueError("GTT CLI integration failed: " + result.stderr[:300])
                evidence.setdefault("gtt_cli", {})[action] = json.loads(result.stdout)
        assert all(job["tokens_reserved"] == 0 for job in evidence.get("jobs", {}).values())
        evidence["expected_model_calls"] = expected_models
        evidence["passed"] = status["state"] == "succeeded" and len(reports) == expected_models and all(r.get("content", {}).get("complete") is True for r in reports.values())
    finally:
        if run_id:
            try:
                client.call("router_workflow_cancel", {"run_id": run_id})
            except (OSError, ValueError):
                pass
        process.terminate()
        process.wait(timeout=10)
        evidence["production_files_unchanged"] = all(p.exists() and hashlib.sha256(p.read_bytes()).hexdigest() == before[str(p)] for p in production_files if str(p) in before)
        (out / "evidence.json").write_text(json.dumps(evidence, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"phase": "finished", "passed": evidence.get("passed", False), "production_files_unchanged": evidence["production_files_unchanged"], "evidence": str(out / "evidence.json")}), flush=True)
    if not evidence.get("passed"):
        raise ValueError("Real workload did not meet all acceptance conditions; inspect preserved evidence")


if __name__ == "__main__":
    main()
