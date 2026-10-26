"""Prepare a reviewable Windows package; apply only after maintenance approval.

No model generation is performed. Shared files are hash-checked and backed up.
An admission guard prevents new work while an idle coordinator is replaced.
"""
import argparse
import ctypes
from ctypes import wintypes
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request
import uuid

PRODUCT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PRODUCT))
from gtt_router.processes import background_options


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None


def contained(path, root):
    value, boundary = path.resolve(), root.resolve()
    if not value.is_relative_to(boundary):
        raise ValueError("Target is outside the reviewed directory")
    return value


def atomic(path, data):
    descriptor, temporary = tempfile.mkstemp(prefix=".gtt-update-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)  # only this function's exclusive temporary file


def replace_checked(path, data, expected):
    if sha(path) != expected:
        raise ValueError("Shared file changed before replacement")
    atomic(path, data)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def rpc(state, method=None, params=None):
    endpoint = json.loads((state / "daemon-endpoint.json").read_text(encoding="utf-8-sig"))
    port, token = endpoint["port"], endpoint["token"]
    if type(port) is not int or not 1024 <= port <= 65535 or not re.fullmatch(r"[A-Za-z0-9_-]{16,512}", token):
        raise ValueError("Invalid local endpoint")
    data = json.dumps({"method": method, "params": params or {}}).encode() if method else None
    request = urllib.request.Request(f"http://127.0.0.1:{port}/" + ("manage" if method else "health"),
        data=data, headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
    with urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect()).open(request, timeout=120 if params and params.get("action") == "refresh" else 10) as reply:
        value = json.load(reply)
    if value.get("error"):
        raise RuntimeError("Coordinator management request failed")
    return value.get("result", value)


def counts(state):
    path = state / "router-state.sqlite"
    connection = sqlite3.connect(f"file:{path.as_posix()}?mode=ro", uri=True, timeout=3)
    try:
        connection.execute("BEGIN")
        states = dict(connection.execute("SELECT state,COUNT(*) FROM jobs GROUP BY state"))
        data = {"jobs": sum(states.values()), "job_states": states,
            "active_jobs": sum(states.get(key, 0) for key in ("queued", "running", "cancelling", "needs_codex"))}
        for table in ("dispatches", "operator_operations", "visual_runs", "usage_records"):
            data[table] = connection.execute("SELECT COUNT(*) FROM " + table).fetchone()[0]
        return data
    finally:
        connection.close()


def stop_daemon(pid, config, entries):
    if type(pid) is not int or pid <= 0:
        raise ValueError("Invalid daemon PID")
    # Acquire a process handle before checking identity, then terminate that
    # exact process object. No image-name kill and no unrelated MCP clients.
    api = ctypes.WinDLL("kernel32", use_last_error=True)
    api.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    api.OpenProcess.restype = wintypes.HANDLE
    api.TerminateProcess.argtypes = [wintypes.HANDLE, wintypes.UINT]
    api.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
    api.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = api.OpenProcess(0x00100001, False, pid)
    if not handle:
        raise OSError("Cannot open coordinator process")
    try:
        command = f"$p=Get-CimInstance Win32_Process -Filter 'ProcessId={pid}'; $p | Select-Object ExecutablePath,CommandLine | ConvertTo-Json -Compress"
        result = subprocess.run(["powershell.exe", "-NoProfile", "-Command", command], capture_output=True,
            text=True, timeout=15, **background_options())
        info = json.loads(result.stdout)
        line = info.get("CommandLine", "").replace("/", "\\").casefold()
        if not re.search(r"\bdaemon\b", line) or str(config).replace("/", "\\").casefold() not in line or not any(str(p).replace("/", "\\").casefold() in line for p in entries):
            raise ValueError("Coordinator process identity changed")
        if not api.TerminateProcess(handle, 0) or api.WaitForSingleObject(handle, 10000) != 0:
            raise OSError("Coordinator did not terminate")
    finally:
        api.CloseHandle(handle)


def rewrite_mcp(data, entry):
    text = data.decode("utf-8-sig")
    before = tomllib.loads(text)
    target = before["mcp_servers"]["gtt-router"]
    args = list(target["args"])
    positions = [i for i, value in enumerate(args) if isinstance(value, str) and value.lower().endswith("router.py")]
    if len(positions) != 1:
        raise ValueError("Ambiguous MCP entry point")
    args[positions[0]] = str(entry)
    section = re.search(r'(?ms)^\[mcp_servers\.(?:gtt-router|"gtt-router")\]\s*\n(.*?)(?=^\[|\Z)', text)
    if not section:
        raise ValueError("MCP section not found")
    body, replaced = re.subn(r'(?m)^args\s*=\s*\[[^\n]*\]', lambda _: "args = " + json.dumps(args, ensure_ascii=False), section[1], count=1)
    if replaced != 1:
        raise ValueError("MCP arguments need an explicit reviewed edit")
    updated = text[:section.start(1)] + body + text[section.end(1):]
    after = tomllib.loads(updated)
    expected = json.loads(json.dumps(before, default=str))
    expected["mcp_servers"]["gtt-router"]["args"] = args
    if json.loads(json.dumps(after, default=str)) != expected:
        raise ValueError("Unexpected MCP configuration change")
    return (b"\xef\xbb\xbf" if data.startswith(b"\xef\xbb\xbf") else b"") + updated.encode()


def prepare(plan_path):
    home = Path.home().resolve()
    install = (home / "AppData/Local/Programs/GlobalTokenTracker").resolve()
    state = (home / ".globaltokentracker/router").resolve()
    config, mcp = state / "config.json", home / ".codex/config.toml"
    skill = (home / ".codex/skills/gtt-coordinator").resolve()
    package = plan_path.parent / "install-candidate"
    if package.exists() or not install.is_dir() or not config.is_file() or not mcp.is_file():
        raise ValueError("Use a new package directory and the existing installation")
    candidate = package / "model-router"
    candidate.mkdir(parents=True)
    files = [PRODUCT / "router.py", PRODUCT / "workflow-templates.json", PRODUCT / "benchmark-seed.json"]
    files += list((PRODUCT / "gtt_router").glob("*.py"))
    files += [PRODUCT / "scripts/owned_process_worker.py", PRODUCT / "scripts/hidden_console_worker.py"]
    for source in files:
        if source.is_symlink():
            raise ValueError("Runtime source must be an ordinary file")
        target = contained(candidate / source.relative_to(PRODUCT), package)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
    executable = PRODUCT.parents[1] / "target/release/gtt-scheduler-kernel.exe"
    shutil.copy2(executable, package / executable.name)
    skill_target = package / "skill"
    (skill_target / "references").mkdir(parents=True)
    original = (skill / "SKILL.md").read_bytes()
    text = original.decode("utf-8-sig")
    newline = "\r\n" if "\r\n" in text else "\n"
    paragraph = "0.7.0 可由 Rust 接管状态数据库和调度运行记录；检查版本、处理存储故障或切换本机服务时，读取 [Rust 状态运行时](references/rust-runtime.md)。日常派发协议保持兼容，不因新后端而自动重放未知结果。"
    if "references/rust-runtime.md" not in text:
        anchor = "## 分配与 token 效率"
        if text.count(anchor) != 1:
            raise ValueError("Skill needs an explicit reviewed insertion")
        text = text.replace(anchor, paragraph + newline * 2 + anchor, 1)
    (skill_target / "SKILL.md").write_bytes((b"\xef\xbb\xbf" if original.startswith(b"\xef\xbb\xbf") else b"") + text.encode())
    shutil.copy2(PRODUCT / "skill/references/rust-runtime.md", skill_target / "references/rust-runtime.md")
    raw = json.loads(config.read_text(encoding="utf-8-sig"))
    if Path(raw["state_dir"]).resolve() != state:
        raise ValueError("Unexpected state directory")
    rewrite_mcp(mcp.read_bytes(), install / "model-router/router.py")
    guards = {home / ".globaltokentracker/external-interface.json", Path(raw["ledger_path"]).resolve().parent / "external-interface.json"}
    for path in guards:
        if path.exists():
            value = json.loads(path.read_text(encoding="utf-8-sig")) if path.is_file() else None
            if not isinstance(value, dict) or value.keys() != {"schema_version", "enabled"} or type(value["schema_version"]) is not int or value["schema_version"] != 1 or type(value["enabled"]) is not bool:
                raise ValueError("Invalid existing interface switch")
        if not path.parent.is_dir():
            raise ValueError("Interface switch directory unavailable")
    plan = {"version": "0.7.0", "package": str(package), "install": str(install), "state": str(state),
        "config": str(config), "mcp": str(mcp), "skill": str(skill), "guards": [str(p) for p in sorted(guards)],
        "expected": {str(p): sha(p) for p in [config, mcp, skill / "SKILL.md", skill / "references/rust-runtime.md", install / "gtt-scheduler-kernel.exe"]},
        "runtime_directory_exists": (install / "model-router").exists(),
        "manifest": {str(p.relative_to(package)).replace("\\", "/"): sha(p) for p in package.rglob("*") if p.is_file()},
        "current": {"health": rpc(state), "counts": counts(state)}, "production_switched": False}
    plan_path.write_text(json.dumps(plan, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"plan": str(plan_path), "files": len(plan["manifest"]), "counts": plan["current"]["counts"]}, ensure_ascii=False))


def activate(plan_path):
    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    package, install, state, config, mcp, skill = [Path(plan[k]).resolve() for k in ("package", "install", "state", "config", "mcp", "skill")]
    for name, expected in plan["manifest"].items():
        if sha(contained(package / name, package)) != expected:
            raise ValueError("Candidate package changed")
    for name, expected in plan["expected"].items():
        if sha(Path(name)) != expected:
            raise ValueError("Shared target changed since review: " + name)
    runtime = contained(install / "model-router", install)
    if runtime.exists() or plan["runtime_directory_exists"]:
        raise ValueError("This reviewed plan requires a new bundled runtime directory")
    before_config, before_mcp = config.read_bytes(), mcp.read_bytes()
    old_config = json.loads(before_config.decode("utf-8-sig"))
    old_args = tomllib.loads(before_mcp.decode("utf-8-sig"))["mcp_servers"]["gtt-router"]["args"]
    old_entry = Path(next(a for a in old_args if a.lower().endswith("router.py"))).resolve()
    backup = state / "backups" / ("rust-0.7.0-" + uuid.uuid4().hex[:12])
    backup.mkdir(parents=True)
    for path in (config, mcp, skill / "SKILL.md"):
        shutil.copy2(path, backup / ("codex-config.toml" if path == mcp else "SKILL.md" if path.name == "SKILL.md" else "config.json"))
    for name in ("scheduling.json", "daemon-endpoint.json"):
        if (state / name).is_file():
            shutil.copy2(state / name, backup / name)
    guard_bytes = json.dumps({"schema_version": 1, "enabled": False}).encode()
    originals = {Path(p): Path(p).read_bytes() if Path(p).is_file() else None for p in plan["guards"]}
    for i, (path, value) in enumerate(originals.items()):
        if value is not None:
            (backup / f"interface-{i}.json").write_bytes(value)
    receipt = {"backup": str(backup), "version": "0.7.0", "installed": False, "native_generations": 0}
    new_pid, replaced = None, {}
    old_stopped = False
    own_runtime = False
    try:
        for path in originals:
            expected = hashlib.sha256(originals[path]).hexdigest() if originals[path] is not None else None
            replace_checked(path, guard_bytes, expected)
        before = counts(state)
        if before["active_jobs"]:
            raise RuntimeError("Coordinator still has active work; update deferred")
        source = sqlite3.connect(f"file:{(state / 'router-state.sqlite').as_posix()}?mode=ro", uri=True)
        target = sqlite3.connect(backup / "router-state.sqlite")
        try:
            source.backup(target)
        finally:
            target.close()
            source.close()
        stage = contained(install / ("model-router.stage-" + uuid.uuid4().hex[:12]), install)
        shutil.copytree(package / "model-router", stage)
        stage.rename(runtime)
        own_runtime = True
        replace_checked(install / "gtt-scheduler-kernel.exe", (package / "gtt-scheduler-kernel.exe").read_bytes(), plan["expected"][str(install / "gtt-scheduler-kernel.exe")])
        updated = old_config | {"schema_version": 2, "scheduler_kernel": {"backend": "rust", "executable": str(install / "gtt-scheduler-kernel.exe"), "timeout_s": 10}}
        replace_checked(config, json.dumps(updated, ensure_ascii=False, indent=2).encode(), plan["expected"][str(config)])
        replaced[config] = sha(config)
        replace_checked(mcp, rewrite_mcp(before_mcp, runtime / "router.py"), plan["expected"][str(mcp)])
        replaced[mcp] = sha(mcp)
        replace_checked(skill / "SKILL.md", (package / "skill/SKILL.md").read_bytes(), plan["expected"][str(skill / "SKILL.md")])
        replaced[skill / "SKILL.md"] = sha(skill / "SKILL.md")
        replace_checked(skill / "references/rust-runtime.md", (package / "skill/references/rust-runtime.md").read_bytes(), plan["expected"][str(skill / "references/rust-runtime.md")])
        health = rpc(state)
        if counts(state)["active_jobs"] or any(p.read_bytes() != guard_bytes for p in originals):
            raise RuntimeError("Admission state changed during preparation")
        stop_daemon(health["pid"], config, [old_entry])
        old_stopped = True
        result = subprocess.run([sys.executable, str(runtime / "router.py"), "call", "--config", str(config), "--method", "router_scheduling"],
            capture_output=True, timeout=40, **background_options())
        if result.returncode:
            raise RuntimeError("Installed coordinator failed to start; inspect daemon.log")
        health = rpc(state)
        new_pid = health["pid"]
        if health.get("version") != "0.7.0" or health.get("state_owner") != "rust" or health.get("scheduler_kernel_alive") is not True:
            raise RuntimeError("Installed runtime identity mismatch")
        after = counts(state)
        if any(before[k] != after[k] for k in ("jobs", "usage_records", "dispatches", "operator_operations", "visual_runs")):
            raise RuntimeError("Historical row counts changed during activation")
        receipt.update(installed=True, before=before, after=after, health=health,
            kernel_sha256=sha(install / "gtt-scheduler-kernel.exe"), config_sha256=sha(config), mcp_sha256=sha(mcp))
    except Exception as error:
        receipt["error"] = type(error).__name__ + ": " + str(error)
        try:
            if new_pid:
                stop_daemon(new_pid, config, [runtime / "router.py"])
            for path, data in ((config, before_config), (mcp, before_mcp), (skill / "SKILL.md", (backup / "SKILL.md").read_bytes())):
                if path in replaced and sha(path) == replaced[path]:
                    atomic(path, data)
            if own_runtime and runtime.exists():
                failed = contained(install / ("model-router.failed-" + uuid.uuid4().hex[:8]), install)
                runtime.rename(failed)
            if old_stopped:
                subprocess.run([sys.executable, str(old_entry), "call", "--config", str(config), "--method", "router_scheduling"],
                    capture_output=True, timeout=40, **background_options())
            receipt["rollback"] = "runtime/config only; durable database retained"
        except Exception as rollback_error:
            receipt["rollback_error"] = type(rollback_error).__name__
        raise
    finally:
        for path, data in originals.items():
            if path.is_file() and path.read_bytes() == guard_bytes:
                if data is None:
                    path.unlink()  # restore absence of this task-created guard
                else:
                    atomic(path, data)
        (backup / "activation.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2), encoding="utf-8")
        plan_path.with_name("activation.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2), encoding="utf-8")
    # Explicit catalog refresh is discovery only, after restoring user switches.
    interface_enabled = all(value is None or json.loads(value.decode("utf-8-sig"))["enabled"] for value in originals.values())
    view = rpc(state, "router_scheduling", {"action": "refresh" if interface_enabled else "get"})
    receipt["route_count"] = len(view.get("routes", []))
    receipt["settings"] = {k: view["settings"][k] for k in ("revision", "enabled", "mode")}
    receipt["pool_count"] = len(view["settings"]["pool"])
    (backup / "activation.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2), encoding="utf-8")
    plan_path.with_name("activation.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(receipt, ensure_ascii=False, indent=2))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--plan", required=True)
    parser.add_argument("--apply", action="store_true", help="Requires the user's explicit maintenance approval")
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("This installation workflow is Windows-only")
    plan_path = Path(args.plan).resolve()
    plan_path.parent.mkdir(parents=True, exist_ok=True)
    activate(plan_path) if args.apply else prepare(plan_path)


if __name__ == "__main__":
    main()
