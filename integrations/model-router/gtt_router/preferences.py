"""Explicit, expiring caller scopes; no natural-language or global overrides."""

import math

from .models import number, reject_secrets


DEFAULTS = {"preferred_sources": [], "preferred_models": [], "preferred_routes": [],
            "weight": 40, "concurrency_bias": 0, "max_parallel": None,
            "price_weight": None, "prefer_free": None, "prefer_new_models": None}
SCHEMA = {"type": "object", "additionalProperties": False, "properties": {
    **{key: {"type": "array", "maxItems": count, "items": {"type": "string", "minLength": 1, "maxLength": 200}}
       for key, count in [("preferred_sources", 16), ("preferred_models", 32), ("preferred_routes", 32)]},
    "weight": {"type": "number", "minimum": 0, "maximum": 100, "default": 40},
    "concurrency_bias": {"type": "number", "minimum": -1, "maximum": 1, "default": 0},
    "max_parallel": {"type": ["integer", "null"], "minimum": 1, "maximum": 32},
    "price_weight": {"type": ["number", "null"], "minimum": 0, "maximum": 100},
    "prefer_free": {"type": ["boolean", "null"]}, "prefer_new_models": {"type": ["boolean", "null"]},
}}


def validate_scope(value, *, required=False):
    if not isinstance(value, str) or len(value) > 200 or (required and not value):
        raise ValueError("preference_scope must be a bounded scope ID")
    reject_secrets(value)
    return value


def parse_preferences(value, prior=None):
    if not isinstance(value, dict) or set(value) - set(DEFAULTS):
        raise ValueError("Unknown or invalid task preference fields")
    result = {**DEFAULTS, **(prior or {}), **value}
    for key, count in [("preferred_sources", 16), ("preferred_models", 32), ("preferred_routes", 32)]:
        items = result[key]
        if not isinstance(items, list) or len(items) > count or any(not isinstance(v, str) or not v or len(v) > 200 for v in items):
            raise ValueError(key + " must be a bounded list of exact inventory identifiers")
        for item in items:
            reject_secrets(item)
        result[key] = list(dict.fromkeys(items))
    for key, low, high in [("weight", 0, 100), ("concurrency_bias", -1, 1), ("price_weight", 0, 100)]:
        if key == "price_weight" and result[key] is None:
            continue
        if number(result[key]) is None or not low <= result[key] <= high:
            raise ValueError(f"{key} must be between {low} and {high}")
    for key in ["prefer_free", "prefer_new_models"]:
        if result[key] is not None and not isinstance(result[key], bool):
            raise ValueError(key + " must be a boolean")
    cap = result["max_parallel"]
    if cap is not None and (not isinstance(cap, int) or isinstance(cap, bool) or not 1 <= cap <= 32):
        raise ValueError("max_parallel must be null or an integer between 1 and 32")
    return result


def parallel_advice(preferences, global_limit):
    limit = max(1, int(global_limit))
    bias = preferences.get("concurrency_bias", 0)
    cap = min(limit, preferences.get("max_parallel") or limit)
    if bias < 0:
        cap = min(cap, max(1, math.ceil(limit * (1 + .75 * bias))))
    advised = max(1, min(cap, round(1 + (cap - 1) * (bias + 1) / 2)))
    return {"effective_max_parallel": cap, "recommended_parallel": advised, "concurrency_bias": bias}
