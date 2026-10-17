"""Read-only, bounded projection of the GTT ledger. No raw_json or prompts."""

import hashlib
import re
from contextlib import closing
from pathlib import Path
import sqlite3

from .models import now_ms, number, redact

ALLOWANCE_WINDOWS = {"5h_block", "weekly", "monthly", "daily", "api_pool", "auto_pool"}


def identifier(value):
    return (isinstance(value, str) and 0 < len(value) <= 128 and "://" not in value
            and not value.startswith(("/", "\\", "ghp_", "github_pat_", "AIza")) and ".." not in value
            and re.match(r"^[A-Za-z]:[/\\]", value) is None
            and redact(value) == value and all(c.isalnum() or c in "_-./: ()" for c in value))


def account_key(value):
    return hashlib.sha256(str(value).encode()).hexdigest()[:16] if value is not None else None


def snapshot(path, lookback_days=14, quota_ttl_s=300):
    path = Path(path).expanduser().resolve()
    if not path.is_file():
        return {"available": False, "reason": "ledger_missing", "sources": [], "models": [], "quotas": []}
    try:
        with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=2)) as conn:
            conn.row_factory = sqlite3.Row
            conn.execute("PRAGMA query_only=ON")
            conn.execute("PRAGMA trusted_schema=OFF")
            conn.execute("BEGIN")
            captured = now_ms()
            deadline = now_ms() + 3000
            conn.set_progress_handler(lambda: int(now_ms() > deadline), 1000)
            required = {"usage_events", "quota_snapshots", "sources"}
            tables = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            if not required <= tables:
                return {"available": False, "reason": "ledger_schema_mismatch", "sources": [], "models": [], "quotas": []}
            sources = [dict(r) for r in conn.execute(
                "SELECT source,enabled,last_synced_at,files_seen,rows_ingested FROM sources ORDER BY source LIMIT 128")]
            models = [dict(r) for r in conn.execute(
                "SELECT app,provider_id,model,COUNT(*) AS samples,MAX(ts_start) AS last_seen,"
                "AVG(CASE WHEN duration_ms>0 THEN duration_ms END) AS mean_duration_ms,"
                "AVG(CASE WHEN ttft_ms>0 THEN ttft_ms END) AS mean_ttft_ms "
                "FROM usage_events WHERE ts_start>=? AND model IS NOT NULL "
                "GROUP BY app,provider_id,model ORDER BY samples DESC LIMIT 512",
                (captured - int(lookback_days * 86400000),))]
            sources = [s for s in sources if identifier(s["source"])]
            clean_models = []
            for m in models:
                if identifier(m["app"]) and identifier(m["model"]) and 0 < m["last_seen"] <= captured:
                    if m["provider_id"] is not None and not identifier(m["provider_id"]):
                        m["provider_id"] = account_key(m["provider_id"])
                    clean_models.append(m)
            models = clean_models
            rows = conn.execute(
                "SELECT s.app,s.account,s.captured_at,s.window_kind,s.used,s.limit_value,s.used_percent,s.resets_at "
                "FROM (SELECT DISTINCT app,account,window_kind FROM quota_snapshots) k "
                "JOIN quota_snapshots s ON s.id=(SELECT id FROM quota_snapshots WHERE app=k.app "
                "AND account IS k.account AND window_kind=k.window_kind ORDER BY captured_at DESC,id DESC LIMIT 1) "
                "ORDER BY s.app,s.window_kind LIMIT 256")
            quotas = []
            for row in rows:
                q = dict(row)
                if not identifier(q["app"]) or not identifier(q["window_kind"]):
                    continue
                q["account_key"] = account_key(q.pop("account"))
                q["stale"] = q["captured_at"] <= 0 or captured - q["captured_at"] > quota_ttl_s * 1000
                q["reset_passed"] = q["resets_at"] is not None and q["resets_at"] <= captured
                pct = number(q["used_percent"])
                q["remaining_percent"] = 100 - pct if pct is not None and 0 <= pct <= 100 and q["captured_at"] <= captured and q["window_kind"] in ALLOWANCE_WINDOWS and not q["stale"] and not q["reset_passed"] else None
                q["scope"] = "app_account_window"
                quotas.append(q)
            return {"available": True, "captured_at": captured, "quota_ttl_s": quota_ttl_s, "sources": sources, "models": models, "quotas": quotas,
                    "limits": ["Historical observations do not prove current generation availability",
                               "Quota is unknown for sources without a fresh, matching allowance window",
                               "Historical duration is not normalized for task size or effort"]}
    except (sqlite3.Error, OSError, TypeError, ValueError, OverflowError):
        return {"available": False, "reason": "ledger_unavailable", "sources": [], "models": [], "quotas": []}


def quota_for(route, ledger):
    matching = [q for q in ledger.get("quotas", []) if q["app"] == route.quota_app]
    if route.quota_account is not None:
        matching = [q for q in matching if q["account_key"] == route.quota_account]
    else:
        # Never transfer an arbitrary account's quota to the active CLI account.
        matching = [q for q in matching if q["account_key"] is None]
    now = now_ms()
    checked = []
    for quota in matching:
        q = dict(quota)
        timestamp = number(q.get("captured_at"))
        reset = number(q.get("resets_at"))
        # Production snapshots carry capture metadata; recheck age at each
        # routing decision so the catalog's minute cache cannot extend quota TTL.
        if ledger.get("captured_at") is not None and (timestamp is None or timestamp <= 0 or timestamp > now
                or now - timestamp > ledger.get("quota_ttl_s", 300) * 1000 or reset is not None and reset <= now):
            q["remaining_percent"] = None
        checked.append(q)
    matching = checked
    values = [q["remaining_percent"] for q in matching if number(q.get("remaining_percent")) is not None and 0 <= q["remaining_percent"] <= 100]
    return {"known": bool(values), "remaining_percent": min(values) if values else None,
            "scope": "app_account_window", "windows": matching}
