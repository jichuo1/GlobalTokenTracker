//! Versioned, whitelisted observation protocol. Snapshot never opens Store,
//! discovers source files, migrates the ledger or polls credentials/network.

use crate::{Engine, Store, adapters, quota, store::now_ms};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const PROTOCOL_VERSION: u32 = 1;
const LEASE_KEY: &str = "coordination.refresh.lease.v1";
const COOLDOWN_KEY: &str = "coordination.refresh.last.v1";
const ALLOWANCE_WINDOWS: &[&str] = &[
    "5h_block",
    "weekly",
    "monthly",
    "daily",
    "api_pool",
    "auto_pool",
];

#[derive(Clone)]
pub struct SnapshotOptions {
    pub source: Option<String>,
    pub lookback_days: u32,
    pub quota_ttl_secs: u32,
    pub model_limit: usize,
    pub quota_limit: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            source: None,
            lookback_days: 14,
            quota_ttl_secs: 300,
            model_limit: 512,
            quota_limit: 256,
        }
    }
}

/// Resolve both current and pre-rename installs without moving/creating files.
pub fn readonly_db_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("GTT_DATA_DIR") {
        return PathBuf::from(dir).join("ledger.db");
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let current = home.join(".globaltokentracker").join("ledger.db");
    let legacy = home.join(".codeledger").join("ledger.db");
    if !current.exists() && legacy.is_file() {
        legacy
    } else {
        current
    }
}

pub fn response(operation: &str, status: &str) -> Value {
    json!({"protocol":"gtt.coordination", "protocol_version":PROTOCOL_VERSION,
        "gtt_version":env!("CARGO_PKG_VERSION"), "operation":operation,
        "captured_at":now_ms(), "status":status, "warnings":[]})
}

pub fn failure(operation: &str, code: &str) -> Value {
    let mut result = response(operation, "unavailable");
    result["error"] = json!({"code":code, "retryable":matches!(code, "ledger_busy" | "ledger_unavailable" | "refresh_busy" | "refresh_throttled")});
    result
}

fn known_source(source: &str) -> bool {
    adapters::registry().iter().any(|a| a.id() == source)
}

pub fn capabilities() -> Value {
    let mut result = response("capabilities", "ok");
    result["features"] = json!({"consistent_read":true, "implicit_refresh":false,
        "source_filter":true, "scoped_refresh":true, "quota_poll_opt_in":true,
        "refresh_lease":true, "opaque_accounts":true});
    result["limits"] = json!({"max_models":512, "max_quotas":256,
        "max_lookback_days":90, "max_quota_ttl_secs":86400,
        "busy_timeout_ms":750});
    result["sources"] = json!(
        adapters::registry()
            .iter()
            .map(|a| json!({
                "source":a.id(), "metering":match a.capability() {
                    adapters::Capability::Precise => "precise",
                    adapters::Capability::Estimate => "estimate",
                    adapters::Capability::Metadata => "metadata",
                }, "quota_poll_supported":matches!(a.id(), "codex" | "cursor"),
                "execution_availability":"unknown"
            }))
            .collect::<Vec<_>>()
    );
    result
}

// Model/provider identifiers may originate in untrusted session data. Do not
// export URLs, absolute paths, control characters or arbitrarily large labels.
fn label(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > 128
        || value.contains("://")
        || value.starts_with(['/', '\\'])
        || (value.as_bytes().get(1) == Some(&b':')
            && value
                .as_bytes()
                .get(2)
                .is_some_and(|c| *c == b'/' || *c == b'\\'))
        || value.contains("..")
        || ["ghp_", "github_pat_", "AIza"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
        || (value.starts_with("sk-") && value.len() >= 19)
        || !value
            .chars()
            .all(|c| c.is_alphanumeric() || "_-./: ()".contains(c))
    {
        None
    } else {
        Some(value.to_owned())
    }
}

fn opaque(value: &str) -> String {
    Sha256::digest(value.as_bytes())[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && *v >= 0.0)
}

pub fn snapshot(path: &Path, options: &SnapshotOptions) -> Value {
    if options.source.as_deref().is_some_and(|s| !known_source(s)) {
        return failure("snapshot", "unknown_source");
    }
    if !(1..=90).contains(&options.lookback_days)
        || !(1..=86400).contains(&options.quota_ttl_secs)
        || !(1..=512).contains(&options.model_limit)
        || !(1..=256).contains(&options.quota_limit)
    {
        return failure("snapshot", "invalid_options");
    }
    if !path.is_file() {
        return failure("snapshot", "ledger_missing");
    }
    match read_snapshot(path, options) {
        Ok(value) => value,
        Err(rusqlite::Error::SqliteFailure(e, _))
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            failure("snapshot", "ledger_busy")
        }
        Err(_) => failure("snapshot", "ledger_unavailable"),
    }
}

fn columns(conn: &Connection, table: &str, expected: &[&str]) -> rusqlite::Result<bool> {
    // Only internal literal table names reach this helper.
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(expected.iter().all(|name| names.iter().any(|n| n == name)))
}

fn read_snapshot(path: &Path, options: &SnapshotOptions) -> rusqlite::Result<Value> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(750))?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
    let tx = conn.transaction()?;
    // First read fixes the WAL snapshot used by every section below.
    let _: i64 = tx.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
    let result = project_snapshot(&tx, options)?;
    tx.commit()?;
    Ok(result)
}

fn project_snapshot(tx: &Connection, options: &SnapshotOptions) -> rusqlite::Result<Value> {
    let mut result = response("snapshot", "ok");
    let now = result["captured_at"].as_i64().unwrap();
    let mut warnings = Vec::new();
    let mut sources = Vec::new();
    let mut models = Vec::new();
    let mut quotas = Vec::new();
    let mut truncated = Vec::new();
    let filter = options.source.as_deref();
    let mut sections = 0;
    if columns(
        tx,
        "sources",
        &[
            "source",
            "enabled",
            "last_synced_at",
            "last_error",
            "files_seen",
            "rows_ingested",
        ],
    )? {
        sections += 1;
        let mut st = tx.prepare(
            "SELECT source,enabled,last_synced_at,last_error IS NOT NULL,files_seen,rows_ingested
            FROM sources WHERE (?1 IS NULL OR source=?1) ORDER BY source LIMIT 129",
        )?;
        for row in st.query_map([filter], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })? {
            let (source, enabled, last_synced_at, has_error, files, rows) = row?;
            if let Some(source) = label(&source) {
                sources.push(
                    json!({"source":source,"enabled":enabled!=0,"last_synced_at":last_synced_at,
                    "has_error":has_error,"files_seen":files.max(0),"rows_ingested":rows.max(0)}),
                );
            } else {
                warnings.push("identifier_redacted");
            }
        }
        if sources.len() > 128 {
            sources.truncate(128);
            truncated.push("sources");
        }
    } else {
        warnings.push("sources_schema_unavailable");
    }
    if columns(
        tx,
        "usage_events",
        &[
            "app",
            "provider_id",
            "model",
            "ts_start",
            "duration_ms",
            "ttft_ms",
        ],
    )? {
        sections += 1;
        let mut st = tx.prepare("SELECT app,provider_id,model,COUNT(*),MAX(ts_start),
            AVG(CASE WHEN duration_ms>0 THEN duration_ms END),AVG(CASE WHEN ttft_ms>0 THEN ttft_ms END)
            FROM usage_events WHERE ts_start>=?1 AND ts_start<=?2 AND model IS NOT NULL
            AND (?3 IS NULL OR app=?3) GROUP BY app,provider_id,model
            ORDER BY COUNT(*) DESC,app,provider_id,model LIMIT ?4")?;
        for row in st.query_map(
            params![
                now - i64::from(options.lookback_days) * 86_400_000,
                now,
                filter,
                (options.model_limit + 1) as i64
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<f64>>(5)?,
                    r.get::<_, Option<f64>>(6)?,
                ))
            },
        )? {
            let (app, provider, model, samples, last_seen, duration, ttft) = row?;
            if let (Some(app), Some(model)) = (label(&app), label(&model)) {
                let provider = provider.map(|p| {
                    label(&p).unwrap_or_else(|| {
                        warnings.push("identifier_redacted");
                        opaque(&p)
                    })
                });
                models.push(json!({"app":app,"provider_id":provider,"model":model,"samples":samples,
                    "last_seen":last_seen,"mean_duration_ms":finite(duration),"mean_ttft_ms":finite(ttft),
                    "evidence":"historical_usage","execution_availability":"unknown"}));
            } else {
                warnings.push("identifier_redacted");
            }
        }
        if models.len() > options.model_limit {
            models.truncate(options.model_limit);
            truncated.push("models");
        }
    } else {
        warnings.push("models_schema_unavailable");
    }
    if columns(
        tx,
        "quota_snapshots",
        &[
            "id",
            "app",
            "account",
            "captured_at",
            "window_kind",
            "used",
            "limit_value",
            "used_percent",
            "resets_at",
        ],
    )? {
        sections += 1;
        let mut st = tx.prepare("SELECT s.app,s.account,s.captured_at,s.window_kind,s.used,s.limit_value,s.used_percent,s.resets_at
            FROM (SELECT DISTINCT app,account,window_kind FROM quota_snapshots WHERE (?1 IS NULL OR app=?1)) k
            JOIN quota_snapshots s ON s.id=(SELECT id FROM quota_snapshots WHERE app=k.app AND account IS k.account
                AND window_kind=k.window_kind ORDER BY captured_at DESC,id DESC LIMIT 1)
            ORDER BY s.app,s.window_kind,s.account LIMIT ?2")?;
        for row in st.query_map(params![filter, (options.quota_limit + 1) as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<f64>>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<f64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
            ))
        })? {
            let (app, account, captured, window, used, limit, pct, reset) = row?;
            if let (Some(app), Some(window)) = (label(&app), label(&window)) {
                let allowance = ALLOWANCE_WINDOWS.contains(&window.as_str());
                let stale = captured <= 0
                    || now.saturating_sub(captured) > i64::from(options.quota_ttl_secs) * 1000;
                let future = captured > now;
                let reset_passed = reset.is_some_and(|r| r <= now);
                let pct = finite(pct).filter(|p| *p <= 100.0);
                let remaining = if allowance && !stale && !future && !reset_passed {
                    pct.map(|p| 100.0 - p)
                } else {
                    None
                };
                let semantic = if allowance {
                    "allowance_percent"
                } else if window == "credits" {
                    "remaining_balance"
                } else if window == "session_ctx" {
                    "context_usage"
                } else {
                    "unknown"
                };
                let state = if future {
                    "clock_skew"
                } else if stale {
                    "stale"
                } else if reset_passed {
                    "reset_passed"
                } else if allowance && pct.is_none() {
                    "invalid_percent"
                } else if semantic == "unknown" {
                    "unknown"
                } else {
                    "fresh"
                };
                quotas.push(json!({"app":app,"account_key":account.as_deref().map(opaque),"captured_at":captured,
                    "window_kind":window,"used":finite(used),"limit_value":finite(limit),"used_percent":pct,"resets_at":reset,
                    "remaining_percent":remaining,"remaining_balance":if semantic=="remaining_balance" && state=="fresh" {finite(used)} else {None},
                    "value_semantics":semantic,"state":state,"stale":stale,"reset_passed":reset_passed,
                    "scope":"app_account_window"}));
            } else {
                warnings.push("identifier_redacted");
            }
        }
        if quotas.len() > options.quota_limit {
            quotas.truncate(options.quota_limit);
            truncated.push("quotas");
        }
    } else {
        warnings.push("quotas_schema_unavailable");
    }
    warnings.sort_unstable();
    warnings.dedup();
    result["status"] = json!(if sections == 0 {
        "unavailable"
    } else if warnings.is_empty() && truncated.is_empty() {
        "ok"
    } else {
        "partial"
    });
    if sections == 0 {
        result["error"] = json!({"code":"ledger_schema_unavailable","retryable":false});
    }
    result["schema_version"] = if columns(tx, "schema_migrations", &["version"])? {
        json!(
            tx.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| r
                .get::<_, Option<
                i64,
            >>(
                0
            ))?
        )
    } else {
        Value::Null
    };
    result["sources"] = json!(sources);
    result["models"] = json!(models);
    result["quotas"] = json!(quotas);
    result["warnings"] = json!(warnings);
    result["truncated"] = json!(truncated);
    result["observation"] = json!({"consistent":true,"lookback_days":options.lookback_days,"quota_ttl_secs":options.quota_ttl_secs,
        "execution_availability":"unknown","latency_normalized":false});
    Ok(result)
}

/// Cooperative cross-process refresh gate. An OS file lock remains held for
/// the entire operation, including process pauses and heartbeat failures.
/// The persistent lease provides diagnostics, crash recovery and cooldown.
struct RefreshLease {
    _file_lock: std::fs::File,
    stop: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

fn state(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM app_state WHERE key=?1", [key], |r| {
        r.get(0)
    })
    .optional()
}

fn acquire_refresh_lock(path: &Path) -> anyhow::Result<Result<std::fs::File, &'static str>> {
    if path.is_dir() {
        anyhow::bail!("ledger_not_file");
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let canonical = if path.exists() {
        path.canonicalize()?
    } else {
        parent.canonicalize()?.join(
            path.file_name()
                .ok_or_else(|| anyhow::anyhow!("ledger_filename_missing"))?,
        )
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(canonical.with_extension("coordination-refresh.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Ok(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(Err("refresh_busy")),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

impl RefreshLease {
    #[cfg(test)]
    fn claim(path: &Path, cooldown_secs: u32) -> anyhow::Result<Result<Self, &'static str>> {
        Self::claim_with_interval(path, cooldown_secs, Duration::from_secs(20))
    }

    #[cfg(test)]
    fn claim_with_interval(
        path: &Path,
        cooldown_secs: u32,
        heartbeat_interval: Duration,
    ) -> anyhow::Result<Result<Self, &'static str>> {
        let file_lock = match acquire_refresh_lock(path)? {
            Ok(lock) => lock,
            Err(code) => return Ok(Err(code)),
        };
        Self::claim_locked(path, cooldown_secs, heartbeat_interval, file_lock)
    }

    fn claim_locked(
        path: &Path,
        cooldown_secs: u32,
        heartbeat_interval: Duration,
        file_lock: std::fs::File,
    ) -> anyhow::Result<Result<Self, &'static str>> {
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.busy_timeout(Duration::from_millis(750))?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let now = now_ms();
        // The OS lock is authoritative: successfully acquiring it proves no
        // conforming refresh is still alive, even if a crashed owner's JSON
        // lease has not expired. Reclaim it without a two-minute dead period.
        if let Some(raw) = state(&tx, COOLDOWN_KEY)?
            && raw.parse::<i64>().ok().is_some_and(|last| {
                last <= now && now.saturating_sub(last) < i64::from(cooldown_secs) * 1000
            })
        {
            return Ok(Err("refresh_throttled"));
        }
        let owner = format!("{}-{now}", std::process::id());
        let initial = json!({"owner":owner,"expires_at":now+120_000}).to_string();
        tx.execute(
            "INSERT OR REPLACE INTO app_state(key,value) VALUES (?1,?2)",
            params![LEASE_KEY, initial],
        )?;
        tx.commit()?;
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let signal = stop.clone();
        let live = alive.clone();
        let worker = std::thread::spawn(move || {
            let mut current = initial;
            let mut heartbeat_failed = false;
            loop {
                let (lock, changed) = &*signal;
                let guard = lock.lock().unwrap();
                if *guard {
                    break;
                }
                let (guard, _) = changed.wait_timeout(guard, heartbeat_interval).unwrap();
                if *guard {
                    break;
                }
                if heartbeat_failed {
                    continue;
                }
                let next = json!({"owner":owner,"expires_at":now_ms()+120_000}).to_string();
                if conn
                    .execute(
                        "UPDATE app_state SET value=?1 WHERE key=?2 AND value=?3",
                        params![next, LEASE_KEY, current],
                    )
                    .ok()
                    != Some(1)
                {
                    live.store(false, std::sync::atomic::Ordering::Release);
                    heartbeat_failed = true;
                    continue;
                }
                current = next;
            }
            // Completion time also throttles failures. Never delete another owner.
            if let Ok(tx) = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            {
                let deleted = tx
                    .execute(
                        "DELETE FROM app_state WHERE key=?1 AND value=?2",
                        params![LEASE_KEY, current],
                    )
                    .ok()
                    == Some(1);
                if deleted
                    && tx
                        .execute(
                            "INSERT OR REPLACE INTO app_state(key,value) VALUES (?1,?2)",
                            params![COOLDOWN_KEY, now_ms().to_string()],
                        )
                        .is_err()
                {
                    return;
                }
                let _ = tx.commit();
            }
        });
        Ok(Ok(Self {
            _file_lock: file_lock,
            stop,
            worker: Some(worker),
            alive,
        }))
    }
}

impl Drop for RefreshLease {
    fn drop(&mut self) {
        let (lock, changed) = &*self.stop;
        *lock.lock().unwrap() = true;
        changed.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Mutating operation, explicitly requested by a caller. Unknown/disabled
/// sources are rejected before discovery. Quota polling is separately opt-in.
pub fn refresh(
    path: &Path,
    source: Option<&str>,
    all: bool,
    poll_quota: bool,
    cooldown_secs: u32,
) -> Value {
    if all == source.is_some()
        || source.is_some_and(|s| !known_source(s))
        || !(1..=3600).contains(&cooldown_secs)
    {
        return failure("refresh", "invalid_refresh_scope");
    }
    match run_refresh(path, source, poll_quota, cooldown_secs) {
        Ok(value) => value,
        Err(_) => failure("refresh", "refresh_failed"),
    }
}

fn run_refresh(
    path: &Path,
    source: Option<&str>,
    poll_quota: bool,
    cooldown_secs: u32,
) -> anyhow::Result<Value> {
    let file_lock = match acquire_refresh_lock(path)? {
        Ok(lock) => lock,
        Err(code) => return Ok(failure("refresh", code)),
    };
    // Never restore a backup as a response to lock contention or corruption.
    let store = Store::try_open(path)?;
    let lease = match RefreshLease::claim_locked(
        path,
        cooldown_secs,
        Duration::from_secs(20),
        file_lock,
    )? {
        Ok(lease) => lease,
        Err(code) => return Ok(failure("refresh", code)),
    };
    let mut result = response("refresh", "ok");
    let mut rows = Vec::new();
    let engine = Engine::new(store)?;
    let health = engine.store.source_health()?;
    for adapter in adapters::registry() {
        let id = adapter.id();
        if source.is_some_and(|s| s != id) {
            continue;
        }
        if !lease.alive.load(std::sync::atomic::Ordering::Acquire) {
            rows.push(json!({"source":id,"status":"lease_lost"}));
            break;
        }
        if health.iter().any(|h| h.source == id && !h.enabled) {
            rows.push(json!({"source":id,"status":"disabled"}));
            continue;
        }
        let mut row = match engine.scan_source(id) {
            Ok(r) => json!({"source":id,"status":if r.errors.is_empty(){"ok"}else{"partial"},
                "files_seen":r.files_seen,"files_scanned":r.files_scanned,"events_ingested":r.events_ingested,
                "quotas_ingested":r.quotas,"error_count":r.errors.len()}),
            Err(_) => json!({"source":id,"status":"failed","error_code":"scan_failed"}),
        };
        if !lease.alive.load(std::sync::atomic::Ordering::Acquire) {
            row["status"] = json!("interrupted");
            row["error_code"] = json!("refresh_lease_lost");
        }
        row["quota_poll"] = json!("not_requested");
        if poll_quota {
            row["quota_poll"] = json!(if matches!(id, "codex" | "cursor") {
                "credentials_unavailable"
            } else {
                "unsupported"
            });
            if lease.alive.load(std::sync::atomic::Ordering::Acquire) {
                for outcome in quota::poll_selected(&[id]) {
                    let mut inserted = 0;
                    let mut failed = outcome.error.is_some();
                    for q in &outcome.quotas {
                        match engine.store.insert_quota(q) {
                            Ok(true) => inserted += 1,
                            Ok(false) => {}
                            Err(_) => failed = true,
                        }
                    }
                    row["quota_poll"] = json!(if failed { "failed" } else { "ok" });
                    row["quota_rows_inserted"] = json!(inserted);
                }
            } else {
                row["quota_poll"] = json!("lease_lost");
            }
        }
        rows.push(row);
    }
    if rows.iter().any(|r| {
        r["status"] != "ok"
            || !matches!(
                r["quota_poll"].as_str(),
                Some("ok" | "not_requested") | None
            )
    }) {
        result["status"] = json!("partial");
    }
    result["sources"] = json!(rows);
    result["completed_at"] = json!(now_ms());
    drop(lease);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    #[test]
    fn qualified_identifiers_are_compatible_without_admitting_paths_or_urls() {
        assert_eq!(label("provider:region"), Some("provider:region".into()));
        assert!(label("C:/private/file").is_none());
        assert!(label("https://private.example").is_none());
    }
    fn cleanup(path: PathBuf) {
        let lock = path.with_extension("coordination-refresh.lock");
        if lock.exists() {
            std::fs::remove_file(lock).unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }
    fn fixture() -> (PathBuf, Store) {
        let path = std::env::temp_dir().join(format!(
            "gtt-coordination-core-{}-{}.db",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Store::open(&path).unwrap();
        (path, store)
    }
    #[test]
    fn projection_remains_consistent_across_committed_wal_writer() {
        let (path, store) = fixture();
        store
            .conn()
            .execute(
                "INSERT INTO sources(source,rows_ingested) VALUES('codex',1)",
                [],
            )
            .unwrap();
        let mut reader =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let tx = reader.transaction().unwrap();
        let _: i64 = tx
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
            .unwrap();
        store
            .conn()
            .execute(
                "UPDATE sources SET rows_ingested=99 WHERE source='codex'",
                [],
            )
            .unwrap();
        let s = project_snapshot(&tx, &SnapshotOptions::default()).unwrap();
        assert_eq!(s["sources"][0]["rows_ingested"], 1);
        tx.commit().unwrap();
        drop(reader);
        drop(store);
        cleanup(path);
    }
    #[test]
    fn concurrent_claim_is_exclusive_and_completion_cooldown_is_shared() {
        let (path, store) = fixture();
        let first = RefreshLease::claim(&path, 30).unwrap().unwrap();
        assert!(matches!(
            RefreshLease::claim(&path, 30).unwrap(),
            Err("refresh_busy")
        ));
        drop(first);
        assert!(matches!(
            RefreshLease::claim(&path, 30).unwrap(),
            Err("refresh_throttled")
        ));
        drop(store);
        cleanup(path);
    }
    #[test]
    fn expired_lease_can_recover_and_old_owner_cannot_delete_new_owner() {
        let (path, store) = fixture();
        store
            .set_state(
                LEASE_KEY,
                &json!({"owner":"dead","expires_at":now_ms()-1}).to_string(),
            )
            .unwrap();
        let first = RefreshLease::claim(&path, 30).unwrap().unwrap();
        let replacement = json!({"owner":"replacement","expires_at":now_ms()+120_000}).to_string();
        store.set_state(LEASE_KEY, &replacement).unwrap();
        drop(first);
        assert_eq!(store.get_state(LEASE_KEY).unwrap(), Some(replacement));
        drop(store);
        cleanup(path);
    }
    #[test]
    fn expired_metadata_never_overrides_live_os_lock() {
        let (path, store) = fixture();
        let first = RefreshLease::claim(&path, 30).unwrap().unwrap();
        store
            .set_state(
                LEASE_KEY,
                &json!({"owner":"expired","expires_at":now_ms()-1}).to_string(),
            )
            .unwrap();
        assert!(matches!(
            RefreshLease::claim(&path, 30).unwrap(),
            Err("refresh_busy")
        ));
        drop(first);
        drop(store);
        cleanup(path);
    }
    #[test]
    fn heartbeat_database_failure_retains_gate_until_operation_finishes() {
        let (path, store) = fixture();
        let first = RefreshLease::claim_with_interval(&path, 30, Duration::from_millis(10))
            .unwrap()
            .unwrap();
        store.conn().execute_batch("BEGIN IMMEDIATE").unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while first.alive.load(Ordering::Acquire) && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let failed = !first.alive.load(Ordering::Acquire);
        store.conn().execute_batch("ROLLBACK").unwrap();
        assert!(failed, "heartbeat did not observe database lock");
        assert!(store.get_state(LEASE_KEY).unwrap().is_some());
        assert!(matches!(
            RefreshLease::claim(&path, 30).unwrap(),
            Err("refresh_busy")
        ));
        drop(first);
        assert!(store.get_state(LEASE_KEY).unwrap().is_none());
        drop(store);
        cleanup(path);
    }
    #[test]
    fn orphaned_fresh_lease_and_future_cooldown_recover_without_permanent_block() {
        let (path, store) = fixture();
        store
            .set_state(
                LEASE_KEY,
                &json!({"owner":"dead","expires_at":now_ms()+120_000}).to_string(),
            )
            .unwrap();
        store
            .set_state(COOLDOWN_KEY, &(now_ms() + 600_000).to_string())
            .unwrap();
        let lease = RefreshLease::claim(&path, 30).unwrap().unwrap();
        drop(lease);
        drop(store);
        cleanup(path);
    }
}
