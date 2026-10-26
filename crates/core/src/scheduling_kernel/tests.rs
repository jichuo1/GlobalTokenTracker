use super::execute;
use serde_json::{Value, json};

fn call(operation: &str, payload: Value) -> Value {
    execute(&json!({"protocol":1,"operation":operation,"payload":payload})).unwrap()
}

#[test]
fn unknown_paid_results_are_not_success_and_empty_operator_reports_fail() {
    for state in ["failed", "partial", "interrupted"] {
        assert_eq!(
            call(
                "job_state",
                json!({"state":state,"results":[],"attempts":[{"unknown_paid_outcome":true}]})
            )["state"],
            "unknown"
        );
    }
    assert_eq!(
        call(
            "job_state",
            json!({"state":"succeeded","results":[],"require_nonempty":true})
        )["state"],
        "failed"
    );
    assert_eq!(
        call(
            "job_state",
            json!({"state":"succeeded","results":[{"ok":true}]})
        )["state"],
        "succeeded"
    );
}

#[test]
fn bidirectional_reports_do_not_block_an_operator_start_or_accept_for_codex() {
    let definition = json!({"nodes":[{"id":"c","node_type":"codex"},{"id":"o","node_type":"operator"},{"id":"r","node_type":"reviewer"}],
        "edges":[{"from":"c","to":"o"},{"from":"o","to":"r","kind":"report"},{"from":"r","to":"c","kind":"report"}]});
    let payload = json!({"definition":definition,"controlled":true,"has_decisions":true,
        "nodes":{"c":{"state":"succeeded"},"o":{"state":"pending"},"r":{"state":"pending"}}});
    let result = call("workflow", payload.clone());
    assert_eq!(result["nodes"][1]["readiness"], "ready");
    assert_eq!(result["nodes"][2]["readiness"], "wait");
    let mut done = payload;
    done["nodes"]["o"]["state"] = json!("succeeded");
    done["nodes"]["r"]["state"] = json!("succeeded");
    assert_eq!(call("workflow", done)["state"], "awaiting_codex");
}

#[test]
fn report_progress_traverses_reviewers_but_results_wait_for_actual_review() {
    let definition = json!({"nodes":[{"id":"o","node_type":"operator"},{"id":"r","node_type":"reviewer"},{"id":"c","node_type":"codex"}],
        "edges":[{"from":"o","to":"r","kind":"report"},{"from":"r","to":"c","kind":"report"}]});
    assert_eq!(
        call(
            "reports",
            json!({"definition":definition,"source":"o","kind":"progress"})
        )["targets"],
        json!(["r", "c"])
    );
    assert_eq!(
        call(
            "reports",
            json!({"definition":definition,"source":"o","kind":"operation_result"})
        )["targets"],
        json!(["r"])
    );
}

#[test]
fn protocol_mismatch_and_report_cycles_are_rejected() {
    assert!(execute(&json!({"protocol":2,"operation":"health"})).is_err());
    let definition = json!({"nodes":[{"id":"a"},{"id":"b"}],"edges":[{"from":"a","to":"b","kind":"report"},{"from":"b","to":"a","kind":"report"}]});
    assert!(execute(&json!({"protocol":1,"operation":"reports","payload":{"definition":definition,"source":"a","kind":"progress"}})).is_err());
}

#[test]
fn reserved_capacity_prevents_overcommit_and_estimates_are_not_added_twice() {
    let base = json!({"task":{"max_output_tokens":1000,"output_mode":"fixed"},"prompt_chars":200,
        "job":{"max_attempts":3,"attempt_count":0,"tokens_used":1000,"tokens_reserved":4000,"tokens_estimated":1000,"token_budget":5000},
        "limits":{},"candidates":[{"route":{"route_id":"r","source":"s","failure_domain":"p"}}]});
    assert_eq!(
        call("reserve", base.clone())["waiting_reason"],
        "token_budget_exhausted"
    );
    let mut available = base;
    available["job"]["tokens_reserved"] = json!(0);
    let result = call("reserve", available);
    assert_eq!(result["route_id"], "r");
    assert_eq!(result["reserved_token_estimate"], 1100);
}
