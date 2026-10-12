//! Cross-device views computed on the viewer: the merged quota list and
//! overlap warnings.

use crate::coordination::opaque;
use crate::store::{MODEL_EXPR, Store};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// The latest quota for one (app, account, window) across all devices.
/// Quotas are per account: never summed, only the freshest report is kept.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MergedQuota {
    pub app: String,
    /// SHA-256 pseudonym of the account (the same on every device).
    pub account_key: Option<String>,
    pub window_kind: String,
    pub captured_at: i64,
    pub used: Option<f64>,
    pub limit_value: Option<f64>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    /// The device that reported this (freshest) value.
    pub device_id: String,
    pub device_name: String,
    pub local: bool,
}

/// Local latest quotas plus enabled peers' quotas, freshest per
/// (app, account key, window kind); on a tie this device wins.
pub fn merged_quotas(store: &Store) -> Result<Vec<MergedQuota>> {
    let (local_id, local_name) = super::local_device(store)?;
    let mut best: BTreeMap<(String, String, String), MergedQuota> = BTreeMap::new();
    let mut offer = |q: MergedQuota| {
        let key = (
            q.app.clone(),
            q.account_key.clone().unwrap_or_default(),
            q.window_kind.clone(),
        );
        match best.get(&key) {
            Some(cur) if cur.captured_at >= q.captured_at => {}
            _ => {
                best.insert(key, q);
            }
        }
    };
    for q in store.latest_quotas()? {
        offer(MergedQuota {
            app: q.app,
            account_key: q.account.as_deref().map(opaque),
            window_kind: q.window_kind,
            captured_at: q.captured_at,
            used: q.used,
            limit_value: q.limit_value,
            used_percent: q.used_percent,
            resets_at: q.resets_at,
            device_id: local_id.clone(),
            device_name: local_name.clone(),
            local: true,
        });
    }
    let mut st = store.conn().prepare(
        "SELECT q.app, q.account_key, q.window_kind, q.captured_at, q.used, q.limit_value,
                q.used_percent, q.resets_at, q.device_id, p.name
         FROM peer_quotas q JOIN peers p ON p.device_id = q.device_id
         WHERE p.enabled = 1 ORDER BY q.device_id",
    )?;
    let rows = st.query_map([], |r| {
        Ok(MergedQuota {
            app: r.get(0)?,
            account_key: Some(r.get::<_, String>(1)?).filter(|k| !k.is_empty()),
            window_kind: r.get(2)?,
            captured_at: r.get(3)?,
            used: r.get(4)?,
            limit_value: r.get(5)?,
            used_percent: r.get(6)?,
            resets_at: r.get(7)?,
            device_id: r.get(8)?,
            device_name: r.get(9)?,
            local: false,
        })
    })?;
    for q in rows {
        offer(q?);
    }
    Ok(best.into_values().collect())
}

/// Two devices that look like they count the same usage — e.g. both scan a
/// synced folder of the same tool's logs. Flagged, never deduplicated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OverlapWarning {
    pub code: String,
    pub device_a: String,
    pub device_b: String,
    pub app: String,
    /// Distinct UTC days with at least one identical hour group.
    pub days: usize,
    pub matching_hours: usize,
}

/// Days inspected for overlaps.
pub const OVERLAP_LOOKBACK_DAYS: i64 = 30;
/// Identical-hour days that make an overlap likely.
pub const OVERLAP_MIN_DAYS: usize = 3;

/// One UTC hour of one model's usage, as a comparable signature.
type Signature = (i64, String, u64, u64, u64, u64);

/// Pairs of devices whose hour groups for an app are identical (same UTC
/// hour, model, events, input/output/cache-read tokens) on at least
/// [`OVERLAP_MIN_DAYS`] of the last [`OVERLAP_LOOKBACK_DAYS`] days.
pub fn overlaps(store: &Store) -> Result<Vec<OverlapWarning>> {
    let since_hour = (crate::store::now_ms() / super::BUCKET_MS) - OVERLAP_LOOKBACK_DAYS * 24;
    let (local_id, _) = super::local_device(store)?;
    // device → app → signatures
    let mut by_device: BTreeMap<String, HashMap<String, BTreeSet<Signature>>> = BTreeMap::new();
    let mut push = |device: String, app: String, sig: Signature| {
        by_device
            .entry(device)
            .or_default()
            .entry(app)
            .or_default()
            .insert(sig);
    };
    let n = |r: &rusqlite::Row, i: usize| -> rusqlite::Result<u64> {
        Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0))
    };
    {
        let mut st = store.conn().prepare(&format!(
            "SELECT ts_start / {} AS h, app, {MODEL_EXPR}, COUNT(*), COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0), COALESCE(SUM(cache_read_tokens),0)
             FROM usage_events WHERE ts_start >= ?1 GROUP BY h, app, 3",
            super::BUCKET_MS
        ))?;
        let rows = st.query_map([since_hour * super::BUCKET_MS], |r| {
            Ok((
                r.get::<_, String>(1)?,
                (r.get(0)?, r.get(2)?, n(r, 3)?, n(r, 4)?, n(r, 5)?, n(r, 6)?),
            ))
        })?;
        for row in rows {
            let (app, sig) = row?;
            push(local_id.clone(), app, sig);
        }
    }
    {
        let mut st = store.conn().prepare(
            "SELECT g.device_id, g.app, g.utc_hour, g.model, g.events, g.input_tokens,
                    g.output_tokens, g.cache_read_tokens
             FROM peer_groups g JOIN peers p ON p.device_id = g.device_id
             WHERE p.enabled = 1 AND g.utc_hour >= ?1",
        )?;
        let rows = st.query_map([since_hour], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                (r.get(2)?, r.get(3)?, n(r, 4)?, n(r, 5)?, n(r, 6)?, n(r, 7)?),
            ))
        })?;
        for row in rows {
            let (device, app, sig) = row?;
            push(device, app, sig);
        }
    }
    let devices: Vec<&String> = by_device.keys().collect();
    let mut out = Vec::new();
    for (i, a) in devices.iter().enumerate() {
        for b in &devices[i + 1..] {
            let (apps_a, apps_b) = (&by_device[*a], &by_device[*b]);
            let mut apps: Vec<&String> =
                apps_a.keys().filter(|k| apps_b.contains_key(*k)).collect();
            apps.sort();
            for app in apps {
                let same: Vec<&Signature> = apps_a[app]
                    .intersection(&apps_b[app])
                    .filter(|s| s.2 > 0 && s.3 + s.4 + s.5 > 0)
                    .collect();
                let days: BTreeSet<i64> = same.iter().map(|s| s.0.div_euclid(24)).collect();
                if days.len() >= OVERLAP_MIN_DAYS {
                    out.push(OverlapWarning {
                        code: "possible_shared_source".into(),
                        device_a: (*a).clone(),
                        device_b: (*b).clone(),
                        app: app.clone(),
                        days: days.len(),
                        matching_hours: same.len(),
                    });
                }
            }
        }
    }
    Ok(out)
}
