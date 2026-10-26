//! Scheduling decisions and the coordinator's durable state owner. Native
//! credentials, model calls, workspace operations and paid retries remain
//! outside this boundary.
mod reservation;
mod routing;
mod runtime;
mod storage;
mod transitions;
pub use runtime::Kernel;
#[cfg(test)]
mod tests;
mod workflow;

use serde_json::{Value, json};

pub const PROTOCOL: u64 = 1;

pub fn execute(request: &Value) -> Result<Value, String> {
    if request["protocol"].as_u64() != Some(PROTOCOL) {
        return Err("scheduler_kernel_protocol_mismatch".into());
    }
    match s(request, "operation") {
        "health" => Ok(
            json!({"protocol":PROTOCOL,"features":["rank","reserve","workflow","job_state","reports"]}),
        ),
        "rank" => routing::rank(&request["payload"]),
        "reserve" => reservation::reserve(&request["payload"]),
        "workflow" => workflow::advance(&request["payload"]),
        "job_state" => Ok(workflow::job_state(&request["payload"])),
        "reports" => workflow::reports(&request["payload"]),
        _ => Err("scheduler_kernel_unknown_operation".into()),
    }
}

pub(super) fn n(value: &Value, key: &str, default: f64) -> f64 {
    value[key]
        .as_f64()
        .filter(|v| v.is_finite())
        .unwrap_or(default)
}
pub(super) fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
pub(super) fn b(value: &Value, key: &str) -> bool {
    value[key].as_bool().unwrap_or(false)
}
pub(super) fn has(values: &Value, text: &str) -> bool {
    values
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(text)))
}
pub(super) fn array(value: &Value) -> Result<&Vec<Value>, String> {
    value
        .as_array()
        .ok_or_else(|| "scheduler_kernel_invalid_array".into())
}
// Match Python's binary64 round(), including half-even ties, instead of
// rounding a multiplied value (which can introduce a second rounding).
pub(super) fn rounded(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(value)
}

pub(super) fn generation(task: &Value, route: &Value, size: f64) -> Value {
    let base = n(task, "max_output_tokens", 8192.);
    let cap = n(task, "output_cap", 524_288.);
    let mode = s(task, "output_mode");
    let requested = if mode == "fixed" {
        base
    } else {
        let scale = (size.max(1.) / 8000.).sqrt().max(1.);
        cap.min(n(task, "output_target", 0.).max((base * scale / 8192.).ceil() * 8192.))
    };
    let mut effective = route["output_limit"]
        .as_f64()
        .filter(|v| *v > 0.)
        .map_or(requested, |v| {
            if mode == "native" {
                v
            } else {
                requested.min(v)
            }
        })
        .min(cap);
    if n(route, "context_window", 0.) > 0. {
        effective = effective.min(n(route, "context_window", 0.) - (size / 2.).ceil());
    }
    let budget_limit = if mode == "fixed" {
        Value::Null
    } else {
        let remaining = n(
            task,
            "budget_remaining_tokens",
            n(task, "token_budget", 4_000_000.),
        );
        let available = (remaining.floor() - (size / 2.).ceil()).max(0.);
        effective = effective.min(available);
        json!(available as i64)
    };
    json!({"requested_output_tokens":requested as i64,"effective_output_tokens":effective as i64,
        "output_mode":mode,"native_output_limit":route["output_limit"],"budget_output_limit":budget_limit,
        "minimum_output_tokens":n(task,"output_floor",256.) as i64})
}
