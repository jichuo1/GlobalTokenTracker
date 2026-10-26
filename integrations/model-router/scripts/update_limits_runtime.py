"""Scoped 0.7.1 update of an existing Rust installation, with checked rollback."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import time
import uuid

from install_rust_runtime import PRODUCT, atomic, contained, counts, replace_checked, rpc, sha, stop_daemon
from gtt_router.daemon import startup_lock
from gtt_router.processes import background_options
from gtt_router import __version__ as VERSION
from gtt_router.settings_merge import read_history, history_bytes, digest as settings_digest
from gtt_router.scheduling import validate as validate_settings

RUNTIME_FILES = ["gtt_router/" + name + ".py" for name in
    ("__init__", "models", "profiles", "slicing", "mcp", "service", "scheduling", "daemon", "output_steps", "store",
     "operator", "operator_workflow", "controller_work", "rust_kernel", "settings_merge")]
RUNTIME_FILES.append("workflow-templates.json")
SKILL_NOTE = ("支持 output_steps_v1 的服务可显式设置 max_output_steps=2..16，所有完整 JSON 段共用原总预算、尝试次数与硬截止；"
    "仅全部段校验通过且末段 final=true 才验收。后续失败保留 complete=false 的部分内容，不重放未知付费结果。"
    "adaptive/native 首次和后续预约均按剩余预算收紧；fixed 和历史用量保护保留。操作员仍由 Codex 按授权文件或行范围分片。")
EFFICIENCY_NOTE = ("安排操作员前先判断分工收益：少量确定性修改通常由母节点直接完成；多个独立且足够大的分片再并行。"
    "母节点提供函数级算法、边界和验收条件，避免把完整设计问题再交给高速操作员重复思考。"
    "同一路线反复把输出额度耗在思考且未完成时，先核对真实失败用量并在当前已授权模型池内选择合格替代，"
    "不要只提高上限或降低质量门槛。token_efficiency统计包含新发生且有真实计量的失败请求；未知用量仍不能当低成本。"
    "分别报告原生执行时间、主线验收等待和端到端时间，不将非任务等待计入提速结论。")
CONTROLLER_NOTE = ("支持 controller_work_v1/operator_recovery_v1 时，母节点可在下发后认领独立研究、规划和检验，"
    "用就绪队列与回报收件箱决定下个动作，不默认阻塞等待全部分支。真实活动用短租约报告，"
    "当前文件检验有读写冲突与过期证据检查；确定截断可由母节点绑定提案恢复，未知结果不重放。"
    "使用时读取 [母节点并行工作与截断恢复](references/controller-work.md)。")


def history(state):
    conn = sqlite3.connect(f"file:{(state / 'router-state.sqlite').as_posix()}?mode=ro", uri=True)
    try:
        conn.execute("BEGIN")
        result = {}
        for table in ("jobs", "usage_records", "dispatches", "operator_operations", "visual_runs"):
            rows = conn.execute("SELECT * FROM " + table).fetchall()
            serialized = sorted(json.dumps(row, ensure_ascii=True, default=str) for row in rows)
            result[table] = {"count": len(rows), "sha256": hashlib.sha256("\n".join(serialized).encode()).hexdigest()}
        return result
    finally:
        conn.close()


def prepare(plan_path, settings_baselines=()):
    home = Path.home().resolve()
    install = (home / "AppData/Local/Programs/GlobalTokenTracker").resolve(strict=True)
    state = (home / ".globaltokentracker/router").resolve(strict=True)
    config = state / "config.json"
    raw = json.loads(config.read_text(encoding="utf-8-sig"))
    if raw["schema_version"] != 2 or Path(raw["state_dir"]).resolve() != state or raw["scheduler_kernel"]["backend"] != "rust":
        raise ValueError("This update requires the existing schema-2 Rust installation")
    kernel = contained(Path(raw["scheduler_kernel"]["executable"]), install)
    if kernel != install / "gtt-scheduler-kernel.exe":
        raise ValueError("Unexpected kernel location")
    candidate = plan_path.parent / "limits-candidate"
    candidate.mkdir()
    rows = []
    for name in RUNTIME_FILES:
        source, target = PRODUCT / name, contained(install / "model-router" / name, install)
        staged = candidate / name
        staged.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, staged)
        rows.append({"target": str(target), "candidate": str(staged), "before": sha(target), "after": sha(staged)})
    staged = candidate / kernel.name
    shutil.copy2(PRODUCT.parents[1] / "target/release" / kernel.name, staged)
    rows.append({"target": str(kernel), "candidate": str(staged), "before": sha(kernel), "after": sha(staged)})
    skill = (home / ".codex/skills/gtt-coordinator/SKILL.md").resolve(strict=True)
    original = skill.read_bytes()
    text = original.decode("utf-8-sig")
    newline = "\r\n" if "\r\n" in text else "\n"
    anchor = "## 分配与 token 效率"
    if "支持 output_steps_v1 的服务" not in text:
        if text.count(anchor) != 1:
            raise ValueError("Skill anchor is ambiguous")
        text = text.replace(anchor, SKILL_NOTE + newline * 2 + anchor)
    if "安排操作员前先判断分工收益" not in text:
        text = text.replace(anchor, EFFICIENCY_NOTE + newline * 2 + anchor)
    if "references/controller-work.md" not in text:
        text = text.replace(anchor, CONTROLLER_NOTE + newline * 2 + anchor)
    staged = candidate / "SKILL.md"
    staged.write_bytes((b"\xef\xbb\xbf" if original.startswith(b"\xef\xbb\xbf") else b"") + text.encode())
    rows.append({"target": str(skill), "candidate": str(staged), "before": sha(skill), "after": sha(staged)})
    reference = contained(skill.parent / "references/controller-work.md", skill.parent / "references")
    staged_reference = candidate / "controller-work.md"
    shutil.copy2(PRODUCT / "skill/references/controller-work.md", staged_reference)
    rows.insert(len(rows) - 1, {"target": str(reference), "candidate": str(staged_reference), "before": sha(reference), "after": sha(staged_reference)})
    history = state / "scheduling-history.json"
    snapshots = read_history(history)
    imported = []
    def collect(value, depth=0):
        if depth > 12:
            return
        if isinstance(value, dict):
            if {"revision", "enabled", "pool", "workflows"} <= set(value):
                snapshot = validate_settings(value)
                revision = snapshot["revision"]
                if revision in snapshots and settings_digest(snapshots[revision]) != settings_digest(snapshot):
                    raise ValueError("Ambiguous recorded settings revision: " + str(revision))
                snapshots[revision] = snapshot
                imported.append(revision)
            else:
                for child in value.values():
                    collect(child, depth + 1)
        elif isinstance(value, list):
            for child in value:
                collect(child, depth + 1)
    for baseline in settings_baselines:
        baseline = Path(baseline).resolve(strict=True)
        collect(json.loads(baseline.read_text(encoding="utf-8")))
    current_settings = rpc(state, "router_scheduling", {"action": "get"})["settings"]
    snapshots[current_settings["revision"]] = validate_settings(current_settings)
    staged_history = candidate / "scheduling-history.json"
    staged_history.write_bytes(history_bytes(snapshots))
    rows.append({"target": str(history), "candidate": str(staged_history), "before": sha(history), "after": sha(staged_history)})
    guards = {home / ".globaltokentracker/external-interface.json", Path(raw["ledger_path"]).resolve().parent / "external-interface.json"}
    for guard in guards:
        if guard.exists():
            value = json.loads(guard.read_text(encoding="utf-8-sig"))
            if value.keys() != {"schema_version", "enabled"} or value["schema_version"] != 1 or type(value["enabled"]) is not bool:
                raise ValueError("Invalid interface configuration")
    protected = [config, state / "scheduling.json", home / ".codex/config.toml"]
    plan = {"version": VERSION, "install": str(install), "state": str(state), "config": str(config),
        "skill": str(skill), "skill_references": [str(reference)], "state_artifacts": [str(history)],
        "settings_baseline_revisions": sorted(set(imported)), "rows": rows, "guards": [str(p) for p in sorted(guards)],
        "protected": {str(p): sha(p) for p in protected}, "health_before": rpc(state), "counts_before": counts(state)}
    plan_path.write_text(json.dumps(plan, indent=2), encoding="utf-8")
    print(json.dumps({"plan": str(plan_path), "files": len(rows), "health": plan["health_before"], "counts": plan["counts_before"]}))


def launch(state, entry, config):
    with (state / "daemon.log").open("ab") as log:
        process = subprocess.Popen([sys.executable, str(entry), "daemon", "--config", str(config)],
            stdin=subprocess.DEVNULL, stdout=log, stderr=log, **background_options(detached=True))
    until = time.monotonic() + 15
    while time.monotonic() < until:
        if process.poll() is not None:
            raise RuntimeError("Installed service exited; inspect daemon.log")
        try:
            health = rpc(state)
            if health["pid"] == process.pid:
                return health
        except (OSError, ValueError, RuntimeError):
            pass
        time.sleep(.2)
    stop_daemon(process.pid, config, [entry])
    raise RuntimeError("Installed service startup timed out")


def restore_files(rows, backup, replaced, mutable_targets=()):
    for index in reversed(replaced):
        row = rows[index]
        path = Path(row["target"])
        if sha(path) != row["after"]:
            if path.resolve() in mutable_targets:
                continue
            raise ValueError("Installed file changed during rollback")
        if row["before"] is None:
            # Move only the new file this update owns, keeping it recoverable.
            path.replace(backup / ("new-file-" + str(index)))
        else:
            atomic(path, (backup / str(index)).read_bytes())


def activate(plan_path):
    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    install, state, config, skill = [Path(plan[k]).resolve() for k in ("install", "state", "config", "skill")]
    entry = contained(install / "model-router/router.py", install)
    references = {contained(Path(p), skill.parent / "references") for p in plan.get("skill_references", [])}
    state_artifacts = {contained(Path(p), state) for p in plan.get("state_artifacts", [])}
    for row in plan["rows"]:
        target = Path(row["target"]).resolve()
        if target != skill and target not in references and target not in state_artifacts:
            contained(target, install)
        if sha(target) != row["before"] or sha(Path(row["candidate"])) != row["after"]:
            raise ValueError("Runtime changed since preparation")
    if any(sha(Path(p)) != h for p, h in plan["protected"].items() if Path(p).name != "scheduling.json"):
        raise ValueError("Configuration changed since preparation")
    backup = state / "backups" / ("limits-" + VERSION + "-" + uuid.uuid4().hex[:12])
    backup.mkdir(parents=True)
    for i, row in enumerate(plan["rows"]):
        if row["before"] is not None:
            shutil.copy2(row["target"], backup / str(i))
    for i, path in enumerate(plan["protected"]):
        shutil.copy2(path, backup / ("protected-" + str(i)))
    shutil.copy2(plan_path, backup / "plan.json")
    guards = {Path(p): Path(p).read_bytes() if Path(p).is_file() else None for p in plan["guards"]}
    disabled = json.dumps({"schema_version": 1, "enabled": False}).encode()
    for i, data in enumerate(guards.values()):
        if data is not None:
            (backup / ("guard-" + str(i))).write_bytes(data)
    replaced, stopped, new_pid = [], False, None
    ready = True
    receipt = {"backup": str(backup), "version": VERSION, "installed": False}
    try:
        for path, original in guards.items():
            replace_checked(path, disabled, hashlib.sha256(original).hexdigest() if original is not None else None)
        before = counts(state)
        if before["active_jobs"]:
            raise RuntimeError("Active jobs exist; update deferred")
        receipt["history_before"] = history(state)
        source = sqlite3.connect(f"file:{(state / 'router-state.sqlite').as_posix()}?mode=ro", uri=True)
        destination = sqlite3.connect(backup / "router-state.sqlite")
        try:
            source.backup(destination)
        finally:
            destination.close()
            source.close()
        with startup_lock(state):
            health = rpc(state)
            if counts(state)["active_jobs"] or any(path.read_bytes() != disabled for path in guards):
                raise RuntimeError("Admission changed before update")
            stop_daemon(health["pid"], config, [entry])
            stopped, ready = True, False
            try:
                for i, row in enumerate(plan["rows"]):
                    deadline = time.monotonic() + 10
                    while True:
                        try:
                            replace_checked(Path(row["target"]), Path(row["candidate"]).read_bytes(), row["before"])
                            break
                        except PermissionError:
                            if time.monotonic() >= deadline:
                                raise
                            time.sleep(.2)  # the old Rust process exits when its stdin closes
                    replaced.append(i)
                health = launch(state, entry, config)
                new_pid = health["pid"]
                if (health.get("version") != VERSION or health.get("state_owner") != "rust"
                        or not health.get("scheduler_kernel_alive") or not {"output_steps_v1", "controller_work_v1", "operator_recovery_v1", "atomic_edit_bundle_v1", "settings_rebase_v1"} <= set(health.get("features", []))):
                    raise RuntimeError("Runtime identity/feature verification failed")
                after = counts(state)
                receipt["history_after"] = history(state)
                if before != after or receipt["history_before"] != receipt["history_after"]:
                    raise RuntimeError("Historical records changed")
                if any(sha(Path(p)) != h for p, h in plan["protected"].items() if Path(p).name != "scheduling.json"):
                    raise RuntimeError("Protected configuration changed")
                if any(sha(Path(row["target"])) != row["after"] for row in plan["rows"] if Path(row["target"]).resolve() not in state_artifacts):
                    raise RuntimeError("Installed bytes do not match candidate")
                scheduling_unchanged = all(sha(Path(p)) == h for p, h in plan["protected"].items() if Path(p).name == "scheduling.json")
                if not scheduling_unchanged:
                    validate_settings(rpc(state, "router_scheduling", {"action": "get"})["settings"])
                ready = True
                receipt.update(installed=True, health=health, before=before, after=after,
                    configuration_unchanged=scheduling_unchanged, concurrent_scheduling_change_preserved=not scheduling_unchanged)
            except Exception:
                if new_pid:
                    stop_daemon(new_pid, config, [entry])
                restore_files(plan["rows"], backup, replaced, mutable_targets=state_artifacts)
                rollback = launch(state, entry, config)
                ready = rollback.get("version") == plan["health_before"]["version"] and rollback.get("scheduler_kernel_alive") is True
                receipt["rollback_health"] = rollback
                raise
    except Exception as error:
        receipt["error"] = type(error).__name__ + ": " + str(error)
        receipt["old_service_stopped"] = stopped
        raise
    finally:
        if ready:
            for path, original in guards.items():
                if path.is_file() and path.read_bytes() == disabled:
                    if original is None:
                        path.unlink()  # only this update's temporary guard
                    else:
                        atomic(path, original)
        receipt["admission_restored"] = ready
        (backup / "activation.json").write_text(json.dumps(receipt, indent=2), encoding="utf-8")
        plan_path.with_name("activation.json").write_text(json.dumps(receipt, indent=2), encoding="utf-8")
    interface_enabled = all(value is None or json.loads(value.decode("utf-8-sig"))["enabled"] for value in guards.values())
    view = rpc(state, "router_scheduling", {"action": "refresh" if interface_enabled else "get"})
    receipt["route_count"] = len(view["routes"])
    receipt["settings_revision"] = view["settings"]["revision"]
    (backup / "activation.json").write_text(json.dumps(receipt, indent=2), encoding="utf-8")
    plan_path.with_name("activation.json").write_text(json.dumps(receipt, indent=2), encoding="utf-8")
    print(json.dumps(receipt, ensure_ascii=False))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--settings-baseline", type=Path, action="append", default=[])
    arguments = parser.parse_args()
    arguments.plan = arguments.plan.resolve()
    arguments.plan.parent.mkdir(parents=True, exist_ok=True)
    activate(arguments.plan) if arguments.apply else prepare(arguments.plan, arguments.settings_baseline)
