"""Verify dynamic reservations and resident automatic pages using existing routes."""
import ast
import hashlib
import json
from pathlib import Path
import sys
import time

PRODUCT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PRODUCT))
from gtt_router.daemon import LocalClient


def main():
    client = LocalClient(Path.home() / ".globaltokentracker/router/config.json", autostart=False)
    health = client.runtime_info()
    assert health["state_owner"] == "rust" and "output_steps_v1" in health["features"]
    version = health["version"]
    output = PRODUCT.parents[1] / ("target/diagnostics/output-limits-live/formal-validation-" + version + ".json")
    if output.exists():
        raise ValueError("Inspect existing request identities before repeating paid validation")
    source = (PRODUCT / "gtt_router/slicing.py").read_text(encoding="utf-8")
    text = "PART A\n" + "\n\n".join(ast.get_source_segment(source, n) for n in ast.parse(source).body
                                     if isinstance(n, ast.FunctionDef) and n.name in {"requested_output", "output_budget"})
    text += "\n\nPART B\n" + (PRODUCT / "gtt_router/activity.py").read_text(encoding="utf-8")
    base = {"kind": "extract", "context": text, "max_output_tokens": 65536, "token_budget": 22000,
        "workflow_id": "limits-formal-20261025", "allowed_routes": ["39aa71a79b77acde8cc0"],
        "preferred_route": "39aa71a79b77acde8cc0", "allow_fallback": False, "deadline_s": 300,
        "ttft_s": 300, "hard_deadline_s": 600, "idle_s": 180, "report_max_chars": 16000}
    evidence = {"health": health, "cases": []}
    for name in ("fixed-rejection", "native-auto-pages"):
        task = dict(base, output_mode="fixed" if name == "fixed-rejection" else "native",
            max_attempts=1 if name == "fixed-rejection" else 3,
            max_output_steps=1 if name == "fixed-rejection" else 3, work_key="formal:" + name)
        task["prompt"] = ("核对源代码的输出预算与活动超时。严格用两段完整JSON回答：第一段仅PART A的两个函数，"
            "第二段仅PART B的ActivityWatch。每段2项finding，包含准确代码锚点和边界输入/预期结果；"
            "每段正文不超过1200汉字，summary不超过150汉字。第1段output_step.index=1、final=false、next='PART B: ActivityWatch'；"
            "第2段index=2、final=true、next为空。每段complete=true只表示该段完整。sources=[]，不调用工具，不猜测未提供代码。")
        request_id = "formal-" + version + "-20261025:" + name
        record = {"name": name, "request_id": request_id, "task": task}
        evidence["cases"].append(record)
        record["plan"] = client.call("router_dispatch", {"action": "plan", "request": {"kind": "task", "task": task}})
        record["receipt"] = client.call("router_dispatch", {"action": "submit", "request_id": request_id, "request": {"kind": "task", "task": task}})
        output.write_text(json.dumps(evidence, ensure_ascii=False, indent=2), encoding="utf-8")
        ident = record["receipt"]["dispatch_id"]
        print(json.dumps({"case": name, "dispatch_id": ident}), flush=True)
        deadline = time.monotonic() + 650
        while time.monotonic() < deadline:
            status = client.call("router_dispatch", {"action": "status", "dispatch_id": ident})
            if status["state"] not in {"queued", "running", "admitting", "cancelling"}:
                break
            time.sleep(1)
        else:
            client.call("router_dispatch", {"action": "cancel", "dispatch_id": ident})
            raise RuntimeError("Formal validation deadline exceeded; own request cancelled")
        record["status"] = status
        page = client.call("router_dispatch", {"action": "report", "dispatch_id": ident, "worker": 0, "max_chars": 30000})
        text_parts = [page["report_json"]]
        while page["has_more"]:
            page = client.call("router_dispatch", {"action": "report", "dispatch_id": ident, "worker": 0,
                "offset": page["next_offset"], "max_chars": 30000})
            text_parts.append(page["report_json"])
        raw = "".join(text_parts)
        assert hashlib.sha256(raw.encode()).hexdigest() == page["sha256"]
        record["report"] = json.loads(raw)
        job = status["detail"]
        if name == "fixed-rejection":
            assert job["attempt_count"] == 0 and job["tokens_used"] == 0
            assert record["report"]["error"]["code"] == "token_budget_exhausted"
        else:
            assert status["state"] == "succeeded", record["report"].get("error")
            assert record["report"]["content"]["complete"] is True
            assert len(record["report"]["output_steps"]) == 2
            assert len(job["attempts"]) == 2 and job["tokens_reserved"] == 0 and job["tokens_used"] <= task["token_budget"]
            assert all(a["effective_output_tokens"] < a["decision"]["native_output_limit"] for a in job["attempts"])
            assert job["attempts"][1]["effective_output_tokens"] < job["attempts"][0]["effective_output_tokens"]
        record["checks_passed"] = True
        output.write_text(json.dumps(evidence, ensure_ascii=False, indent=2), encoding="utf-8")
        print(json.dumps({"case": name, "state": status["state"], "attempts": job["attempt_count"],
            "tokens_used": job["tokens_used"], "effective": [a["effective_output_tokens"] for a in job["attempts"]]}), flush=True)
    print(json.dumps({"evidence": str(output), "verified": True}), flush=True)


if __name__ == "__main__":
    main()
