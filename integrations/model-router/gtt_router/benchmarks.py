"""Versioned benchmark priors. Exact aliases only; no fuzzy identity merges."""

import json
import os
from pathlib import Path
import urllib.error
import urllib.request

from .models import now_ms, number


def load_profiles(path):
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8-sig"))
        if data.get("schema_version") != 1:
            return {}
        profiles = {}
        for item in data.get("models", []):
            if not isinstance(item.get("aliases"), list):
                continue
            profile = {k: item[k] for k in ["intelligence", "terminal_bench", "benchmark_version", "harness", "effort",
                                                   "output_tps", "answer_latency_s", "context_window", "source_url", "captured_at", "preferred_kinds",
                                                   "intelligence_captured_at", "terminal_bench_captured_at", "intelligence_index_version", "allowed_kinds"] if k in item}
            for key in ["intelligence", "terminal_bench"]:
                if key in profile and (number(profile[key]) is None or not 0 <= profile[key] <= 100):
                    raise ValueError("Invalid benchmark score")
            for alias in item["aliases"]:
                if alias in profiles:
                    raise ValueError("Ambiguous benchmark alias")
                profiles[alias] = profile
        return profiles
    except (OSError, json.JSONDecodeError):
        return {}


def refresh_aa(path, aliases, api_key_env="AA_API_KEY"):
    """Optional official API. Lack of a key preserves the existing cache."""
    key = os.environ.get(api_key_env)
    if not key:
        return {"updated": False, "reason": "AA_API_KEY_not_configured", "cache_preserved": True}
    request = urllib.request.Request("https://artificialanalysis.ai/api/v2/language/models",
                                     headers={"x-api-key": key, "User-Agent": "GTT-ModelRouter/0.1"})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            raw = response.read(8 * 1024 * 1024 + 1)
        if len(raw) > 8 * 1024 * 1024:
            return {"updated": False, "reason": "benchmark_response_too_large", "cache_preserved": True}
        payload = json.loads(raw)
    except (OSError, ValueError, urllib.error.URLError):
        return {"updated": False, "reason": "benchmark_api_unavailable", "cache_preserved": True}
    try:
        old = json.loads(Path(path).read_text(encoding="utf-8-sig"))
    except (OSError, ValueError):
        old = {"schema_version": 1, "models": []}
    by_slug = {m.get("slug"): m for m in payload.get("data", [])}
    updated = 0
    for row in old.get("models", []):
        slug = row.get("aa_slug")
        if slug not in by_slug:
            continue
        model = by_slug[slug]
        evaluations = model.get("evaluations", {})
        score = number(evaluations.get("artificial_analysis_intelligence_index"))
        if score is None or not 0 <= score <= 100:
            continue
        row["intelligence"] = score
        # Individual evaluation fields depend on package and schema version.
        # Preserve the old Terminal-Bench score AND its observation timestamp;
        # a fresh Intelligence Index response cannot refresh an absent metric.
        row["intelligence_captured_at"] = now_ms()
        performance = model.get("performance", {})
        tps = number(performance.get("median_output_tokens_per_second"))
        if tps is not None and tps > 0:
            row["output_tps"] = tps
        row["source_url"] = "https://artificialanalysis.ai/data-api"
        row["intelligence_index_version"] = payload.get("intelligence_index_version")
        updated += 1
    if not updated:
        return {"updated": False, "reason": "no_exact_alias_matches", "cache_preserved": True}
    destination = Path(path)
    tmp = destination.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(old, indent=2, ensure_ascii=False), encoding="utf-8")
    tmp.replace(destination)
    return {"updated": True, "model_count": updated, "source": "Artificial Analysis official API"}
