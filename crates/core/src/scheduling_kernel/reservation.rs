use super::{array, b, generation, n, s};
use serde_json::{Value, json};

/// One decision against a snapshot taken under the coordinator's capacity lock.
/// Python applies the returned delta while retaining that lock. There is never
/// a second resource ledger, replay, or fall back after a failed kernel call.
pub(super) fn reserve(v: &Value) -> Result<Value, String> {
    let t = &v["task"];
    let job = &v["job"];
    let limits = &v["limits"];
    let inflight = &v["inflight"];
    if n(job, "attempt_count", 0.) >= n(job, "max_attempts", 0.) {
        return Ok(json!({"error":"attempt_budget_exhausted"}));
    }
    let mut fit = false;
    let mut potential = false;
    let mut capacity = false;
    let mut permanent = None;
    let mut reservation = None;
    let floor = n(t, "output_floor", 256.);
    let input = (n(v, "prompt_chars", 0.) / 2.).ceil();
    let exploration = b(v, "exploration");
    let ceiling = n(job, "token_budget", 0.);
    let exploration_ceiling = (ceiling * n(v, "exploration_ratio", 0.)).trunc();
    let mut remaining = ceiling - n(job, "tokens_used", 0.);
    if exploration {
        remaining = remaining.min(exploration_ceiling - n(job, "exploration_tokens_used", 0.));
    }
    let mut budget_task = t.clone();
    budget_task["budget_remaining_tokens"] = json!(remaining);
    for c in array(&v["candidates"])? {
        let route = &c["route"];
        let source = s(route, "source");
        let domain = s(route, "failure_domain");
        let mut g = generation(&budget_task, route, n(v, "prompt_chars", 0.));
        let mut output = n(&g, "effective_output_tokens", 0.);
        if output < floor {
            if g["budget_output_limit"]
                .as_f64()
                .is_some_and(|limit| limit < floor)
            {
                permanent = Some("token_budget_exhausted");
            }
            continue;
        }
        fit = true;
        if floor > 256. {
            let mut available = ceiling - n(job, "tokens_used", 0.) - input;
            if exploration {
                available = available
                    .min(exploration_ceiling - n(job, "exploration_tokens_used", 0.) - input);
            }
            if available >= floor {
                output = output.min(available.trunc());
                g["effective_output_tokens"] = json!(output as u64);
            }
        }
        let estimate = (input + output).max(n(c, "history_tokens", 0.).ceil());
        let reason = if n(job, "tokens_used", 0.) + estimate > ceiling {
            Some("token_budget_exhausted")
        } else if exploration
            && n(job, "exploration_tokens_used", 0.) + estimate > exploration_ceiling
        {
            Some("exploration_budget_exhausted")
        } else {
            None
        };
        if reason.is_some() {
            permanent = permanent.or(reason);
            continue;
        }
        potential = true;
        if n(job, "tokens_used", 0.) + n(job, "tokens_reserved", 0.) + estimate > ceiling {
            reservation = Some("token_budget_exhausted");
            continue;
        }
        if exploration
            && n(job, "exploration_tokens_used", 0.)
                + n(job, "exploration_tokens_reserved", 0.)
                + estimate
                > exploration_ceiling
        {
            reservation = Some("exploration_budget_exhausted");
            continue;
        }
        let global = n(limits, "global_parallel", 4.);
        let foreground = n(limits, "group_reserved_foreground_slots", 1.)
            .min(global - 1.)
            .max(0.);
        if b(job, "research_group") && n(v, "total_inflight", 0.) >= global - foreground {
            capacity = true;
            continue;
        }
        if n(v, "total_inflight", 0.) >= global
            || n(v, "total_inflight", 0.) + n(v, "active_codex", 0.)
                >= n(limits, "combined_parallel", 6.)
            || n(inflight, source, 0.)
                >= n(
                    &limits["source_parallel"],
                    source,
                    n(route, "max_parallel", 2.),
                )
            || n(inflight, &format!("provider:{domain}"), 0.)
                >= n(&t["source_parallel"], source, n(route, "max_parallel", 2.))
        {
            capacity = true;
            continue;
        }
        return Ok(
            json!({"route_id":route["route_id"],"reserved_token_estimate":estimate as u64,"exploration":exploration,"generation":g}),
        );
    }
    if !fit {
        return Ok(json!({"error":permanent.unwrap_or("context_exceeded")}));
    }
    if !potential {
        return Ok(json!({"error":permanent.unwrap_or("token_budget_exhausted")}));
    }
    let waiting = if capacity {
        "queue_deadline"
    } else {
        reservation.unwrap_or("queue_deadline")
    };
    Ok(
        json!({"waiting_reason":waiting,"exhausted_reason":if capacity{"external_capacity_full"}else{reservation.unwrap_or("external_capacity_full")}}),
    )
}
