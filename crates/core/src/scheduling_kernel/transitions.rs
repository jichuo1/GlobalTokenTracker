//! Native transactions for frequently updated health and progress journals.
use super::{
    array, b, n, s,
    storage::{State, db_error},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

impl State {
    pub fn atomic<T>(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let own = self.conn.is_autocommit();
        if own {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(db_error)?;
        }
        let result = action(self);
        if own {
            if result.is_ok() {
                self.commit()?;
            } else {
                self.rollback()?;
            }
        }
        result
    }

    pub fn health_transition(&mut self, p: &Value) -> Result<Value, String> {
        self.atomic(|store|{
            let now=n(p,"now_ms",0.) as i64;
            for change in array(&p["changes"])?{
                let key=s(change,"key");
                let prior=store.exec(&json!({"sql":"SELECT * FROM health WHERE key=?","params":[key]}))?;
                let mut h=json!({});
                if let Some(row)=array(&prior["rows"])?.first(){
                    for (name,value) in array(&prior["columns"])?.iter().zip(array(row)?){h[name.as_str().unwrap()]=value.clone();}
                }
                let age=(now as f64-n(&h,"penalty_updated_at",0.)).max(0.)/1000.;
                let penalty=n(&h,"penalty",0.)*(-std::f64::consts::LN_2*age/1800.).exp();
                match s(change,"type"){
                    "success"=>{
                        let duration=n(change,"duration_ms",0.);
                        let latency=h["latency_ms"].as_f64().map_or(duration,|v|0.7*v+0.3*duration);
                        store.conn.execute("INSERT INTO health(key,successes,latency_ms) VALUES(?1,1,?2) ON CONFLICT(key) DO UPDATE SET successes=successes+1,latency_ms=excluded.latency_ms,cooldown_until=0,last_error=NULL",params![key,latency]).map_err(db_error)?;
                        store.conn.execute("UPDATE health SET penalty=?1,penalty_updated_at=?2,failure_streak=0,next_probe_at=0,probe_failures=0,verification_required=0,last_verified_at=?2 WHERE key=?3",params![penalty*0.35,now,key]).map_err(db_error)?;
                        for (field,input) in [("tokens_ema","tokens"),("input_chars_ema","input_chars"),("measured_tokens_ema","measured_tokens")]{
                            if let Some(value)=change[input].as_f64(){
                                let value=h[field].as_f64().map_or(value,|prior|0.7*prior+0.3*value);
                                let extra=if input=="measured_tokens"{",measured_samples=measured_samples+1"}else{""};
                                store.conn.execute(&format!("UPDATE health SET {field}=?1{extra} WHERE key=?2"),params![value,key]).map_err(db_error)?;
                            }
                        }
                        if change["measured_tokens"].is_number()
                            && let Some(input)=change["input_chars"].as_f64(){
                                let value=h["measured_input_chars_ema"].as_f64().map_or(input,|v|0.7*v+0.3*input);
                                store.conn.execute("UPDATE health SET measured_input_chars_ema=?1 WHERE key=?2",params![value,key]).map_err(db_error)?;
                        }
                    },
                    "failure"=>{
                        let code=s(change,"code");let cooldown=n(change,"cooldown_s",0.);
                        store.conn.execute("INSERT INTO health(key,failures,cooldown_until,last_error) VALUES(?1,1,?2,?3) ON CONFLICT(key) DO UPDATE SET failures=failures+1,cooldown_until=MAX(cooldown_until,excluded.cooldown_until),last_error=excluded.last_error",params![key,now+(cooldown*1000.) as i64,code]).map_err(db_error)?;
                        store.conn.execute("UPDATE health SET penalty=?1,penalty_updated_at=?2,failure_streak=failure_streak+1,next_probe_at=?3 WHERE key=?4",
                            params![(penalty+n(change,"penalty_step",0.16)).min(0.8),now,now+(cooldown.max(n(change,"recovery_delay_s",1800.))*1000.) as i64,key]).map_err(db_error)?;
                        if matches!(code,"auth_failed"|"quota_exhausted"|"model_unavailable"|"native_protocol_mismatch"|"native_configuration_error"|"reasoning_level_required")
                            || (key.starts_with("route:")&&n(&h,"failure_streak",0.)>=2.&&matches!(code,"timeout"|"provider_request_failed"|"adapter_error")){
                            store.conn.execute("UPDATE health SET verification_required=1 WHERE key=?1",[key]).map_err(db_error)?;
                        }
                    },
                    "recovered"=>{
                        store.conn.execute("INSERT INTO health(key,last_verified_at,last_probe_at) VALUES(?1,?2,?2) ON CONFLICT(key) DO UPDATE SET penalty=0,penalty_updated_at=excluded.last_verified_at,failure_streak=0,cooldown_until=0,next_probe_at=0,probe_failures=0,last_probe_at=excluded.last_probe_at,last_error=NULL,verification_required=0,last_verified_at=excluded.last_verified_at",params![key,now]).map_err(db_error)?;
                    },
                    "defer"=>{
                        store.conn.execute("UPDATE health SET next_probe_at=?1,last_probe_at=?2,probe_failures=probe_failures+?3 WHERE key=?4",params![now+(n(change,"delay_s",0.)*1000.) as i64,now,i64::from(b(change,"failed")),key]).map_err(db_error)?;
                    },_=>return Err("runtime_unknown_health_transition".into()),
                }
            }
            Ok(Value::Null)
        })
    }

    pub fn append_event(&mut self, p: &Value) -> Result<Value, String> {
        self.atomic(|store|{
            let workflow=s(p,"workflow_id");let event=s(p,"event_id");let payload=s(p,"payload");let now=n(p,"now_ms",0.) as i64;
            store.conn.execute("DELETE FROM coordination_events WHERE expires_at<=?1",[now]).map_err(db_error)?;
            let prior=store.conn.query_row("SELECT seq,payload FROM coordination_events WHERE workflow_id=?1 AND event_id=?2",params![workflow,event],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?))).optional().map_err(db_error)?;
            if let Some((seq,old))=prior{
                if old!=payload{return Err("runtime_event_conflict".into());}
                return Ok(json!({"event_id":event,"cursor":seq,"deduplicated":true}));
            }
            let count:i64=store.conn.query_row("SELECT COUNT(*) FROM coordination_events WHERE workflow_id=?1 AND expires_at>?2",params![workflow,now],|r|r.get(0)).map_err(db_error)?;
            if count>=10000{return Err("runtime_event_capacity".into());}
            store.conn.execute("INSERT INTO coordination_events(workflow_id,event_id,created_at,expires_at,payload) VALUES(?1,?2,?3,?4,?5)",params![workflow,event,now,now+(n(p,"ttl_s",21600.)*1000.) as i64,payload]).map_err(db_error)?;
            Ok(json!({"event_id":event,"cursor":store.conn.last_insert_rowid(),"deduplicated":false}))
        })
    }

    pub fn report_event(&mut self, p: &Value) -> Result<Value, String> {
        self.atomic(|store|{
            let run=store.run_projection(s(p,"run_id"))?;
            let targets=super::workflow::reports(&json!({"definition":run["definition"],"source":p["source"],"kind":p["kind"]}))?;
            for target in array(&targets["targets"])?{
                let value=json!({"event_id":p["event_id"],"source_node":p["source"],"target_node":target,"kind":p["kind"],"work_revision":p["work_revision"],"at":p["now_ms"],"payload":p["payload"]});
                let identity=format!("{}:revision:{}:{}",s(p,"event_id"),n(p,"work_revision",1.) as u64,target.as_str().unwrap());
                store.conn.execute("INSERT OR IGNORE INTO workflow_reports(run_id,event_id,payload) VALUES(?1,?2,?3)",params![s(p,"run_id"),identity,value.to_string()]).map_err(db_error)?;
            }
            Ok(targets)
        })
    }
}
