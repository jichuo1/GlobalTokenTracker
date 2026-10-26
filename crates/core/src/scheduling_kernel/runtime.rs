use super::{array, b, execute, n, reservation, routing, s, storage::State, workflow};
use serde_json::{Value, json};

#[derive(Default)]
pub struct Kernel {
    state: Option<State>,
    catalog: Vec<Value>,
    catalog_key: String,
    calls: u64,
}

impl Kernel {
    pub fn handle(&mut self, request: &Value) -> Result<Value, String> {
        if request["protocol"].as_u64() != Some(super::PROTOCOL) {
            return Err("scheduler_kernel_protocol_mismatch".into());
        }
        self.calls += 1;
        let p = &request["payload"];
        match s(request, "operation") {
            "health" => Ok(
                json!({"protocol":super::PROTOCOL,"features":["rank","reserve","workflow","job_state","reports","state_v1","runtime_v1","admit_v1"],"state_open":self.state.is_some(),"calls":self.calls}),
            ),
            "db_open" => {
                if self.state.is_some() {
                    return Err("sqlite_operational:state_already_open".into());
                }
                self.state = Some(State::open(s(p, "path"))?);
                Ok(json!({"owner":"rust"}))
            }
            "db_close" => {
                self.state.take();
                self.catalog.clear();
                self.catalog_key.clear();
                Ok(Value::Null)
            }
            "db_exec" => self.state()?.exec(p),
            "db_script" => self.state()?.script(s(p, "sql")),
            "db_commit" => self.state()?.commit(),
            "db_health" => self.state()?.health_transition(p),
            "db_append_event" => self.state()?.append_event(p),
            "db_report_event" => self.state()?.report_event(p),
            "db_rollback" => self.state()?.rollback(),
            "db_job_get" => self.state()?.read("job", p),
            "db_job_put" => self.state()?.put("job", p),
            "db_run_get" => self.state()?.read("run", p),
            "db_run_put" => self.state()?.put("run", p),
            "db_catalog" => {
                let routes = array(&p["routes"])?;
                if routes.len() > 16384 {
                    return Err("scheduler_kernel_route_limit".into());
                }
                self.catalog.clone_from(routes);
                self.catalog_key = s(p, "key").into();
                Ok(json!({"key":self.catalog_key}))
            }
            "db_rank" => {
                let prepared = self.prepare_rank(p)?;
                routing::rank(&prepared)
            }
            "db_admit" => self.admit(p),
            "db_workflow" => {
                let run = self.state()?.run_projection(s(p, "run_id"))?;
                let mut states = run["nodes"].clone();
                if let Some(changed) = p["states"].as_object() {
                    for (id, value) in changed {
                        states[id]["state"] = value.clone();
                    }
                }
                workflow::advance(
                    &json!({"definition":run["definition"],"nodes":states,"controlled":s(&run,"mode")=="operator_v1",
                    "has_decisions":b(&run,"has_decisions"),"cancelled":b(p,"cancelled")||s(&run,"state")=="cancelling"}),
                )
            }
            "db_job_state" => self
                .state()?
                .job_state(s(p, "job_id"), b(p, "require_nonempty")),
            "db_reports" => {
                let run = self.state()?.run_projection(s(p, "run_id"))?;
                workflow::reports(
                    &json!({"definition":run["definition"],"source":p["source"],"kind":p["kind"]}),
                )
            }
            _ => execute(request),
        }
    }

    fn state(&mut self) -> Result<&mut State, String> {
        self.state
            .as_mut()
            .ok_or_else(|| "sqlite_operational:state_not_open".into())
    }

    fn prepare_rank(&mut self, p: &Value) -> Result<Value, String> {
        if self.catalog_key.is_empty() || s(p, "catalog_key") != self.catalog_key {
            return Err("runtime_catalog_changed".into());
        }
        let mut prepared = p.clone();
        let now = n(p, "now_ms", 0.);
        let selection = &p["selection"];
        let mut routes = self.catalog.clone();
        for route in &mut routes {
            route["selection_reason"] = if !p["selection_error"].is_null() {
                p["selection_error"].clone()
            } else {
                selection_reason(selection, route, now)
            };
            route["quota"] = quota_for(route, &p["ledger"], now);
        }
        let (health, feedback) = self.state()?.routing_snapshot(s(&p["task"], "kind"))?;
        prepared["routes"] = json!(routes);
        prepared["health"] = health;
        prepared["feedback"] = feedback;
        Ok(prepared)
    }

    fn admit(&mut self, p: &Value) -> Result<Value, String> {
        let prepared = self.prepare_rank(p)?;
        let ranked = routing::rank(&prepared)?;
        let candidates = array(&ranked["candidates"])?;
        if candidates.is_empty() {
            let rejected = array(&ranked["rejected"])?;
            let floor = n(&p["task"], "output_floor", 256.);
            let code = if floor > 256.
                && rejected
                    .iter()
                    .any(|r| super::has(&r["reasons"], "output_below_recovery_floor"))
            {
                "output_recovery_unavailable"
            } else if rejected.iter().any(|r| {
                r["reasons"].as_array().is_some_and(|rs| {
                    !rs.is_empty()
                        && rs.iter().all(|v| {
                            matches!(
                                v.as_str(),
                                Some("context_too_large" | "context_output_headroom_exhausted")
                            )
                        })
                })
            }) {
                "context_exceeded"
            } else {
                "no_eligible_route"
            };
            return Ok(json!({"error":code}));
        }
        let mut request = p["reservation"].clone();
        request["task"] = p["task"].clone();
        request["inflight"] = p["inflight"].clone();
        let rows=candidates.iter().map(|candidate|{
            let id=s(candidate,"route_id");let route=array(&prepared["routes"]).unwrap().iter().find(|r|s(r,"route_id")==id).unwrap();
            json!({"route":route,"history_tokens":prepared["health"][format!("route:{id}:kind:{}",s(&p["task"],"kind"))]["tokens_ema"]})
        }).collect::<Vec<_>>();
        request["candidates"] = json!(rows);
        let mut decision = reservation::reserve(&request)?;
        if let Some(id) = decision["route_id"].as_str() {
            let candidate = candidates
                .iter()
                .find(|c| s(c, "route_id") == id)
                .unwrap()
                .clone();
            decision["candidate"] = candidate;
        }
        Ok(decision)
    }
}

fn selection_reason(value: &Value, route: &Value, now: f64) -> Value {
    if !b(value, "enabled") {
        return json!("scheduling_disabled");
    }
    if value["sources"][s(route, "source")].as_bool() == Some(false) {
        return json!("source_dispatch_disabled");
    }
    if s(value, "mode") == "directed" {
        let pool = value["pool"].as_array();
        let live = pool
            .into_iter()
            .flatten()
            .filter(|r| n(r, "expires_at", 0.) > now)
            .collect::<Vec<_>>();
        if live.is_empty() {
            return json!(if pool.is_some_and(|p| !p.is_empty()) {
                "directed_pool_expired"
            } else {
                "directed_pool_empty"
            });
        }
        if !live
            .iter()
            .any(|r| s(r, "route_id") == s(route, "route_id"))
        {
            return json!("route_not_selected");
        }
    }
    Value::Null
}

fn quota_for(route: &Value, ledger: &Value, now: f64) -> Value {
    let values = ledger["quotas"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|q| {
            s(q, "app") == s(route, "quota_app") && q["account_key"] == route["quota_account"]
        })
        .filter(|q| {
            ledger["captured_at"].is_null() || {
                let captured = n(q, "captured_at", 0.);
                captured > 0.
                    && captured <= now
                    && now - captured <= n(ledger, "quota_ttl_s", 300.) * 1000.
                    && q["resets_at"].as_f64().is_none_or(|v| v > now)
            }
        })
        .filter_map(|q| {
            q["remaining_percent"]
                .as_f64()
                .filter(|v| (0. ..=100.).contains(v))
        });
    let remaining = values.reduce(f64::min);
    json!({"known":remaining.is_some(),"remaining_percent":remaining})
}
