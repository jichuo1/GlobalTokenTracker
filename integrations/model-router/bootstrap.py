"""Prepare user-local runtime configuration; do not edit IDE credentials or PATH."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import sys
import uuid

from gtt_router.daemon import secure_write


def bootstrap(state_dir, agy_path, expected_sha512=None):
    state_dir = Path(state_dir).expanduser().resolve()
    config_path = state_dir / "config.json"
    if config_path.exists():
        raise ValueError("Existing coordinator configuration is preserved; inspect and update it explicitly")
    agy_path = Path(agy_path).expanduser().resolve()
    if not agy_path.is_file():
        raise ValueError("Antigravity CLI executable is missing")
    if expected_sha512 and hashlib.sha512(agy_path.read_bytes()).hexdigest() != expected_sha512.lower():
        raise ValueError("Antigravity CLI does not match the expected official manifest digest")
    node = shutil.which("node")
    if not node:
        raise ValueError("Node.js runtime is required for the installed ZCode CLI")
    zroot = Path.home() / "AppData" / "Local" / "Programs" / "ZCode" / "resources"
    zcli = zroot / "glm" / "zcode.cjs"
    builtin = zroot / "config" / "provider" / "zcode-builtin.json"
    personal = Path.home() / ".zcode" / "v2" / "provider_config.json"
    if not all(p.is_file() for p in [zcli, builtin, personal]):
        raise ValueError("Installed ZCode runtime/configuration was not found; configure its adapter explicitly")
    (state_dir / "bin").mkdir(parents=True, exist_ok=True)
    agy = state_dir / "bin" / agy_path.name
    if agy != agy_path:
        shutil.copy2(agy_path, agy)
    benchmarks = state_dir / "benchmarks.json"
    if not benchmarks.exists():
        shutil.copy2(Path(__file__).with_name("benchmark-seed.json"), benchmarks)
    ledger = Path.home() / ".globaltokentracker" / "ledger.db"
    project_id = str(uuid.uuid4())
    project_path = Path.home() / ".gemini" / "config" / "projects" / (project_id + ".json")
    cache = str(Path.home() / ".gemini" / "antigravity-cli" / "brain").replace("\\", "/")
    secure_write(project_path, {
        "id": project_id, "name": "GTT 协同辅助", "projectResources": {}, "settings": {},
        "permissionGrants": {"permissionGrants": {
            "allow": ["read_url(*)", "read_file(" + cache + ")"],
            "deny": ["command(*)", "write_file(*)", "execute_url(*)", "mcp(*)"], "ask": []}, "v2Migrated": True}})
    config = {
        "schema_version": 1, "state_dir": str(state_dir), "ledger_path": str(ledger), "benchmark_file": str(benchmarks),
        "allow_codex_fallback": True, "codex_fallback_model": "gpt-6-luna",
        "gtt_interface": {"enabled": True, "timeout_s": 5, "allow_legacy_fallback": True},
        "recovery": {"enabled": True, "poll_s": 60, "initial_delay_s": 1800,
                     "max_backoff_s": 21600, "probe_timeout_s": 30,
                     "max_probes_per_hour": 2, "max_tokens_per_hour": 24000},
        "limits": {"global_parallel": 4, "combined_parallel": 6, "max_active_jobs": 4, "max_queued_jobs": 16,
                   "source_parallel": {"zcode": 2, "antigravity": 2}, "quota_ttl_s": 300,
                   "external_queue_grace_s": 3, "max_context_chars": 64000},
        "sources": [
            {"id": "zcode", "adapter": "zcode_rpc", "enabled": True, "command": [str(Path(node).resolve()), str(zcli), "app-server"],
             "provider_config": str(personal), "builtin_config": str(builtin), "max_parallel": 2},
            {"id": "antigravity", "adapter": "antigravity_cli", "enabled": True, "command": [str(agy)], "max_parallel": 2, "project_id": project_id},
        ],
        "profile_overrides": {},
    }
    secure_write(config_path, config)
    return {"config_path": str(config_path), "source_count": len(config["sources"]), "third_party_python_dependencies": 0,
            "native_credentials_copied": False, "ide_settings_changed": False}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--state-dir", default=str(Path.home() / ".globaltokentracker" / "router"))
    parser.add_argument("--antigravity-cli", required=True)
    parser.add_argument("--antigravity-sha512")
    arguments = parser.parse_args()
    try:
        print(json.dumps(bootstrap(arguments.state_dir, arguments.antigravity_cli, arguments.antigravity_sha512), ensure_ascii=False))
    except (ValueError, OSError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
