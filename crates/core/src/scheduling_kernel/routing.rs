use super::{array, b, generation, has, n, rounded, s};
use serde_json::{Value, json};

fn availability(h: &Value, now: f64) -> f64 {
    let age = (now - n(h, "penalty_updated_at", 0.)).max(0.) / 1000.;
    rounded(
        (1. - n(h, "penalty", 0.) * (-std::f64::consts::LN_2 * age / 1800.).exp()).max(0.25),
        4,
    )
}

fn price(t: &Value, r: &Value, p: &Value, input: f64, output: f64, now: f64) -> Value {
    let rates = &r["pricing"];
    let age = now - n(rates, "captured_at", 0.);
    let mut known = rates.as_object().is_some_and(|v| !v.is_empty())
        && (0. ..=86_400_000.).contains(&age)
        && s(rates, "currency") == "USD"
        && s(rates, "unit") == "per_million_tokens"
        && ["input", "output"]
            .iter()
            .all(|k| rates[k].as_f64().is_some_and(|v| v.is_finite() && v >= 0.));
    let cost = (input * n(rates, "input", 0.) + output * n(rates, "output", 0.)) / 1_000_000.;
    known &= cost.is_finite();
    let free = known
        && ["input", "output", "cache_read", "cache_write"]
            .iter()
            .all(|k| n(rates, k, 0.) == 0.);
    let volume = ((input + output).max(n(t, "token_budget", 24000.) / n(t, "fanout", 1.).max(1.))
        / 32768.)
        .min(1.);
    let weight = n(p, "price_weight", n(t, "price_weight", 30.));
    let mut boost = if known {
        weight * (0.35 + 0.65 * volume) / (1. + cost * 10.)
    } else {
        0.
    };
    if free && p["prefer_free"].as_bool().unwrap_or(b(t, "prefer_free")) {
        boost += 10. * volume;
    }
    json!({"known":known,"free":free,"estimated_request_usd":if known {json!(cost)}else{Value::Null},
        "score_boost":rounded(boost,3),"price_weight":weight,"source":if known {rates["source"].clone()}else{json!("unknown")},
        "captured_at":if known {rates["captured_at"].clone()}else{Value::Null},"input_tokens_estimate":input as u64,
        "output_tokens_estimate":output as i64,"note":"Provider catalog estimate; subscription entitlement and actual invoice may differ"})
}

pub(super) fn rank(v: &Value) -> Result<Value, String> {
    let routes = array(&v["routes"])?;
    if routes.len() > 16384 {
        return Err("scheduler_kernel_route_limit".into());
    }
    let t = &v["task"];
    let p = &v["preferences"];
    let inflight = &v["inflight"];
    let now = n(v, "now_ms", 0.);
    let size = n(t, "payload_chars", 0.);
    let logical = matches!(s(t, "kind"), "code_review" | "logic_check" | "cross_check");
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    for r in routes {
        let id = s(r, "route_id");
        let source = s(r, "source");
        let domain = s(r, "failure_domain");
        let health = &v["health"][format!("route:{id}")];
        let kh = &v["health"][format!("route:{id}:kind:{}", s(t, "kind"))];
        let ph = &v["health"][format!("provider:{domain}")];
        let sh = &v["health"][format!("source:{source}")];
        let rw = availability(health, now);
        let pw = availability(ph, now);
        let sw = availability(sh, now);
        let quota = &r["quota"];
        let g = generation(t, r, size);
        let profile = &r["profile"];
        let meta = &r["catalog_metadata"];
        let mut reasons: Vec<String> = Vec::new();
        if !s(r, "selection_reason").is_empty() {
            reasons.push(s(r, "selection_reason").into());
        }
        let mut reject = |condition: bool, reason: &str| {
            if condition {
                reasons.push(reason.into());
            }
        };
        reject(
            n(&g, "effective_output_tokens", 0.) < 256.,
            "context_output_headroom_exhausted",
        );
        reject(
            n(&g, "effective_output_tokens", 0.) < n(t, "output_floor", 256.),
            "output_below_recovery_floor",
        );
        reject(
            !b(r, "enabled") || !b(r, "auth_hint"),
            "disabled_or_no_auth_hint",
        );
        reject(has(&v["excluded"], id), "already_attempted");
        reject(
            !t["allowed_routes"].is_null() && !has(&t["allowed_routes"], id),
            "outside_task_route_pool",
        );
        reject(
            b(t, "needs_web") && !has(&r["capabilities"], "web"),
            "web_not_supported",
        );
        reject(
            !profile["allowed_kinds"].is_null() && !has(&profile["allowed_kinds"], s(t, "kind")),
            "task_kind_not_allowed",
        );
        reject(
            n(r, "context_window", 0.) > 0. && size > n(r, "context_window", 0.) * 1.5,
            "context_too_large",
        );
        reject(
            n(health, "cooldown_until", 0.).max(n(ph, "cooldown_until", 0.)) > now,
            "circuit_open",
        );
        reject(
            b(health, "verification_required")
                || n(health, "verification_required", 0.) != 0.
                || b(ph, "verification_required")
                || n(ph, "verification_required", 0.) != 0.
                || b(sh, "verification_required")
                || n(sh, "verification_required", 0.) != 0.,
            "recovery_verification_required",
        );
        reject(
            b(quota, "known") && n(quota, "remaining_percent", 0.) <= 0.,
            "fresh_quota_exhausted",
        );
        reject(
            !s(t, "preferred_route").is_empty()
                && !b(t, "allow_fallback")
                && s(t, "preferred_route") != id,
            "pinned_route_only",
        );
        let fresh = |key: &str| {
            let age = (now - n(profile, key, n(profile, "captured_at", 0.))) / 86_400_000.;
            (0. ..=30.).contains(&age)
        };
        let intelligence = profile["intelligence"]
            .as_f64()
            .filter(|_| fresh("intelligence_captured_at"));
        let terminal = profile["terminal_bench"]
            .as_f64()
            .filter(|_| fresh("terminal_bench_captured_at"))
            .unwrap_or(0.);
        let mut quality = intelligence.unwrap_or(n(profile, "quality_prior", 30.));
        let feedback = &v["feedback"][format!("{id}:{}", s(t, "kind"))];
        if n(feedback, "samples", 0.) >= 3. {
            quality = 0.65 * quality + 0.35 * n(feedback, "acceptance", 0.) * 100.;
        }
        reject(quality < n(t, "min_quality", 0.), "quality_floor_not_met");
        let measured = kh["latency_ms"]
            .as_f64()
            .or_else(|| health["latency_ms"].as_f64())
            .map(|v| v / 1000.);
        let mut eta = measured.unwrap_or(n(
            profile,
            "latency_prior_s",
            n(profile, "answer_latency_s", 45.),
        ));
        let reference = n(kh, "input_chars_ema", 4096.);
        let reference = if reference == 0. { 4096. } else { reference }.max(1024.);
        let multiplier = (size / reference).max(1.);
        let mut scale = multiplier;
        let mut adjustment = "input_size";
        let observed = kh["measured_tokens_ema"].as_f64();
        let observed_chars = kh["measured_input_chars_ema"].as_f64();
        if let (Some(cost), Some(chars)) = (observed, observed_chars)
            && measured.is_some()
            && matches!(s(t, "kind"), "extract" | "summarize")
            && n(kh, "measured_samples", 0.) > 0.
            && chars >= 0.
            && cost > 4. * (chars / 2.).ceil().max(1.)
        {
            let bounded = (size / 2.).ceil() + n(&g, "effective_output_tokens", 0.);
            if bounded < cost {
                scale = (bounded / cost).max(0.25);
                adjustment = "smaller_output_ceiling_after_output_dominated_history";
            }
        }
        eta *= scale;
        eta += n(inflight, source, 0.) * eta
            / n(&t["source_parallel"], source, n(r, "max_parallel", 2.)).max(1.);
        let remaining = n(v, "remaining_s", 180.);
        let window = if s(t, "timeout_mode") == "activity"
            && s(meta, "activity_observability") == "final_only"
        {
            remaining.min(n(t, "ttft_s", n(t, "deadline_s", 180.)))
        } else {
            remaining
        };
        reject(eta > window, "unlikely_to_meet_deadline");
        if !reasons.is_empty() {
            rejected.push(json!({"route_id":id,"model":r["model"],"reasons":reasons}));
            continue;
        }
        let strategy = s(t, "routing_strategy");
        let (qw, tw, swgt) = match strategy {
            "quality" => (0.72, 0.18, 0.04),
            "speed" => (0.05, 0.01, 0.70),
            "reliability" => (0.40, 0.05, 0.10),
            "token_efficiency" => (0.40, 0.05, 0.15),
            _ if logical => (0.55, 0.12, 0.10),
            _ => (0.25, 0.02, 0.37),
        };
        let verified = n(health, "last_verified_at", 0.);
        let recent = verified > 0. && (0. ..=86_400_000.).contains(&(now - verified));
        let confidence = if recent {
            if strategy == "reliability" { 15. } else { 4. }
        } else {
            0.
        };
        let measured_cost = kh["measured_tokens_ema"]
            .as_f64()
            .filter(|_| n(kh, "measured_samples", 0.) >= 3.);
        let measured_reference = n(kh, "measured_input_chars_ema", 4096.);
        let measured_reference = if measured_reference == 0. {
            4096.
        } else {
            measured_reference
        }
        .max(1024.);
        let token_penalty = if strategy == "token_efficiency" {
            measured_cost.map_or(5., |v| {
                (v * (size / measured_reference).max(1.) / 4096. * 2.).min(20.)
            })
        } else {
            0.
        };
        let fit = if has(&profile["preferred_kinds"], s(t, "kind")) {
            8.
        } else {
            0.
        };
        let penalty = n(&v["diversity"], domain, 0.) * 7.;
        let mut score = quality * qw
            + terminal * tw
            + 100. / (1. + eta / 20.) * swgt
            + rw * 12.
            + fit
            + confidence
            - penalty
            - token_penalty;
        let pool = if t["allowed_routes"].is_array() {
            t["allowed_routes"].as_array().cloned().unwrap_or_default()
        } else if s(&v["selection"], "mode") == "directed" {
            array(&v["selection"]["pool"])?
                .iter()
                .map(|r| r["route_id"].clone())
                .collect()
        } else {
            vec![]
        };
        if let Some(index) = pool.iter().position(|v| v.as_str() == Some(id)) {
            score += 1000. * (pool.len() - index) as f64;
        }
        score += if b(quota, "known") {
            n(quota, "remaining_percent", 0.) * 0.03
        } else {
            -2.
        };
        if s(t, "preferred_route") == id {
            score += 100.;
        }
        let matches: Vec<&str> = [
            ("preferred_sources", source),
            ("preferred_models", s(r, "model")),
            ("preferred_routes", id),
        ]
        .into_iter()
        .filter_map(|(key, value)| has(&p[key], value).then_some(key))
        .collect();
        let preference_boost = n(p, "weight", 40.) * matches.len() as f64;
        let parallel = n(r, "max_parallel", 2.);
        let free_provider =
            (parallel - n(inflight, &format!("provider:{domain}"), 0.)).max(0.) / parallel.max(1.);
        let concurrency_boost = n(p, "concurrency_bias", 0.).max(0.) * free_provider * 6.;
        let pricing = price(
            t,
            r,
            p,
            size.midpoint(4096.).ceil(),
            n(&g, "effective_output_tokens", 0.),
            now,
        );
        let newer = if p["prefer_new_models"]
            .as_bool()
            .unwrap_or(b(t, "prefer_new_models"))
            && b(meta, "latest_in_family")
            && n(meta, "distinct_versions", 0.) >= 2.
            && meta["version"].as_array().is_some_and(|a| !a.is_empty())
        {
            if recent { 5. } else { 1. }
        } else {
            0.
        };
        let free_boost =
            if b(&pricing, "free") && p["prefer_free"].as_bool().unwrap_or(b(t, "prefer_free")) {
                free_provider * 4.
            } else {
                0.
            };
        score += preference_boost
            + concurrency_boost
            + n(&pricing, "score_boost", 0.)
            + newer
            + free_boost;
        let weight = rw * pw * sw;
        score *= if strategy == "reliability" {
            weight * weight
        } else {
            weight
        };
        let mut candidate = json!({"route_id":id,"score":rounded(score,3),"eta_s":rounded(eta,2),"quality":rounded(quality,2),"observed_token_cost":kh["tokens_ema"],
            "reasons":{"quality_source":if intelligence.is_some(){"benchmark+feedback"}else{"heuristic+feedback"},"benchmark_fresh":fresh("intelligence_captured_at"),"latency_source":if measured.is_some(){"measured_route_ewma"}else{"prior"},
                "success_rate":rounded(rw,3),"quota_known":b(quota,"known"),"input_time_multiplier":rounded(multiplier,3),"reference_input_chars":rounded(reference,0),"latency_scale":rounded(scale,3),"latency_adjustment":adjustment,
                "deadline_basis":if window<remaining{"initial_response_window"}else{"absolute_execution_horizon"},"eta_window_s":rounded(window,2),"routing_strategy":strategy,"recent_generation_verified":recent,
                "task_preference_matches":matches,"task_preference_boost":preference_boost,"concurrency_boost":rounded(concurrency_boost,3),"price":pricing,"newer_model_boost":newer,"free_parallel_boost":rounded(free_boost,3),
                "token_cost_source":if measured_cost.is_some(){"reported_same_kind_history"}else{"unknown"},"token_efficiency_penalty":rounded(token_penalty,3),"availability_weight":rounded(weight,4),"source_weight":sw,"provider_weight":pw,"route_weight":rw,"penalty_half_life_s":1800,"preferred_task_fit":fit!=0.,"diversity_penalty":penalty}});
        candidate
            .as_object_mut()
            .unwrap()
            .extend(g.as_object().unwrap().clone());
        candidates.push(candidate);
    }
    candidates.sort_by(|a, b| {
        n(b, "score", 0.)
            .total_cmp(&n(a, "score", 0.))
            .then_with(|| s(a, "route_id").cmp(s(b, "route_id")))
    });
    Ok(json!({"candidates":candidates,"rejected":rejected}))
}
