use super::{array, b, s};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

fn terminal(state: &str) -> bool {
    matches!(
        state,
        "succeeded" | "failed" | "blocked" | "unknown" | "cancelled" | "interrupted"
    )
}

type Adjacency = BTreeMap<String, Vec<String>>;

fn indexes(definition: &Value) -> Result<(Adjacency, Adjacency), String> {
    let mut tasks: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut reports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let nodes = array(&definition["nodes"])?;
    if nodes.len() > 256 || array(&definition["edges"])?.len() > 4096 {
        return Err("scheduler_kernel_graph_limit".into());
    }
    let ids: BTreeSet<&str> = nodes.iter().map(|n| s(n, "id")).collect();
    if ids.len() != nodes.len() || ids.contains("") {
        return Err("scheduler_kernel_invalid_nodes".into());
    }
    for e in array(&definition["edges"])? {
        let from = s(e, "from");
        let to = s(e, "to");
        if from == to || !ids.contains(from) || !ids.contains(to) {
            return Err("scheduler_kernel_invalid_edge".into());
        }
        match e["kind"].as_str().unwrap_or("task") {
            "task" => tasks.entry(to.into()).or_default().push(from.into()),
            "report" => reports.entry(to.into()).or_default().push(from.into()),
            _ => return Err("scheduler_kernel_invalid_edge_kind".into()),
        }
    }
    // Fail closed even when called without the Python protocol validator.
    for incoming in [&tasks, &reports] {
        let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (target, sources) in incoming {
            for source in sources {
                outgoing.entry(source).or_default().push(target);
            }
        }
        let mut degrees: BTreeMap<&str, usize> = ids
            .iter()
            .map(|id| (*id, incoming.get(*id).map_or(0, Vec::len)))
            .collect();
        let mut ready: VecDeque<&str> = degrees
            .iter()
            .filter_map(|(id, n)| (*n == 0).then_some(*id))
            .collect();
        let mut visited = 0;
        while let Some(id) = ready.pop_front() {
            visited += 1;
            for target in outgoing.get(id).into_iter().flatten() {
                let degree = degrees.get_mut(target).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(target);
                }
            }
        }
        if visited != nodes.len() {
            return Err("scheduler_kernel_graph_cycle".into());
        }
    }
    Ok((tasks, reports))
}

fn report_sources(
    id: &str,
    kind: &str,
    outgoing: &Adjacency,
    reports: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    if matches!(kind, "codex" | "operator" | "input") || !reports.contains_key(id) {
        return vec![];
    }
    let mut descendants = BTreeSet::new();
    let mut pending = vec![id.to_string()];
    while let Some(current) = pending.pop() {
        for target in outgoing.get(&current).into_iter().flatten() {
            if descendants.insert(target.clone()) {
                pending.push(target.clone());
            }
        }
    }
    reports
        .get(id)
        .into_iter()
        .flatten()
        .filter(|source| kind == "reviewer" || !descendants.contains(*source))
        .cloned()
        .collect()
}

pub(super) fn advance(v: &Value) -> Result<Value, String> {
    let d = &v["definition"];
    let (tasks, reports) = indexes(d)?;
    let mut outgoing: Adjacency = BTreeMap::new();
    for (target, sources) in &tasks {
        for source in sources {
            outgoing
                .entry(source.clone())
                .or_default()
                .push(target.clone());
        }
    }
    let states = &v["nodes"];
    let controlled = b(v, "controlled");
    let mut rows = Vec::new();
    for spec in array(&d["nodes"])? {
        let id = s(spec, "id");
        let state = s(&states[id], "state");
        if !terminal(state)
            && !matches!(
                state,
                "pending" | "awaiting_assignment" | "awaiting_plan" | "running" | "executing"
            )
        {
            return Err("scheduler_kernel_invalid_node_state".into());
        }
        let kind = s(spec, "node_type");
        let deps = tasks.get(id).cloned().unwrap_or_default();
        let sources = if controlled {
            report_sources(id, kind, &outgoing, &reports)
        } else {
            reports.get(id).cloned().unwrap_or_default()
        };
        let readiness =
            if b(v, "cancelled") {
                if matches!(state, "running" | "executing") {
                    "cancel_active"
                } else if !terminal(state) {
                    "cancel"
                } else {
                    "wait"
                }
            } else if terminal(state)
                || !matches!(state, "pending" | "awaiting_assignment")
                || (controlled && (!b(v, "has_decisions") || kind == "codex"))
            {
                "wait"
            } else if deps.iter().chain(&sources).any(|id| {
                terminal(s(&states[id], "state")) && s(&states[id], "state") != "succeeded"
            }) {
                "block"
            } else if deps
                .iter()
                .chain(&sources)
                .any(|id| s(&states[id], "state") != "succeeded")
            {
                "wait"
            } else {
                "ready"
            };
        rows.push(
            json!({"node_id":id,"dependencies":deps,"reports":sources,"readiness":readiness}),
        );
    }
    let all_terminal = array(&d["nodes"])?
        .iter()
        .all(|n| terminal(s(&states[s(n, "id")], "state")));
    let active = array(&d["nodes"])?
        .iter()
        .filter(|n| matches!(s(&states[s(n, "id")], "state"), "running" | "executing"))
        .count();
    let run_state = if b(v, "cancelled") {
        if active == 0 {
            "cancelled"
        } else {
            "cancelling"
        }
    } else if controlled && b(v, "has_decisions") && all_terminal {
        "awaiting_codex"
    } else if !controlled && all_terminal {
        if array(&d["nodes"])?
            .iter()
            .all(|n| s(&states[s(n, "id")], "state") == "succeeded")
        {
            "succeeded"
        } else {
            "failed"
        }
    } else if controlled && active == 0 {
        "awaiting_codex"
    } else {
        "running"
    };
    Ok(
        json!({"nodes":rows,"state":run_state,"execution_complete":controlled&&b(v,"has_decisions")&&all_terminal}),
    )
}

pub(super) fn job_state(job: &Value) -> Value {
    let done = matches!(
        s(job, "state"),
        "succeeded" | "partial" | "failed" | "cancelled" | "interrupted" | "needs_codex"
    );
    let state = if !done {
        "running"
    } else if s(job, "state") == "succeeded"
        && job["results"].as_array().is_some_and(|a| {
            (!b(job, "require_nonempty") || !a.is_empty()) && a.iter().all(|r| b(r, "ok"))
        })
    {
        "succeeded"
    } else if s(job, "state") == "interrupted"
        || job["attempts"].as_array().is_some_and(|a| {
            a.iter()
                .any(|r| s(&r["error"], "code") == "timeout" || b(r, "unknown_paid_outcome"))
        })
    {
        "unknown"
    } else {
        "failed"
    };
    json!({"terminal":done,"state":state})
}

pub(super) fn reports(v: &Value) -> Result<Value, String> {
    let d = &v["definition"];
    indexes(d)?;
    let source = s(v, "source");
    let kind = s(v, "kind");
    let types: BTreeMap<&str, &str> = array(&d["nodes"])?
        .iter()
        .map(|n| (s(n, "id"), s(n, "node_type")))
        .collect();
    if !types.contains_key(source) {
        return Err("scheduler_kernel_unknown_node".into());
    }
    let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    // Preserve source-specific edge order, also used by event cursors.
    for edge in array(&d["edges"])? {
        if s(edge, "kind") == "report" {
            outgoing
                .entry(s(edge, "from"))
                .or_default()
                .push(s(edge, "to"));
        }
    }
    let mut pending = VecDeque::from([source]);
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    while let Some(current) = pending.pop_front() {
        for target in outgoing.get(current).into_iter().flatten() {
            if seen.insert(*target) {
                targets.push(*target);
                let ty = types[target];
                if ty != "codex"
                    && (kind == "progress"
                        || matches!(ty, "operator" | "merge" | "gate" | "output"))
                {
                    pending.push_back(target);
                }
            }
        }
    }
    Ok(json!({"targets":targets}))
}
