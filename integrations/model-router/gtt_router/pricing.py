"""Source-attributed price preferences, independent from token efficiency."""

import math
from .models import number, now_ms


def normalize_pricing(value, source, captured_at=None):
    if not isinstance(value, dict):
        return {}
    rates = {key: number(value.get(key)) for key in ["input", "output", "cache_read", "cache_write"]}
    if any(v is not None and v < 0 for v in rates.values()):
        return {}
    if rates["input"] is None or rates["output"] is None:
        return {}
    # OpenCode's native model costs are USD per million tokens. Other bridges
    # must declare the same unit; names such as '-free' do not establish price.
    currency = value.get("currency", "USD")
    unit = value.get("unit", "per_million_tokens")
    if currency != "USD" or unit != "per_million_tokens":
        return {}
    tiers = value.get("context_over_200k", {})
    tiers = tiers if isinstance(tiers, dict) else {}
    tier_input, tier_output = number(tiers.get("input")), number(tiers.get("output"))
    if tier_input is not None and tier_output is not None and tier_input >= 0 and tier_output >= 0:
        rates["input"], rates["output"] = max(rates["input"], tier_input), max(rates["output"], tier_output)
    return {**{k: v for k, v in rates.items() if v is not None}, "currency": currency,
            "unit": unit, "source": source, "captured_at": int(number(captured_at, now_ms()))}


def price_preference(task, route, input_tokens, output_tokens, preferences=None):
    values = preferences or {}
    weight = values.get("price_weight")
    weight = task.price_weight if weight is None else weight
    prefer_free = values.get("prefer_free")
    prefer_free = task.prefer_free if prefer_free is None else prefer_free
    p = route.pricing
    age = now_ms() - p.get("captured_at", 0)
    known = bool(p) and 0 <= age <= 86400000 and p.get("currency") == "USD" and p.get("unit") == "per_million_tokens" and all(number(p.get(k)) is not None and p[k] >= 0 for k in ["input", "output"])
    cost = None
    if known:
        # No speculative cache hits: input is charged at its uncached rate.
        cost = (input_tokens * p["input"] + output_tokens * p["output"]) / 1_000_000
        if not math.isfinite(cost):
            known, cost = False, None
    free = known and all(p.get(k, 0) == 0 for k in ["input", "output", "cache_read", "cache_write"])
    volume = min(1, max(input_tokens + output_tokens, task.token_budget / max(1, task.fanout)) / 32768)
    boost = weight * (.35 + .65 * volume) / (1 + cost * 10) if cost is not None else 0
    if free and prefer_free:
        boost += 10 * volume
    return {"known": known, "free": free, "estimated_request_usd": cost,
            "score_boost": round(boost, 3), "price_weight": weight,
            "source": p.get("source") if known else "unknown", "captured_at": p.get("captured_at") if known else None,
            "input_tokens_estimate": input_tokens, "output_tokens_estimate": output_tokens,
            "note": "Provider catalog estimate; subscription entitlement and actual invoice may differ"}
