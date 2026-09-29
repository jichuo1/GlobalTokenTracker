// Scratch benchmark — NOT committed.
use globaltokentracker_core::store::default_db_path;
use globaltokentracker_core::viewmodel::{Range, day_start_ms, local_utc_offset};
use globaltokentracker_core::Store;
use std::time::Instant;

fn t<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let s = Instant::now();
    let r = f();
    println!("  {:<34} {:>7.1} ms", label, s.elapsed().as_secs_f64() * 1000.0);
    r
}

fn main() {
    let t_open = Instant::now();
    let store = Store::open(&default_db_path()).unwrap();
    println!("Store::open {:.1} ms", t_open.elapsed().as_secs_f64() * 1000.0);
    let tz = local_utc_offset();
    let t0 = day_start_ms(0);
    for (name, r) in [("今日", Range::Today), ("近7天", Range::Week), ("近30天", Range::Month), ("全部", Range::All)] {
        println!("== {name} ==");
        let (s, e) = (r.start_ms(), r.end_ms());
        t("bucket_models", || store.bucket_models(s, e, r == Range::Today, &tz, None, None).unwrap());
        t("totals(span)", || store.totals(s, e, None, None).unwrap());
        t("totals(today)", || store.totals(Some(t0), None, None, None).unwrap());
        t("totals(all)", || store.totals(None, None, None, None).unwrap());
        t("by_app", || store.by_app(s, e, None, None).unwrap());
        t("by_model", || store.by_model(s, e, None, None).unwrap());
        t("app_names", || store.app_names().unwrap());
        t("model_names", || store.model_names(None).unwrap());
        t("latest_quotas", || store.latest_quotas().unwrap());
        t("unpriced_models", || store.unpriced_models().unwrap());
        t("OVERVIEW TOTAL", || store.overview(r, None, None).unwrap());
    }
    println!("== CUBE ==");
    let cube = t("Cube::build (one scan)", || globaltokentracker_core::Cube::build(&store).unwrap());
    println!("  groups={} approx={} KB", cube.group_count(), cube.approx_bytes() / 1024);
    let days: std::collections::BTreeSet<i64> = [20_000i64].into_iter().collect();
    t("Cube::refreshed(today+yday)", || cube.refreshed(&store, &days).unwrap());
    for (name, r) in [("今日", Range::Today), ("近7天", Range::Week), ("近30天", Range::Month), ("全部", Range::All)] {
        let sql = store.overview(r, None, None).unwrap();
        let mut parts = None;
        let s = Instant::now();
        for _ in 0..1000 { parts = cube.overview_parts(r, None, None); }
        let us = s.elapsed().as_secs_f64() * 1000.0;
        let p = parts.unwrap();
        println!("  overview_parts({name}) x1000: {:.3} ms/call | events sql={} cube={} | cost sql={:.4} cube={:.4} | buckets {}=={}",
            us / 1000.0, sql.span.events, p.span.events, sql.span.cost_usd, p.span.cost_usd, sql.daily.len(), p.daily.len());
        assert_eq!(sql.span.events, p.span.events);
        assert_eq!(sql.all.events, p.all.events);
        assert_eq!(sql.by_model.len(), p.by_model.len());
    }
    // Real persisted filters from ui.json (read-only) — SQL vs cube, every range.
    if let Ok(txt) = std::fs::read_to_string(default_db_path().parent().unwrap().join("ui.json")) {
        let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
        let strs = |k: &str| v.get(k).and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect::<Vec<_>>());
        let (apps, models) = (strs("apps"), strs("models"));
        println!("== real filters: apps={apps:?} models={models:?} ==");
        for (name, r) in [("今日", Range::Today), ("近7天", Range::Week), ("近30天", Range::Month), ("全部", Range::All)] {
            let sql = store.overview(r, apps.as_deref(), models.as_deref()).unwrap();
            let p = cube.overview_parts(r, apps.as_deref(), models.as_deref()).unwrap();
            let ok = sql.span.events == p.span.events && (sql.span.cost_usd - p.span.cost_usd).abs() < 1e-6
                && sql.today.events == p.today.events && sql.all.events == p.all.events
                && sql.daily.len() == p.daily.len() && sql.by_model.len() == p.by_model.len() && sql.by_app.len() == p.by_app.len()
                && sql.models == p.models && sql.apps == p.apps;
            println!("  {name}: sql events={} cost={:.4} | cube events={} cost={:.4} | buckets {}/{} | {}",
                sql.span.events, sql.span.cost_usd, p.span.events, p.span.cost_usd, sql.daily.len(), p.daily.len(), if ok {"MATCH"} else {"MISMATCH"});
        }
        println!("  event_count sql={} cube={}", store.event_count(apps.as_deref(), models.as_deref()).unwrap(), cube.event_count(apps.as_deref(), models.as_deref()));
    }
    println!("== detail(0,200) ==");
    t("events_page", || store.detail(0, 200, None, None).unwrap());
}
