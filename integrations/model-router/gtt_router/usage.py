"""Durable, credential-free usage contract consumed by GTT (version 1).

Reservations are scheduling estimates, never authoritative ledger consumption.
Native-backed tools keep their request identity instead of creating a second bill.
"""

import math

TOKEN_KEYS = {
    "inputTokens", "outputTokens", "totalTokens", "cacheReadTokens", "cacheWriteTokens", "reasoningTokens",
    "input_tokens", "output_tokens", "total_tokens", "cache_read_tokens", "cache_write_tokens",
    "cache_read_input_tokens", "cache_creation_input_tokens", "reasoning_tokens", "thinking_tokens",
}
MAX_TOKENS = 1_000_000_000_000


def count(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool) and 0 <= value <= MAX_TOKENS and math.isfinite(value) and int(value) == value:
        return int(value)
    return None


def clean_usage(value):
    return {k: count(v) for k, v in value.items() if k in TOKEN_KEYS and count(v) is not None} if isinstance(value, dict) else {}


def positive_usage(value):
    """Native boot-zero placeholders do not prove zero generation usage."""
    if isinstance(value, dict):
        value = dict(value)
        for key, dimensions in [("inputTokens", {"cacheRead": "cacheReadTokens", "cacheWrite": "cacheWriteTokens"}),
                                ("outputTokens", {"reasoning": "reasoningTokens"})]:
            nested = value.get(key)
            if isinstance(nested, dict):
                value[key] = nested.get("total")
                for native, target in dimensions.items():
                    if target not in value and native in nested:
                        value[target] = nested[native]
    cleaned = clean_usage(value)
    return cleaned if any(v > 0 for v in cleaned.values()) else {}


def total_tokens(value):
    usage = clean_usage(value)
    explicit = [usage[k] for k in ("totalTokens", "total_tokens") if k in usage]
    dimensions = normalize(usage)
    known = sum(dimensions.values()) - dimensions["reasoning_tokens"]
    if explicit:
        return max(*explicit, known)
    return known if any(k in usage for k in TOKEN_KEYS - {"reasoning_tokens", "reasoningTokens", "thinking_tokens"}) else None


def normalize(value, total=None):
    u = clean_usage(value)
    camel = "inputTokens" in u or "outputTokens" in u
    cache_read = u.get("cacheReadTokens", u.get("cache_read_tokens", u.get("cache_read_input_tokens", 0)))
    cache_write = u.get("cacheWriteTokens", u.get("cache_write_tokens", u.get("cache_creation_input_tokens", 0)))
    inp = u.get("inputTokens", u.get("input_tokens", 0))
    # ZCode generateText exposes AI SDK inputTokens.total, including cache.
    if camel:
        inp = max(0, inp - cache_read - cache_write)
    out = u.get("outputTokens", u.get("output_tokens", 0))
    result = {"input_tokens": inp, "output_tokens": out,
              "reasoning_tokens": u.get("reasoningTokens", u.get("reasoning_tokens", u.get("thinking_tokens", 0))),
              "cache_read_tokens": cache_read, "cache_write_5m_tokens": cache_write,
              "cache_write_1h_tokens": 0, "unclassified_tokens": 0}
    known = inp + out + cache_read + cache_write
    supplied = count(total)
    if supplied is None:
        supplied = u.get("totalTokens", u.get("total_tokens"))
    if supplied is not None and supplied > known:
        result["unclassified_tokens"] = supplied - known
    return result


def project_attempt(job_id, index, attempt, results):
    if attempt.get("generation_not_started") is True:
        return None
    if attempt.get("state") not in {"succeeded", "failed", "running"}:
        return None
    if attempt.get("state")=="running" and attempt.get("token_measurement")!="reported":
        return None
    usage = attempt.get("usage", {})
    # Old releases saved successful raw usage only on the result, not attempt.
    if not usage and attempt.get("state") == "succeeded":
        matches = [r for r in results if r.get("worker") == attempt.get("worker") and r.get("route", {}).get("route_id") == attempt.get("route_id")]
        if len(matches) == 1:
            usage = matches[0].get("usage", {})
    measured = total_tokens(usage)
    if measured is None and "tokens_charged" not in attempt:
        return None
    amount = measured if measured is not None else attempt.get("tokens_charged")
    record = {"version": 1, "record_key": f"job:{job_id}:attempt:{index}",
              "app": attempt.get("app", {"antigravity": "gemini_antigravity"}.get(attempt.get("source"), attempt.get("source"))),
              "provider_id": attempt.get("provider"), "model": attempt.get("actual_model", attempt.get("model")),
              "request_model": attempt.get("model"), "session_id": job_id,
              "ts_start": attempt.get("started_at"), "duration_ms": attempt.get("duration_ms"),
              "status": attempt.get("state"), "measurement": "reported" if measured is not None else attempt.get("token_measurement", "estimated"),
              "native_dedup_key": attempt.get("native_dedup_key"),
              "native_backed": attempt.get("native_backed", attempt.get("source") in {"opencode", "antigravity", "codex_agent"}),
              **normalize(usage, amount)}
    return record
