//! The coordinator database is owned by this process. Python retains a small
//! SQL compatibility surface while hot job/run writes use atomic native APIs.
use super::{array, n, s};
use rusqlite::{
    Connection, OptionalExtension, params, params_from_iter,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    path::Path,
    time::Duration,
};

pub(super) fn db_error(error: rusqlite::Error) -> String {
    let class = if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        "sqlite_integrity"
    } else {
        "sqlite_operational"
    };
    format!("{class}:{error}")
}

fn sql_value(value: &Value) -> Result<SqlValue, String> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(b) => Ok(SqlValue::Integer(i64::from(*b))),
        Value::String(s) => Ok(SqlValue::Text(s.clone())),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .ok_or_else(|| "sqlite_operational:unsupported number".into()),
        Value::Object(o) if o.len() == 1 && o.contains_key("$blob") => array(&o["$blob"])?
            .iter()
            .map(|v| {
                v.as_u64()
                    .and_then(|v| u8::try_from(v).ok())
                    .ok_or_else(|| "sqlite_operational:invalid blob".into())
            })
            .collect::<Result<Vec<_>, String>>()
            .map(SqlValue::Blob),
        _ => Err("sqlite_operational:unsupported parameter type".into()),
    }
}

fn json_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => json!(v),
        ValueRef::Real(v) => json!(v),
        ValueRef::Text(v) => json!(String::from_utf8_lossy(v)),
        ValueRef::Blob(v) => json!({"$blob":v}),
    }
}

pub(super) struct State {
    pub conn: Connection,
    _owner: File,
    tracked: Vec<&'static str>,
    cache: HashMap<(String, String), (u64, Value)>,
}

impl State {
    pub fn open(path: &str) -> Result<Self, String> {
        let path = Path::new(path);
        if !path.is_absolute() {
            return Err("sqlite_operational:state path must be absolute".into());
        }
        let parent = path
            .parent()
            .ok_or("sqlite_operational:invalid state path")?;
        let parent = parent
            .canonicalize()
            .map_err(|_| "sqlite_operational:state directory unavailable")?;
        let path = parent.join(
            path.file_name()
                .ok_or("sqlite_operational:invalid state name")?,
        );
        let owner = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("owner.lock"))
            .map_err(|_| "sqlite_operational:state owner lock unavailable")?;
        owner
            .try_lock()
            .map_err(|_| "sqlite_operational:state_owner_busy")?;
        let conn = Connection::open(path).map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(3))
            .map_err(db_error)?;
        conn.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS runtime_versions(kind TEXT NOT NULL,id TEXT NOT NULL,revision INTEGER NOT NULL,PRIMARY KEY(kind,id));").map_err(db_error)?;
        Ok(Self {
            conn,
            _owner: owner,
            tracked: Vec::new(),
            cache: HashMap::new(),
        })
    }

    fn tracking(&mut self) -> Result<(), String> {
        for (table, kind) in [("jobs", "job"), ("visual_runs", "run")] {
            if self.tracked.contains(&table) {
                continue;
            }
            let exists: bool = self
                .conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [table],
                    |r| r.get(0),
                )
                .map_err(db_error)?;
            if !exists {
                continue;
            }
            for (event, prefix) in [("INSERT", "NEW"), ("UPDATE", "NEW"), ("DELETE", "OLD")] {
                // All interpolated identifiers are the closed constants above.
                self.conn.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS runtime_{table}_{event} AFTER {event} ON {table} BEGIN
                    INSERT INTO runtime_versions(kind,id,revision) VALUES('{kind}',{prefix}.id,1)
                    ON CONFLICT(kind,id) DO UPDATE SET revision=revision+1; END;")).map_err(db_error)?;
            }
            self.tracked.push(table);
        }
        Ok(())
    }

    pub fn script(&mut self, sql: &str) -> Result<Value, String> {
        self.commit()?;
        self.conn.execute_batch(sql).map_err(db_error)?;
        self.cache.clear();
        self.tracking()?;
        Ok(Value::Null)
    }

    pub fn exec(&mut self, p: &Value) -> Result<Value, String> {
        let sql = s(p, "sql");
        let values = array(&p["params"])?
            .iter()
            .map(sql_value)
            .collect::<Result<Vec<_>, _>>()?;
        let verb = sql
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        // Match sqlite3's implicit DML transaction and explicit BEGIN behavior.
        if matches!(verb.as_str(), "INSERT" | "UPDATE" | "DELETE" | "REPLACE")
            && self.conn.is_autocommit()
        {
            self.conn
                .execute_batch("BEGIN DEFERRED")
                .map_err(db_error)?;
        }
        let (columns, rows, readonly) = {
            let mut statement = self.conn.prepare(sql).map_err(db_error)?;
            let readonly = statement.readonly();
            let columns = statement
                .column_names()
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>();
            let count = columns.len();
            let mut rows = Vec::new();
            if count > 0 {
                let mut cursor = statement
                    .query(params_from_iter(values))
                    .map_err(db_error)?;
                while let Some(row) = cursor.next().map_err(db_error)? {
                    rows.push(
                        (0..count)
                            .map(|i| row.get_ref(i).map(json_value).map_err(db_error))
                            .collect::<Result<Vec<_>, _>>()?,
                    );
                }
            } else {
                statement
                    .execute(params_from_iter(values))
                    .map_err(db_error)?;
            }
            (columns, rows, readonly)
        };
        let runtime_changed = !readonly
            && (sql.to_ascii_uppercase().contains("JOBS")
                || sql.to_ascii_uppercase().contains("VISUAL_RUNS")
                || sql.to_ascii_uppercase().contains("RUNTIME_VERSIONS"));
        if matches!(verb.as_str(), "CREATE" | "DROP" | "ALTER") {
            self.cache.clear();
        }
        self.tracking()?;
        Ok(
            json!({"columns":columns,"rows":rows,"rowcount":if readonly{-1}else{self.conn.changes() as i64},"lastrowid":self.conn.last_insert_rowid(),"in_transaction":!self.conn.is_autocommit(),"runtime_changed":runtime_changed}),
        )
    }

    pub fn commit(&mut self) -> Result<Value, String> {
        if !self.conn.is_autocommit()
            && let Err(error) = self.conn.execute_batch("COMMIT")
        {
            let _ = self.conn.execute_batch("ROLLBACK");
            self.cache.clear();
            return Err(db_error(error));
        }
        Ok(Value::Null)
    }
    pub fn rollback(&mut self) -> Result<Value, String> {
        if !self.conn.is_autocommit() {
            self.conn.execute_batch("ROLLBACK").map_err(db_error)?;
        }
        self.cache.clear();
        Ok(Value::Null)
    }
    fn revision(&self, kind: &str, id: &str) -> Result<u64, String> {
        Ok(self
            .conn
            .query_row(
                "SELECT revision FROM runtime_versions WHERE kind=?1 AND id=?2",
                params![kind, id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(db_error)?
            .unwrap_or(0) as u64)
    }

    fn load(&mut self, kind: &str, id: &str) -> Result<Option<u64>, String> {
        self.tracking()?;
        let revision = self.revision(kind, id)?;
        let key = (kind.into(), id.into());
        if let Some((saved, _)) = self.cache.get(&key)
            && *saved == revision
        {
            return Ok(Some(revision));
        }
        let value=match kind {
            "job"=>self.conn.query_row("SELECT state,created_at,updated_at,task,detail FROM jobs WHERE id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?))).optional().map_err(db_error)?
                .map(|(state,created,updated,task,detail)|Ok::<Value,String>(json!({"job_id":id,"state":state,"created_at":created,"updated_at":updated,"task":decode(&task)?,"detail":decode(&detail)?}))).transpose()?,
            "run"=>self.conn.query_row("SELECT payload FROM visual_runs WHERE id=?1",[id],|r|r.get::<_,String>(0)).optional().map_err(db_error)?.map(|v|decode(&v)).transpose()?,
            _=>return Err("sqlite_operational:invalid runtime kind".into()),
        };
        if let Some(value) = value {
            if self.cache.len() >= 128 {
                self.cache.clear();
            }
            self.cache.insert(key, (revision, value));
            return Ok(Some(revision));
        }
        Ok(None)
    }

    pub fn get(&mut self, kind: &str, id: &str) -> Result<Option<(u64, Value)>, String> {
        let Some(revision) = self.load(kind, id)? else {
            return Ok(None);
        };
        Ok(Some((
            revision,
            self.cache[&(kind.into(), id.into())].1.clone(),
        )))
    }

    pub fn run_projection(&mut self, id: &str) -> Result<Value, String> {
        self.load("run", id)?.ok_or("runtime_unknown_run")?;
        let run = &self.cache[&("run".into(), id.into())].1;
        let nodes = array(&run["definition"]["nodes"])?;
        let specs = nodes
            .iter()
            .map(|n| json!({"id":n["id"],"node_type":n["node_type"]}))
            .collect::<Vec<_>>();
        let states = nodes
            .iter()
            .map(|n| {
                let id = s(n, "id");
                (id.into(), json!({"state":run["nodes"][id]["state"]}))
            })
            .collect::<Map<String, Value>>();
        Ok(
            json!({"definition":{"nodes":specs,"edges":run["definition"]["edges"]},"nodes":states,
            "mode":run["mode"],"state":run["state"],"has_decisions":run["decisions"].as_array().is_some_and(|v|!v.is_empty())}),
        )
    }

    pub fn job_state(&mut self, id: &str, require_nonempty: bool) -> Result<Value, String> {
        self.load("job", id)?.ok_or("runtime_unknown_job")?;
        let job = &self.cache[&("job".into(), id.into())].1;
        let results = array(&job["detail"]["results"])?
            .iter()
            .map(|r| json!({"ok":r["ok"]}))
            .collect::<Vec<_>>();
        let attempts=array(&job["detail"]["attempts"])?.iter().map(|a|json!({"unknown_paid_outcome":a["unknown_paid_outcome"],"error":{"code":a["error"]["code"]}})).collect::<Vec<_>>();
        Ok(super::workflow::job_state(
            &json!({"state":job["state"],"results":results,"attempts":attempts,"require_nonempty":require_nonempty}),
        ))
    }

    pub fn read(&mut self, kind: &str, p: &Value) -> Result<Value, String> {
        self.tracking()?;
        let revision = self.revision(kind, s(p, "id"))?;
        if p["since_revision"].as_u64() == Some(revision)
            && self.cache.contains_key(&(kind.into(), s(p, "id").into()))
        {
            return Ok(json!({"revision":revision,"unchanged":true}));
        }
        let Some((revision, value)) = self.get(kind, s(p, "id"))? else {
            return Ok(json!({"missing":true}));
        };
        if p["since_revision"].as_u64() == Some(revision) {
            Ok(json!({"revision":revision,"unchanged":true}))
        } else {
            Ok(json!({"revision":revision,"value":value}))
        }
    }

    pub fn put(&mut self, kind: &str, p: &Value) -> Result<Value, String> {
        self.tracking()?;
        let own = self.conn.is_autocommit();
        if own {
            self.conn
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(db_error)?;
        }
        let result = self.put_inner(kind, p);
        if own {
            if result.is_ok() {
                self.commit()?;
            } else {
                self.rollback()?;
            }
        }
        result
    }
    fn put_inner(&mut self, kind: &str, p: &Value) -> Result<Value, String> {
        let id = s(p, "id");
        let prior = self.get(kind, id)?;
        let revision = self.revision(kind, id)?;
        if p["expected_revision"].as_u64() != Some(revision) {
            return Err("runtime_revision_conflict".into());
        }
        let mut value = prior.as_ref().map_or_else(|| json!({}), |(_, v)| v.clone());
        apply_patch(&mut value, &p["patch"])?;
        let now = n(p, "now_ms", 0.) as i64;
        let mut event_error = Value::Null;
        let mut event_gap_recorded = false;
        match kind {
            "run" => {
                if s(&value, "id") != id {
                    return Err("sqlite_operational:run identity changed".into());
                }
                self.conn.execute("INSERT INTO visual_runs VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET updated_at=excluded.updated_at,payload=excluded.payload",
                    params![id,now,value.to_string()]).map_err(db_error)?;
            }
            "job" => {
                if let Some((_, old)) = &prior
                    && s(old, "state") == "merged"
                    && (s(&value, "state") != "merged" || old["detail"] != value["detail"])
                {
                    return Err("runtime_merged_readonly".into());
                }
                let created = prior
                    .as_ref()
                    .map_or(now, |(_, v)| n(v, "created_at", now as f64) as i64);
                self.conn.execute("INSERT INTO jobs VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET state=excluded.state,updated_at=excluded.updated_at,detail=excluded.detail",
                    params![id,s(&value,"state"),created,now,value["task"].to_string(),value["detail"].to_string()]).map_err(db_error)?;
                for record in array(&p["usage"])? {
                    self.conn.execute("INSERT INTO usage_records(record_key,payload) VALUES(?1,?2) ON CONFLICT(record_key) DO UPDATE SET payload=excluded.payload,seq=(SELECT COALESCE(MAX(seq),0)+1 FROM usage_records) WHERE payload<>excluded.payload",
                        params![s(record,"record_key"),s(record,"payload")]).map_err(db_error)?;
                }
                for event in p["events"].as_array().into_iter().flatten() {
                    if let Err(error) = self.append_event(event) {
                        if matches!(
                            error.as_str(),
                            "runtime_event_capacity" | "runtime_event_conflict"
                        ) {
                            if error == "runtime_event_capacity" {
                                self.conn.execute("INSERT INTO coordination_gaps VALUES(?1,1,?2) ON CONFLICT(workflow_id) DO UPDATE SET dropped=dropped+1,updated_at=excluded.updated_at",
                                    params![s(event,"workflow_id"),now]).map_err(db_error)?;
                                event_gap_recorded = true;
                            }
                            event_error = json!(error);
                            break;
                        }
                        return Err(error);
                    }
                }
                value["job_id"] = json!(id);
                value["created_at"] = json!(created);
                value["updated_at"] = json!(now);
                if let Some((_, old)) = &prior {
                    value["task"] = old["task"].clone();
                }
            }
            _ => return Err("sqlite_operational:invalid runtime kind".into()),
        }
        let revision = self.revision(kind, id)?;
        self.cache
            .insert((kind.into(), id.into()), (revision, value.clone()));
        Ok(
            json!({"revision":revision,"created_at":value["created_at"],"updated_at":value["updated_at"],"event_error":event_error,"event_gap_recorded":event_gap_recorded}),
        )
    }

    pub fn routing_snapshot(&mut self, kind: &str) -> Result<(Value, Value), String> {
        let rows = self.exec(&json!({"sql":"SELECT * FROM health","params":[]}))?;
        let columns = array(&rows["columns"])?;
        let mut health = Map::new();
        for row in array(&rows["rows"])? {
            let object: Map<String, Value> = columns
                .iter()
                .zip(array(row)?)
                .map(|(k, v)| (k.as_str().unwrap_or("").into(), v.clone()))
                .collect();
            let key = object
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            health.insert(key, Value::Object(object));
        }
        let mut feedback = Map::new();
        let mut statement = self
            .conn
            .prepare("SELECT route_id,accepted,rejected FROM feedback WHERE kind=?1")
            .map_err(db_error)?;
        let mut rows = statement.query([kind]).map_err(db_error)?;
        while let Some(row) = rows.next().map_err(db_error)? {
            let id: String = row.get(0).map_err(db_error)?;
            let yes: i64 = row.get(1).map_err(db_error)?;
            let no: i64 = row.get(2).map_err(db_error)?;
            let count = yes + no;
            feedback.insert(format!("{id}:{kind}"),json!({"samples":count,"acceptance":if count==0{Value::Null}else{json!(yes as f64/count as f64)}}));
        }
        Ok((Value::Object(health), Value::Object(feedback)))
    }
}

fn decode(text: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str(text) {
        return Ok(value);
    }
    // Legacy Python JSON allowed non-finite usage metadata. Preserve quoted
    // report text and map only bare non-finite numbers to unknown, never zero.
    let bytes = text.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let (mut quoted, mut escaped, mut index) = (false, false, 0);
    while index < bytes.len() {
        let byte = bytes[index];
        if quoted {
            output.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            quoted = true;
            output.push(byte);
            index += 1;
            continue;
        }
        let token = [
            b"NaN".as_slice(),
            b"Infinity".as_slice(),
            b"-Infinity".as_slice(),
        ]
        .into_iter()
        .find(|token| {
            bytes[index..].starts_with(token)
                && (index == 0 || b"[,: \r\n\t".contains(&bytes[index - 1]))
                && (index + token.len() == bytes.len()
                    || b"}], \r\n\t".contains(&bytes[index + token.len()]))
        });
        if let Some(token) = token {
            output.extend_from_slice(b"null");
            index += token.len();
        } else {
            output.push(byte);
            index += 1;
        }
    }
    serde_json::from_slice(&output).map_err(|_| "sqlite_operational:invalid persisted JSON".into())
}

pub(super) fn apply_patch(value: &mut Value, patch: &Value) -> Result<(), String> {
    for op in array(patch)? {
        let path = array(&op["path"])?;
        if path.is_empty() {
            if s(op, "op") != "set" {
                return Err("runtime_invalid_patch".into());
            }
            *value = op["value"].clone();
            continue;
        }
        let mut target = &mut *value;
        for part in &path[..path.len() - 1] {
            target = match target {
                Value::Object(o) => o.get_mut(part.as_str().ok_or("runtime_invalid_patch")?),
                Value::Array(a) => {
                    a.get_mut(part.as_u64().ok_or("runtime_invalid_patch")? as usize)
                }
                _ => None,
            }
            .ok_or("runtime_invalid_patch")?;
        }
        let last = &path[path.len() - 1];
        match target {
            Value::Object(o) => {
                let key = last.as_str().ok_or("runtime_invalid_patch")?;
                match s(op, "op") {
                    "set" => {
                        o.insert(key.into(), op["value"].clone());
                    }
                    "remove" => {
                        o.remove(key);
                    }
                    _ => return Err("runtime_invalid_patch".into()),
                }
            }
            Value::Array(a) => {
                let index = last.as_u64().ok_or("runtime_invalid_patch")? as usize;
                match s(op, "op") {
                    "set" if index == a.len() => a.push(op["value"].clone()),
                    "set" if index < a.len() => a[index] = op["value"].clone(),
                    "remove" if index < a.len() => {
                        a.remove(index);
                    }
                    _ => return Err("runtime_invalid_patch".into()),
                }
            }
            _ => return Err("runtime_invalid_patch".into()),
        }
    }
    Ok(())
}
