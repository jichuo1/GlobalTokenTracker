"""Public contracts; runtime credentials never belong in these records."""

from dataclasses import asdict, dataclass, field
import hashlib
import math
import re
import time

TASK_KINDS = {"research", "extract", "summarize", "code_review", "logic_check", "cross_check"}
LOGICAL_KINDS = {"code_review", "logic_check", "cross_check"}
MAX_OUTPUT_TOKENS = 524288
LOGICAL_OUTPUT_TOKENS = 32768
MAX_TASK_TOKENS = 4000000
DEFAULT_TASK_TOKENS = 200000
DEFAULT_REPORT_CHARS = 120000
MAX_REPORT_CHARS = 4 * 1024 * 1024
TERMINAL_STATES = {"succeeded", "partial", "failed", "cancelled", "interrupted", "needs_codex"}
WEB_TOOLS = {"search_web", "read_url_content", "view_file"}


def now_ms():
    return int(time.time() * 1000)


def number(value, default=None):
    if isinstance(value, bool):
        return default
    if isinstance(value, (float, int)):
        try:
            converted = float(value)
            return converted if math.isfinite(converted) else default
        except (OverflowError, ValueError):
            return default
    return default


def stable_id(*parts):
    return hashlib.sha256("\0".join(str(p) for p in parts).encode()).hexdigest()[:20]


def redact(text):
    text = re.sub(r"(?i)Bearer\s+[A-Za-z0-9_.\-]{12,}", "Bearer [REDACTED]", str(text))
    return re.sub(r"\bsk-[A-Za-z0-9_\-]{16,}\b", "[REDACTED]", text)


def reject_secrets(text):
    if redact(text) != text or re.search(r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----", text):
        raise ValueError("Payload contains a credential or private key; send a redacted excerpt")


@dataclass
class Route:
    source: str
    provider: str
    model: str
    effort: str = ""
    label: str = ""
    enabled: bool = True
    auth_hint: bool = False
    capabilities: list = field(default_factory=lambda: ["text"])
    context_window: int | None = None
    adapter: str = ""
    runtime: dict = field(default_factory=dict, repr=False)
    profile: dict = field(default_factory=dict)
    quota_app: str = ""
    quota_account: str | None = None
    max_parallel: int = 2
    output_limit: int | None = None
    pricing: dict = field(default_factory=dict)
    catalog_metadata: dict = field(default_factory=dict)

    @property
    def id(self):
        return stable_id(self.source, self.provider, self.model, self.effort)

    @property
    def failure_domain(self):
        return stable_id(self.source, self.provider, self.runtime.get("account_key", "default"))

    def public(self):
        return {k: v for k, v in {
            "route_id": self.id, "source": self.source, "provider": self.provider,
            "model": self.model, "effort": self.effort, "label": self.label,
            "enabled": self.enabled, "auth_hint": self.auth_hint,
            "capabilities": self.capabilities, "context_window": self.context_window,
            "adapter": self.adapter, "profile": self.profile,
            "output_limit": self.output_limit,
            "pricing": self.pricing, "catalog_metadata": self.catalog_metadata,
            "failure_domain": self.failure_domain,
        }.items() if v is not None}

    def output_ceiling(self, requested):
        limit = self.output_limit
        if isinstance(limit, int) and not isinstance(limit, bool) and limit > 0:
            return limit if requested is None else min(requested, limit)
        return requested


@dataclass
class Task:
    kind: str
    prompt: str
    context: str = ""
    deadline_s: float = 180
    fanout: int = 1
    verify: bool = False
    needs_web: bool = False
    min_quality: float = 0
    max_attempts: int = 6
    max_output_tokens: int = 8192
    priority: int = 50
    preferred_route: str | None = None
    allow_fallback: bool = True
    base_revision: str = ""
    context_label: str = ""
    workflow_id: str = "default"
    work_key: str = ""
    token_budget: int = 24000
    focus_urls: list = field(default_factory=list)
    focus_terms: list = field(default_factory=list)
    web_tool_budget: int = 6
    routing_strategy: str = "auto"
    preference_scope: str = ""
    timeout_mode: str = "activity"
    hard_deadline_s: float = 3600
    ttft_s: float | None = None
    idle_s: float = 180
    stall_grace_s: float = 15
    price_weight: float = 30
    prefer_free: bool = True
    prefer_new_models: bool = True
    output_mode: str = "fixed"
    queue_wait_s: float = 120
    report_max_chars: int = DEFAULT_REPORT_CHARS
    allowed_routes: list | None = None
    # Node-level reasoning effort override (scheduling graphs): the route keeps
    # its catalog identity for pool/gating, the adapter call uses this level.
    route_effort: str = ""

    @classmethod
    def parse(cls, data, max_context_chars=64000, *, runtime_limits=None):
        from .profiles import DEFAULT_LIMITS
        limits = DEFAULT_LIMITS | (runtime_limits or {})
        if runtime_limits is not None:
            max_context_chars = limits["max_context_chars"]
        if not isinstance(data, dict):
            raise ValueError("Task must be an object")
        unknown = set(data) - set(cls.__dataclass_fields__)
        if unknown:
            raise ValueError("Unknown task fields: " + ", ".join(sorted(unknown)))
        task = cls(**data)
        task._runtime_limits = limits
        task._queue_wait_explicit = "queue_wait_s" in data
        if not isinstance(task.report_max_chars, int) or isinstance(task.report_max_chars, bool) or not 1000 <= task.report_max_chars <= limits["max_report_chars"]:
            raise ValueError(f"report_max_chars must be an integer between 1000 and {limits['max_report_chars']}")
        from .preferences import validate_scope
        validate_scope(task.preference_scope)
        if not isinstance(task.output_mode, str) or task.output_mode not in {"fixed", "adaptive", "native"}:
            raise ValueError("output_mode must be fixed, adaptive or native")
        if "output_mode" not in data and "max_output_tokens" not in data:
            task.output_mode = "adaptive"
        if task.timeout_mode not in {"activity", "hard"}:
            raise ValueError("timeout_mode must be activity or hard")
        for key, low, high in [("hard_deadline_s", 5, limits["max_hard_deadline_s"]), ("idle_s", 1, limits["max_idle_s"]),
                               ("stall_grace_s", 0, 300), ("price_weight", 0, 100), ("queue_wait_s", 0, limits["max_queue_wait_s"])]:
            value = number(getattr(task, key))
            if value is None or not low <= value <= high:
                raise ValueError(f"{key} must be between {low} and {high}")
        if task.ttft_s is not None and (number(task.ttft_s) is None or not 1 <= task.ttft_s <= limits["max_ttft_s"]):
            raise ValueError(f"ttft_s must be null or between 1 and {limits['max_ttft_s']}")
        for key in ["prefer_free", "prefer_new_models"]:
            if not isinstance(getattr(task, key), bool):
                raise ValueError(f"{key} must be a boolean")
        if task.kind in LOGICAL_KINDS and "min_quality" not in data:
            task.min_quality = 38
        if task.kind in LOGICAL_KINDS:
            if "max_output_tokens" not in data:
                task.max_output_tokens = LOGICAL_OUTPUT_TOKENS
            if "deadline_s" not in data:
                task.deadline_s = 600
        if task.fanout > 1 and "verify" not in data:
            task.verify = True
        if task.kind not in TASK_KINDS:
            raise ValueError("Unsupported bounded task kind")
        if not isinstance(task.routing_strategy, str) or task.routing_strategy not in {"auto", "quality", "speed", "reliability", "token_efficiency"}:
            raise ValueError("Unsupported routing strategy")
        if not isinstance(task.prompt, str) or not task.prompt.strip():
            raise ValueError("prompt must be a nonempty string")
        if not isinstance(task.context, str):
            raise ValueError("context must be a string")
        if len(task.prompt) + len(task.context) > max_context_chars:
            raise ValueError("Payload exceeds the bounded excerpt limit")
        reject_secrets(task.prompt + "\n" + task.context)
        for key, low, high in [("deadline_s", 5, limits["max_deadline_s"]), ("fanout", 1, limits["max_fanout"]),
                               ("min_quality", 0, 100), ("max_attempts", 1, limits["max_attempts"]),
                               ("max_output_tokens", 256, limits["max_output_tokens"]), ("priority", 0, 100)]:
            value = number(getattr(task, key))
            if value is None or not low <= value <= high:
                raise ValueError(f"{key} must be between {low} and {high}")
        for key in ["fanout", "max_attempts", "max_output_tokens", "priority"]:
            if int(getattr(task, key)) != getattr(task, key):
                raise ValueError(f"{key} must be an integer")
        for key in ["verify", "needs_web", "allow_fallback"]:
            if not isinstance(getattr(task, key), bool):
                raise ValueError(f"{key} must be a boolean")
        if task.max_attempts < task.fanout + int(task.verify):
            raise ValueError("Attempt budget cannot cover the requested workers and verifier")
        if task.timeout_mode == "activity" and task.hard_deadline_s < task.deadline_s:
            raise ValueError("hard_deadline_s cannot be shorter than the initial response window")
        if "token_budget" not in data:
            # Reserve room for input, the generated instruction wrapper and
            # verifier reports. This is a ceiling, not prepaid consumption.
            prompt_allowance = math.ceil((len(task.prompt) + len(task.context) + 4096) / 2)
            calls = task.fanout + int(task.verify)
            if task.allow_fallback and task.max_attempts > calls:
                calls += 1  # one bounded replacement, not pre-paid consumption
            from .slicing import requested_output
            output_allowance = limits["max_output_tokens"] if task.output_mode == "native" else requested_output(task)
            allowance = calls * (output_allowance + prompt_allowance)
            if task.verify:
                allowance += math.ceil(task.fanout * 5000 / 2)
            default_cap = limits["max_task_tokens"] if task.max_output_tokens > 65536 or task.output_mode == "native" else min(DEFAULT_TASK_TOKENS, limits["max_task_tokens"])
            task.token_budget = min(default_cap, 24000 if allowance <= 24000 else math.ceil(allowance / 8192) * 8192)
        if number(task.token_budget) is None or not 1000 <= task.token_budget <= limits["max_task_tokens"]:
            raise ValueError(f"token_budget must be between 1000 and {limits['max_task_tokens']}")
        if not isinstance(task.web_tool_budget, int) or isinstance(task.web_tool_budget, bool) or not 2 <= task.web_tool_budget <= 12:
            raise ValueError("web_tool_budget must be between 2 and 12")
        from .hook_guard import public_url
        if not isinstance(task.focus_urls, list) or len(task.focus_urls) > 4 or any(not isinstance(u, str) or len(u) > 600 or not public_url(u) for u in task.focus_urls):
            raise ValueError("focus_urls must contain at most four public web URLs")
        if not isinstance(task.focus_terms, list) or len(task.focus_terms) > 8 or any(not isinstance(t, str) or not t or len(t) > 64 for t in task.focus_terms):
            raise ValueError("focus_terms must be a bounded keyword list")
        for key in ["base_revision", "context_label", "workflow_id", "work_key", "route_effort"]:
            if not isinstance(getattr(task, key), str) or len(getattr(task, key)) > 300:
                raise ValueError(f"{key} must be a bounded string")
        if task.preferred_route is not None and not isinstance(task.preferred_route, str):
            raise ValueError("preferred_route must be a route ID")
        if task.allowed_routes is not None:
            if not isinstance(task.allowed_routes,list) or len(task.allowed_routes)>128 or any(not isinstance(v,str) or not v or len(v)>200 for v in task.allowed_routes):
                raise ValueError("allowed_routes must be null or a bounded list of route IDs")
            for route in task.allowed_routes:reject_secrets(route)
            task.allowed_routes=list(dict.fromkeys(task.allowed_routes))
        return task

    def public(self):
        d = asdict(self)
        d.pop("prompt")
        d.pop("context")
        d["payload_chars"] = len(self.prompt) + len(self.context)
        return d


class WorkerFailure(Exception):
    def __init__(self, code, message=None, scope="route", retry_after_s=None, usage=None, metering=None, details=None):
        super().__init__(message or code)
        self.code = code
        self.scope = scope
        self.retry_after_s = retry_after_s
        self.usage = usage
        self.metering = metering or {}
        self.details = details if isinstance(details, dict) else {}

    def public(self):
        result = {"code": self.code, "scope": self.scope, "message": redact(str(self))[:500]}
        # Diagnostic counters only. Never serialize arbitrary vendor payloads.
        allowed = {"requested_output_tokens", "reported_output_tokens", "reported_reasoning_tokens", "visible_output_chars", "report_chars", "json_error_offset", "frame_chars", "native_exit_code", "stderr_chars"}
        details = {k: v for k, v in self.details.items() if k in allowed and isinstance(v, int) and not isinstance(v, bool) and 0 <= v <= 1_000_000_000_000}
        finish_reason = self.details.get("finish_reason")
        if isinstance(finish_reason, str) and finish_reason in {"length", "stop", "error"}:
            details["finish_reason"] = finish_reason
        reason = self.details.get("contract_reason")
        if isinstance(reason, str) and reason in {"duplicate_key", "nonfinite_number", "ambiguous_envelope", "multiple_documents",
                "non_text_report", "report_too_large", "missing_json_object", "invalid_json", "excessive_nesting",
                "unexpected_root", "invalid_unicode", "required_completion_fields", "required_report_arrays", "unexpected_native_frame_root", "incomplete_json"}:
            details["contract_reason"] = reason
        timeout_reason = self.details.get("timeout_reason")
        if isinstance(timeout_reason, str) and timeout_reason in {"first_activity_timeout", "inactivity_timeout", "hard_deadline"}:
            details["timeout_reason"] = timeout_reason
        if details:
            result["details"] = details
        return result
