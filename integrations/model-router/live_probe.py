"""Small, source-free live acceptance probes; writes only user-local evidence."""

import json
from pathlib import Path
import time
import sys

from gtt_router.daemon import LocalClient


root = Path.home() / ".globaltokentracker" / "router"
client = LocalClient(root / "config.json")
inventory = client.call("router_inventory")
routes = inventory["routes"]
preferred = next(r["route_id"] for r in routes if r["source"] == "zcode" and r["provider"] == "new-provider-2")
tasks = {
    "zcode_review_failover": {
        "kind": "code_review", "prompt": "审阅这个局部函数：对缓存值为 0 的输入，指出具体逻辑错误、触发条件和最小修复思路。仅根据所给代码回答，不访问本地文件，不执行工具。",
        "context": "cache = {'a': 0}\ndef cached(key):\n    value = cache.get(key)\n    if value:\n        return value\n    return expensive_compute(key)\n",
        "context_label": "synthetic-cache-fixture", "deadline_s": 140, "max_attempts": 3,
        "max_output_tokens": 3072, "token_budget": 24000, "preferred_route": preferred,
        "min_quality": 40, "workflow_id": "live-probe", "work_key": "synthetic-cache:review",
    },
    "flash_research_fanout": {
        "kind": "research", "prompt": "查阅 MCP 官方规范，核实 stdio 消息是否每行一个 JSON 对象，以及服务器日志是否可以写入 stdout。打开实际官方规范页面并给出其 URL。只需短结论、两条证据和不确定项。",
        "needs_web": True, "fanout": 2, "verify": True, "deadline_s": 220,
        "min_quality": 38, "max_attempts": 7, "max_output_tokens": 8192, "token_budget": 110000,
        "workflow_id": "live-probe", "work_key": "mcp-stdio:research",
        "focus_urls": ["https://modelcontextprotocol.io/specification/2025-11-25/basic/transports"],
        "focus_terms": ["stdio", "stdout", "stderr"], "web_tool_budget": 4,
    },
}
report = {"route_count": len(routes), "gtt_source_count": len(inventory["gtt"]["sources"]), "checks": {}}
if "--research-only" in sys.argv:
    prior = root / "live-validation.json"
    if prior.exists():
        report = json.loads(prior.read_text(encoding="utf-8"))
        old = report.get("checks", {}).get("flash_research_fanout")
        if old:
            report.setdefault("prior_research_jobs", []).append(old["job_id"])
    tasks = {"flash_research_fanout": tasks["flash_research_fanout"]}
for name, task in tasks.items():
    started = time.monotonic()
    job = client.call("router_submit", {"task": task})
    report["checks"][name] = {"job_id": job["job_id"], "submit_ms": round((time.monotonic() - started) * 1000, 1)}
    print(json.dumps({"name": name, **report["checks"][name]}), flush=True)
pending = set(tasks)
until = time.monotonic() + 235
while pending and time.monotonic() < until:
    for name in list(pending):
        entry = report["checks"][name]
        data = client.call("router_collect", {"job_id": entry["job_id"], "max_chars": 30000})
        if data["state"] in {"succeeded", "partial", "failed", "cancelled", "needs_codex"}:
            entry["result"] = data
            entry["status"] = client.call("router_status", {"job_id": entry["job_id"]})
            entry["ok"] = data["state"] == "succeeded"
            pending.remove(name)
            print(json.dumps({"name": name, "state": data["state"], "workers": len(data["results"]), "attempts": entry["status"]["attempt_count"]}), flush=True)
            (root / "live-validation.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    if pending:
        time.sleep(1)
report["pending"] = sorted(pending)
(root / "live-validation.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
print(json.dumps({"live_checks": {name: check.get("ok", False) for name, check in report["checks"].items()}, "pending": report["pending"]}), flush=True)
