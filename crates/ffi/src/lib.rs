//! globaltokentracker-ffi — C ABI over `globaltokentracker-core` for native
//! shells (the SwiftUI macOS app under `macos/`).
//!
//! Contract (mirrored in `include/gtt.h`):
//! - Every call takes/returns UTF-8 JSON and is synchronous; shells call from
//!   a background queue, never the main thread.
//! - Every result is an envelope `{"ok":true,"data":…}` or
//!   `{"ok":false,"error":"…"}`; panics are caught and reported the same way,
//!   so nothing unwinds across the ABI.
//! - Returned strings are owned by Rust: release each with `gtt_free`.
//!
//! This is the same data path as the WinUI shell's `load_all`/`load_page`:
//! the view models are core's, so both shells show identical numbers.
#![allow(unsafe_code)]

use globaltokentracker_core::store::default_db_path;
use globaltokentracker_core::store::{PriceRow, filter_prices};
use globaltokentracker_core::viewmodel::{QuickRange, Range, rank_quotas};
use globaltokentracker_core::{
    Cube, Engine, Store, adapters, external_interface, pricing, quota, trend, update,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

/// Detail table page size — same as the WinUI shell.
const DETAIL_PAGE_SIZE: i64 = 200;

/// Aggregates from the previous load: range/filter changes and scans that
/// touched a few days refresh it instead of rebuilding (see `Cube::refreshed`).
static CUBE: Mutex<Option<Arc<Cube>>> = Mutex::new(None);

/// Price table as last read (rows + live sync stamp): search keystrokes
/// filter this in memory via core's `filter_prices` instead of re-querying.
/// Dropped on every price refresh.
static PRICES: Mutex<Option<PriceTable>> = Mutex::new(None);

/// Rows + the live-source sync stamp they were read with.
type PriceTable = (Arc<Vec<PriceRow>>, Option<i64>);

#[derive(Deserialize, Default)]
#[serde(default)]
struct PricesReq {
    query: String,
    disputed_only: bool,
    /// Re-read the table from the ledger (page opened, after a refresh).
    reload: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RangeReq {
    key: String,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LoadReq {
    range: RangeReq,
    apps: Option<Vec<String>>,
    models: Option<Vec<String>>,
    /// Run the filesystem scan first (off for the instant startup paint).
    scan: bool,
    /// Rebuild views even when the scan ingested nothing.
    force_views: bool,
    /// Drop cached aggregates (manual refresh, after a pricing repass).
    rebuild: bool,
    /// Treat prices as due regardless of age (once per launch).
    force_prices: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PageReq {
    offset: i64,
    limit: Option<i64>,
    apps: Option<Vec<String>>,
    models: Option<Vec<String>>,
}

/// Custom-window construction for the calendar / hour dial. Pure time math —
/// no ledger access — so shells may call it synchronously.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WindowOp {
    /// Inclusive civil days ("YYYY-MM-DD"), either order.
    Days { first: String, last: String },
    /// Inclusive hour slots 0..=23 of one civil day, either order.
    Hours { day: String, h1: i8, h2: i8 },
    /// One of `QuickRange::key()`.
    Quick { key: String },
    /// Read an existing window back.
    Describe { range: RangeReq },
}

fn civil(s: &str) -> Result<jiff::civil::Date, String> {
    s.parse().map_err(|e| format!("bad date {s:?}: {e}"))
}

fn range_op(req: WindowOp) -> Result<Value, String> {
    let r = match req {
        WindowOp::Days { first, last } => Range::custom_days(civil(&first)?, civil(&last)?),
        WindowOp::Hours { day, h1, h2 } => {
            if !(0..=23).contains(&h1) || !(0..=23).contains(&h2) {
                return Err("hours must be 0..=23".into());
            }
            Range::custom_hours(civil(&day)?, h1, h2)
        }
        WindowOp::Quick { key } => QuickRange::from_key(&key)
            .ok_or_else(|| format!("unknown quick range {key:?}"))?
            .range(),
        WindowOp::Describe { range } => {
            Range::from_config(&range.key, range.start_ms, range.end_ms)
        }
    };
    let now = jiff::Zoned::now();
    let span = r.custom_day_span();
    Ok(json!({
        "range": r,
        "first_day": span.map(|s| s.0.to_string()),
        "last_day": span.map(|s| s.1.to_string()),
        "days": span.map(|(a, b)| a.until(b).map_or(1, |d| d.get_days() + 1)),
        "hour_span": r.hour_span(),
        "hourly": r.hourly(),
        // The shell's clock for disabling future days / hours.
        "today": now.date().to_string(),
        "now_hour": now.hour(),
    }))
}

fn open() -> Result<Store, String> {
    Store::open(&default_db_path()).map_err(|e| e.to_string())
}

fn load(req: LoadReq) -> Result<Value, String> {
    let range = Range::from_config(&req.range.key, req.range.start_ms, req.range.end_ms);
    let prev = if req.rebuild {
        None
    } else {
        CUBE.lock().map_err(|e| e.to_string())?.clone()
    };
    let store = open()?;
    let (store, report, scan_ms) = if req.scan {
        let engine = Engine::new(store).map_err(|e| e.to_string())?;
        let t = std::time::Instant::now();
        let report = engine.scan_once();
        (engine.store, report, t.elapsed().as_millis())
    } else {
        (store, Ok(Default::default()), 0)
    };
    let changed = report
        .as_ref()
        .map(|r| r.events_ingested > 0 || r.quotas > 0)
        .unwrap_or(true);
    let price_due = req.force_prices || pricing::prices_stale(&store).unwrap_or(false);
    let stale_day = prev.as_ref().is_some_and(|c| c.is_day_stale());
    let scan = match &report {
        Ok(r) => json!({
            "files_seen": r.files_seen,
            "events_ingested": r.events_ingested,
            "quotas": r.quotas,
            "errors": r.errors,
        }),
        Err(e) => json!({ "errors": [e.to_string()] }),
    };
    if prev.is_some() && !req.force_views && !changed && !stale_day {
        return Ok(json!({
            "fresh": false, "scan_ms": scan_ms, "price_due": price_due, "scan": scan,
        }));
    }
    let cube = match (&prev, &report) {
        (Some(c), Ok(r)) if !r.rollup_full => c.refreshed(&store, &r.rollup_days),
        _ => Cube::build(&store),
    }
    .map_err(|e| e.to_string())?;
    let cube = Arc::new(cube);
    *CUBE.lock().map_err(|e| e.to_string())? = Some(cube.clone());
    let (apps, models) = (req.apps.as_deref(), req.models.as_deref());
    let vm = store
        .overview_from_cube(&cube, range, apps, models)
        .map_err(|e| e.to_string())?;
    let rows = store
        .events_page(DETAIL_PAGE_SIZE, 0, apps, models)
        .map_err(|e| e.to_string())?;
    let (bars, avg) = trend::drawn(&vm.daily);
    let trend = json!({ "buckets": bars, "avg": avg });
    let quotas_ranked = rank_quotas(&vm.quotas, globaltokentracker_core::store::now_ms());
    Ok(json!({
        "fresh": true,
        "scan_ms": scan_ms,
        "price_due": price_due,
        "scan": scan,
        "trend": trend,
        "quotas_ranked": quotas_ranked,
        "vm": vm,
        "detail": { "rows": rows, "total": cube.event_count(apps, models), "page_size": DETAIL_PAGE_SIZE },
    }))
}

fn detail_page(req: PageReq) -> Result<Value, String> {
    let limit = req.limit.unwrap_or(DETAIL_PAGE_SIZE).clamp(1, 5000);
    let rows = open()?
        .events_page(
            limit,
            req.offset.max(0),
            req.apps.as_deref(),
            req.models.as_deref(),
        )
        .map_err(|e| e.to_string())?;
    Ok(json!({ "rows": rows }))
}

fn sources() -> Result<Value, String> {
    let health = open()?.source_health().map_err(|e| e.to_string())?;
    let adapters: Vec<Value> = adapters::registry()
        .iter()
        .map(|a| json!({ "id": a.id(), "name": a.display_name() }))
        .collect();
    Ok(json!({ "health": health, "adapters": adapters }))
}

fn prices(req: PricesReq) -> Result<Value, String> {
    let mut cache = PRICES.lock().map_err(|e| e.to_string())?;
    if req.reload || cache.is_none() {
        let store = open()?;
        let rows = store.price_rows(5000).map_err(|e| e.to_string())?;
        *cache = Some((
            Arc::new(rows),
            pricing::last_live_sync(&store).unwrap_or(None),
        ));
    }
    let (all, synced_at) = cache.clone().ok_or("price table unavailable")?;
    drop(cache);
    let filtering = !req.query.trim().is_empty() || req.disputed_only;
    let shown: Vec<PriceRow> = if filtering {
        filter_prices(&all, &req.query, req.disputed_only)
    } else {
        all.as_ref().clone()
    };
    let rows: Vec<Value> = shown
        .iter()
        .map(|r| {
            let mut v = json!(r);
            v["disputed"] = json!(r.disputed());
            v
        })
        .collect();
    Ok(json!({ "rows": rows, "total": all.len(), "synced_at": synced_at }))
}

// No `power::worker` here: shells call us on *their* thread pool (Swift's
// cooperative pool / GCD workqueues), where Darwin rejects QoS changes and a
// thread name would mislabel a shared thread. The caller's task priority
// (`.utility` in Core.swift) already sets the QoS.
fn refresh_prices() -> Result<Value, String> {
    let r = pricing::refresh(&open()?).map_err(|e| e.to_string())?;
    if r.rules_repass {
        // Every booked price moved: cached aggregates are stale.
        *CUBE.lock().map_err(|e| e.to_string())? = None;
    }
    *PRICES.lock().map_err(|e| e.to_string())? = None;
    Ok(json!({
        "sources": r.sources,
        "failed": r.failed,
        "repriced": r.repriced,
        "rules_repass": r.rules_repass,
    }))
}

fn poll_quotas() -> Result<Value, String> {
    let store = open()?;
    let (mut inserted, mut errors) = (0u64, Vec::new());
    for o in quota::poll_all() {
        if let Some(e) = o.error {
            errors.push(format!("{}: {e}", o.app));
        }
        for q in o.quotas {
            if matches!(store.insert_quota(&q), Ok(true)) {
                inserted += 1;
            }
        }
    }
    Ok(json!({ "inserted": inserted, "errors": errors }))
}

fn watch_roots() -> Value {
    let mut out: Vec<String> = Vec::new();
    for a in adapters::registry() {
        for r in a.watch_roots() {
            let s = r.to_string_lossy().into_owned();
            if r.is_dir() && !out.contains(&s) {
                out.push(s);
            }
        }
    }
    json!(out)
}

fn check_update(channel: &str) -> Result<Value, String> {
    let rel = update::check(update::Channel::from_key(channel)).map_err(|e| e.to_string())?;
    Ok(json!({
        "current": update::current_version().to_string(),
        "update": rel.map(|r| json!({
            "tag": r.tag,
            "version": r.version.to_string(),
            "html_url": r.html_url,
            "asset_name": r.asset_name,
        })),
    }))
}

/// The shared external-coordination switch (`external-interface.json`).
/// `enabled: null` reads; a bool writes, then reports the state on disk.
fn external(enabled: Option<bool>) -> Result<Value, String> {
    let path = external_interface::settings_path();
    let mut error = None;
    if let Some(on) = enabled
        && let Err(e) = external_interface::write(&path, on)
    {
        error = Some(e.to_string());
    }
    let state = external_interface::read(&path);
    Ok(json!({
        "enabled": state.is_ok_and(|s| s.enabled),
        "invalid": state.is_err_and(|e| e == external_interface::AccessError::InvalidSettings),
        "error": error,
        "path": path.to_string_lossy(),
    }))
}

/// Check `channel`, then download + SHA-256-verify the platform asset.
/// Never executes anything: the shell decides how to open it.
fn download_update(channel: &str) -> Result<Value, String> {
    let rel = update::check(update::Channel::from_key(channel))
        .map_err(|e| e.to_string())?
        .ok_or("already up to date")?;
    let path = update::download(&rel).map_err(|e| format!("{e:#}"))?;
    Ok(json!({ "version": rel.version.to_string(), "path": path.to_string_lossy() }))
}

fn info() -> Value {
    json!({
        "version": update::current_version().to_string(),
        "db_path": default_db_path().to_string_lossy(),
        "tz_offset": globaltokentracker_core::viewmodel::local_utc_offset(),
    })
}

// ---- ABI plumbing ---------------------------------------------------------

fn envelope(r: Result<Value, String>) -> *mut c_char {
    let v = match r {
        Ok(data) => json!({ "ok": true, "data": data }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    // serde_json never emits interior NULs (they're escaped as \u0000).
    CString::new(v.to_string()).map_or(std::ptr::null_mut(), CString::into_raw)
}

fn guarded(f: impl FnOnce() -> Result<Value, String>) -> *mut c_char {
    envelope(catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        Err(format!("internal error: {msg}"))
    }))
}

/// Parse a JSON request; NULL or empty → the type's defaults.
fn arg<T: for<'de> Deserialize<'de> + Default>(p: *const c_char) -> Result<T, String> {
    if p.is_null() {
        return Ok(T::default());
    }
    let s = unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|e| e.to_string())?;
    if s.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(s).map_err(|e| format!("bad request: {e}"))
}

/// Load (optionally scan first) and return the overview VM + first detail page.
///
/// # Safety
/// `req_json` is NULL or a valid NUL-terminated UTF-8 string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_load(req_json: *const c_char) -> *mut c_char {
    guarded(|| load(arg(req_json)?))
}

/// One page of the detail table (`{offset, limit?, apps?, models?}`).
///
/// # Safety
/// `req_json` is NULL or a valid NUL-terminated UTF-8 string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_detail_page(req_json: *const c_char) -> *mut c_char {
    guarded(|| detail_page(arg(req_json)?))
}

/// Per-source health rows plus the adapter registry (id → display name).
#[unsafe(no_mangle)]
pub extern "C" fn gtt_sources() -> *mut c_char {
    guarded(sources)
}

/// Price table filtered by core's search (`{query, disputed_only, reload}`),
/// plus the unfiltered total and the live-source sync stamp.
///
/// # Safety
/// `req_json` is NULL or a valid NUL-terminated UTF-8 string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_prices(req_json: *const c_char) -> *mut c_char {
    guarded(|| prices(arg(req_json)?))
}

/// Read (`-1`) or set (`0` / `1`) the external coordination interface switch.
#[unsafe(no_mangle)]
pub extern "C" fn gtt_external_interface(set: i32) -> *mut c_char {
    guarded(|| external(if set < 0 { None } else { Some(set != 0) }))
}

/// Download + verify the update for `channel`; returns the local file path.
///
/// # Safety
/// `channel` is NULL or a valid NUL-terminated UTF-8 string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_download_update(channel: *const c_char) -> *mut c_char {
    guarded(|| {
        let ch = if channel.is_null() {
            "stable".to_string()
        } else {
            unsafe { CStr::from_ptr(channel) }
                .to_string_lossy()
                .into_owned()
        };
        download_update(&ch)
    })
}

/// Network price refresh; may take seconds.
#[unsafe(no_mangle)]
pub extern "C" fn gtt_refresh_prices() -> *mut c_char {
    guarded(refresh_prices)
}

/// Poll vendor quota channels once and store new snapshots.
#[unsafe(no_mangle)]
pub extern "C" fn gtt_poll_quotas() -> *mut c_char {
    guarded(poll_quotas)
}

/// Existing directories every adapter reads — the shell watches these.
#[unsafe(no_mangle)]
pub extern "C" fn gtt_watch_roots() -> *mut c_char {
    guarded(|| Ok(watch_roots()))
}

/// GitHub release check on `channel` ("stable" | "alpha"); NULL = stable.
///
/// # Safety
/// `channel` is NULL or a valid NUL-terminated UTF-8 string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_check_update(channel: *const c_char) -> *mut c_char {
    guarded(|| {
        let ch = if channel.is_null() {
            "stable".to_string()
        } else {
            unsafe { CStr::from_ptr(channel) }
                .to_string_lossy()
                .into_owned()
        };
        check_update(&ch)
    })
}

/// Build or describe a custom statistics window (see `WindowOp`).
///
/// # Safety
/// `req_json` is NULL or a valid NUL-terminated UTF-8 string that outlives the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_range(req_json: *const c_char) -> *mut c_char {
    guarded(|| {
        if req_json.is_null() {
            return Err("gtt_range needs a request".into());
        }
        let s = unsafe { CStr::from_ptr(req_json) }
            .to_str()
            .map_err(|e| e.to_string())?;
        range_op(serde_json::from_str(s).map_err(|e| format!("bad request: {e}"))?)
    })
}

/// Version, ledger path and local UTC offset.
#[unsafe(no_mangle)]
pub extern "C" fn gtt_info() -> *mut c_char {
    guarded(|| Ok(info()))
}

/// Release a string returned by any `gtt_*` call. NULL is a no-op.
///
/// # Safety
/// `s` is NULL or a pointer returned by a `gtt_*` call, freed at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gtt_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(p: *mut c_char) -> Value {
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
        unsafe { gtt_free(p) };
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn load_on_scratch_ledger_round_trips() {
        let dir = std::env::temp_dir().join(format!("gtt-ffi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: single test touching the env in this crate.
        unsafe { std::env::set_var("GTT_DATA_DIR", &dir) };
        let req = CString::new(r#"{"range":{"key":"month"},"rebuild":true}"#).unwrap();
        let v = call(unsafe { gtt_load(req.as_ptr()) });
        assert_eq!(v["ok"], true, "{v}");
        let data = &v["data"];
        assert_eq!(data["fresh"], true);
        assert_eq!(data["vm"]["range"]["key"], "month");
        assert!(data["vm"]["span"]["events"].is_u64());
        assert!(data["detail"]["rows"].is_array());

        let v = call(unsafe { gtt_load(std::ptr::null()) });
        assert_eq!(v["ok"], true, "null request uses defaults: {v}");

        let bad = CString::new("{not json").unwrap();
        let v = call(unsafe { gtt_load(bad.as_ptr()) });
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().starts_with("bad request"));

        let req = CString::new(r#"{"op":"hours","day":"2026-10-12","h1":18,"h2":9}"#).unwrap();
        let v = call(unsafe { gtt_range(req.as_ptr()) });
        assert_eq!(v["data"]["range"]["key"], "custom", "{v}");
        assert_eq!(v["data"]["hour_span"], json!([9, 18]));
        assert_eq!(v["data"]["days"], 1);
        assert_eq!(v["data"]["hourly"], true);
        let req =
            CString::new(r#"{"op":"days","first":"2026-10-09","last":"2026-10-03"}"#).unwrap();
        let v = call(unsafe { gtt_range(req.as_ptr()) });
        assert_eq!(v["data"]["first_day"], "2026-10-03");
        assert_eq!(v["data"]["days"], 7);
        assert!(v["data"]["hour_span"].is_null());
        let req = CString::new(r#"{"op":"quick","key":"last_month"}"#).unwrap();
        assert_eq!(call(unsafe { gtt_range(req.as_ptr()) })["ok"], true);
        let req = CString::new(r#"{"op":"hours","day":"2026-10-12","h1":24,"h2":1}"#).unwrap();
        assert_eq!(call(unsafe { gtt_range(req.as_ptr()) })["ok"], false);

        let req = CString::new(r#"{"query":"","disputed_only":true,"reload":true}"#).unwrap();
        let v = call(unsafe { gtt_prices(req.as_ptr()) });
        assert_eq!(v["ok"], true, "{v}");
        // A never-scanned ledger has no seed prices yet; only the shape matters.
        assert!(v["data"]["total"].is_u64());
        assert!(
            v["data"]["rows"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["disputed"] == true)
        );
        assert!(v["data"]["vm"].is_null());
        let req = CString::new(r#"{"range":{"key":"week"},"rebuild":true}"#).unwrap();
        let v = call(unsafe { gtt_load(req.as_ptr()) });
        assert!(v["data"]["trend"]["avg"].is_array());
        assert!(v["data"]["quotas_ranked"].is_array());

        for p in [gtt_sources(), gtt_watch_roots(), gtt_info()] {
            assert_eq!(call(p)["ok"], true);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
