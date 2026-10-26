"""Read-only SQLite backup, then migrate ONLY that isolated copy with Rust."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import sqlite3
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from gtt_router.models import now_ms
from gtt_router.service import RouterService


def fingerprint(conn, query):
    digest = hashlib.sha256()
    count = 0
    for row in conn.execute(query):
        digest.update(json.dumps(tuple(row), ensure_ascii=False, separators=(",", ":")).encode())
        digest.update(b"\n")
        count += 1
    return {"rows": count, "sha256": digest.hexdigest()}


def capture(path):
    conn = sqlite3.connect(f"file:{path.as_posix()}?mode=ro", uri=True)
    try:
        conn.execute("BEGIN")
        return {"job_content": fingerprint(conn, "SELECT id,task,detail FROM jobs ORDER BY id"),
            "job_states": dict(conn.execute("SELECT state,COUNT(*) FROM jobs GROUP BY state")),
            "usage": fingerprint(conn, "SELECT record_key,payload FROM usage_records ORDER BY record_key"),
            "operations": fingerprint(conn, "SELECT identity,state,payload FROM operator_operations ORDER BY identity"),
            "dispatches": fingerprint(conn, "SELECT id,payload FROM dispatches ORDER BY id"),
            "run_states": dict(conn.execute("SELECT json_extract(payload,'$.state'),COUNT(*) FROM visual_runs GROUP BY json_extract(payload,'$.state')"))}
    finally:
        conn.close()


class EmptyCatalog:
    routes = []
    ledger = {}
    refreshed_at = now_ms()
    def refresh(self, force=False):
        return self.public()
    def public(self):
        return {"routes": [], "ledger": {}, "discovery_errors": [], "refreshed_at": self.refreshed_at}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True)
    parser.add_argument("--destination", required=True)
    parser.add_argument("--kernel", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    source, root = Path(args.source).resolve(), Path(args.destination).resolve()
    if not source.is_file() or root.exists() or root == source.parent:
        parser.error("use an existing source and a new isolated destination")
    root.mkdir(parents=True)
    target = root / "router-state.sqlite"
    reader = sqlite3.connect(f"file:{source.as_posix()}?mode=ro", uri=True)
    writer = sqlite3.connect(target)
    try:
        reader.backup(writer)
    finally:
        writer.close()
        reader.close()
    settings = source.parent / "scheduling.json"
    if settings.is_file():
        shutil.copy2(settings, root / settings.name)
    before = capture(target)
    service = RouterService({"state_dir": str(root), "scheduler_kernel": {"backend": "rust", "executable": str(Path(args.kernel).resolve())},
        "recovery": {"enabled": False}}, EmptyCatalog(), {}, interface_paths=[root / "switch.json"])
    try:
        health = service.scheduler_kernel.call("health", {})
        assert health["state_open"] and service.store.native
    finally:
        service.shutdown()
    after = capture(target)
    preserved = {key: before[key] == after[key] for key in ("job_content", "usage", "operations", "dispatches")}
    report = {"source_read_only": str(source), "isolated_copy": str(target), "native_generations": 0,
        "before": before, "after": after, "preserved": preserved, "state_owner": "rust"}
    Path(args.output).write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    if not all(preserved.values()):
        raise SystemExit("Inspect migration differences before deployment")


if __name__ == "__main__":
    main()
