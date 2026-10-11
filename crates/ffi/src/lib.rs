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
use globaltokentracker_core::viewmodel::Range;
use globaltokentracker_core::{Cube, Engine, Store, adapters, pricing, quota, update};
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
    Ok(json!({
        "fresh": true,
        "scan_ms": scan_ms,
        "price_due": price_due,
        "scan": scan,
        "vm": vm,
        "detail": { "rows": rows, "total": cube.event_count(apps, models), "page_size": DETAIL_PAGE_SIZE },
    }))
}

fn detail_page(req: PageReq) -> Result<Value, String> {
    let limit = req.limit.unwrap_or(DETAIL_PAGE_SIZE).clamp(1, 5000);
    let rows = open()?
        .events_page(limit, req.offset.max(0), req.apps.as_deref(), req.models.as_deref())
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

fn prices() -> Result<Value, String> {
    let store = open()?;
    let rows = store.price_rows(5000).map_err(|e| e.to_string())?;
    let synced_at = pricing::last_live_sync(&store).unwrap_or(None);
    Ok(json!({ "rows": rows, "synced_at": synced_at }))
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
    let s = unsafe { CStr::from_ptr(p) }.to_str().map_err(|e| e.to_string())?;
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

/// Price table and the live-source sync stamp.
#[unsafe(no_mangle)]
pub extern "C" fn gtt_prices() -> *mut c_char {
    guarded(prices)
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
            unsafe { CStr::from_ptr(channel) }.to_string_lossy().into_owned()
        };
        check_update(&ch)
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

        for p in [gtt_sources(), gtt_prices(), gtt_watch_roots(), gtt_info()] {
            assert_eq!(call(p)["ok"], true);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
