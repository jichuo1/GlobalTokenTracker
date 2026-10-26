"""Explainable routing with deadline, quota, capability, and failure-domain gates."""

import math

from .bridge import quota_for
from .models import now_ms, number
from .pricing import price_preference
from .model_updates import new_model_preference
from .slicing import output_budget


def rank_routes(task, routes, ledger, store, inflight=None, excluded=None, remaining_s=None, diversity=None, preferences=None):
    if getattr(store, "scheduler_kernel", None) is not None:
        return store.scheduler_kernel.rank(task, routes, ledger, store, inflight, excluded, remaining_s, diversity, preferences)
    inflight, excluded, diversity = inflight or {}, excluded or set(), diversity or {}
    preferences = preferences or {}
    horizon = task.hard_deadline_s if task.timeout_mode == "activity" else task.deadline_s
    remaining_s = horizon if remaining_s is None else remaining_s
    candidates, rejected = [], []
    from .scheduling import settings, route_reason
    from .controls import DispatchBlocked
    try:
        selection = settings(store, selector_only=True)
        selection_error = None
    except DispatchBlocked as error:
        selection, selection_error = {}, error.code
    logical = task.kind in {"code_review", "logic_check", "cross_check"}
    for route in routes:
        reasons = []
        selected = selection_error or route_reason(selection, route)
        if selected:
            reasons.append(selected)
        health = store.health("route:" + route.id)
        kind_health = store.health("route:" + route.id + ":kind:" + task.kind)
        provider_health = store.health("provider:" + route.failure_domain)
        source_health = store.health("source:" + route.source)
        route_availability = store.availability("route:" + route.id)
        provider_availability = store.availability("provider:" + route.failure_domain)
        source_availability = store.availability("source:" + route.source)
        quota = quota_for(route, ledger)
        generation = output_budget(task, route)
        if generation["effective_output_tokens"] < 256:
            reasons.append("context_output_headroom_exhausted")
        if generation["effective_output_tokens"] < getattr(task, "_output_floor", 256):
            reasons.append("output_below_recovery_floor")
        if not route.enabled or not route.auth_hint:
            reasons.append("disabled_or_no_auth_hint")
        if route.id in excluded:
            reasons.append("already_attempted")
        if task.allowed_routes is not None and route.id not in task.allowed_routes:
            reasons.append("outside_task_route_pool")
        if task.needs_web and "web" not in route.capabilities:
            reasons.append("web_not_supported")
        if route.profile.get("allowed_kinds") is not None and task.kind not in route.profile["allowed_kinds"]:
            reasons.append("task_kind_not_allowed")
        if route.context_window and (len(task.prompt) + len(task.context)) > route.context_window * 1.5:
            # Conservative bound for mixed CJK/code text; still only a preflight estimate.
            reasons.append("context_too_large")
        if max(health["cooldown_until"], provider_health["cooldown_until"]) > now_ms():
            reasons.append("circuit_open")
        if health.get("verification_required") or provider_health.get("verification_required") or source_health.get("verification_required"):
            reasons.append("recovery_verification_required")
        if quota["known"] and quota["remaining_percent"] <= 0:
            reasons.append("fresh_quota_exhausted")
        if task.preferred_route and not task.allow_fallback and route.id != task.preferred_route:
            reasons.append("pinned_route_only")
        benchmark_age = (now_ms() - route.profile.get("intelligence_captured_at", route.profile.get("captured_at", 0))) / 86400000
        benchmark_fresh = 0 <= benchmark_age <= 30
        terminal_age = (now_ms() - route.profile.get("terminal_bench_captured_at", route.profile.get("captured_at", 0))) / 86400000
        intelligence = number(route.profile.get("intelligence")) if benchmark_fresh else None
        terminal = number(route.profile.get("terminal_bench")) if 0 <= terminal_age <= 30 else None
        # Priors are explicitly heuristic when an exact, fresh benchmark is absent.
        prior = number(route.profile.get("quality_prior"), 30)
        quality = intelligence if intelligence is not None else prior
        feedback = store.quality(route.id, task.kind)
        if feedback["samples"] >= 3:
            quality = .65 * quality + .35 * (feedback["acceptance"] * 100)
        if quality < task.min_quality:
            reasons.append("quality_floor_not_met")
        latency = kind_health["latency_ms"] if kind_health["latency_ms"] is not None else health["latency_ms"]
        measured = latency / 1000 if latency is not None else None
        eta = measured if measured is not None else number(route.profile.get("latency_prior_s"), number(route.profile.get("answer_latency_s"), 45))
        payload_chars = len(task.prompt) + len(task.context)
        reference_chars = max(1024, kind_health.get("input_chars_ema") or 4096)
        input_multiplier = max(1, payload_chars / reference_chars)
        latency_scale, latency_adjustment = input_multiplier, "input_size"
        observed_cost = number(kind_health.get("measured_tokens_ema"))
        observed_chars = number(kind_health.get("measured_input_chars_ema"))
        if (measured is not None and task.kind in {"extract", "summarize"} and kind_health.get("measured_samples", 0) > 0
                and observed_cost is not None and observed_chars is not None and observed_chars >= 0
                and observed_cost > 4 * max(1, math.ceil(observed_chars / 2))):
            bounded_cost = math.ceil(payload_chars / 2) + generation["effective_output_tokens"]
            if bounded_cost < observed_cost:
                latency_scale = max(.25, bounded_cost / observed_cost)
                latency_adjustment = "smaller_output_ceiling_after_output_dominated_history"
        eta *= latency_scale
        queue_cost = inflight.get(route.source, 0) * eta / max(1, getattr(task, "_source_parallel", {}).get(route.source, route.max_parallel))
        eta += queue_cost
        effective_window = remaining_s
        if task.timeout_mode == "activity" and route.catalog_metadata.get("activity_observability") == "final_only":
            effective_window = min(effective_window, task.ttft_s or task.deadline_s)
        if eta > effective_window:
            reasons.append("unlikely_to_meet_deadline")
        if reasons:
            rejected.append({"route_id": route.id, "model": route.model, "reasons": reasons})
            continue
        # Lifetime counts are diagnostic; they cannot permanently suppress a
        # route that has recovered. Recent penalties decay back toward neutral.
        success_rate = route_availability["weight"]
        availability_weight = route_availability["weight"] * provider_availability["weight"] * source_availability["weight"]
        speed = 100 / (1 + eta / 20)
        weights = {"quality": (.72, .18, .04), "speed": (.05, .01, .70),
                   "reliability": (.40, .05, .10), "token_efficiency": (.40, .05, .15)}
        qw, tw, sw = weights.get(task.routing_strategy, (.55, .12, .10) if logical else (.25, .02, .37))
        quality_component = quality * qw
        terminal_component = (terminal or 0) * tw
        speed_component = speed * sw
        age = now_ms() - health.get("last_verified_at", 0)
        recently_verified = health.get("last_verified_at", 0) > 0 and 0 <= age <= 86400000
        confidence = (15 if task.routing_strategy == "reliability" else 4) if recently_verified else 0
        measured_cost = kind_health.get("measured_tokens_ema") if kind_health.get("measured_samples", 0) >= 3 else None
        token_penalty = 0
        if task.routing_strategy == "token_efficiency":
            cost_multiplier = max(1, payload_chars / max(1024, kind_health.get("measured_input_chars_ema") or 4096))
            token_penalty = min(20, measured_cost * cost_multiplier / 4096 * 2) if measured_cost is not None else 5
        fit = 8 if task.kind in route.profile.get("preferred_kinds", []) else 0
        penalty = diversity.get(route.failure_domain, 0) * 7
        score = quality_component + terminal_component + speed_component + success_rate * 12 + fit + confidence - penalty - token_penalty
        if task.allowed_routes is not None:
            score += 1000 * (len(task.allowed_routes)-task.allowed_routes.index(route.id))
        elif selection.get("mode") == "directed":
            # Pool order is deliberate; still subject to every hard gate above.
            index = next(i for i, r in enumerate(selection["pool"]) if r["route_id"] == route.id)
            score += 1000 * (len(selection["pool"]) - index)
        if quota["known"]:
            score += quota["remaining_percent"] * .03
        else:
            score -= 2
        if task.preferred_route == route.id:
            score += 100
        matches = [key for key, value in [("preferred_sources", route.source), ("preferred_models", route.model), ("preferred_routes", route.id)]
                   if value in preferences.get(key, [])]
        preference_boost = preferences.get("weight", 40) * len(matches)
        free_provider = max(0, route.max_parallel - inflight.get("provider:" + route.failure_domain, 0)) / max(1, route.max_parallel)
        concurrency_boost = max(0, preferences.get("concurrency_bias", 0)) * free_provider * 6
        price = price_preference(task, route, math.ceil((payload_chars + 4096) / 2),
                                 generation["effective_output_tokens"], preferences)
        prefer_new = preferences.get("prefer_new_models")
        prefer_new = task.prefer_new_models if prefer_new is None else prefer_new
        prefer_free = preferences.get("prefer_free")
        prefer_free = task.prefer_free if prefer_free is None else prefer_free
        newer_boost = new_model_preference(task, route, store, enabled=prefer_new)
        free_parallel_boost = free_provider * 4 if price["free"] and prefer_free else 0
        score += preference_boost + concurrency_boost + price["score_boost"] + newer_boost + free_parallel_boost
        score *= availability_weight ** (2 if task.routing_strategy == "reliability" else 1)
        candidates.append({"route": route, "score": round(score, 3), "eta_s": round(eta, 2), "quality": round(quality, 2),
                           **generation,
                           "observed_token_cost": kind_health.get("tokens_ema"),
                           "reasons": {"quality_source": "benchmark+feedback" if intelligence is not None else "heuristic+feedback",
                                       "benchmark_fresh": benchmark_fresh, "latency_source": "measured_route_ewma" if measured is not None else "prior",
                                       "success_rate": round(success_rate, 3), "quota_known": quota["known"],
                                       "input_time_multiplier": round(input_multiplier, 3), "reference_input_chars": round(reference_chars),
                                       "latency_scale": round(latency_scale, 3), "latency_adjustment": latency_adjustment,
                                       "deadline_basis": "initial_response_window" if effective_window < remaining_s else "absolute_execution_horizon",
                                       "eta_window_s": round(effective_window, 2),
                                       "routing_strategy": task.routing_strategy, "recent_generation_verified": recently_verified,
                                       "task_preference_matches": matches, "task_preference_boost": preference_boost,
                                       "concurrency_boost": round(concurrency_boost, 3),
                                       "price": price, "newer_model_boost": newer_boost,
                                       "free_parallel_boost": round(free_parallel_boost, 3),
                                       "token_cost_source": "reported_same_kind_history" if measured_cost is not None else "unknown",
                                       "token_efficiency_penalty": round(token_penalty, 3),
                                       "availability_weight": round(availability_weight, 4),
                                       "source_weight": source_availability["weight"], "provider_weight": provider_availability["weight"],
                                       "route_weight": route_availability["weight"], "penalty_half_life_s": 1800,
                                       "preferred_task_fit": bool(fit), "diversity_penalty": penalty}})
    candidates.sort(key=lambda c: (-c["score"], c["route"].id))
    return candidates, rejected


def public_plan(candidates, rejected):
    return {"candidates": [{"route": c["route"].public(), **{k: v for k, v in c.items() if k != "route"}} for c in candidates[:12]],
            "rejected": rejected[:128], "note": "Scores are routing preferences, not a claim that a model is correct; ETA is an estimate"}
