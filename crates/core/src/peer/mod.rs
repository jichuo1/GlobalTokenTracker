//! LAN peer protocol `gtt.peer` v1 — read-only, versioned usage aggregates
//! another GTT device can pull to show this one next to its own.
//!
//! Peers exchange compact aggregates, never raw events: UTC hour × app × model
//! groups (so a viewer in any time zone can re-bucket them into its own local
//! days), the latest quota per app/account/window, and source health. Each
//! UTC day carries a content fingerprint of exactly the groups that are
//! exported, so a viewer re-pulls only days whose content changed — upserts,
//! prunes, archives and re-imports included, which a rowid watermark would
//! miss.
//!
//! Reads follow `coordination`: a read-only SQLite connection, one read
//! transaction per response, fixed error codes and no SQL, paths, project
//! names, session ids or `raw_ref` in any output. Groups use the cube's
//! `MODEL_EXPR` model identity and token semantics, pinned by the
//! equivalence tests below.

pub mod client;
pub mod discovery;
pub mod keys;
mod net;
pub mod pairing;
pub mod server;
pub mod trust;

use crate::coordination::{ALLOWANCE_WINDOWS, columns, finite, label, opaque};
use crate::cube::civil_date;
use crate::store::{MODEL_EXPR, Store, now_ms};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

pub const PROTOCOL: &str = "gtt.peer";
pub const PROTOCOL_VERSION: u32 = 1;
/// Width of a summary bucket: one UTC hour.
pub const BUCKET_MS: i64 = 3_600_000;
pub const DAY_MS: i64 = 86_400_000;
/// Day key of rows without `ts_start` (they count in unbounded totals only).
pub const UNDATED: &str = "undated";
/// Data endpoints; all but `hello` need a bearer token. (`pair` is the
/// pairing upgrade, not a data endpoint.)
pub const ENDPOINTS: [&str; 5] = ["hello", "days", "summary", "quotas", "sources"];
/// Upper bound of `known` fingerprints one summary request may carry.
pub const MAX_KNOWN_DAYS: usize = 4096;
const MAX_SOURCES: usize = 128;
const MAX_QUOTAS: usize = 256;
const DEVICE_ID_KEY: &str = "peer.device_id.v1";
const DEVICE_NAME_KEY: &str = "peer.device_name.v1";
const MAX_NAME_CHARS: usize = 64;

/// Fields every response (success or failure) starts with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub protocol: String,
    pub protocol_version: u32,
    pub gtt_version: String,
    pub operation: String,
    pub captured_at: i64,
    /// `ok` | `partial` | `unavailable`.
    pub status: String,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Meta {
    fn new(operation: &str) -> Self {
        Self {
            protocol: PROTOCOL.into(),
            protocol_version: PROTOCOL_VERSION,
            gtt_version: env!("CARGO_PKG_VERSION").into(),
            operation: operation.into(),
            captured_at: now_ms(),
            status: "ok".into(),
            warnings: Vec::new(),
        }
    }

    fn finish(&mut self, mut warnings: Vec<&'static str>) {
        warnings.sort_unstable();
        warnings.dedup();
        if !warnings.is_empty() {
            self.status = "partial".into();
        }
        self.warnings = warnings.into_iter().map(String::from).collect();
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    #[serde(flatten)]
    pub meta: Meta,
    pub error: ErrorBody,
}

/// A fixed-code failure document (`status = unavailable`).
pub fn failure(operation: &str, code: &str) -> Value {
    let mut meta = Meta::new(operation);
    meta.status = "unavailable".into();
    let retryable = matches!(code, "ledger_busy" | "ledger_unavailable" | "server_busy");
    serde_json::to_value(Failure {
        meta,
        error: ErrorBody {
            code: code.into(),
            retryable,
        },
    })
    .expect("failure serializes")
}

/// This device as peers see it. Created on the read-write path
/// ([`ensure_identity`]) — the read-only protocol path never writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub device_id: String,
    pub device_name: String,
    /// SHA-256 of the device certificate — the trust identity peers pin.
    /// Set by the server from its [`keys::DeviceKey`].
    pub fingerprint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    #[serde(flatten)]
    pub meta: Meta,
    pub device_id: String,
    pub device_name: String,
    /// Certificate fingerprint the server claims. Over TLS the client checks
    /// it equals the certificate actually presented.
    pub fingerprint: Option<String>,
    pub endpoints: Vec<String>,
}

/// Per-day fingerprints — what a viewer compares before pulling `summary`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Days {
    #[serde(flatten)]
    pub meta: Meta,
    pub device_id: String,
    pub schema_version: Option<i64>,
    pub bucket_ms: i64,
    /// UTC day (`YYYY-MM-DD` or `undated`) → content fingerprint.
    pub fingerprints: BTreeMap<String, String>,
}

/// One UTC hour × app × model aggregate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// UTC hour of day 0–23; `None` only in the `undated` day.
    pub hour: Option<u8>,
    pub app: String,
    pub model: String,
    pub events: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub unclassified_tokens: u64,
    pub credits: f64,
    pub cost_usd: f64,
    pub active_ms: u64,
    pub duration_ms: u64,
}

impl Group {
    /// The trend/pie headline convention: input+output+cache_read+unclassified.
    pub fn headline_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.unclassified_tokens
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaySummary {
    /// UTC day `YYYY-MM-DD`, or `undated`.
    pub day: String,
    /// Epoch ms of the UTC midnight opening `day`; `None` for `undated`.
    pub start_ms: Option<i64>,
    pub fingerprint: String,
    pub groups: Vec<Group>,
}

impl DaySummary {
    /// Epoch ms where `group`'s bucket starts; `None` when undated.
    pub fn bucket_start_ms(&self, group: &Group) -> Option<i64> {
        Some(self.start_ms? + i64::from(group.hour?) * BUCKET_MS)
    }

    /// Day index (days since 1970-01-01) of `group` in a fixed-offset local
    /// frame (`off_ms` east of UTC); `None` when undated. Exact for whole-hour
    /// offsets; with :30/:45 offsets a bucket straddling local midnight lands
    /// entirely on the day its start falls in.
    pub fn local_day(&self, group: &Group, off_ms: i64) -> Option<i64> {
        Some((self.bucket_start_ms(group)? + off_ms).div_euclid(DAY_MS))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    #[serde(flatten)]
    pub meta: Meta,
    pub device_id: String,
    pub bucket_ms: i64,
    /// Lower bound (UTC day) the response covers; older days are out of scope.
    pub from: Option<String>,
    /// Days whose fingerprint the viewer did not know, with all their groups.
    pub days: Vec<DaySummary>,
    /// Known days that no longer have any rows.
    pub removed: Vec<String>,
    /// Known days whose fingerprint still matches (not resent).
    pub unchanged: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    pub app: String,
    /// SHA-256 pseudonym of the account — equal across devices for the same
    /// account, so viewers can de-duplicate. Quotas are never summable.
    pub account_key: Option<String>,
    pub window_kind: String,
    pub captured_at: i64,
    pub used: Option<f64>,
    pub limit_value: Option<f64>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub value_semantics: String,
    pub scope: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Quotas {
    #[serde(flatten)]
    pub meta: Meta,
    pub device_id: String,
    pub quotas: Vec<Quota>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub source: String,
    pub enabled: bool,
    pub last_synced_at: Option<i64>,
    pub files_seen: u64,
    pub rows_ingested: u64,
    pub has_error: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sources {
    #[serde(flatten)]
    pub meta: Meta,
    pub device_id: String,
    pub sources: Vec<Source>,
    pub truncated: bool,
}

/// What the viewer already holds, for an incremental summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SummaryQuery {
    /// Only UTC days `>= from` (plus `undated`) are in scope.
    pub from: Option<String>,
    /// Day key → fingerprint the viewer holds.
    pub known: BTreeMap<String, String>,
}

impl SummaryQuery {
    /// Parse the URL query (`from=YYYY-MM-DD&known=DAY:FP,DAY:FP`). `None`
    /// when malformed or over [`MAX_KNOWN_DAYS`].
    pub fn parse(query: &str) -> Option<Self> {
        let mut out = Self::default();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = percent_decode(value)?;
            match key {
                "from" if out.from.is_none() => {
                    if !valid_date(&value) {
                        return None;
                    }
                    out.from = Some(value);
                }
                "known" => {
                    for entry in value.split(',').filter(|e| !e.is_empty()) {
                        let (day, fp) = entry.split_once(':')?;
                        if !(day == UNDATED || valid_date(day)) || !valid_fingerprint(fp) {
                            return None;
                        }
                        out.known.insert(day.into(), fp.into());
                        if out.known.len() > MAX_KNOWN_DAYS {
                            return None;
                        }
                    }
                }
                _ => return None,
            }
        }
        Some(out)
    }

    /// The URL query form of this request (empty when there is nothing to say).
    pub fn to_query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(from) = &self.from {
            parts.push(format!("from={from}"));
        }
        if !self.known.is_empty() {
            let known: Vec<String> = self.known.iter().map(|(d, f)| format!("{d}:{f}")).collect();
            parts.push(format!("known={}", known.join(",")));
        }
        parts.join("&")
    }
}

/// One protocol request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Hello,
    Days,
    Summary(SummaryQuery),
    Quotas,
    Sources,
}

impl Request {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Days => "days",
            Self::Summary(_) => "summary",
            Self::Quotas => "quotas",
            Self::Sources => "sources",
        }
    }
}

/// Answer one request against the ledger at `path`, read-only. `version` is
/// the protocol version the caller asked for; anything but
/// [`PROTOCOL_VERSION`] fails before the ledger is opened.
pub fn respond(path: &Path, identity: &Identity, version: u32, request: &Request) -> Value {
    let operation = request.operation();
    if version != PROTOCOL_VERSION {
        return failure(operation, "unsupported_protocol_version");
    }
    if let Err(error) = crate::external_interface::check(path) {
        return failure(operation, error.code());
    }
    if *request == Request::Hello {
        // Unauthenticated: identity only, no ledger access.
        return serde_json::to_value(Hello {
            meta: Meta::new("hello"),
            device_id: identity.device_id.clone(),
            device_name: identity.device_name.clone(),
            fingerprint: identity.fingerprint.clone(),
            endpoints: ENDPOINTS.iter().map(|e| (*e).to_string()).collect(),
        })
        .unwrap_or_else(|_| failure(operation, "ledger_unavailable"));
    }
    if !path.is_file() {
        return failure(operation, "ledger_missing");
    }
    match read(path, |tx| project(tx, identity, request)) {
        Ok(Ok(value)) => value,
        Ok(Err(code)) => failure(operation, code),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            failure(operation, "ledger_busy")
        }
        Err(_) => failure(operation, "ledger_unavailable"),
    }
}

/// A projected response, or the fixed error code it failed with.
type Projection = Result<Value, &'static str>;

fn project(
    tx: &Connection,
    identity: &Identity,
    request: &Request,
) -> rusqlite::Result<Projection> {
    fn json<T: Serialize>(r: Result<T, &'static str>) -> Projection {
        r.and_then(|v| serde_json::to_value(v).map_err(|_| "ledger_unavailable"))
    }
    Ok(match request {
        Request::Hello => Err("ledger_unavailable"),
        Request::Days => json(days(tx, identity)?),
        Request::Summary(query) => json(summary(tx, identity, query)?),
        Request::Quotas => json(quotas(tx, identity)?),
        Request::Sources => json(sources(tx, identity)?),
    })
}

/// Read-only connection + one read transaction pinning a single WAL snapshot.
fn read<T>(path: &Path, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(750))?;
    conn.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")?;
    let tx = conn.transaction()?;
    // First read fixes the WAL snapshot used by everything below.
    let _: i64 = tx.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
    let result = f(&tx)?;
    tx.commit()?;
    Ok(result)
}

const USAGE_COLUMNS: &[&str] = &[
    "ts_start",
    "app",
    "model",
    "request_model",
    "pricing_model",
    "input_tokens",
    "output_tokens",
    "reasoning_tokens",
    "cache_read_tokens",
    "cache_write_5m_tokens",
    "cache_write_1h_tokens",
    "unclassified_tokens",
    "credits",
    "cost_usd",
    "active_ms",
    "duration_ms",
];

fn days(tx: &Connection, identity: &Identity) -> rusqlite::Result<Result<Days, &'static str>> {
    if !columns(tx, "usage_events", USAGE_COLUMNS)? {
        return Ok(Err("ledger_schema_unavailable"));
    }
    let mut meta = Meta::new("days");
    let mut warnings = Vec::new();
    let fingerprints = scan_days(tx, None, &mut warnings)?
        .into_values()
        .map(|d| (d.day, d.fingerprint))
        .collect();
    meta.finish(warnings);
    Ok(Ok(Days {
        meta,
        device_id: identity.device_id.clone(),
        schema_version: schema_version(tx)?,
        bucket_ms: BUCKET_MS,
        fingerprints,
    }))
}

fn summary(
    tx: &Connection,
    identity: &Identity,
    query: &SummaryQuery,
) -> rusqlite::Result<Result<Summary, &'static str>> {
    if !columns(tx, "usage_events", USAGE_COLUMNS)? {
        return Ok(Err("ledger_schema_unavailable"));
    }
    let mut meta = Meta::new("summary");
    let mut warnings = Vec::new();
    let from_ms = match query.from.as_deref().map(date_start_ms) {
        Some(None) => return Ok(Err("invalid_request")),
        Some(Some(ms)) => Some(ms),
        None => None,
    };
    let current = scan_days(tx, from_ms, &mut warnings)?;
    let in_scope =
        |day: &str| day == UNDATED || query.from.as_deref().is_none_or(|from| day >= from);
    let removed = query
        .known
        .keys()
        .filter(|day| in_scope(day) && !current.contains_key(*day))
        .cloned()
        .collect();
    let mut unchanged = 0;
    let mut days = Vec::new();
    for (key, day) in current {
        if query.known.get(&key) == Some(&day.fingerprint) {
            unchanged += 1;
        } else {
            days.push(day);
        }
    }
    meta.finish(warnings);
    Ok(Ok(Summary {
        meta,
        device_id: identity.device_id.clone(),
        bucket_ms: BUCKET_MS,
        from: query.from.clone(),
        days,
        removed,
        unchanged,
    }))
}

/// One scan of `usage_events` into UTC hour × app × model groups, keyed by
/// UTC day, each day fingerprinted over exactly what is exported.
fn scan_days(
    tx: &Connection,
    from_ms: Option<i64>,
    warnings: &mut Vec<&'static str>,
) -> rusqlite::Result<BTreeMap<String, DaySummary>> {
    let mut st = tx.prepare(&format!(
        "SELECT ts_start / {BUCKET_MS} AS h, app, {MODEL_EXPR} AS m, COUNT(*),
                COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                COALESCE(SUM(reasoning_tokens),0), COALESCE(SUM(cache_read_tokens),0),
                COALESCE(SUM(cache_write_5m_tokens+cache_write_1h_tokens),0),
                COALESCE(SUM(unclassified_tokens),0), COALESCE(SUM(credits),0),
                COALESCE(SUM(cost_usd),0), COALESCE(SUM(active_ms),0),
                COALESCE(SUM(duration_ms),0)
         FROM usage_events WHERE ?1 IS NULL OR ts_start IS NULL OR ts_start >= ?1
         GROUP BY h, app, m"
    ))?;
    let count = |r: &rusqlite::Row, i: usize| -> rusqlite::Result<u64> {
        Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0))
    };
    let mut days: BTreeMap<String, DaySummary> = BTreeMap::new();
    let mut rows = st.query(params![from_ms])?;
    while let Some(r) = rows.next()? {
        let bucket: Option<i64> = r.get(0)?;
        let (Some(app), Some(model)) = (
            export_label(&r.get::<_, String>(1)?, warnings),
            export_label(&r.get::<_, String>(2)?, warnings),
        ) else {
            continue;
        };
        let (key, start_ms, hour) = match bucket {
            Some(b) => {
                let day = b.div_euclid(24);
                (
                    civil_date(day),
                    Some(day * DAY_MS),
                    u8::try_from(b.rem_euclid(24)).ok(),
                )
            }
            None => (UNDATED.to_string(), None, None),
        };
        let group = Group {
            hour,
            app,
            model,
            events: count(r, 3)?,
            input_tokens: count(r, 4)?,
            output_tokens: count(r, 5)?,
            reasoning_tokens: count(r, 6)?,
            cache_read_tokens: count(r, 7)?,
            cache_write_tokens: count(r, 8)?,
            unclassified_tokens: count(r, 9)?,
            credits: finite(r.get(10)?).unwrap_or(0.0),
            cost_usd: finite(r.get(11)?).unwrap_or(0.0),
            active_ms: count(r, 12)?,
            duration_ms: count(r, 13)?,
        };
        days.entry(key.clone())
            .or_insert_with(|| DaySummary {
                day: key,
                start_ms,
                fingerprint: String::new(),
                groups: Vec::new(),
            })
            .groups
            .push(group);
    }
    for day in days.values_mut() {
        day.groups
            .sort_by(|a, b| (a.hour, &a.app, &a.model).cmp(&(b.hour, &b.app, &b.model)));
        day.fingerprint = fingerprint(&day.groups);
    }
    Ok(days)
}

/// App/model labels may come from untrusted session data. Conforming labels
/// (and `MODEL_EXPR`'s `?`) pass; anything else becomes an opaque stable key
/// so totals stay complete without exporting the raw text.
fn export_label(value: &str, warnings: &mut Vec<&'static str>) -> Option<String> {
    if value == "?" {
        return Some(value.into());
    }
    label(value).or_else(|| {
        warnings.push("identifier_redacted");
        Some(format!("redacted-{}", opaque(value)))
    })
}

/// SHA-256 over the canonical (sorted) exported groups, first 8 bytes in hex.
pub fn fingerprint(groups: &[Group]) -> String {
    let mut h = Sha256::new();
    for g in groups {
        h.update([g.hour.unwrap_or(0xff)]);
        for text in [&g.app, &g.model] {
            h.update((text.len() as u64).to_le_bytes());
            h.update(text.as_bytes());
        }
        for n in [
            g.events,
            g.input_tokens,
            g.output_tokens,
            g.reasoning_tokens,
            g.cache_read_tokens,
            g.cache_write_tokens,
            g.unclassified_tokens,
            g.credits.to_bits(),
            g.cost_usd.to_bits(),
            g.active_ms,
            g.duration_ms,
        ] {
            h.update(n.to_le_bytes());
        }
    }
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn quotas(tx: &Connection, identity: &Identity) -> rusqlite::Result<Result<Quotas, &'static str>> {
    if !columns(
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
        return Ok(Err("ledger_schema_unavailable"));
    }
    let mut meta = Meta::new("quotas");
    let mut warnings = Vec::new();
    let mut st = tx.prepare(
        "SELECT s.app,s.account,s.captured_at,s.window_kind,s.used,s.limit_value,s.used_percent,s.resets_at
        FROM (SELECT DISTINCT app,account,window_kind FROM quota_snapshots) k
        JOIN quota_snapshots s ON s.id=(SELECT id FROM quota_snapshots WHERE app=k.app AND account IS k.account
            AND window_kind=k.window_kind ORDER BY captured_at DESC,id DESC LIMIT 1)
        ORDER BY s.app,s.window_kind,s.account LIMIT ?1",
    )?;
    let mut out = Vec::new();
    for row in st.query_map([(MAX_QUOTAS + 1) as i64], |r| {
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
        let (app, account, captured_at, window, used, limit, pct, resets_at) = row?;
        let (Some(app), Some(window_kind)) = (label(&app), label(&window)) else {
            warnings.push("identifier_redacted");
            continue;
        };
        let value_semantics = if ALLOWANCE_WINDOWS.contains(&window_kind.as_str()) {
            "allowance_percent"
        } else if window_kind == "credits" {
            "remaining_balance"
        } else if window_kind == "session_ctx" {
            "context_usage"
        } else {
            "unknown"
        };
        out.push(Quota {
            app,
            account_key: account.as_deref().map(opaque),
            window_kind,
            captured_at,
            used: finite(used),
            limit_value: finite(limit),
            used_percent: finite(pct).filter(|p| *p <= 100.0),
            resets_at,
            value_semantics: value_semantics.into(),
            scope: "app_account_window".into(),
        });
    }
    let truncated = out.len() > MAX_QUOTAS;
    out.truncate(MAX_QUOTAS);
    if truncated {
        warnings.push("truncated");
    }
    meta.finish(warnings);
    Ok(Ok(Quotas {
        meta,
        device_id: identity.device_id.clone(),
        quotas: out,
        truncated,
    }))
}

fn sources(
    tx: &Connection,
    identity: &Identity,
) -> rusqlite::Result<Result<Sources, &'static str>> {
    if !columns(
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
        return Ok(Err("ledger_schema_unavailable"));
    }
    let mut meta = Meta::new("sources");
    let mut warnings = Vec::new();
    let mut st = tx.prepare(
        "SELECT source,enabled,last_synced_at,last_error IS NOT NULL,files_seen,rows_ingested
        FROM sources ORDER BY source LIMIT ?1",
    )?;
    let mut out = Vec::new();
    for row in st.query_map([(MAX_SOURCES + 1) as i64], |r| {
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
        let Some(source) = label(&source) else {
            warnings.push("identifier_redacted");
            continue;
        };
        out.push(Source {
            source,
            enabled: enabled != 0,
            last_synced_at,
            files_seen: u64::try_from(files).unwrap_or(0),
            rows_ingested: u64::try_from(rows).unwrap_or(0),
            has_error,
        });
    }
    let truncated = out.len() > MAX_SOURCES;
    out.truncate(MAX_SOURCES);
    if truncated {
        warnings.push("truncated");
    }
    meta.finish(warnings);
    Ok(Ok(Sources {
        meta,
        device_id: identity.device_id.clone(),
        sources: out,
        truncated,
    }))
}

fn schema_version(tx: &Connection) -> rusqlite::Result<Option<i64>> {
    if columns(tx, "schema_migrations", &["version"])? {
        tx.query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
    } else {
        Ok(None)
    }
}

/// Load (or create once) this device's identity. Read-write: call it where a
/// [`Store`] is open anyway — `peer serve` startup or a shell — never from
/// the read-only protocol path. `name` (when given) is sanitized and saved.
pub fn ensure_identity(store: &Store, name: Option<&str>) -> anyhow::Result<Identity> {
    let device_id = match store.get_state(DEVICE_ID_KEY)? {
        Some(id) if valid_device_id(&id) => id,
        _ => {
            let id = new_device_id();
            store.set_state(DEVICE_ID_KEY, &id)?;
            id
        }
    };
    if let Some(name) = name.and_then(clean_name) {
        store.set_state(DEVICE_NAME_KEY, &name)?;
    }
    let device_name = store
        .get_state(DEVICE_NAME_KEY)?
        .and_then(|n| clean_name(&n))
        .or_else(|| {
            ["COMPUTERNAME", "HOSTNAME"]
                .iter()
                .find_map(|k| std::env::var(k).ok().and_then(|n| clean_name(&n)))
        })
        .unwrap_or_else(|| format!("GTT-{}", &device_id[..6]));
    Ok(Identity {
        device_id,
        device_name,
        fingerprint: None,
    })
}

fn valid_device_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 128 random bits from std's OS-seeded `RandomState` (plus time and pid),
/// hashed — an identifier, not a secret; no extra dependency.
fn new_device_id() -> String {
    use std::hash::BuildHasher;
    let mut h = Sha256::new();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    for i in 0u64..4 {
        h.update(
            std::collections::hash_map::RandomState::new()
                .hash_one(i)
                .to_le_bytes(),
        );
    }
    h.finalize()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A display name: trimmed, no control characters, at most 64 characters.
pub(crate) fn clean_name(name: &str) -> Option<String> {
    let name: String = name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    (!name.trim().is_empty()).then(|| name.trim().to_string())
}

fn valid_date(day: &str) -> bool {
    day.len() == 10 && date_start_ms(day).is_some()
}

/// Epoch ms of UTC midnight opening `YYYY-MM-DD`.
fn date_start_ms(day: &str) -> Option<i64> {
    let date: jiff::civil::Date = day.parse().ok()?;
    let ts = date.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    (civil_date(ts.as_millisecond().div_euclid(DAY_MS)) == day).then(|| ts.as_millisecond())
}

fn valid_fingerprint(fp: &str) -> bool {
    fp.len() == 16 && fp.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Minimal `%XX` decoding (no `+`): the query alphabet here is ASCII.
pub(crate) fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `%XX`-encode everything but unreserved ASCII.
pub(crate) fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Every fixed error code of the protocol (server and client side).
pub const ERROR_CODES: &[&str] = &[
    "unsupported_protocol_version",
    "external_interface_disabled",
    "external_interface_settings_invalid",
    "ledger_missing",
    "ledger_busy",
    "ledger_unavailable",
    "ledger_schema_unavailable",
    "invalid_request",
    "unknown_endpoint",
    "method_not_allowed",
    "request_too_large",
    "request_timeout",
    "host_not_allowed",
    "server_busy",
    "auth_required",
    "auth_invalid",
    "auth_unavailable",
    "tls_required",
    "pairing_unavailable",
    "pairing_code_expired",
    "pairing_busy",
    "pairing_locked",
    "pairing_failed",
    "pairing_aborted",
    "pairing_refused",
    "pairing_code_reused",
    "invalid_code",
    "not_paired",
    "ambiguous_peer",
    "connect_failed",
    "tls_failed",
    "plaintext_not_allowed",
    "peer_fingerprint_mismatch",
    "malformed_response",
    "unknown_viewer",
    "ambiguous_viewer",
    "unknown_peer",
];

/// A remote string as one of [`ERROR_CODES`] — nothing else is echoed.
pub(crate) fn known_code(code: &str) -> Option<&'static str> {
    ERROR_CODES.iter().copied().find(|c| *c == code)
}

#[cfg(test)]
mod tests;
