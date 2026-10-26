//! OpenCode adapter (spec §6.3): `~/.local/share/opencode/opencode.db`.
//!
//! One event per ASSISTANT `message` row — the source-of-truth grain:
//! `message.data.tokens`/`cost`/`modelID`/`providerID`/`time.created/completed`
//! are per-API-call values. `session.tokens_*`/`cost` are running sums of the
//! same messages (verified 1:1), but a session row collapses every request to
//! the LAST model used and stamps all usage at `time_created` — a session
//! spanning days lands entirely on its creation day and mislabels any
//! mid-session model switch. Messages fix both, and give real per-call
//! durations instead of session-lifetime spans.
//!
//! `cost > 0` is OpenCode's own computed USD → `provider_reported`;
//! `cost == 0` falls back to the price book (free-tier models price to NULL).
//! High-water mark: `message.time_updated` (1 s overlap for races).
//! Migration: `adapter_state="msg-v2"` — on first encounter the legacy
//! session-grain events (`opencode:session:*`) are purged and the watermark
//! reset, otherwise old + new rows would double-count.

use super::{Capability, ScanOutcome, SourceAdapter, SourceItem, SourceKind};
use crate::model::{CostSource, Provenance, UsageEvent, apps};
use crate::store::Store;
use anyhow::{Result, bail};
use rusqlite::{Connection, OpenFlags};
use std::path::PathBuf;

/// Adapter-state marker persisted in `sync_cursors.adapter_state` once the
/// session-grain → message-grain migration has run for this source.
const MSG_SCHEMA_STATE: &str = "msg-v2";

pub struct OpenCode;

fn data_root() -> PathBuf {
    crate::sync::env_home("XDG_DATA_HOME", ".local/share").join("opencode")
}

fn databases() -> Vec<PathBuf> {
    let root = data_root();
    if let Some(value) = std::env::var_os("OPENCODE_DB").filter(|value| !value.is_empty()) {
        if value == ":memory:" {
            return vec![];
        }
        let path = PathBuf::from(value);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        return path.is_file().then_some(path).into_iter().collect();
    }
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| {
                        name == "opencode.db"
                            || (name.starts_with("opencode-")
                                && path
                                    .extension()
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("db")))
                    })
            {
                paths.push(path);
            }
        }
    }
    paths.sort();
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())));
    paths
}

impl SourceAdapter for OpenCode {
    fn id(&self) -> &'static str {
        apps::OPENCODE
    }
    fn display_name(&self) -> &'static str {
        "OpenCode"
    }
    fn capability(&self) -> Capability {
        Capability::Precise
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![data_root()]
    }

    fn discover(&self) -> Result<Vec<SourceItem>> {
        Ok(databases()
            .into_iter()
            .map(|path| SourceItem {
                key: path.to_string_lossy().into_owned(),
                path,
                kind: SourceKind::Sqlite,
            })
            .collect())
    }

    fn scan_sqlite(&self, item: &SourceItem, store: &Store) -> Result<ScanOutcome> {
        let cur = store.load_cursor(&item.key)?;
        let conn = open_ro(&item.path)?;
        let mut out = ScanOutcome::default();

        let tables = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        let legacy = tables.contains("session") && tables.contains("message");
        let modern = tables.contains("session_v2") && tables.contains("session_message");
        if !legacy && !modern {
            bail!(
                "{}: unsupported opencode schema (no session/message or session_v2/session_message tables)",
                item.path.display()
            );
        }
        let schema_state = if modern { "msg-v3" } else { MSG_SCHEMA_STATE };
        let migrated = cur.state.as_deref() != Some(schema_state);
        let mut since = cur.offset.saturating_sub(1000);
        if migrated {
            since = 0;
        }
        // Identifiers and JSON paths below are fixed schema alternatives.
        let query = |current: bool| {
            let (message, session, predicate, model, provider) = if current {
                (
                    "session_message",
                    "session_v2",
                    "m.type IN ('assistant','compaction')",
                    "$.model.id",
                    "$.model.providerID",
                )
            } else {
                (
                    "message",
                    "session",
                    "json_extract(m.data,'$.role') = 'assistant'",
                    "$.modelID",
                    "$.providerID",
                )
            };
            let exclude = if !current && modern {
                " AND NOT EXISTS (SELECT 1 FROM session_message v WHERE v.id = m.id)"
            } else {
                ""
            };
            let pending = if current {
                "CASE WHEN m.type='compaction' THEN COALESCE(json_extract(m.data,'$.status'),'') NOT IN ('completed','failed') ELSE COALESCE(json_extract(m.data,'$.time.completed'),0)<=0 END"
            } else {
                "0"
            };
            format!("SELECT m.id, m.session_id, s.directory,
                json_extract(m.data,'$.cost'), json_extract(m.data,'$.tokens.input'),
                json_extract(m.data,'$.tokens.output'), json_extract(m.data,'$.tokens.reasoning'),
                json_extract(m.data,'$.tokens.cache.read'), json_extract(m.data,'$.tokens.cache.write'),
                json_extract(m.data,'$.tokens.total'), json_extract(m.data,'{model}'),
                json_extract(m.data,'{provider}'), json_extract(m.data,'$.agent'),
                json_extract(m.data,'$.time.created'), json_extract(m.data,'$.time.completed'),
                json_extract(m.data,'$.error'), m.time_created, m.time_updated, {pending}
                FROM {message} m JOIN {session} s ON s.id = m.session_id
                WHERE {predicate} AND m.time_updated > ?1{exclude}")
        };
        let mut queries = Vec::new();
        if modern {
            queries.push(query(true));
        }
        if legacy {
            queries.push(query(false));
        }
        let sql = queries.join(" UNION ALL ");
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params![since as i64], |r| {
            Ok(MsgRow {
                id: r.get(0)?,
                session_id: r.get(1)?,
                directory: r.get(2)?,
                cost: r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                tin: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
                tout: r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u64,
                treason: r.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u64,
                tcr: r.get::<_, Option<i64>>(7)?.unwrap_or(0).max(0) as u64,
                tcw: r.get::<_, Option<i64>>(8)?.unwrap_or(0).max(0) as u64,
                ttotal: r.get::<_, Option<i64>>(9)?,
                model: r.get(10)?,
                provider: r.get(11)?,
                agent: r.get(12)?,
                created: r.get(13)?,
                completed: r.get(14)?,
                error: r.get::<_, Option<String>>(15)?,
                created_col: r.get::<_, Option<i64>>(16)?.unwrap_or(0),
                updated: r.get::<_, Option<i64>>(17)?.unwrap_or(0),
                pending: r.get(18)?,
            })
        })?;

        let mut max_updated = if migrated { 0 } else { cur.offset as i64 };
        let mut first_pending: Option<i64> = None;
        let mut total_mismatches = 0u64;
        let mut migrated_sessions = std::collections::HashSet::new();
        for row in rows {
            let m = row?;
            max_updated = max_updated.max(m.updated);
            if m.pending {
                first_pending = Some(first_pending.map_or(m.updated, |v| v.min(m.updated)));
            }
            if migrated && migrated_sessions.insert(m.session_id.clone()) {
                // Replace only the aggregate whose exact session is present here.
                store.conn().execute(
                    "DELETE FROM usage_events WHERE app=?1 AND dedup_key=?2",
                    rusqlite::params![apps::OPENCODE, format!("opencode:session:{}", m.session_id)],
                )?;
            }
            // Self-audit: OpenCode records tokens.total as the billed sum;
            // flag when it disagrees with the component fields we ingest.
            if let Some(t) = m.ttotal
                && t != (m.tin + m.tout + m.treason + m.tcr + m.tcw) as i64
            {
                total_mismatches += 1;
            }
            let (cost_usd, cost_source) = if m.cost > 0.0 {
                (Some(m.cost), Some(CostSource::ProviderReported))
            } else {
                (None, None)
            };
            out.events.push(UsageEvent {
                dedup_key: format!("opencode:msg:{}", m.id),
                app: apps::OPENCODE.into(),
                session_id: Some(m.session_id),
                project: m.directory,
                provider_id: m.provider,
                model: m.model.clone(),
                request_model: m.model,
                ts_start: m
                    .created
                    .or_else(|| (m.created_col > 0).then_some(m.created_col)),
                ts_end: m.completed.or_else(|| (m.updated > 0).then_some(m.updated)),
                input_tokens: m.tin,
                output_tokens: m.tout + m.treason,
                reasoning_tokens: m.treason,
                cache_read_tokens: m.tcr,
                cache_write_5m_tokens: m.tcw,
                cost_usd,
                cost_source,
                provenance: Provenance::LocalSqlite,
                duration_ms: match (m.created, m.completed) {
                    (Some(a), Some(b)) if b > a => Some(b - a),
                    _ => None,
                },
                status: m.agent,
                error: m.error.map(|e| e.chars().take(300).collect()),
                raw_ref: Some(format!("{}#message:{}", item.path.display(), m.id)),
                ..Default::default()
            });
        }
        if total_mismatches > 0 {
            out.notes.push(format!(
                "{total_mismatches} message(s) where tokens.total != component sum"
            ));
        }
        if let Some(pending) = first_pending {
            max_updated = max_updated.min(pending.saturating_sub(1).max(0));
        }
        if (max_updated as u64) != cur.offset || migrated {
            store.save_cursor(
                self.id(),
                &item.key,
                &item.path,
                max_updated.max(0) as u64,
                0,
                Some(schema_state),
            )?;
        }
        out.consumed = max_updated.max(0) as u64;
        Ok(out)
    }
}

struct MsgRow {
    id: String,
    session_id: String,
    directory: Option<String>,
    cost: f64,
    tin: u64,
    tout: u64,
    treason: u64,
    tcr: u64,
    tcw: u64,
    ttotal: Option<i64>,
    model: Option<String>,
    provider: Option<String>,
    agent: Option<String>,
    created: Option<i64>,
    completed: Option<i64>,
    error: Option<String>,
    created_col: i64,
    updated: i64,
    pending: bool,
}

pub(crate) fn open_ro(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &std::path::Path) -> Connection {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE session(
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT 'p',
                directory TEXT NOT NULL, agent TEXT, model TEXT,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
             CREATE TABLE message(
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                data TEXT NOT NULL);",
        )
        .unwrap();
        c
    }

    /// `(in, out, cache_read)` + `(created, completed)` — mirrors the real
    /// `message.data` shape for one assistant row.
    fn msg_json(
        model: &str,
        provider: &str,
        cost: f64,
        tok: (i64, i64, i64),
        time: (i64, i64),
    ) -> String {
        let (tin, tout, cr) = tok;
        let (t0, t1) = time;
        format!(
            r#"{{"role":"assistant","agent":"build","modelID":"{model}","providerID":"{provider}",
            "cost":{cost},"tokens":{{"total":{},"input":{tin},"output":{tout},"reasoning":0,
            "cache":{{"write":0,"read":{cr}}}}},"time":{{"created":{t0},"completed":{t1}}}}}"#,
            tin + tout + cr
        )
    }

    fn item(path: &std::path::Path) -> SourceItem {
        SourceItem {
            key: path.to_string_lossy().into(),
            path: path.to_path_buf(),
            kind: SourceKind::Sqlite,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gtt_oc_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn message_level_events_per_model_and_day() {
        let dir = tmpdir("msglvl");
        let src = dir.join("opencode.db");
        let c = fixture(&src);
        // One session created day 1, but messages land on two later days
        // under two different models — the old session-grain adapter
        // misattributed both dimensions.
        c.execute(
            "INSERT INTO session VALUES('s1','p','D:\\proj','build',
             '{\"id\":\"glm-x\"}', 1_000_000, 1_000_000)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO message VALUES('m1','s1',2_000_000,2_000_000,?1)",
            [msg_json(
                "glm-x",
                "opencode-go",
                0.5,
                (100, 10, 500),
                (2_000_000, 2_005_000),
            )],
        )
        .unwrap();
        c.execute(
            "INSERT INTO message VALUES('m2','s1',3_000_000,3_000_000,?1)",
            [msg_json(
                "ox-free",
                "opencode",
                0.0,
                (200, 20, 0),
                (3_000_000, 3_002_000),
            )],
        )
        .unwrap();
        c.execute(
            "INSERT INTO message VALUES('u1','s1',2_500_000,2_500_000,'{\"role\":\"user\"}')",
            [],
        )
        .unwrap();
        drop(c);

        let store = Store::open_memory().unwrap();
        let it = item(&src);
        let out = OpenCode.scan_sqlite(&it, &store).unwrap();
        assert_eq!(out.events.len(), 2); // user row skipped
        let e0 = &out.events[0];
        assert_eq!(e0.dedup_key, "opencode:msg:m1");
        assert_eq!(e0.model.as_deref(), Some("glm-x"));
        assert_eq!(e0.provider_id.as_deref(), Some("opencode-go"));
        assert_eq!(e0.ts_start, Some(2_000_000));
        assert_eq!(e0.ts_end, Some(2_005_000));
        assert_eq!(e0.duration_ms, Some(5_000));
        assert_eq!(e0.input_tokens, 100);
        assert_eq!(e0.cache_read_tokens, 500);
        assert_eq!(e0.cost_usd, Some(0.5));
        assert_eq!(e0.cost_source, Some(CostSource::ProviderReported));
        assert_eq!(e0.project.as_deref(), Some(r"D:\proj"));
        let e1 = &out.events[1];
        assert_eq!(e1.model.as_deref(), Some("ox-free")); // per-message model!
        assert_eq!(e1.cost_usd, None); // free → price book, not fabricated
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_session_events_purged_on_migration() {
        let dir = tmpdir("mig");
        let src = dir.join("opencode.db");
        let c = fixture(&src);
        c.execute(
            "INSERT INTO session VALUES('s1','p','/proj','build',NULL,1,1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO message VALUES('m1','s1',100,100,?1)",
            [msg_json(
                "glm-x",
                "opencode-go",
                0.01,
                (10, 5, 0),
                (100, 200),
            )],
        )
        .unwrap();
        drop(c);

        let store = Store::open_memory().unwrap();
        // Simulate the old adapter's state: session-grain event + watermark.
        store
            .upsert_event(&UsageEvent {
                dedup_key: "opencode:session:s1".into(),
                app: apps::OPENCODE.into(),
                input_tokens: 999_999,
                provenance: Provenance::LocalSqlite,
                ..Default::default()
            })
            .unwrap();
        store
            .save_cursor(apps::OPENCODE, &src.to_string_lossy(), &src, 500, 0, None)
            .unwrap();

        let out = OpenCode.scan_sqlite(&item(&src), &store).unwrap();
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].dedup_key, "opencode:msg:m1");
        // Legacy row must be gone once the engine ingests the scan.
        let gone: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE dedup_key='opencode:session:s1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gone, 0);
        assert_eq!(
            store.load_cursor(&item(&src).key).unwrap().state.as_deref(),
            Some(MSG_SCHEMA_STATE)
        );
        // Incremental: the 1 s watermark overlap refetches the newest row,
        // but the same dedup_key merges in place — no double counting.
        let again = OpenCode.scan_sqlite(&item(&src), &store).unwrap();
        assert_eq!(again.events.len(), 1);
        assert_eq!(again.events[0].dedup_key, "opencode:msg:m1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn output_includes_reasoning_as_a_subset() {
        let dir = tmpdir("reason");
        let src = dir.join("opencode.db");
        let c = fixture(&src);
        c.execute(
            "INSERT INTO session VALUES('s1','p','/proj','build',NULL,1,1)",
            [],
        )
        .unwrap();
        let data = r#"{"role":"assistant","modelID":"glm-x","providerID":"opencode-go","cost":0,
            "tokens":{"total":47,"input":10,"output":5,"reasoning":30,
            "cache":{"write":2,"read":0}},"time":{"created":100,"completed":200}}"#;
        c.execute("INSERT INTO message VALUES('m1','s1',100,100,?1)", [data])
            .unwrap();
        drop(c);

        let store = Store::open_memory().unwrap();
        let out = OpenCode.scan_sqlite(&item(&src), &store).unwrap();
        let e = &out.events[0];
        assert_eq!(e.output_tokens, 35); // 5 output + 30 reasoning
        assert_eq!(e.reasoning_tokens, 30);
        assert_eq!(e.cache_write_5m_tokens, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn updated_message_rescans() {
        let dir = tmpdir("upd");
        let src = dir.join("opencode.db");
        let c = fixture(&src);
        c.execute(
            "INSERT INTO session VALUES('s1','p','/proj','build',NULL,1,1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO message VALUES('m1','s1',100,100,?1)",
            [msg_json("glm-x", "opencode-go", 0.0, (10, 5, 0), (100, 0))],
        )
        .unwrap();
        drop(c);

        let store = Store::open_memory().unwrap();
        let it = item(&src);
        let out1 = OpenCode.scan_sqlite(&it, &store).unwrap();
        assert_eq!(out1.events[0].output_tokens, 5);

        // Streaming completes: same row updated in place.
        let c = Connection::open(&src).unwrap();
        c.execute(
            "UPDATE message SET time_updated=500, data=?1 WHERE id='m1'",
            [msg_json(
                "glm-x",
                "opencode-go",
                0.07,
                (10, 50, 900),
                (100, 500),
            )],
        )
        .unwrap();
        drop(c);

        let out2 = OpenCode.scan_sqlite(&it, &store).unwrap();
        assert_eq!(out2.events.len(), 1);
        assert_eq!(out2.events[0].dedup_key, "opencode:msg:m1");
        assert_eq!(out2.events[0].output_tokens, 50);
        assert_eq!(out2.events[0].cache_read_tokens, 900);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_message_table_is_error() {
        let dir = tmpdir("bad");
        let src = dir.join("opencode.db");
        let c = Connection::open(&src).unwrap();
        c.execute_batch("CREATE TABLE legacy(x TEXT)").unwrap();
        drop(c);
        let store = Store::open_memory().unwrap();
        let err = OpenCode.scan_sqlite(&item(&src), &store).unwrap_err();
        assert!(format!("{err:#}").contains("unsupported opencode schema"));
        std::fs::remove_dir_all(&dir).ok();
    }
    fn modern_fixture(path: &std::path::Path) -> Connection {
        let db = Connection::open(path).unwrap();
        db.execute_batch("CREATE TABLE session_v2(id TEXT PRIMARY KEY,directory TEXT);
            CREATE TABLE session_message(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,seq INTEGER,time_created INTEGER,time_updated INTEGER,data TEXT);
            INSERT INTO session_v2 VALUES('v2-session','/new-project');").unwrap();
        db
    }

    fn modern_data(completed: Option<i64>) -> String {
        serde_json::json!({"agent":"build","model":{"id":"modern-model","providerID":"vendor"},
            "tokens":{"input":10,"output":7,"reasoning":3,"cache":{"read":20,"write":5}},
            "cost":0.25,"time":{"created":100,"completed":completed}})
        .to_string()
    }

    #[test]
    fn modern_schema_counts_cache_and_reasoning_once() {
        let dir = tmpdir("modern");
        let path = dir.join("opencode.db");
        let db = modern_fixture(&path);
        db.execute(
            "INSERT INTO session_message VALUES('new','v2-session','assistant',1,100,200,?1)",
            [modern_data(Some(200))],
        )
        .unwrap();
        drop(db);
        let store = Store::open_memory().unwrap();
        let out = OpenCode.scan_sqlite(&item(&path), &store).unwrap();
        assert_eq!(out.events.len(), 1);
        let event = &out.events[0];
        assert_eq!(event.dedup_key, "opencode:msg:new");
        assert_eq!(event.model.as_deref(), Some("modern-model"));
        assert_eq!(event.provider_id.as_deref(), Some("vendor"));
        assert_eq!(event.project.as_deref(), Some("/new-project"));
        assert_eq!(
            (
                event.input_tokens,
                event.output_tokens,
                event.reasoning_tokens,
                event.cache_read_tokens,
                event.cache_write_5m_tokens
            ),
            (10, 10, 3, 20, 5)
        );
    }

    #[test]
    fn coexisting_schema_and_new_database_preserve_message_records() {
        let dir = tmpdir("coexisting");
        let path = dir.join("one.db");
        let db = fixture(&path);
        db.execute_batch("INSERT INTO session VALUES('s1','p','/old',NULL,NULL,1,1);")
            .unwrap();
        db.execute(
            "INSERT INTO message VALUES('same','s1',100,200,?1)",
            [msg_json("old", "p", 0.0, (1, 2, 0), (100, 200))],
        )
        .unwrap();
        db.execute(
            "INSERT INTO message VALUES('only-old','s1',100,200,?1)",
            [msg_json("old", "p", 0.0, (1, 2, 0), (100, 200))],
        )
        .unwrap();
        drop(db);
        let db = modern_fixture(&path);
        db.execute(
            "INSERT INTO session_message VALUES('same','v2-session','assistant',1,100,200,?1)",
            [modern_data(Some(200))],
        )
        .unwrap();
        drop(db);
        let store = Store::open_memory().unwrap();
        let out = OpenCode.scan_sqlite(&item(&path), &store).unwrap();
        assert_eq!(out.events.len(), 2);
        assert_eq!(
            out.events
                .iter()
                .filter(|v| v.dedup_key == "opencode:msg:same")
                .count(),
            1
        );
        for event in &out.events {
            store.upsert_event(event).unwrap();
        }
        store
            .upsert_event(&UsageEvent {
                dedup_key: "opencode:session:other-profile".into(),
                app: apps::OPENCODE.into(),
                input_tokens: 50,
                ..Default::default()
            })
            .unwrap();
        let second = dir.join("two.db");
        let db = modern_fixture(&second);
        db.execute(
            "INSERT INTO session_message VALUES('second','v2-session','assistant',1,100,200,?1)",
            [modern_data(Some(200))],
        )
        .unwrap();
        drop(db);
        OpenCode.scan_sqlite(&item(&second), &store).unwrap();
        let count: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE app='opencode'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 3,
            "a new database cannot purge existing messages or another profile aggregate"
        );
    }

    #[test]
    fn modern_pending_row_is_revisited_after_late_finalization() {
        let dir = tmpdir("pending");
        let path = dir.join("opencode.db");
        let db = modern_fixture(&path);
        db.execute(
            "INSERT INTO session_message VALUES('pending','v2-session','assistant',1,100,100,?1)",
            [modern_data(None)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO session_message VALUES('later','v2-session','assistant',2,10000,10000,?1)",
            [modern_data(Some(10000))],
        )
        .unwrap();
        let store = Store::open_memory().unwrap();
        OpenCode.scan_sqlite(&item(&path), &store).unwrap();
        assert!(store.load_cursor(&item(&path).key).unwrap().offset < 100);
        db.execute(
            "UPDATE session_message SET data=?1 WHERE id='pending'",
            [modern_data(Some(200))],
        )
        .unwrap();
        drop(db);
        let out = OpenCode.scan_sqlite(&item(&path), &store).unwrap();
        assert!(out.events.iter().any(|event|event.dedup_key=="opencode:msg:pending" && event.ts_end==Some(200)));
        assert_eq!(store.load_cursor(&item(&path).key).unwrap().offset, 10000);
    }

    #[test]
    fn compaction_watermark_follows_status_without_completed_timestamp() {
        let dir = tmpdir("compaction-watermark");
        let path = dir.join("opencode.db");
        let db = modern_fixture(&path);
        let data = |status| {
            serde_json::json!({"status":status,"tokens":{"input":10,"output":2},
                "time":{"created":100}})
            .to_string()
        };
        db.execute(
            "INSERT INTO session_message VALUES('compact','v2-session','compaction',1,100,100,?1)",
            [data("running")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO session_message VALUES('later','v2-session','assistant',2,10000,10000,?1)",
            [modern_data(Some(10000))],
        )
        .unwrap();
        for status in ["completed", "failed"] {
            db.execute(
                "UPDATE session_message SET data=?1 WHERE id='compact'",
                [data("running")],
            )
            .unwrap();
            let store = Store::open_memory().unwrap();
            OpenCode.scan_sqlite(&item(&path), &store).unwrap();
            assert!(store.load_cursor(&item(&path).key).unwrap().offset < 100);
            db.execute(
                "UPDATE session_message SET data=?1 WHERE id='compact'",
                [data(status)],
            )
            .unwrap();
            let out = OpenCode.scan_sqlite(&item(&path), &store).unwrap();
            assert!(
                out.events
                    .iter()
                    .any(|event| event.dedup_key == "opencode:msg:compact")
            );
            assert_eq!(store.load_cursor(&item(&path).key).unwrap().offset, 10000);
            let next = OpenCode.scan_sqlite(&item(&path), &store).unwrap();
            assert!(
                next.events
                    .iter()
                    .all(|event| event.dedup_key != "opencode:msg:compact")
            );
        }
    }

    #[test]
    fn read_only_filename_with_space_and_hash_is_literal() {
        let dir = tmpdir("literal");
        let path = dir.join("source # one.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE marker(value INTEGER);INSERT INTO marker VALUES(42);")
            .unwrap();
        drop(db);
        let db = open_ro(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM marker", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
        assert!(db.execute("INSERT INTO marker VALUES(7)", []).is_err());
    }
}
