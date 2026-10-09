//! GTT collaboration metering v1. Reads only the coordinator's sanitized
//! usage_records table, never jobs/results/prompts/config. Reservations remain
//! outside the authoritative ledger. Native request keys share the original
//! adapter's dedup identity; session-only legacy records rely on native scans.

use super::{Capability, ScanOutcome, SourceAdapter, SourceItem, SourceKind};
use crate::{Provenance, UsageEvent, store::Store};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::path::PathBuf;

pub struct Coordinator;

fn source_path() -> PathBuf {
    std::env::var_os("GTT_ROUTER_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::store::default_db_path()
                .parent()
                .unwrap()
                .join("router/router-state.sqlite")
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    record_key: String,
    app: String,
    provider_id: Option<String>,
    model: Option<String>,
    request_model: Option<String>,
    session_id: Option<String>,
    ts_start: i64,
    duration_ms: Option<i64>,
    status: String,
    measurement: String,
    native_dedup_key: Option<String>,
    native_backed: bool,
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cache_read_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
    unclassified_tokens: u64,
}

impl SourceAdapter for Coordinator {
    fn id(&self) -> &'static str {
        "gtt_coordinator"
    }
    fn display_name(&self) -> &'static str {
        "GTT Collaboration"
    }
    fn capability(&self) -> Capability {
        Capability::Precise
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        source_path()
            .parent()
            .map(PathBuf::from)
            .into_iter()
            .collect()
    }
    fn discover(&self) -> Result<Vec<SourceItem>> {
        let path = source_path();
        Ok(path
            .is_file()
            .then(|| SourceItem {
                key: format!("gtt_coordinator:{}", path.display()),
                path,
                kind: SourceKind::Sqlite,
            })
            .into_iter()
            .collect())
    }
    fn scan_sqlite(&self, item: &SourceItem, store: &Store) -> Result<ScanOutcome> {
        let conn = super::opencode::open_ro(&item.path)?;
        conn.pragma_update(None, "trusted_schema", "OFF")?;
        conn.busy_timeout(std::time::Duration::from_millis(500))?;
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_records')", [], |r| r.get(0))?;
        if !exists {
            return Ok(ScanOutcome::default());
        } // old coordinator
        let cursor = store.load_cursor(&item.key)?;
        let mut seq = cursor.offset as i64;
        let mut query = conn.prepare(
            "SELECT seq,payload FROM usage_records WHERE seq>?1 ORDER BY seq LIMIT 2048",
        )?;
        let mut rows = query.query([seq])?;
        let mut out = ScanOutcome::default();
        while let Some(row) = rows.next()? {
            let current: i64 = row.get(0)?;
            let payload: String = row.get(1)?;
            ensure!(payload.len() <= 65536, "coordinator usage record too large");
            let r: Record = serde_json::from_str(&payload)?;
            ensure!(r.version == 1, "unsupported coordinator usage version");
            ensure!(
                !r.record_key.is_empty()
                    && r.record_key.len() <= 200
                    && !r.app.is_empty()
                    && r.app.len() <= 128,
                "invalid coordinator identity"
            );
            ensure!(
                [
                    r.input_tokens,
                    r.output_tokens,
                    r.reasoning_tokens,
                    r.cache_read_tokens,
                    r.cache_write_5m_tokens,
                    r.cache_write_1h_tokens,
                    r.unclassified_tokens
                ]
                .iter()
                .all(|t| *t <= 1_000_000_000_000),
                "invalid coordinator tokens"
            );
            seq = current;
            if r.measurement != "reported" || (r.native_backed && r.native_dedup_key.is_none()) {
                out.skipped += 1;
                continue;
            }
            let dedup_key = if let Some(key) = r.native_dedup_key {
                ensure!(
                    r.app == "opencode" && key.starts_with("opencode:msg:") && key.len() <= 200,
                    "invalid native usage identity"
                );
                // Preserve native pricing, timing and attribution when present.
                let covered: bool = store.conn().query_row(
                    "SELECT EXISTS(SELECT 1 FROM usage_events WHERE dedup_key=?1 AND input_tokens+output_tokens+cache_read_tokens+cache_write_5m_tokens+cache_write_1h_tokens+unclassified_tokens >= ?2)",
                    rusqlite::params![&key,(r.input_tokens+r.output_tokens+r.cache_read_tokens+r.cache_write_5m_tokens+r.cache_write_1h_tokens+r.unclassified_tokens) as i64],
                    |r| r.get(0),
                )?;
                if covered {
                    out.skipped += 1;
                    continue;
                }
                key
            } else {
                format!("gtt_coordinator:{}", r.record_key)
            };
            out.events.push(UsageEvent {
                dedup_key,
                app: r.app,
                model: r.model,
                request_model: r.request_model,
                provider_id: r.provider_id,
                session_id: r.session_id,
                ts_start: Some(r.ts_start),
                ts_end: r.duration_ms.map(|d| r.ts_start.saturating_add(d)),
                duration_ms: r.duration_ms,
                input_tokens: r.input_tokens,
                output_tokens: r.output_tokens,
                reasoning_tokens: r.reasoning_tokens,
                cache_read_tokens: r.cache_read_tokens,
                cache_write_5m_tokens: r.cache_write_5m_tokens,
                cache_write_1h_tokens: r.cache_write_1h_tokens,
                unclassified_tokens: r.unclassified_tokens,
                status: Some(format!("gtt:{}:{}", r.measurement, r.status)),
                provenance: Provenance::LocalSqlite,
                raw_ref: Some(format!("{}#usage_records:{current}", item.path.display())),
                ..Default::default()
            });
        }
        if seq as u64 != cursor.offset {
            store.save_cursor(self.id(), &item.key, &item.path, seq as u64, 0, None)?;
        }
        Ok(out)
    }
}
