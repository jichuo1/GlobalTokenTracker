"""Actual native A/B load: serial receipt waiting versus overlapping controller work.

Both arms use the same service, model, fixtures, limits and checks. A deliberately
small first response limit tests known-truncation recovery, not model quality at
its native maximum. No unknown request is retried and no partial edit executes.
"""
import argparse
import copy
import hashlib
import json
from pathlib import Path
import random
import statistics
import sys
import tempfile
import time

PRODUCT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PRODUCT))
import benchmark_workflow_efficiency as gold
from gtt_router.daemon import LocalClient
from gtt_router.models import Task, Route
from gtt_router.slicing import output_budget

OUT = PRODUCT.parents[1] / "target/diagnostics/controller-overlap-20261025/paired-load-0.7.4"
WORKFLOW = "controller-pair-20261025"
ROUTE = "39aa71a79b77acde8cc0"
LEDGER_FILES = 1


def write(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


def fixtures():
    good = PRODUCT.parents[1] / "target/diagnostics/output-limits-live/efficiency/parallel/solo/usage_ledger.py"
    ledger = good.read_text(encoding="utf-8")
    assert ledger.count('return sum(parts)') == 1 and ledger.count('revision > selected[ident]["revision"]') == 1
    ledger = ledger.replace('return sum(parts)', 'return sum(parts) + raw.get("reasoningTokens", 0)')
    ledger = ledger.replace('revision > selected[ident]["revision"]', 'revision >= selected[ident]["revision"]')
    result = {"deadline_window.py": gold.FILES["small"]["deadline_window.py"]}
    for index in range(1, LEDGER_FILES + 1):
        name = "usage_ledger.py" if LEDGER_FILES == 1 else f"usage_ledger_{index}.py"
        result[name] = ledger if LEDGER_FILES == 1 else ledger.replace("Implement bounded accounting;", f"Implement bounded accounting for provider {index};")
    return result


def independent_checks():
    rng = random.Random(20261025)
    task = Task.parse({"kind": "extract", "prompt": "bounded", "token_budget": 22000, "output_mode": "adaptive"})
    before = task.public()
    checked = 0
    for mode in ("adaptive", "native"):
        task.output_mode = mode
        for _ in range(128):
            size, remaining = rng.randrange(100, 14000), rng.randrange(1000, 22001)
            route = Route("fixture", "bounded", "capacity", output_limit=rng.choice([None, 8192, 128000]),
                          context_window=rng.choice([None, 8192, 32000]))
            decision = output_budget(task, route, size, remaining)
            effective = decision["effective_output_tokens"]
            assert effective <= max(0, remaining - (size + 1) // 2)
            if route.output_limit:
                assert effective <= route.output_limit
            if route.context_window:
                assert effective <= route.context_window - (size + 1) // 2
            checked += 1
    task.output_mode = before["output_mode"]
    assert task.public() == before
    return {"checks": checked, "passed": True, "purpose": "independent output-budget and context-boundary verification"}


def acceptance(folder):
    if not (folder / "usage_ledger.py").exists():
        checks, executed = 25, 0
        for file in sorted(folder.glob("usage_ledger_*.py")):
            with tempfile.TemporaryDirectory(dir=folder) as temp:
                shadow = Path(temp)
                (shadow / "deadline_window.py").write_bytes((folder / "deadline_window.py").read_bytes())
                (shadow / "usage_ledger.py").write_bytes(file.read_bytes())
                result = acceptance(shadow)
                checks += result["checks"] - 25
                executed += result["checks"]
        return {"checks": checks, "checks_executed": executed, "passed": True,
                "sha256": {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in folder.glob("*.py")}}
    small = gold.validate("small", folder)
    source = gold.load_module(folder / "usage_ledger.py")
    checks = small["checks"]
    for raw, expected in [({}, None), (None, None), ({"total_tokens": 0}, 0),
        ({"total_tokens": 12, "inputTokens": 999}, 12), ({"total_tokens": True, "totalTokens": 8}, 8),
        ({"total_tokens": float("nan"), "inputTokens": 5, "outputTokens": 7, "reasoningTokens": 6}, 12),
        ({"input_tokens": 1.0, "output_tokens": 2.0}, 3), ({"total_tokens": 10 ** 400}, 10 ** 400),
        ({"input_tokens": True, "inputTokens": 5, "output_tokens": 2}, 7),
        ({"input_tokens": "1", "output_tokens": 2}, None), ({"inputTokens": 1, "outputTokens": float("inf")}, None),
        ({"total_tokens": -1, "totalTokens": 3.5}, None)]:
        assert source["normalize_usage"](raw) == expected
        checks += 1
    rows = [{"attempt_id": "a", "revision": 0, "usage": {"total_tokens": 10}},
        {"attempt_id": "b", "revision": 1, "reserved_tokens": 20, "unknown_paid_outcome": True},
        {"attempt_id": "a", "revision": 2, "usage": {"inputTokens": 5, "outputTokens": 8, "reasoningTokens": 7}},
        {"attempt_id": "a", "revision": 2, "usage": {"total_tokens": 999}},
        {"attempt_id": "c", "revision": 0, "generation_not_started": True, "reserved_tokens": 100},
        {"attempt_id": "invalid", "revision": True, "reserved_tokens": 100}]
    original = copy.deepcopy(rows)
    assert source["merge_attempts"](rows) == {"tokens_used": 33, "tokens_estimated": 20, "unknown_count": 1, "ids": ["a", "b", "c"]}
    assert rows == original
    assert source["merge_attempts"]([]) == {"tokens_used": 0, "tokens_estimated": 0, "unknown_count": 0, "ids": []}
    assert gold.load_module(folder / "deadline_window.py")["__doc__"] == gold.FILES["small"]["deadline_window.py"].split('"""')[1]
    return {"checks": checks + 4, "passed": True, "sha256": small["sha256"]}


def run(client, policy, trial):
    directory = OUT / f"{trial}-{policy}"
    directory.mkdir(parents=True)
    inputs = fixtures()
    for name, body in inputs.items():
        (directory / name).write_text(body, encoding="utf-8")
    saved = client.call("router_scheduling", {"action": "get"})["settings"]["workflows"][0]
    mother = copy.deepcopy(next(n for n in saved["nodes"] if n["node_type"] == "codex"))
    operators = [copy.deepcopy(n) for n in saved["nodes"] if n["node_type"] == "operator"][:2]
    for i, node in enumerate(operators):
        node.update(route_id=ROUTE, effort="high", fallback_routes=[])
        node["task"].update(max_output_tokens=256 if i == 0 else 8192, output_mode="fixed" if i == 0 else "adaptive",
            token_budget=16000 if i == 0 else 30000, max_attempts=1, allow_fallback=False,
            deadline_s=300, hard_deadline_s=600, ttft_s=300)
    first, second = [n["id"] for n in operators]
    definition = {"id": saved["id"], "name": "真实母节点调度对照:" + policy, "max_parallel": 2, "token_budget": 50000,
        "max_rounds": 3, "luna_supervision": saved["luna_supervision"], "nodes": [mother, *operators],
        "edges": [e for node in operators for e in [{"from": mother["id"], "to": node["id"]},
                                                        {"from": node["id"], "to": mother["id"], "kind": "report"}]]}
    key = f"controller-pair-0.7.4-{LEDGER_FILES}-20261025:{trial}:{policy}"
    evidence = {"policy": policy, "trial": trial, "request_id": key, "health": client.runtime_info(),
        "input_sha256": {n: hashlib.sha256((directory / n).read_bytes()).hexdigest() for n in inputs},
        "definition": definition, "controls": [], "status": None, "success": False, "start": time.time()}
    path = directory / "evidence.json"
    write(path, evidence)
    receipt = client.call("router_dispatch", {"action": "submit", "request_id": key, "request": {"kind": "workflow",
        "definition": definition, "workflow_id": WORKFLOW, "input_text": "修复两个独立文件的已定位缺陷；只修改分配文件，母节点负责独立检验与验收。",
        "execution": {"root": str(directory.resolve()), "writable_paths": list(inputs), "commands": {}}}})
    ident = receipt["dispatch_id"]
    evidence["receipt"] = receipt
    def status():
        return client.call("router_dispatch", {"action": "status", "dispatch_id": ident})["detail"]
    sequence = 0
    def control(decision):
        nonlocal sequence
        sequence += 1
        identity = key + ":control:" + str(sequence)
        started = time.time()
        current = status()
        result = client.call("router_dispatch", {"action": "coordinate", "dispatch_id": ident,
            "expected_revision": current["control_revision"], "event_id": identity, "decision": decision})
        evidence["controls"].append({"event_id": identity, "decision": decision, "start": started, "end": time.time(),
                                     "revision": result["control_revision"]})
        write(path, evidence)
        return result
    def wait(predicate):
        until = time.monotonic() + 620
        while time.monotonic() < until:
            current = status()
            if predicate(current):
                return current
            if current["state"] in {"interrupted", "unknown", "cancelled"}:
                raise RuntimeError("Workflow interrupted/unknown; no paid replay")
            time.sleep(.2)
        raise RuntimeError("Paired workload deadline exceeded")
    def parent_checks():
        control({"type": "controller_work", "op": "start", "item_id": "independent-checks"})
        evidence["parent_checks"] = independent_checks()
        control({"type": "controller_work", "op": "finish", "item_id": "independent-checks",
                 "summary": "Completed 256 independent output-budget/context checks; no producer file was read or changed."})
    try:
        control({"type": "controller_work", "op": "enqueue", "items": [
            {"id": "independent-checks", "kind": "verification", "instruction": "独立核对预算与模型容量/上下文的256组边界", "paths": [], "access": "snapshot"},
            {"id": "final-checks", "kind": "verification", "instruction": "检验两份实际产物和精确SHA，错误则拒绝验收", "paths": list(inputs),
             "access": "current", "depends_on": [first, second]}]})
        initial = "修复deadline_window.py的三处边界：无活动也受hard约束，idle_end包含grace，未观察到response时取max(first,idle_end)。必须保留原docstring、签名与其它代码。本次用一个write_text返回完整文件。sources=[]，不得执行工具。"
        ledger = '只用两次精确replace_text修复usage_ledger.py：将 return sum(parts) + raw.get("reasoningTokens", 0) 改为 return sum(parts)；将 revision >= selected[ident]["revision"] 改为严格大于。保留原文件的其它每个字符。使用快照完整SHA，sources=[]，不要重写或重新设计模块。'
        control({"type": "assign", "assignments": [
            {"node_id": first, "paths": ["deadline_window.py"], "instruction": initial},
            {"node_id": second, "paths": [name for name in inputs if name != "deadline_window.py"],
             "instruction": ledger.replace("usage_ledger.py", "每个分配的usage_ledger模块") if LEDGER_FILES > 1 else ledger}]})
        print(json.dumps({"phase": "submitted", "policy": policy, "trial": trial, "dispatch_id": ident}), flush=True)
        if policy == "overlap":
            wait(lambda r: r["nodes"][second].get("job_id") is not None)
            parent_checks()
        current = wait(lambda r: r["nodes"][first]["state"] in {"failed", "unknown", "succeeded"})
        if policy == "serial":
            wait(lambda r: r["nodes"][second]["state"] in {"succeeded", "failed", "unknown", "blocked"})
            parent_checks()
            current = status()
        proposal = next((p for p in current.get("review_proposals", []) if p.get("kind") == "generation_recovery" and p["node_id"] == first), None)
        if proposal is None:
            raise RuntimeError("The deliberate first-page pressure did not produce a safe known-truncation proposal")
        evidence["proposal"] = proposal
        evidence["independent_branch_state_at_recovery"] = current["nodes"][second]["state"]
        recovery = '改为三个最小replace_text：无活动 return first 改为 return min(hard_deadline, first)；idle_end = last_activity + idle 加上 + grace；最后else first改为else max(first, idle_end)。其它内容逐字保留，复制准确SHA，返回完整operations JSON。'
        control({"type": "revise", "proposal_digest": proposal["digest"],
            "assignment": {"node_id": first, "paths": ["deadline_window.py"], "instruction": recovery},
            "generation": {"output_mode": "adaptive", "max_output_tokens": 8192, "route_effort": "high"}})
        evidence["recovery_approved_at"] = time.time()
        current = wait(lambda r: all(r["nodes"][node]["state"] in {"succeeded", "failed", "unknown", "blocked"} for node in (first, second)))
        if any(current["nodes"][node]["state"] != "succeeded" for node in (first, second)):
            raise RuntimeError("At least one producer failed; no complete acceptance")
        evidence["producers_done_at"] = time.time()
        control({"type": "controller_work", "op": "start", "item_id": "final-checks"})
        evidence["acceptance"] = acceptance(directory)
        control({"type": "controller_work", "op": "finish", "item_id": "final-checks", "summary": "All " + str(evidence["acceptance"]["checks"]) + " identical contract checks and preserved docstring/SHA checks passed.", "verdict": "accepted"})
        current = status()
        control({"type": "controller_work", "op": "ack_reports", "reports": [
            {"node_id": p["node_id"], "fingerprint": p["fingerprint"]} for p in current["controller_work"]["reports_ready"]]})
        current = status()
        accepted = control({"type": "accept", "evidence_digest": current["evidence_digest"]})
        assert accepted["state"] == "succeeded"
        evidence["success"] = True
    except Exception as error:
        evidence["error"] = type(error).__name__ + ": " + str(error)
        current = status()
        if current["state"] not in {"succeeded", "cancelled", "unknown", "interrupted"}:
            control({"type": "reject", "reason": "Paired workload failed its known-truncation or exact quality contract; preserve usage and partial artifacts."})
    finally:
        evidence["end"] = time.time()
        evidence["elapsed_s"] = evidence["end"] - evidence["start"]
        current = status()
        evidence["status"] = current
        evidence["jobs"] = {job_id: client.call("router_status", {"job_id": job_id}) for job_id in current["job_ids"]}
        evidence["tokens_used"] = sum(j["tokens_used"] for j in evidence["jobs"].values())
        evidence["native_attempts"] = sum(len(j["attempts"]) for j in evidence["jobs"].values())
        latest = current["nodes"][first]
        second_job = evidence["jobs"].get(current["nodes"][second].get("job_id"), {})
        recovery_job = evidence["jobs"].get(latest.get("job_id"), {})
        if latest.get("work_revision", 1) > 1 and second_job.get("attempts") and recovery_job.get("attempts"):
            a, b = recovery_job["attempts"][0], second_job["attempts"][0]
            a_start, b_start = a["started_at"] / 1000, b["started_at"] / 1000
            a_end, b_end = a_start + a.get("duration_ms", 0) / 1000, b_start + b.get("duration_ms", 0) / 1000
            evidence["native_recovery_overlap_s"] = max(0, min(a_end, b_end) - max(a_start, b_start))
            evidence["recovery_deadline_preserved"] = recovery_job["deadline_at"] == evidence["proposal"]["deadline_at"]
            evidence["independent_branch_not_replayed"] = second_job["attempt_count"] == 1
        write(path, evidence)
        print(json.dumps({"phase": "completed", "policy": policy, "trial": trial, "success": evidence["success"],
            "elapsed_s": evidence["elapsed_s"], "tokens": evidence["tokens_used"], "overlap_s": evidence.get("native_recovery_overlap_s"),
            "error": evidence.get("error")}), flush=True)
    return evidence


def main():
    global OUT, LEDGER_FILES
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--execute-native", action="store_true", required=True)
    parser.add_argument("--ledger-files", type=int, choices=(1, 4), default=1)
    parser.add_argument("--pairs", type=int, choices=(1, 2), default=2)
    args = parser.parse_args()
    LEDGER_FILES = args.ledger_files
    if LEDGER_FILES > 1:
        OUT = OUT.with_name("paired-batch4-0.7.4")
    if OUT.exists():
        raise ValueError("Existing paired identities must be reconciled before any paid repetition")
    OUT.mkdir(parents=True)
    client = LocalClient(Path.home() / ".globaltokentracker/router/config.json", autostart=False)
    health = client.runtime_info()
    assert health["version"] == "0.7.4" and {"controller_work_v1", "operator_recovery_v1", "atomic_edit_bundle_v1"} <= set(health["features"])
    records = []
    policies = ("serial", "overlap", "overlap", "serial") if args.pairs == 2 else ("serial", "overlap")
    for trial, policy in enumerate(policies, 1):
        if sum(r["tokens_used"] for r in records) >= 100000:
            break
        records.append(run(client, policy, trial))
        write(OUT / "summary.json", {"runs": [{k: r.get(k) for k in ("policy", "trial", "success", "elapsed_s", "tokens_used", "native_attempts", "native_recovery_overlap_s", "recovery_deadline_preserved", "independent_branch_not_replayed", "error")} for r in records]})
    summary = json.loads((OUT / "summary.json").read_text())
    summary["design"] = "ABBA policy order, identical native model/effort/input/limits/contract checks on the same 0.7.4 service. Deliberate first-output pressure; not a fix claim for all 16k reasoning exhaustion."
    for policy in ("serial", "overlap"):
        all_runs = [r for r in records if r["policy"] == policy]
        passed = [r for r in all_runs if r["success"]]
        summary[policy] = {"runs": len(all_runs), "successes": len(passed), "tokens_total": sum(r["tokens_used"] for r in all_runs),
            "mean_elapsed_s": statistics.mean(r["elapsed_s"] for r in passed) if passed else None}
    if all(summary[p]["successes"] == args.pairs for p in ("serial", "overlap")):
        summary["observed_mean_time_difference_pct"] = (1 - summary["overlap"]["mean_elapsed_s"] / summary["serial"]["mean_elapsed_s"]) * 100
        summary["actual_recovery_overlap_observed"] = any(r.get("native_recovery_overlap_s", 0) > 0 for r in records if r["policy"] == "overlap")
        summary["wall_time_reduction_pct"] = summary["observed_mean_time_difference_pct"] if summary["actual_recovery_overlap_observed"] else None
    write(OUT / "summary.json", summary)
    print(json.dumps({"summary": str(OUT / "summary.json"), **{p: summary[p] for p in ("serial", "overlap")},
        "wall_time_reduction_pct": summary.get("wall_time_reduction_pct")}), flush=True)


if __name__ == "__main__":
    main()
