"""Strict input and leader-output contracts for bounded research groups."""

from dataclasses import asdict, dataclass
import math

from .hook_guard import public_url
from .models import reject_secrets
from .coordination import validate_communication
from .report_contract import feedback_slot


GROUP_SCHEMA = {
    "type": "object",
    "additionalProperties": False,
    "required": ["topic"],
    "properties": {
        "topic": {"type": "string", "minLength": 1},
        "context": {"type": "string"},
        "needs_web": {"type": "boolean", "default": True},
        "member_count": {"type": "integer", "minimum": 3, "maximum": 8},
        "max_rounds": {"type": "integer", "minimum": 1, "maximum": 64, "default": 12},
        "deadline_s": {"type": "integer", "minimum": 5, "maximum": 604800, "default": 21600},
        "token_budget": {"type": "integer", "minimum": 1000, "maximum": 50000000, "default": 50000000},
        "leader_min_quality": {"type": "number", "minimum": 38, "maximum": 100, "default": 38},
        "worker_min_quality": {"type": "number", "minimum": 0, "maximum": 100, "default": 0},
        "workflow_id": {"type": "string", "maxLength": 300, "default": "default"},
        "work_key": {"type": "string", "maxLength": 300, "default": ""},
        "context_label": {"type": "string", "maxLength": 300, "default": ""},
        "base_revision": {"type": "string", "maxLength": 300, "default": ""},
        "preference_scope": {"type": "string", "maxLength": 200, "default": ""},
        "price_weight": {"type": "number", "minimum": 0, "maximum": 100, "default": 30},
        "prefer_free": {"type": "boolean", "default": True},
        "prefer_new_models": {"type": "boolean", "default": True},
        "slice_mode": {"type": "string", "enum": ["adaptive", "off"], "default": "adaptive"},
        "slice_target_chars": {"type": "integer", "minimum": 1000, "maximum": 64000, "default": 12000},
        "slice_min_chars": {"type": "integer", "minimum": 500, "maximum": 64000, "default": 2000},
        "slice_max_chars": {"type": "integer", "minimum": 1000, "maximum": 128000, "default": 32000},
        "max_slices": {"type": "integer", "minimum": 2, "maximum": 256, "default": 64},
        "max_parallel": {"type": ["integer", "null"], "minimum": 1, "maximum": 32},
        "output_mode": {"type": "string", "enum": ["adaptive", "native"], "default": "adaptive"},
        "exploration_mode": {"type": "string", "enum": ["guided", "off"], "default": "guided"},
        "max_exploration_rounds": {"type": "integer", "minimum": 0, "maximum": 8, "default": 2},
        "exploration_budget_ratio": {"type": "number", "minimum": 0, "maximum": 0.3, "default": 0.15},
    },
}

LEADER_DECISIONS = {"continue_research", "submit_codex", "request_extension", "request_seat_change", "blocked", "propose_merge"}
LEADER_TASK_ROLES = {"collector", "analyst", "checker"}
LEADER_TASK_KINDS = {"research", "extract", "summarize", "code_review", "logic_check"}
_LEADER_KEYS = {"action", "rationale", "summary", "next_tasks", "uncertainties", "member_count", "replace_roles", "feedback"}
_TASK_KEYS = {"role", "kind", "prompt", "focus_urls", "needs_web", "context_range", "evidence_workers", "information_request_ids"}

LEADER_SCHEMA = {
    "type": "object", "additionalProperties": False, "required": ["action"],
    "properties": {
        "action": {"type": "string", "enum": sorted(LEADER_DECISIONS)},
        "communications": {"type": "array", "maxItems": 4, "items": {"type": "object", "additionalProperties": False,
            "required": ["summary"], "properties": {
                "to_group": {"type": "string", "maxLength": 200}, "to_role": {"type": "string", "maxLength": 100},
                "kind": {"type": "string", "enum": ["assignment", "evidence", "conflict", "progress", "suggestion", "merge_proposal"]},
                "summary": {"type": "string", "minLength": 1, "maxLength": 1200},
                "evidence_refs": {"type": "array", "maxItems": 4, "items": {"type": "object", "additionalProperties": False,
                    "required": ["group_id"], "properties": {"group_id": {"type": "string"}, "worker": {"type": "integer", "minimum": 0},
                        "message_id": {"type": "string"}, "url": {"type": "string"}, "anchor": {"type": "string", "maxLength": 300}}}},
            }}},
        "merge_groups": {"type": "array", "minItems": 2, "maxItems": 4, "uniqueItems": True, "items": {"type": "string", "minLength": 1, "maxLength": 200}},
        "rationale": {"type": "string", "maxLength": 2000},
        "exploration": {"type": "boolean"},
        "research_leads": {"type": "array", "maxItems": 4, "items": {"type": "object", "additionalProperties": False,
            "required": ["question", "rationale", "priority", "expected_value"], "properties": {
                "question": {"type": "string", "minLength": 1, "maxLength": 1000},
                "rationale": {"type": "string", "minLength": 1, "maxLength": 1000},
                "priority": {"type": "string", "enum": ["low", "normal", "high"]},
                "expected_value": {"type": "string", "minLength": 1, "maxLength": 1000},
                "evidence_refs": {"type": "array", "maxItems": 4, "items": {"type": "object"}},
            }}},
        "summary": {"type": "string", "maxLength": 5000},
        "uncertainties": {"type": "array", "maxItems": 12, "items": {"type": "string", "maxLength": 1000}},
        "member_count": {"type": "integer", "minimum": 3, "maximum": 8},
        "replace_roles": {"type": "array", "maxItems": 7, "items": {"type": "string", "minLength": 1, "maxLength": 100}},
        "feedback": {"type": "array", "maxItems": 256, "items": {
            "type": "object", "additionalProperties": False, "required": ["worker", "accepted", "reason"],
            "properties": {"worker": {"type": "integer", "minimum": 0, "maximum": 256},
                           "accepted": {"type": "boolean"}, "reason": {"type": "string", "maxLength": 1000}},
        }},
        "next_tasks": {"type": "array", "maxItems": 256, "items": {
            "type": "object", "additionalProperties": False,
            "required": ["role", "kind", "prompt"],
            "properties": {
                "role": {"type": "string", "enum": sorted(LEADER_TASK_ROLES)},
                "kind": {"type": "string", "enum": sorted(LEADER_TASK_KINDS)},
                "prompt": {"type": "string", "minLength": 1, "maxLength": 8000},
                "focus_urls": {"type": "array", "maxItems": 4, "items": {"type": "string", "maxLength": 600, "format": "uri"}},
                "needs_web": {"type": "boolean", "default": False},
                "context_range": {"type": "array", "minItems": 2, "maxItems": 2,
                                  "items": {"type": "integer", "minimum": 0}},
                "evidence_workers": {"type": "array", "maxItems": 32, "uniqueItems": True,
                                     "items": {"type": "integer", "minimum": 0}},
                "information_request_ids": {"type": "array", "maxItems": 8, "uniqueItems": True,
                                            "items": {"type": "string", "maxLength": 30}},
            },
        }},
    },
    "allOf": [
        {"if": {"properties": {"action": {"const": "continue_research"}}, "required": ["action"]},
         "then": {"not": {"required": ["member_count"]}}},
        {"if": {"properties": {"action": {"const": "request_seat_change"}}, "required": ["action"]},
         "then": {"required": ["member_count"], "not": {"anyOf": [
             {"required": ["next_tasks"]}, {"required": ["replace_roles"]}, {"required": ["feedback"]}]}}},
        {"if": {"properties": {"action": {"enum": ["submit_codex", "request_extension", "blocked"]}}, "required": ["action"]},
         "then": {"not": {"anyOf": [
             {"required": ["member_count"]}, {"required": ["next_tasks"]},
             {"required": ["replace_roles"]}, {"required": ["feedback"]}]}}},
        {"if": {"properties": {"action": {"const": "propose_merge"}}, "required": ["action"]},
         "then": {"required": ["merge_groups"], "not": {"anyOf": [
             {"required": ["member_count"]}, {"required": ["next_tasks"]}, {"required": ["replace_roles"]}, {"required": ["feedback"]}]}}},
        {"if": {"properties": {"action": {"not": {"const": "propose_merge"}}}, "required": ["action"]},
         "then": {"not": {"required": ["merge_groups"]}}},
    ],
}


def _bounded_int(value, name, low, high, default):
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{name} must be an integer")
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError(f"{name} must be finite")
    if value < low or value > high or int(value) != value:
        raise ValueError(f"{name} must be an integer between {low} and {high}")
    return int(value)


def _bounded_number(value, name, low, high, default):
    if value is None:
        return default
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{name} must be a number")
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError(f"{name} must be finite")
    if value < low or value > high:
        raise ValueError(f"{name} must be between {low} and {high}")
    return float(value)


def _bounded_text(value, name, limit, *, nonempty=False):
    if not isinstance(value, str) or len(value) > limit or (nonempty and not value.strip()):
        raise ValueError(f"{name} must be a {'nonempty ' if nonempty else ''}string of at most {limit} characters")
    return value


@dataclass
class GroupRequest:
    topic: str
    context: str = ""
    needs_web: bool = True
    member_count: int | None = None
    max_rounds: int = 12
    deadline_s: int = 21600
    token_budget: int = 50000000
    leader_min_quality: float = 38
    worker_min_quality: float = 0
    workflow_id: str = "default"
    work_key: str = ""
    context_label: str = ""
    base_revision: str = ""
    preference_scope: str = ""
    price_weight: float = 30
    prefer_free: bool = True
    prefer_new_models: bool = True
    slice_mode: str = "adaptive"
    slice_target_chars: int = 12000
    slice_min_chars: int = 2000
    slice_max_chars: int = 32000
    max_slices: int = 64
    max_parallel: int | None = None
    output_mode: str = "adaptive"
    exploration_mode: str = "guided"
    max_exploration_rounds: int = 2
    exploration_budget_ratio: float = .15
    max_output_tokens: None = None

    @classmethod
    def parse(cls, data):
        if not isinstance(data, dict):
            raise ValueError("Group request must be an object")
        fields = {"topic", "context", "needs_web", "member_count", "max_rounds", "deadline_s", "token_budget",
                  "leader_min_quality", "worker_min_quality", "workflow_id", "work_key", "context_label", "base_revision", "preference_scope",
                  "price_weight", "prefer_free", "prefer_new_models", "slice_mode", "slice_target_chars",
                  "slice_min_chars", "slice_max_chars", "max_slices", "max_parallel", "output_mode",
                  "exploration_mode", "max_exploration_rounds", "exploration_budget_ratio"}
        unknown = set(data) - fields
        if unknown:
            raise ValueError("Unknown group fields: " + ", ".join(sorted(str(k) for k in unknown)))
        topic = data.get("topic")
        if not isinstance(topic, str) or not topic.strip():
            raise ValueError("topic must be a nonempty string")
        context = data.get("context", "")
        if not isinstance(context, str):
            raise ValueError("context must be a string")
        if len(topic) > 8000 or len(context) > 2000000:
            raise ValueError("Group topic/context exceeds the bounded material limit")
        controls = {}
        for key, low, high, default in (("slice_target_chars", 1000, 64000, 12000),
                ("slice_min_chars", 500, 64000, 2000), ("slice_max_chars", 1000, 128000, 32000),
                ("max_slices", 2, 256, 64), ("max_parallel", 1, 32, None)):
            controls[key] = _bounded_int(data.get(key), key, low, high, default)
        if not controls["slice_min_chars"] <= controls["slice_target_chars"] <= controls["slice_max_chars"]:
            raise ValueError("Slice sizes must satisfy min <= target <= max")
        for key, choices, default in (("slice_mode", {"adaptive", "off"}, "adaptive"),
                                      ("output_mode", {"adaptive", "native"}, "adaptive"),
                                      ("exploration_mode", {"guided", "off"}, "guided")):
            value = data.get(key, default)
            if not isinstance(value, str) or value not in choices:
                raise ValueError(key + " has an unsupported value")
            controls[key] = value
        controls["max_exploration_rounds"] = _bounded_int(data.get("max_exploration_rounds"), "max_exploration_rounds", 0, 8, 2)
        controls["exploration_budget_ratio"] = _bounded_number(data.get("exploration_budget_ratio"), "exploration_budget_ratio", 0, .3, .15)
        reject_secrets(topic + "\n" + context)
        needs_web = data.get("needs_web", True)
        if not isinstance(needs_web, bool):
            raise ValueError("needs_web must be a boolean")
        for field in ("prefer_free", "prefer_new_models"):
            if field in data and not isinstance(data[field], bool):
                raise ValueError(field + " must be a boolean")
        member_count = data.get("member_count")
        if member_count is not None:
            member_count = _bounded_int(member_count, "member_count", 3, 8, None)
        strings = {}
        from .preferences import validate_scope
        strings["preference_scope"] = validate_scope(data.get("preference_scope", ""))
        for key, default in (("workflow_id", "default"), ("work_key", ""), ("context_label", ""), ("base_revision", "")):
            strings[key] = _bounded_text(data.get(key, default), key, 300)
        return cls(topic=topic, context=context, needs_web=needs_web, member_count=member_count,
                   max_rounds=_bounded_int(data.get("max_rounds"), "max_rounds", 1, 64, 12),
                   deadline_s=_bounded_int(data.get("deadline_s"), "deadline_s", 5, 604800, 21600),
                   token_budget=_bounded_int(data.get("token_budget"), "token_budget", 1000, 50000000, 50000000),
                   leader_min_quality=_bounded_number(data.get("leader_min_quality"), "leader_min_quality", 38, 100, 38),
                   worker_min_quality=_bounded_number(data.get("worker_min_quality"), "worker_min_quality", 0, 100, 0),
                   price_weight=_bounded_number(data.get("price_weight"), "price_weight", 0, 100, 30),
                   prefer_free=data.get("prefer_free", True), prefer_new_models=data.get("prefer_new_models", True),
                   **strings, **controls)

    def public(self):
        result = asdict(self)
        result.pop("topic")
        result.pop("context")
        result["payload_chars"] = len(self.topic) + len(self.context)
        return result


def parse_leader_decision(data, max_members=8, max_tasks=None):
    """Validate advisory leader output; this function never executes it."""
    if not isinstance(data, dict):
        raise ValueError("Leader decision must be an object")
    action = data.get("action")
    if not isinstance(action, str) or action not in LEADER_DECISIONS:
        raise ValueError("Unsupported leader decision action")
    data = dict(data)
    declared_requests = None
    if action == "continue_research" and "information_request_ids" in data:
        declared_requests = data.pop("information_request_ids")
        if (not isinstance(declared_requests, list) or len(declared_requests) > 8
                or any(not isinstance(v, str) or not v or len(v) > 30 for v in declared_requests)
                or len(set(declared_requests)) != len(declared_requests)):
            raise ValueError("Information request IDs must be a bounded unique list")
    if action != "continue_research":
        for key in ("next_tasks","replace_roles","feedback"):
            if data.get(key)==[]:
                data.pop(key)
    action_keys = {
        "continue_research": {"next_tasks", "replace_roles", "feedback", "exploration"},
        "request_seat_change": {"member_count"},
        "request_extension": set(),
        "submit_codex": set(),
        "blocked": set(),
        "propose_merge": {"merge_groups"},
    }
    if not isinstance(max_members, int) or isinstance(max_members, bool) or not 3 <= max_members <= 32:
        raise ValueError("Leader contract seat bound must be 3..32")
    max_tasks = max_members - 1 if max_tasks is None else _bounded_int(max_tasks, "max_tasks", 2, 256, None)
    allowed = {"action", "rationale", "summary", "uncertainties", "communications", "research_leads"} | action_keys[action]
    unknown = set(data) - allowed
    if unknown:
        raise ValueError("Fields not allowed for this leader action: " + ", ".join(sorted(str(k) for k in unknown)))
    exploration = data.get("exploration", False)
    if not isinstance(exploration, bool):
        raise ValueError("exploration must be a boolean")
    leads = data.get("research_leads", [])
    if not isinstance(leads, list) or len(leads) > 4:
        raise ValueError("At most four research leads per decision")
    clean_leads = []
    for lead in leads:
        if not isinstance(lead, dict) or set(lead) - {"question", "rationale", "priority", "expected_value", "evidence_refs"}:
            raise ValueError("Invalid research lead fields")
        if not isinstance(lead.get("priority"), str) or lead.get("priority") not in {"low", "normal", "high"}:
            raise ValueError("Invalid research lead priority")
        texts = {key: _bounded_text(lead.get(key), key, 1000, nonempty=True) for key in ("question", "rationale", "expected_value")}
        reject_secrets("\n".join(texts.values()))
        communication = validate_communication({"summary": texts["question"], "evidence_refs": lead.get("evidence_refs", [])})
        clean_leads.append({**texts, "priority": lead["priority"], "evidence_refs": communication.get("evidence_refs", [])})
    rationale = _bounded_text(data.get("rationale", ""), "rationale", 2000)
    summary = _bounded_text(data.get("summary", ""), "summary", 5000)
    uncertainties = data.get("uncertainties", [])
    if not isinstance(uncertainties, list) or len(uncertainties) > 12:
        raise ValueError("uncertainties must contain at most 12 strings")
    uncertainties = [_bounded_text(v, "uncertainty", 1000) for v in uncertainties]
    member_count = None
    if "member_count" in data:
        member_count = _bounded_int(data["member_count"], "member_count", 3, max_members, None)
    replace_roles = data.get("replace_roles", [])
    if not isinstance(replace_roles, list) or len(replace_roles) > max_members - 1:
        raise ValueError("Replacement count exceeds group seats")
    clean_replace_roles = []
    for role_id in replace_roles:
        role_id = _bounded_text(role_id, "replace role ID", 100, nonempty=True)
        if role_id in clean_replace_roles:
            raise ValueError("replace_roles must be unique")
        clean_replace_roles.append(role_id)
    feedback = data.get("feedback", [])
    if not isinstance(feedback, list) or len(feedback) > max_tasks:
        raise ValueError("Feedback count exceeds group seats")
    clean_feedback = []
    feedback_workers = set()
    for item in feedback:
        if not isinstance(item, dict) or set(item) != {"worker", "accepted", "reason"}:
            raise ValueError("feedback items require exactly worker, accepted, and reason")
        worker = feedback_slot(item["worker"], max_tasks)
        if worker in feedback_workers:
            raise ValueError("feedback worker IDs must be unique")
        feedback_workers.add(worker)
        if not isinstance(item["accepted"], bool):
            raise ValueError("feedback accepted must be a boolean")
        reason = _bounded_text(item["reason"], "feedback reason", 1000, nonempty=True)
        clean_feedback.append({"worker": worker, "accepted": item["accepted"], "reason": reason})
    next_tasks = data.get("next_tasks", [])
    if not isinstance(next_tasks, list) or len(next_tasks) > max_tasks:
        raise ValueError("Task count exceeds group seats")
    clean_tasks = []
    all_text = [rationale, summary, *uncertainties]
    for task in next_tasks:
        if not isinstance(task, dict) or set(task) - _TASK_KEYS:
            raise ValueError("Invalid next task fields")
        role, kind = task.get("role"), task.get("kind")
        if not isinstance(role, str) or role not in LEADER_TASK_ROLES or not isinstance(kind, str) or kind not in LEADER_TASK_KINDS:
            raise ValueError("Unsupported next task role or kind")
        prompt = _bounded_text(task.get("prompt"), "next task prompt", 8000, nonempty=True)
        reject_secrets(prompt)
        urls = task.get("focus_urls", [])
        if not isinstance(urls, list) or len(urls) > 4 or any(not isinstance(url, str) or len(url) > 600 or not public_url(url) for url in urls):
            raise ValueError("focus_urls must contain at most four public web URLs")
        task_needs_web = task.get("needs_web", False)
        if not isinstance(task_needs_web, bool):
            raise ValueError("next task needs_web must be a boolean")
        clean_tasks.append({"role": role, "kind": kind, "prompt": prompt, "focus_urls": list(urls), "needs_web": task_needs_web})
        if "context_range" in task:
            bounds = task["context_range"]
            if not isinstance(bounds, list) or len(bounds) != 2:
                raise ValueError("context_range requires [start, end] character offsets")
            bounds = [_bounded_int(v, "context offset", 0, 2000000, None) for v in bounds]
            if any(v is None for v in bounds) or bounds[0] > bounds[1]:
                raise ValueError("context_range must be ordered integer offsets")
            clean_tasks[-1]["context_range"] = bounds
        if "evidence_workers" in task:
            refs = task["evidence_workers"]
            if not isinstance(refs, list) or len(refs) > 32:
                raise ValueError("At most 32 evidence workers per follow-up task")
            refs = [_bounded_int(v, "evidence worker", 0, 1000000, None) for v in refs]
            if any(v is None for v in refs) or len(set(refs)) != len(refs):
                raise ValueError("Evidence worker IDs must be unique integers")
            clean_tasks[-1]["evidence_workers"] = refs
        if "information_request_ids" in task:
            refs = task["information_request_ids"]
            if not isinstance(refs, list) or len(refs) > 8 or any(not isinstance(v, str) or not v or len(v) > 30 for v in refs) or len(set(refs)) != len(refs):
                raise ValueError("Information request IDs must be a bounded unique list")
            clean_tasks[-1]["information_request_ids"] = refs
        all_text.append(prompt)
    if declared_requests is not None and set(declared_requests) != {value for task in clean_tasks for value in task.get("information_request_ids", [])}:
        raise ValueError("Top-level information request IDs must exactly repeat next task bindings")
    all_text.extend(item["reason"] for item in clean_feedback)
    reject_secrets("\n".join(all_text))
    communications = data.get("communications", [])
    if not isinstance(communications, list) or len(communications) > 4:
        raise ValueError("At most four leader communications per round")
    communications = [validate_communication(item) for item in communications]
    merge_groups = data.get("merge_groups")
    if action == "propose_merge":
        if not isinstance(merge_groups, list) or not 2 <= len(merge_groups) <= 4:
            raise ValueError("Merge proposal must identify 2..4 groups")
        merge_groups = [_bounded_text(value, "merge group ID", 200, nonempty=True) for value in merge_groups]
        if len(set(merge_groups)) != len(merge_groups):
            raise ValueError("Merge group IDs must be unique")
        reject_secrets("\n".join(merge_groups))
    if action == "request_seat_change":
        if member_count is None:
            raise ValueError("request_seat_change requires member_count")
    elif member_count is not None:
        raise ValueError("member_count is only allowed for request_seat_change")
    return {"action": action, "rationale": rationale, "summary": summary,
            "next_tasks": clean_tasks, "uncertainties": uncertainties,
            **({"member_count": member_count} if member_count is not None else {}),
            **({"replace_roles": clean_replace_roles} if "replace_roles" in data else {}),
            **({"feedback": clean_feedback} if "feedback" in data else {}),
            **({"communications": communications} if "communications" in data else {}),
            **({"research_leads": clean_leads} if "research_leads" in data else {}),
            **({"exploration": exploration} if "exploration" in data else {}),
            **({"merge_groups": merge_groups} if action == "propose_merge" else {})}
