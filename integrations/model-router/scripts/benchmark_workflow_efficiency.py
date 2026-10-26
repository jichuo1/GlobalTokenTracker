"""Paired real-file workloads. Timings include agent work, not function microbenchmarks."""
import argparse
import ast
import copy
import hashlib
import json
import math
from pathlib import Path
import sys
import time

PRODUCT = Path(__file__).resolve().parents[1]
ROOT = PRODUCT.parents[1] / "target/diagnostics/output-limits-live/efficiency"
SESSION = Path.home() / ".codex/sessions/2026/10/25/rollout-2026-10-25T14-03-43-01a171a8-6337-72d1-9cd5-cff0576f8b3f.jsonl"
sys.path.insert(0, str(PRODUCT))
from gtt_router.daemon import LocalClient

FILES = {
"small": {"deadline_window.py": '''"""Activity deadline calculation. Times are finite seconds, validated upstream.
Return min(hard_deadline, first response expectation) before activity.
first = min(soft_deadline, started + ttft).
With activity, idle expiry includes idle + grace. After a real response use idle
expiry; native-phase-only activity may extend first with max(first, idle expiry).
Every branch remains bounded by hard_deadline. Do not change the signature.
"""

def expires_at(started, last_activity, soft_deadline, hard_deadline, ttft, idle, grace, response):
    first = min(soft_deadline, started + ttft)
    if last_activity is None:
        return first
    idle_end = last_activity + idle
    return min(hard_deadline, idle_end if response else first)
'''},
"parallel": {"usage_ledger.py": '''"""Implement bounded accounting; standard library only, no IO or mutation.
normalize_usage(raw) -> int or None. Only nonnegative finite whole numbers are
valid; bool and strings are invalid. Prefer total_tokens then totalTokens if valid.
Otherwise require one valid input field (input_tokens, inputTokens) and one valid
output field (output_tokens, outputTokens); sum them. Reasoning is already a subset
of output and must never be added. Unknown or malformed usage returns None.

merge_attempts(rows) -> {tokens_used:int,tokens_estimated:int,unknown_count:int,ids:list}.
Each dict has attempt_id (nonempty str), revision (nonnegative int excluding bool),
usage, reserved_tokens, unknown_paid_outcome, generation_not_started.
Ignore invalid rows/identities/revisions. Deduplicate by attempt_id; largest
revision wins, first row wins ties. Preserve first valid occurrence order of ids.
generation_not_started is True charges zero; otherwise use normalize_usage when
known, else a valid nonnegative whole reserved_tokens or zero. Estimated charges
are a SUBSET of tokens_used. unknown_count counts selected rows whose
unknown_paid_outcome is exactly True. Do not mutate input rows.
"""

def normalize_usage(raw):
    return None


def merge_attempts(rows):
    return {"tokens_used": 0, "tokens_estimated": 0, "unknown_count": 0, "ids": []}
''',
"source_ranges.py": '''"""Implement exact Unicode source partitioning and safe resume; no IO.
partition(text, maximum) returns [{start:int,end:int,text:str,sha256:str}]. Reject
non-string text or non-int/bool/nonpositive maximum with ValueError. Empty text
returns []. Offsets use Python characters. Every character is covered once in
order; chunks are nonempty and <=maximum. At each start, provisional stop is
min(len(text), start+maximum). If not at end, prefer the last complete double
newline in [start+max(1,maximum//2), stop), then single newline in that range;
include the separator in the preceding chunk, otherwise use provisional stop.
sha256 hashes each chunk's UTF-8 bytes. Never split UTF-8 bytes or trim whitespace.

resume_chunks(chunks, succeeded, unknown) -> {pending:list,unknown:list,complete:bool}.
States identify exact (start,end) tuple pairs. Unknown has priority if both sets
contain a range. Preserve original order. Pending excludes succeeded AND unknown;
unknown is returned separately and never auto-replayed. complete is true only
if both returned lists are empty. Do not mutate any input.
"""

def partition(text, maximum):
    return []


def resume_chunks(chunks, succeeded, unknown):
    return {"pending": chunks, "unknown": [], "complete": False}
'''}}


def metrics():
    latest = None
    with SESSION.open(encoding="utf-8") as stream:
        for line in stream:
            if '"token_count"' not in line:
                continue
            value = json.loads(line)
            if value.get("type") == "event_msg" and value.get("payload", {}).get("type") == "token_count":
                latest = value["payload"].get("info", {}).get("total_token_usage")
    return latest


def load_module(path):
    source = path.read_text(encoding="utf-8")
    tree = ast.parse(source)
    banned = {"open", "eval", "exec", "compile", "getattr", "setattr", "globals", "locals", "input", "print", "__import__"}
    allowed_imports = {"math", "hashlib", "collections", "numbers", "typing"}
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            assert all(n.name in allowed_imports for n in node.names)
        if isinstance(node, ast.ImportFrom):
            assert node.module in allowed_imports and all(n.name != "*" for n in node.names)
        assert not isinstance(node, (ast.ClassDef, ast.With, ast.AsyncWith, ast.Global, ast.Nonlocal))
        if isinstance(node, ast.Attribute):
            assert not node.attr.startswith("_")
        if isinstance(node, ast.Name):
            assert node.id not in banned and not node.id.startswith("__")
    namespace = {}
    exec(compile(tree, str(path), "exec"), namespace)
    return namespace


def validate(case, directory):
    checks = 0
    def check(value):
        nonlocal checks
        assert value
        checks += 1
    if case == "small":
        fn = load_module(directory / "deadline_window.py")["expires_at"]
        for last in (None, 0, 12, 80):
            for hard in (5, 45, 100):
                for response in (False, True):
                    first = min(50, 10 + 20)
                    end = first if last is None else last + 10 + 7 if response else max(first, last + 10 + 7)
                    check(fn(10, last, 50, hard, 20, 10, 7, response) == min(hard, end))
    else:
        usage = load_module(directory / "usage_ledger.py")
        fn = usage["normalize_usage"]
        for raw, expected in [({}, None), (None, None), ({"total_tokens": 0}, 0),
            ({"total_tokens": 12, "input_tokens": 999}, 12),
            ({"total_tokens": True, "totalTokens": 8}, 8),
            ({"total_tokens": float("nan"), "inputTokens": 5, "outputTokens": 7, "reasoningTokens": 6}, 12),
            ({"input_tokens": 1.0, "output_tokens": 2.0}, 3),
            ({"total_tokens": 10 ** 400}, 10 ** 400),
            ({"input_tokens": True, "inputTokens": 5, "output_tokens": 2}, 7),
            ({"input_tokens": "1", "output_tokens": 2}, None),
            ({"inputTokens": 1, "outputTokens": float("inf")}, None),
            ({"total_tokens": -1, "totalTokens": 3.5}, None)]:
            check(fn(raw) == expected)
        rows = [{"attempt_id": "a", "revision": 0, "usage": {"total_tokens": 10}},
            {"attempt_id": "b", "revision": 1, "reserved_tokens": 20, "unknown_paid_outcome": True},
            {"attempt_id": "a", "revision": 2, "usage": {"inputTokens": 5, "outputTokens": 8, "reasoningTokens": 7}},
            {"attempt_id": "a", "revision": 2, "usage": {"total_tokens": 999}},
            {"attempt_id": "c", "revision": 0, "generation_not_started": True, "reserved_tokens": 100},
            {"attempt_id": "d", "revision": True, "reserved_tokens": 100}, None,
            {"attempt_id": "e", "revision": 0, "reserved_tokens": True},
            {"attempt_id": "", "revision": 0, "reserved_tokens": 100}]
        original = copy.deepcopy(rows)
        check(usage["merge_attempts"](rows) == {"tokens_used": 33, "tokens_estimated": 20, "unknown_count": 1, "ids": ["a", "b", "c", "e"]})
        check(rows == original)
        check(usage["merge_attempts"]([]) == {"tokens_used": 0, "tokens_estimated": 0, "unknown_count": 0, "ids": []})
        ranges = load_module(directory / "source_ranges.py")
        for text in ("", "中文🙂\n\nAlpha\nBeta\n尾", "a" * 31, " a\n b\n\n c "):
            for maximum in (1, 2, 5, 8):
                chunks = ranges["partition"](text, maximum)
                check("".join(c["text"] for c in chunks) == text)
                cursor = 0
                for chunk in chunks:
                    check(chunk["start"] == cursor and 0 < chunk["end"] - cursor <= maximum)
                    check(chunk["text"] == text[cursor:chunk["end"]])
                    check(chunk["sha256"] == hashlib.sha256(chunk["text"].encode()).hexdigest())
                    cursor = chunk["end"]
        check(ranges["partition"]("abcd\n\nefgh", 8)[0]["text"] == "abcd\n\n")
        for invalid in (0, -1, True, 2.5, "4"):
            try:
                ranges["partition"]("abc", invalid)
            except ValueError:
                check(True)
            else:
                check(False)
        try:
            ranges["partition"](None, 3)
        except ValueError:
            check(True)
        else:
            check(False)
        chunks = ranges["partition"]("abcdefghi", 3)
        original = copy.deepcopy(chunks)
        result = ranges["resume_chunks"](chunks, {(0, 3), (3, 6)}, {(3, 6)})
        check(result == {"pending": [chunks[2]], "unknown": [chunks[1]], "complete": False})
        check(chunks == original)
        check(ranges["resume_chunks"](chunks, {(0, 3), (3, 6), (6, 9)}, set())["complete"] is True)
    return {"checks": checks, "passed": True, "sha256": {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in directory.glob("*.py")}}


def prepare():
    for case, files in FILES.items():
        for arm in ("solo", "workflow", "optimized"):
            folder = ROOT / case / arm
            folder.mkdir(parents=True)
            for name, source in files.items():
                (folder / name).write_text(source, encoding="utf-8")
    print(json.dumps({"root": str(ROOT), "cases": list(FILES)}))


def begin(case):
    record = {"case": case, "arm": "solo", "tokens_before": metrics(), "start_wall": time.time()}
    (ROOT / case / "solo-measure.json").write_text(json.dumps(record), encoding="utf-8")
    for path in sorted((ROOT / case / "solo").glob("*.py")):
        print(str(path) + "\n" + path.read_text(encoding="utf-8"))


def finish(case, arm):
    result = validate(case, ROOT / case / arm)
    record_path = ROOT / case / (arm + "-measure.json")
    record = json.loads(record_path.read_text(encoding="utf-8"))
    record.update(validation=result, validated_wall=time.time())
    if arm == "solo":
        record.update(tokens_after=metrics())
        record["elapsed_s"] = record["validated_wall"] - record["start_wall"]
    record_path.write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({k: record[k] for k in ("case", "arm", "validation", "validated_wall", "elapsed_s") if k in record}, ensure_ascii=False))


def accept(case, arm):
    record_path = ROOT / case / (arm + "-measure.json")
    record = json.loads(record_path.read_text(encoding="utf-8"))
    assert record["validation"] == validate(case, ROOT / case / arm)
    client = LocalClient(Path.home() / ".globaltokentracker/router/config.json", autostart=False)
    ident = record["receipt"]["dispatch_id"]
    status = client.call("router_dispatch", {"action": "status", "dispatch_id": ident})
    action = next(a for a in status["detail"]["pending_actions"] if a["type"] == "accept")
    accepted = client.call("router_dispatch", {"action": "coordinate", "dispatch_id": ident,
        "expected_revision": status["detail"]["control_revision"], "event_id": record["request_id"] + ":accept",
        "decision": {"type": "accept", "evidence_digest": action["evidence_digest"],
            "reason": "Codex reviewed the actual bounded source; identical independent acceptance checks passed; current SHA256 matches the checked files."}})
    record.update(accepted=accepted, accepted_wall=time.time(), tokens_after_accept=metrics())
    record["elapsed_s"] = record["accepted_wall"] - record["start_wall"]
    record_path.write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"case": case, "arm": arm, "state": accepted["state"], "elapsed_s": record["elapsed_s"]}))


def run_workflow(case, arm):
    folder = ROOT / case / arm
    if arm == "balanced" and not folder.exists():
        folder.mkdir()
        for name, source in FILES[case].items():
            (folder / name).write_text(source, encoding="utf-8")
    record_path = ROOT / case / (arm + "-measure.json")
    if record_path.exists():
        raise ValueError("An existing benchmark identity must be reconciled, not replayed")
    client = LocalClient(Path.home() / ".globaltokentracker/router/config.json", autostart=False)
    settings = client.call("router_scheduling", {"action": "get"})["settings"]
    saved = settings["workflows"][0]
    master = copy.deepcopy(next(n for n in saved["nodes"] if n["node_type"] == "codex"))
    operators = [copy.deepcopy(n) for n in saved["nodes"] if n["node_type"] == "operator"][:len(FILES[case])]
    for operator in operators:
        operator["effort"] = operator["effort"] or operators[0]["effort"]
        if arm in {"optimized", "balanced"}:
            reviewer = next(n for n in saved["nodes"] if n["node_type"] == "reviewer")
            operator["route_id"] = reviewer["route_id"]
            operator["effort"] = reviewer["effort"]
            if arm == "balanced":
                operator["effort"] = "high"
        operator["task"].update(output_mode="adaptive", max_attempts=1, token_budget=48000,
            max_output_tokens=16384, hard_deadline_s=600, deadline_s=300, ttft_s=300, allow_fallback=False)
    definition = {"id": saved["id"], "name": "同任务效率对照：" + case + ":" + arm,
        "token_budget": 100000, "max_parallel": saved["max_parallel"], "luna_supervision": saved["luna_supervision"],
        "max_rounds": 1, "nodes": [master, *operators],
        "edges": [{"from": master["id"], "to": n["id"]} for n in operators] +
                 [{"from": n["id"], "to": master["id"], "kind": "report"} for n in operators]}
    prompt = "实现各文件完整docstring合同，只修改分配文件；使用标准库，无IO。Codex会使用相同独立测试与代码审阅验收。"
    request = {"kind": "workflow", "definition": definition, "workflow_id": "limits-formal-20261025", "input_text": prompt,
        "execution": {"root": str(folder.resolve()), "writable_paths": list(FILES[case]), "commands": {}}}
    key = "efficiency-v071-20261025:" + case + ":" + arm
    record = {"case": case, "arm": arm, "request_id": key, "definition": definition,
        "tokens_before": metrics(), "start_wall": time.time()}
    record["receipt"] = client.call("router_dispatch", {"action": "submit", "request": request, "request_id": key})
    ident = record["receipt"]["dispatch_id"]
    assignments = [{"node_id": node["id"], "paths": [name], "instruction": prompt + "\n目标：" + name,
                    "acceptance": ["完整实现docstring的所有分支和边界", "不引入IO、第三方库或额外文件", "返回完整operations JSON，sources=[]"]}
                   for node, name in zip(operators, FILES[case])]
    if arm in {"optimized", "balanced"}:
        algorithms = {
            "deadline_window.py": "母节点已确认最小修正：无活动分支也取min(hard,first)；idle_end加grace；只有native phase时取max(first,idle_end)，再统一受hard约束。只改这三处。",
            "usage_ledger.py": "母节点算法：先实现可复用的非负整数验证器，bool/字符串/非有限/非整数float均无效，整数不转float。规范化usage先依次尝试两个total别名，再分别找input/output的有效别名相加；不加reasoning。merge先用插入有序dict按id选revision最大的记录，严格大于才替换。第二遍统计unknown；未开始生成直接跳过计费；已报告整数用量优先，未知用量才用有效reservation并同时增加estimated，estimated不再叠加进used。保留首见id顺序。",
            "source_ranges.py": "母节点算法：partition校验str和type(maximum)is int且>0；用while start<len(text)，stop=min(n,start+maximum)。非尾块先在后半窗口rfind双换行，失败再找单换行；找到后end包含整个分隔符。不按字节切分，sha256仅编码当前切片。resume逐项取(start,end)，先判断unknown，再判断不在succeeded的pending；两列表都空才complete。保留对象和输入顺序，不修改输入。"}
        for assignment in assignments:
            assignment["instruction"] += "\n" + algorithms[assignment["paths"][0]]
    client.call("router_dispatch", {"action": "coordinate", "dispatch_id": ident, "expected_revision": 0,
        "event_id": key + ":assign", "decision": {"type": "assign", "assignments": assignments}})
    record["assigned_wall"] = time.time()
    record["tokens_after_assignment"] = metrics()
    record_path.write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"case": case, "arm": arm, "dispatch_id": ident, "path": str(record_path)}), flush=True)
    until = time.monotonic() + 650
    while time.monotonic() < until:
        status = client.call("router_dispatch", {"action": "status", "dispatch_id": ident})
        if status["detail"].get("execution_complete") or status["state"] in {"failed", "blocked", "unknown", "cancelled", "interrupted"}:
            break
        time.sleep(.2)
    else:
        client.call("router_dispatch", {"action": "cancel", "dispatch_id": ident})
        raise RuntimeError("Benchmark timed out")
    record.update(status=status, execution_complete_wall=time.time())
    record["execution_s"] = record["execution_complete_wall"] - record["start_wall"]
    record["jobs"] = {k: client.call("router_status", {"job_id": n["job_id"]}) for k, n in status["detail"]["nodes"].items() if n.get("job_id")}
    record_path.write_text(json.dumps(record, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"case": case, "arm": arm, "execution_s": record["execution_s"], "tokens": status["detail"]["tokens_used"],
        "nodes": {k: n["state"] for k, n in status["detail"]["nodes"].items()}}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("prepare", "begin", "finish", "run", "accept"))
    parser.add_argument("--case", choices=FILES)
    parser.add_argument("--arm", default="solo", choices=("solo", "workflow", "optimized", "balanced"))
    args = parser.parse_args()
    if args.action == "prepare":
        prepare()
    elif args.action == "begin":
        begin(args.case)
    elif args.action == "finish":
        finish(args.case, args.arm)
    elif args.action == "run":
        run_workflow(args.case, args.arm)
    else:
        accept(args.case, args.arm)
