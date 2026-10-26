"""Validated runtime call policies. Explicit task values override preset defaults."""

import copy
import math

DEFAULT_LIMITS = {
    "max_output_tokens": 524288, "max_task_tokens": 4000000, "max_report_chars": 4194304,
    "max_context_chars": 64000, "max_deadline_s": 900, "max_hard_deadline_s": 86400,
    "max_ttft_s": 3600, "max_idle_s": 3600, "max_queue_wait_s": 3600, "max_attempts": 16, "max_fanout": 4,
    "max_protocol_frame_bytes": 16777216, "max_protocol_buffer_bytes": 67108864,
    "max_workflow_nodes": 32, "max_workflow_tokens": 4000000, "max_workflow_parallel": 8,
    "max_workflow_input_chars":32000,"max_node_prompt_chars":16000,"max_node_role_chars":2000,
}
# The transport's implementation envelopes are distinct from active policy.
ABS_LIMITS = DEFAULT_LIMITS | {
    "max_output_tokens": 16777216, "max_task_tokens": 100000000, "max_report_chars": 16777216,
    "max_context_chars": 1048576, "max_deadline_s": 86400, "max_hard_deadline_s": 604800,
    "max_ttft_s": 86400, "max_idle_s": 86400, "max_queue_wait_s": 86400, "max_attempts": 128, "max_fanout": 32,
    "max_protocol_frame_bytes": 134217728, "max_protocol_buffer_bytes": 536870912,
    "max_workflow_nodes": 256, "max_workflow_tokens": 100000000, "max_workflow_parallel": 32,
    "max_workflow_input_chars":1048576,"max_node_prompt_chars":262144,"max_node_role_chars":8192,
}
MIN_LIMITS = {k: 1 for k in DEFAULT_LIMITS} | {"max_output_tokens": 256, "max_task_tokens": 1000, "max_report_chars": 1000,
    "max_context_chars": 1000, "max_deadline_s": 5, "max_hard_deadline_s": 5, "max_queue_wait_s": 0,
    "max_protocol_frame_bytes": 2097152, "max_protocol_buffer_bytes": 2097152, "max_workflow_tokens": 1000}
PRESETS = {
    "inherit": {"title": "沿用现有设置"},
    "balanced": {"title": "均衡", "defaults": {"common": {"max_attempts": 4, "hard_deadline_s": 3600}},
                 "source_options": {"opencode": {"agent_steps": 2, "poll_interval_s": .25}}},
    "economy": {"title": "节省用量", "parallel": 2, "defaults": {"common": {"prefer_free": True, "price_weight": 80,
        "max_attempts": 3, "routing_strategy": "token_efficiency"}}, "source_options": {"opencode": {"agent_steps": 2, "poll_interval_s": .4}}},
    "throughput": {"title": "并行吞吐", "parallel": 8, "coordinator_threads": 8, "combined_parallel": 8, "defaults": {"common": {"queue_wait_s": 300, "max_attempts": 4}},
                   "source_options": {"opencode": {"agent_steps": 3, "poll_interval_s": .2}}},
    "deep": {"title": "长任务 / 大报告", "limits": {"max_deadline_s": 3600, "max_protocol_frame_bytes": 67108864},
             "defaults": {"common": {"deadline_s": 1200, "hard_deadline_s": 14400, "ttft_s": 1200,
                 "max_output_tokens": 131072, "token_budget": 800000, "report_max_chars": 1048576, "output_mode": "adaptive"}},
             "source_options": {"opencode": {"agent_steps": 4, "poll_interval_s": .4, "http_timeout_s": 10}}},
    "compatibility": {"title": "原生协议兼容", "source_options": {"zcode": {"selection_validation": "requested_options", "zcode_payload": "prompt"},
                       "opencode": {"agent_steps": 2, "poll_interval_s": .3, "http_timeout_s": 10, "message_limit": 16}}},
    "connectivity": {"title": "连通性排查", "recovery": {"max_probes_per_hour": 10, "max_tokens_per_hour": 64000, "probe_timeout_s": 60}},
}
EMPTY = {"preset": "inherit", "limits": {}, "defaults": {}, "source_options": {}, "parallel": None, "source_parallel": {}, "combined_parallel": None, "coordinator_threads": None, "recovery": {}}


def validate(value):
    if not isinstance(value, dict) or set(value) - set(EMPTY):
        raise ValueError("Unknown call profile fields")
    result = copy.deepcopy(EMPTY) | value
    if not isinstance(result["preset"], str) or result["preset"] not in PRESETS:
        raise ValueError("Unknown call profile preset")
    for key in ["limits", "defaults", "source_options", "source_parallel", "recovery"]:
        if not isinstance(result[key], dict):
            raise ValueError(key + " must be an object")
    for key, val in result["limits"].items():
        if key not in ABS_LIMITS or isinstance(val, bool) or not isinstance(val, int) or not MIN_LIMITS[key] <= val <= ABS_LIMITS[key]:
            raise ValueError("Invalid runtime limit: " + str(key))
    recovery_ranges = {"poll_s":(10,3600),"initial_delay_s":(60,86400),"max_backoff_s":(60,604800),"probe_timeout_s":(5,120),"max_probes_per_hour":(0,100),"max_tokens_per_hour":(0,100000000)}
    for key, val in result["recovery"].items():
        if key=="enabled":
            if not isinstance(val,bool): raise ValueError("Recovery enabled must be boolean")
        elif key not in recovery_ranges or isinstance(val,bool) or not isinstance(val,int) or not recovery_ranges[key][0]<=val<=recovery_ranges[key][1]:
            raise ValueError("Invalid recovery setting")
    if result["parallel"] is not None and (isinstance(result["parallel"], bool) or not isinstance(result["parallel"], int) or not 1 <= result["parallel"] <= 32):
        raise ValueError("parallel must be null or 1..32")
    for key in ["combined_parallel", "coordinator_threads"]:
        val = result[key]
        if val is not None and (isinstance(val, bool) or not isinstance(val, int) or not 1 <= val <= 64):
            raise ValueError(key + " must be null or 1..64")
    if len(result["source_parallel"]) > 64 or any(not isinstance(k, str) or not k or len(k) > 200 or isinstance(v, bool) or not isinstance(v, int) or not 1 <= v <= 32 for k,v in result["source_parallel"].items()):
        raise ValueError("Invalid source parallel limits")
    default_fields = {"deadline_s", "hard_deadline_s", "ttft_s", "idle_s", "stall_grace_s", "queue_wait_s", "max_attempts", "fanout", "verify",
        "max_output_tokens", "output_mode", "max_output_steps", "report_max_chars", "token_budget", "min_quality", "timeout_mode", "routing_strategy", "prefer_free", "price_weight", "allow_fallback"}
    from .models import TASK_KINDS, Task, reject_secrets
    for source in result["source_parallel"]:
        reject_secrets(source)
    for kind, values in result["defaults"].items():
        if kind not in {"common", *TASK_KINDS} or not isinstance(values, dict) or set(values) - default_fields:
            raise ValueError("Unsupported profile task defaults")
    resolved = resolve_unchecked(result)
    if resolved["limits"]["max_protocol_frame_bytes"] > resolved["limits"]["max_protocol_buffer_bytes"]:
        raise ValueError("Protocol buffer must accommodate at least one maximum frame")
    for kind in TASK_KINDS:
        values = resolved["defaults"].get("common", {}) | resolved["defaults"].get(kind, {})
        Task.parse({"kind": kind, "prompt": "Validate configured task defaults", **values}, runtime_limits=resolved["limits"])
    options_allowed = {"agent_steps", "max_turns", "poll_interval_s", "http_timeout_s", "message_limit", "selection_validation", "zcode_payload", "safe_mode", "partial_messages"}
    for source, options in result["source_options"].items():
        if not isinstance(source, str) or not source or len(source) > 200 or not isinstance(options, dict) or set(options) - options_allowed:
            raise ValueError("Unsupported native source options")
        reject_secrets(source)
        for key, val in options.items():
            if key in {"safe_mode", "partial_messages"}:
                if not isinstance(val, bool): raise ValueError(key + " must be boolean")
            elif key in {"selection_validation", "zcode_payload"}:
                allowed = {"exact", "requested_options"} if key == "selection_validation" else {"prompt", "messages"}
                if not isinstance(val, str) or val not in allowed: raise ValueError("Unsupported " + key)
            else:
                low, high = {"agent_steps": (1,128), "max_turns": (1,128), "poll_interval_s": (.05,5), "http_timeout_s": (.1,120), "message_limit": (1,128)}[key]
                if isinstance(val, bool) or not isinstance(val, (int,float)) or not math.isfinite(val) or not low <= val <= high or key in {"agent_steps","max_turns","message_limit"} and int(val)!=val:
                    raise ValueError("Invalid native source option: " + key)
    return result


def resolve_unchecked(value):
    value = copy.deepcopy(EMPTY) | value
    preset = PRESETS[value["preset"]]
    result = copy.deepcopy(EMPTY) | {k:copy.deepcopy(v) for k,v in preset.items() if k != "title"}
    result["preset"] = value["preset"]
    result["limits"] = DEFAULT_LIMITS | result["limits"] | value["limits"]
    for group in ["defaults", "source_options"]:
        for key, items in value[group].items():
            result[group][key] = result[group].get(key, {}) | items
    if value["parallel"] is not None: result["parallel"] = value["parallel"]
    for key in ["combined_parallel", "coordinator_threads"]:
        if value[key] is not None: result[key] = value[key]
    result["source_parallel"].update(value["source_parallel"])
    result["recovery"].update(value["recovery"])
    return result


def resolve(value):
    return resolve_unchecked(validate(value))


def presets():
    return [{"id":k,"title":v["title"],"value":copy.deepcopy(EMPTY)|{name:copy.deepcopy(item) for name,item in v.items() if name!="title"}|{"preset":k}} for k,v in PRESETS.items()]
