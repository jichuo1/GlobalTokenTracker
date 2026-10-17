"""Narrow API shared by MCP and the local resident coordinator."""

from .benchmarks import refresh_aa
import math
from .models import reject_secrets


def _group_args(params, allowed, required):
    unknown = set(params) - set(allowed)
    missing = set(required) - set(params)
    if unknown:
        raise ValueError("Unknown group arguments: " + ", ".join(sorted(str(k) for k in unknown)))
    if missing:
        raise ValueError("Missing group arguments: " + ", ".join(sorted(missing)))
    return params


def _group_id(params):
    value = params.get("group_id")
    if not isinstance(value, str) or not value or len(value) > 200:
        raise ValueError("group_id must be a nonempty bounded string")


def _whole_number(value, name, low, high):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{name} must be an integer")
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError(f"{name} must be finite")
    if value < low or value > high or int(value) != value:
        raise ValueError(f"{name} must be an integer between {low} and {high}")
    return int(value)


def _approved(params):
    if params.get("approved") is not True:
        raise ValueError("approved must be explicitly true")


def _reason(params):
    reason = params.get("reason")
    if not isinstance(reason, str) or not reason.strip() or len(reason) > 2000:
        raise ValueError("reason must be a nonempty string of at most 2000 characters")
    reject_secrets(reason)


def dispatch(service, method, params):
    service.require_external_interface()
    params = params or {}
    if not isinstance(params, dict):
        raise ValueError("Arguments must be an object")
    if method == "router_inventory":
        return service.inventory(**params)
    if method == "router_plan":
        return service.plan(params["task"])
    if method == "router_recheck":
        if "route_id" not in params or set(params) - {"route_id", "workflow_id"}:
            raise ValueError("router_recheck requires route_id and optional workflow_id")
        return service.recovery.recheck(params["route_id"], params.get("workflow_id", "default"))
    if method == "router_preferences":
        _group_args(params, {"action", "scope_id", "preferences", "ttl_s"}, {"action"})
        return service.preferences(**params)
    if method == "router_submit":
        return service.submit(params["task"])
    if method == "router_status":
        return service.status(**params)
    if method == "router_collect":
        return service.collect(**params)
    if method == "router_cancel":
        return service.cancel(**params)
    if method == "router_feedback":
        return service.feedback(**params)
    if method == "router_sync_codex_agents":
        return service.sync_codex_agents(**params)
    if method == "router_complete_codex":
        return service.complete_codex(**params)
    if method == "router_events":
        _group_args(params, {"workflow_id", "cursor", "limit", "group_id", "role"}, {"workflow_id"})
        return service.coordination.events(**params)
    if method == "router_message":
        _group_args(params, {"workflow_id", "to_group", "to_role", "kind", "summary", "evidence_refs", "from_group", "event_id", "ttl_s"},
                    {"workflow_id", "to_role", "kind", "summary"})
        return service.coordination.message(**params)
    if method == "router_supervisor":
        _group_args(params, {"action", "workflow_id", "group_ids", "actor_state", "proposal", "event_id"}, {"action", "workflow_id"})
        return service.coordination.supervisor(**params)
    if method == "router_group_merge":
        _group_args(params, {"group_ids", "approved", "reason", "expected_revisions", "token_budget"}, {"group_ids", "approved", "reason", "expected_revisions"})
        _approved(params)
        _reason(params)
        if "token_budget" in params:
            params["token_budget"] = _whole_number(params["token_budget"], "token_budget", 1000, 100000000)
        return service.groups.merge(**params)
    if method == "router_refresh_benchmarks":
        result = refresh_aa(service.config["benchmark_file"], service.config.get("benchmark_aliases", {}))
        if result.get("updated"):
            service.catalog.refresh()
        return result
    group_methods = {
        "router_group_plan": ("plan", {"request"}, {"request"}),
        "router_group_submit": ("submit", {"request"}, {"request"}),
        "router_group_status": ("status", {"group_id"}, {"group_id"}),
        "router_group_collect": ("collect", {"group_id", "cursor", "max_chars", "wait_ms"}, {"group_id"}),
        "router_group_cancel": ("cancel", {"group_id"}, {"group_id"}),
        "router_group_extend": ("extend", {"group_id", "reason", "token_budget", "approved"}, {"group_id", "reason", "approved"}),
        "router_group_adjust_seats": ("adjust_seats", {"group_id", "member_count", "reason", "approved"}, {"group_id", "member_count", "reason", "approved"}),
        "router_group_resume": ("resume", {"group_id", "reason", "deadline_s", "approved_deadline", "retry_unknown_slices"}, {"group_id", "reason"}),
        "router_group_complete": ("complete", {"group_id", "accepted", "note"}, {"group_id", "accepted"}),
    }
    spec = group_methods.get(method)
    if spec:
        target, allowed, required = spec
        _group_args(params, allowed, required)
        if target in {"plan", "submit"}:
            if not isinstance(params["request"], dict):
                raise ValueError("request must be an object")
            return getattr(service.groups, target)(params["request"])
        _group_id(params)
        if target == "collect":
            params["cursor"] = _whole_number(params.get("cursor", 0), "cursor", 0, 2**63 - 1)
            params["max_chars"] = _whole_number(params.get("max_chars", 12000), "max_chars", 2000, 30000)
            params["wait_ms"] = _whole_number(params.get("wait_ms", 0), "wait_ms", 0, 5000)
        elif target == "extend":
            _approved(params)
            _reason(params)
            if "token_budget" in params:
                params["token_budget"] = _whole_number(params["token_budget"], "token_budget", 1000, 100000000)
        elif target == "adjust_seats":
            _approved(params)
            _reason(params)
            params["member_count"] = _whole_number(params["member_count"], "member_count", 3, 32)
        elif target == "resume":
            _reason(params)
            if params.get("deadline_s") is not None:
                params["deadline_s"] = _whole_number(params["deadline_s"], "deadline_s", 5, 604800)
            if "approved_deadline" in params and not isinstance(params["approved_deadline"], bool):
                raise ValueError("approved_deadline must be a boolean")
            if "retry_unknown_slices" in params and not isinstance(params["retry_unknown_slices"], bool):
                raise ValueError("retry_unknown_slices must be a boolean")
        elif target == "complete":
            if not isinstance(params["accepted"], bool):
                raise ValueError("accepted must be a boolean")
            note = params.get("note", "")
            if not isinstance(note, str) or len(note) > 2000:
                raise ValueError("note must be a string of at most 2000 characters")
            reject_secrets(note)
        return getattr(service.groups, target)(**params)
    raise ValueError("Unknown coordinator method")
