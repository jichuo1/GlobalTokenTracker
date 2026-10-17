"""Version-negotiated GTT CLI observations with a read-only legacy fallback."""

import json
import os
from pathlib import Path
import re
import subprocess
import threading

from .bridge import ALLOWANCE_WINDOWS, snapshot, identifier
from .models import now_ms, number
from .processes import background_options

MAX_OUTPUT_BYTES = 524288


def integer(value, minimum=0):
    return isinstance(value, int) and not isinstance(value, bool) and minimum <= value <= 2**63 - 1


def nonnegative(value):
    n = number(value)
    return n if n is not None and n >= 0 else None


def interface_command(settings):
    command = settings.get("command")
    if command is not None:
        if (not isinstance(command, list) or not 1 <= len(command) <= 16
                or any(not isinstance(arg, str) or len(arg) > 4096 or "\0" in arg for arg in command)
                or not Path(command[0]).is_absolute()):
            raise ValueError("Invalid gtt_interface.command")
        return command
    local = os.environ.get("LOCALAPPDATA")
    if local:
        installed = Path(local) / "Programs" / "GlobalTokenTracker" / "globaltokentracker-cli.exe"
        if installed.is_file():
            return [str(installed)]
    return None


def bounded_call(command, timeout_s):
    """Bound memory/output, kill and reap timed-out CLI; never invoke a shell."""
    with subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                          stderr=subprocess.DEVNULL, **background_options()) as process:
        output = []
        def read():
            try:
                data = process.stdout.read(MAX_OUTPUT_BYTES + 1)
                output.append(data)
                if len(data) > MAX_OUTPUT_BYTES:
                    process.kill()
            except (OSError, ValueError):
                output.append(b"")
        reader = threading.Thread(target=read, daemon=True)
        reader.start()
        try:
            process.wait(timeout=timeout_s)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            reader.join(timeout=1)
            raise ValueError("gtt_interface_timeout") from None
        reader.join(timeout=1)
        if reader.is_alive():
            raise ValueError("gtt_interface_output_incomplete")
        data = output[0] if output else b""
        if len(data) > MAX_OUTPUT_BYTES:
            raise ValueError("gtt_interface_output_limit")
        if process.returncode != 0:
            raise ValueError("gtt_interface_unsupported_or_failed")
        return data


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("gtt_interface_duplicate_key")
        result[key] = value
    return result


def _nonfinite(value):
    raise ValueError("gtt_interface_nonfinite_json")


def validate_snapshot(data, quota_ttl_s=300):
    body = json.loads(data, object_pairs_hook=_object, parse_constant=_nonfinite)
    if (not isinstance(body, dict) or body.get("protocol") != "gtt.coordination"
            or body.get("operation") != "snapshot"):
        raise ValueError("gtt_interface_invalid_envelope")
    if type(body.get("protocol_version")) is not int or body["protocol_version"] != 1:
        raise ValueError("gtt_interface_version_mismatch")
    captured = body.get("captured_at")
    now = now_ms()
    if not integer(captured, 1) or not -5000 <= now - captured <= 30000:
        raise ValueError("gtt_interface_snapshot_expired")
    status = body.get("status")
    if status not in {"ok", "partial", "unavailable"}:
        raise ValueError("gtt_interface_invalid_status")
    origin = {"transport": "gtt_cli_v1", "protocol_version": 1, "status": status}
    if status == "unavailable":
        code = body.get("error", {}).get("code") if isinstance(body.get("error"), dict) else None
        reason = code if isinstance(code, str) and re.fullmatch(r"[a-z0-9_]{1,64}", code) else "ledger_unavailable"
        return {"available": False, "reason": reason, "captured_at": captured,
                "sources": [], "models": [], "quotas": [], "origin": origin}
    if not isinstance(body.get("observation"), dict) or body["observation"].get("consistent") is not True:
        raise ValueError("gtt_interface_inconsistent_snapshot")
    for key, limit in [("sources", 128), ("models", 512), ("quotas", 256)]:
        if not isinstance(body.get(key), list) or len(body[key]) > limit:
            raise ValueError("gtt_interface_invalid_section")
    result = {"available": True, "captured_at": captured, "quota_ttl_s": quota_ttl_s, "sources": [], "models": [], "quotas": [], "origin": origin}
    for row in body["sources"]:
        if (not isinstance(row, dict) or not identifier(row.get("source"))
                or type(row.get("enabled")) is not bool
                or any(not integer(row.get(k)) for k in ["files_seen", "rows_ingested"])):
            raise ValueError("gtt_interface_invalid_source")
        last = row.get("last_synced_at")
        result["sources"].append({"source": row["source"], "enabled": row["enabled"],
                                  "last_synced_at": last if integer(last, 1) and last <= now else None,
                                  "files_seen": row["files_seen"], "rows_ingested": row["rows_ingested"],
                                  "has_error": row.get("has_error") is True})
    for row in body["models"]:
        if (not isinstance(row, dict) or not identifier(row.get("app")) or not identifier(row.get("model"))
                or (row.get("provider_id") is not None and not identifier(row["provider_id"]))
                or not integer(row.get("samples"), 1) or not integer(row.get("last_seen"), 1)):
            raise ValueError("gtt_interface_invalid_model")
        if row["last_seen"] > now:
            continue
        result["models"].append({k: row.get(k) for k in ["app", "provider_id", "model", "samples", "last_seen"]} | {
            "mean_duration_ms": nonnegative(row.get("mean_duration_ms")), "mean_ttft_ms": nonnegative(row.get("mean_ttft_ms"))})
    for row in body["quotas"]:
        if not isinstance(row, dict) or not identifier(row.get("app")) or not identifier(row.get("window_kind")):
            raise ValueError("gtt_interface_invalid_quota")
        account = row.get("account_key")
        if account is not None and (not isinstance(account, str) or not re.fullmatch(r"[0-9a-f]{16}", account)):
            raise ValueError("gtt_interface_invalid_account")
        timestamp, reset = row.get("captured_at"), row.get("resets_at")
        if not integer(timestamp) or (reset is not None and not integer(reset)) or row.get("scope") != "app_account_window":
            raise ValueError("gtt_interface_invalid_quota_time_or_scope")
        pct = nonnegative(row.get("used_percent"))
        pct = pct if pct is not None and pct <= 100 else None
        stale = timestamp <= 0 or now - timestamp > quota_ttl_s * 1000
        reset_passed = reset is not None and reset <= now
        allowance = row["window_kind"] in ALLOWANCE_WINDOWS and row.get("value_semantics") == "allowance_percent"
        remaining = (100 - pct if allowance and row.get("state") == "fresh" and pct is not None
                     and timestamp <= now and not stale and not reset_passed else None)
        result["quotas"].append({"app": row["app"], "account_key": account, "captured_at": timestamp,
                                  "window_kind": row["window_kind"], "used": nonnegative(row.get("used")),
                                  "limit_value": nonnegative(row.get("limit_value")), "used_percent": pct, "resets_at": reset,
                                  "stale": stale, "reset_passed": reset_passed, "remaining_percent": remaining,
                                  "value_semantics": row.get("value_semantics") if row.get("value_semantics") in {
                                      "allowance_percent", "remaining_balance", "context_usage", "unknown"} else "unknown",
                                  "scope": "app_account_window"})
    result["warnings"] = [w for w in body.get("warnings", [])[:32] if isinstance(w, str) and re.fullmatch(r"[a-z0-9_]{1,64}", w)] if isinstance(body.get("warnings"), list) else []
    result["truncated"] = [v for v in body.get("truncated", []) if isinstance(v, str) and v in {"sources", "models", "quotas"}] if isinstance(body.get("truncated"), list) else []
    return result


def load_observation(config):
    from .controls import control_paths, require_interface
    require_interface(control_paths(config))
    settings = config.get("gtt_interface", {})
    ttl = config.get("limits", {}).get("quota_ttl_s", 300)
    command = interface_command(settings)
    reason = "gtt_cli_missing"
    if settings.get("enabled", True) and command:
        try:
            args = command + ["--db", config["ledger_path"], "coordination", "snapshot", "--format", "json",
                              "--protocol-version", "1", "--quota-ttl-secs", str(int(ttl))]
            data = bounded_call(args, settings.get("timeout_s", 5))
            return validate_snapshot(data, ttl)
        except (OSError, ValueError) as error:
            reason = str(error) if isinstance(error, ValueError) and str(error).startswith("gtt_interface_") else "gtt_interface_transport_failed"
    elif not settings.get("enabled", True):
        reason = "gtt_interface_disabled"
    if not settings.get("allow_legacy_fallback", True):
        return {"available": False, "reason": reason, "sources": [], "models": [], "quotas": [],
                "origin": {"transport": "none", "status": "unavailable"}}
    result = snapshot(config["ledger_path"], quota_ttl_s=ttl)
    result["origin"] = {"transport": "legacy_sqlite", "status": "ok" if result["available"] else "unavailable", "fallback_reason": reason}
    return result
