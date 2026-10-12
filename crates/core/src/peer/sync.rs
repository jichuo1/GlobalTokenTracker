//! Viewer side: pull every enabled paired device into the ledger's peer
//! tables, incrementally by per-UTC-day fingerprint.
//!
//! Network work runs on one short-lived thread per device (pin-verified TLS,
//! optional mDNS re-location), bounded by a per-device timeout, so one slow or
//! failing device never holds up the others. Results are applied on the
//! caller's thread, one transaction per device: changed days replaced
//! wholesale, removed days deleted, quotas replaced. Peer data only ever
//! lands in `peer_*` tables, which the serving path never reads — so it is
//! never re-shared.

use super::client::{Target, candidates_for, fetch};
use super::devices::{PeerDevice, Registry};
use super::{DaySummary, Days, Quota, Quotas, Summary, SummaryQuery, UNDATED};
use crate::store::{Store, now_ms};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default per-device budget (mDNS lookup + three requests).
pub const DEVICE_TIMEOUT: Duration = Duration::from_secs(45);
/// How often [`sync_if_due`] lets a sync through.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(300);
const LAST_SYNC_KEY: &str = "peer.last_sync_at.v1";
/// Sanity bound on one day's groups (24 hours × apps × models).
const MAX_GROUPS_PER_DAY: usize = 24 * 2048;

#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// Look paired devices up on mDNS by device id before their last address.
    pub rediscover: bool,
    pub device_timeout: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            rediscover: true,
            device_timeout: DEVICE_TIMEOUT,
        }
    }
}

/// The outcome for one device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct DeviceSync {
    pub device_id: String,
    pub name: String,
    pub ok: bool,
    pub error_code: Option<String>,
    pub days_changed: usize,
    pub days_removed: usize,
    pub days_unchanged: usize,
    pub quotas: usize,
    pub quotas_changed: bool,
    pub address: Option<String>,
    pub ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SyncReport {
    pub devices: Vec<DeviceSync>,
    /// Peer data changed — cached aggregates (the cube) must be rebuilt.
    pub changed: bool,
}

/// What one device's worker brings back.
struct Pulled {
    addr: SocketAddr,
    gtt_version: String,
    protocol_version: u32,
    days: Vec<DaySummary>,
    removed: BTreeSet<String>,
    unchanged: usize,
    quotas: Vec<Quota>,
}

pub(crate) type Locate = Arc<dyn Fn(&str, &str) -> Vec<SocketAddr> + Send + Sync>;

/// Pull every enabled paired device. Errors are recorded per device
/// (`peers.last_error_code`); the call itself fails only on ledger errors.
pub fn sync_all(store: &Store, options: &SyncOptions) -> Result<SyncReport> {
    let rediscover = options.rediscover;
    let locate: Locate = Arc::new(move |id: &str, fp: &str| {
        if rediscover {
            super::discovery::locate(id, fp, super::discovery::LOCATE_TIMEOUT)
        } else {
            Vec::new()
        }
    });
    sync_all_via(store, options, &locate)
}

/// The schedule hook for shells: sync when the last one is older than
/// `interval` (the stamp is taken before pulling, so concurrent callers do not
/// pile up). `None` = not due.
pub fn sync_if_due(
    store: &Store,
    interval: Duration,
    options: &SyncOptions,
) -> Result<Option<SyncReport>> {
    let now = now_ms();
    let last = store
        .get_state(LAST_SYNC_KEY)?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let interval_ms = i64::try_from(interval.as_millis()).unwrap_or(i64::MAX);
    if now.saturating_sub(last) < interval_ms && last <= now {
        return Ok(None);
    }
    store.set_state(LAST_SYNC_KEY, &now.to_string())?;
    sync_all(store, options).map(Some)
}

pub(crate) fn sync_all_via(
    store: &Store,
    options: &SyncOptions,
    locate: &Locate,
) -> Result<SyncReport> {
    let registry = Registry::open(store)?;
    let devices: Vec<PeerDevice> = registry.list()?.into_iter().filter(|d| d.enabled).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let started = Instant::now();
    let mut pending = BTreeMap::new();
    for device in &devices {
        let Some(token) = registry.token(&device.device_id)? else {
            pending.insert(device.device_id.clone(), Some(Err("not_paired")));
            continue;
        };
        let known = known_days(store.conn(), &device.device_id)?;
        let id = device.device_id.clone();
        let (tx, locate, device) = (tx.clone(), Arc::clone(locate), device.clone());
        pending.insert(id.clone(), None);
        let spawned = std::thread::Builder::new()
            .name("gtt-peer-sync".into())
            .spawn(move || {
                let located = locate(&device.device_id, &device.fingerprint);
                let target = Target {
                    candidates: candidates_for(&located, &device.last_address),
                    pin: Some(device.fingerprint.clone()),
                    token: Some(token),
                    plaintext: false,
                };
                let _ = tx.send((device.device_id.clone(), pull_device(&target, &known)));
            });
        if spawned.is_err() {
            pending.insert(id, Some(Err("sync_failed")));
        }
    }
    drop(tx);
    let deadline = started + options.device_timeout;
    while pending.values().any(Option::is_none) {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok((id, result)) => {
                pending.insert(id, Some(result));
            }
            Err(_) => break,
        }
    }
    let mut report = SyncReport::default();
    for device in &devices {
        let result = pending
            .remove(&device.device_id)
            .flatten()
            .unwrap_or(Err("sync_timeout"));
        let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let outcome = apply(store, device, result)?;
        report.changed |=
            outcome.days_changed > 0 || outcome.days_removed > 0 || outcome.quotas_changed;
        report.devices.push(DeviceSync { ms, ..outcome });
    }
    Ok(report)
}

fn known_days(conn: &Connection, device_id: &str) -> rusqlite::Result<BTreeMap<String, String>> {
    let mut st =
        conn.prepare("SELECT utc_day, fingerprint FROM peer_day_fps WHERE device_id=?1")?;
    st.query_map([device_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect()
}

/// The fixed error code a non-OK response carries.
fn response_error(status: u16, body: &Value) -> Option<&'static str> {
    let code = body["error"]["code"].as_str().and_then(super::known_code);
    if status != 200 {
        return Some(code.unwrap_or("sync_failed"));
    }
    (body["status"] == "unavailable").then(|| code.unwrap_or("sync_failed"))
}

fn get<T: serde::de::DeserializeOwned>(
    target: &Target,
    endpoint: &str,
    query: &str,
) -> Result<(SocketAddr, T), &'static str> {
    let fetched = fetch(target, super::PROTOCOL_VERSION, endpoint, query)?;
    if let Some(code) = response_error(fetched.status, &fetched.body) {
        return Err(code);
    }
    let parsed = serde_json::from_value(fetched.body).map_err(|_| "malformed_response")?;
    Ok((fetched.addr, parsed))
}

/// `days` → diff → `summary` for the changed span → `quotas`.
fn pull_device(target: &Target, known: &BTreeMap<String, String>) -> Result<Pulled, &'static str> {
    let (addr, days): (_, Days) = get(target, "days", "")?;
    // Later requests go to the address that answered.
    let target = Target {
        candidates: vec![addr],
        ..target.clone()
    };
    let changed: BTreeSet<&String> = days
        .fingerprints
        .iter()
        .filter(|(day, fp)| known.get(*day) != Some(*fp))
        .map(|(day, _)| day)
        .collect();
    let mut removed: BTreeSet<String> = known
        .keys()
        .filter(|day| !days.fingerprints.contains_key(*day))
        .cloned()
        .collect();
    let mut pulled_days = Vec::new();
    let mut unchanged = days.fingerprints.len() - changed.len();
    if !changed.is_empty() {
        // Ask only from the oldest changed day on, sending what we hold there.
        let from = changed
            .iter()
            .filter(|d| d.as_str() != UNDATED)
            .min()
            .map(|d| (*d).clone())
            .or_else(|| {
                days.fingerprints
                    .keys()
                    .filter(|d| *d != UNDATED)
                    .max()
                    .cloned()
            });
        let query = SummaryQuery {
            known: known
                .iter()
                .filter(|(day, _)| {
                    day.as_str() == UNDATED || from.as_ref().is_none_or(|f| *day >= f)
                })
                .map(|(d, f)| (d.clone(), f.clone()))
                .collect(),
            from,
        };
        let (_, summary): (_, Summary) = get(&target, "summary", &query.to_query())?;
        for day in &summary.days {
            validate(day)?;
        }
        removed.extend(summary.removed.iter().cloned());
        unchanged = summary
            .unchanged
            .max(unchanged.saturating_sub(summary.days.len()));
        pulled_days = summary.days;
    }
    let (_, quotas): (_, Quotas) = get(&target, "quotas", "")?;
    Ok(Pulled {
        addr,
        gtt_version: days.meta.gtt_version.chars().take(32).collect(),
        protocol_version: days.meta.protocol_version,
        days: pulled_days,
        removed,
        unchanged,
        quotas: quotas.quotas,
    })
}

/// Peer data is untrusted: the day key, bucket start and hours must agree,
/// and labels must pass the same checks the server applies.
fn validate(day: &DaySummary) -> Result<(), &'static str> {
    let bad = "malformed_response";
    if day.groups.len() > MAX_GROUPS_PER_DAY {
        return Err(bad);
    }
    if day.day == UNDATED {
        if day.start_ms.is_some() || day.groups.iter().any(|g| g.hour.is_some()) {
            return Err(bad);
        }
    } else if super::date_start_ms(&day.day) != day.start_ms || day.start_ms.is_none() {
        return Err(bad);
    }
    for g in &day.groups {
        let label_ok = |s: &str| {
            s == "?"
                || crate::coordination::label(s).is_some()
                || s.strip_prefix("redacted-")
                    .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        };
        if (day.day != UNDATED && g.hour.is_none_or(|h| h > 23))
            || !label_ok(&g.app)
            || !label_ok(&g.model)
            || !(g.credits.is_finite() && g.credits >= 0.0)
            || !(g.cost_usd.is_finite() && g.cost_usd >= 0.0)
        {
            return Err(bad);
        }
    }
    Ok(())
}

/// UTC epoch-hour range `[lo, hi]` of a day key; `-1` alone for `undated`.
fn hour_span(day: &str) -> Option<(i64, i64)> {
    if day == UNDATED {
        return Some((-1, -1));
    }
    let start = super::date_start_ms(day)? / super::BUCKET_MS;
    Some((start, start + 23))
}

/// Record one device's result in a single transaction.
fn apply(
    store: &Store,
    device: &PeerDevice,
    result: Result<Pulled, &'static str>,
) -> Result<DeviceSync> {
    let now = now_ms();
    let mut out = DeviceSync {
        device_id: device.device_id.clone(),
        name: device.name.clone(),
        ..DeviceSync::default()
    };
    let conn = store.conn();
    let pulled = match result {
        Ok(p) => p,
        Err(code) => {
            conn.execute(
                "UPDATE peers SET last_pull_at=?2, last_error_code=?3 WHERE device_id=?1",
                params![device.device_id, now, code],
            )?;
            out.error_code = Some(code.into());
            return Ok(out);
        }
    };
    let id = &device.device_id;
    let tx = conn.unchecked_transaction()?;
    for day in &pulled.removed {
        if let Some((lo, hi)) = hour_span(day) {
            tx.execute(
                "DELETE FROM peer_groups WHERE device_id=?1 AND utc_hour BETWEEN ?2 AND ?3",
                params![id, lo, hi],
            )?;
        }
        tx.execute(
            "DELETE FROM peer_day_fps WHERE device_id=?1 AND utc_day=?2",
            params![id, day],
        )?;
    }
    {
        let mut insert = tx.prepare(
            "INSERT OR REPLACE INTO peer_groups(device_id,utc_hour,app,model,events,input_tokens,
                output_tokens,reasoning_tokens,cache_read_tokens,cache_write_tokens,unclassified_tokens,
                credits,cost_usd,active_ms,duration_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        )?;
        for day in &pulled.days {
            let Some((lo, hi)) = hour_span(&day.day) else {
                continue;
            };
            tx.execute(
                "DELETE FROM peer_groups WHERE device_id=?1 AND utc_hour BETWEEN ?2 AND ?3",
                params![id, lo, hi],
            )?;
            for g in &day.groups {
                let hour = g.hour.map_or(-1, |h| lo + i64::from(h));
                let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
                insert.execute(params![
                    id,
                    hour,
                    g.app,
                    g.model,
                    n(g.events),
                    n(g.input_tokens),
                    n(g.output_tokens),
                    n(g.reasoning_tokens),
                    n(g.cache_read_tokens),
                    n(g.cache_write_tokens),
                    n(g.unclassified_tokens),
                    g.credits,
                    g.cost_usd,
                    n(g.active_ms),
                    n(g.duration_ms)
                ])?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO peer_day_fps(device_id,utc_day,fingerprint) VALUES(?1,?2,?3)",
                params![id, day.day, day.fingerprint],
            )?;
        }
    }
    let before: Vec<(String, String, String, i64)> = {
        let mut st = tx.prepare(
            "SELECT app,account_key,window_kind,captured_at FROM peer_quotas WHERE device_id=?1
             ORDER BY 1,2,3",
        )?;
        st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    tx.execute("DELETE FROM peer_quotas WHERE device_id=?1", [id])?;
    for q in &pulled.quotas {
        if crate::coordination::label(&q.app).is_none()
            || crate::coordination::label(&q.window_kind).is_none()
        {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO peer_quotas(device_id,app,account_key,window_kind,captured_at,
                used,limit_value,used_percent,resets_at,value_semantics)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                id,
                q.app,
                q.account_key.clone().unwrap_or_default(),
                q.window_kind,
                q.captured_at,
                crate::coordination::finite(q.used),
                crate::coordination::finite(q.limit_value),
                crate::coordination::finite(q.used_percent).filter(|p| *p <= 100.0),
                q.resets_at,
                q.value_semantics
            ],
        )?;
    }
    let after: Vec<(String, String, String, i64)> = {
        let mut st = tx.prepare(
            "SELECT app,account_key,window_kind,captured_at FROM peer_quotas WHERE device_id=?1
             ORDER BY 1,2,3",
        )?;
        st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let address = pulled.addr.to_string();
    tx.execute(
        "UPDATE peers SET last_seen_at=?2, last_pull_at=?2, last_error_code=NULL,
            gtt_version=?3, protocol_version=?4, last_address=?5 WHERE device_id=?1",
        params![
            id,
            now,
            pulled.gtt_version,
            pulled.protocol_version,
            address
        ],
    )?;
    tx.commit()?;
    out.ok = true;
    out.days_changed = pulled.days.len();
    out.days_removed = pulled.removed.len();
    out.days_unchanged = pulled.unchanged;
    out.quotas = after.len();
    out.quotas_changed = before != after;
    out.address = Some(address);
    Ok(out)
}
