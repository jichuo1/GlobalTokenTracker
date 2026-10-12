use super::server::{self, ServerState};
use super::*;
use crate::Cube;
use crate::model::{CostSource, Provenance, QuotaSnapshot, UsageEvent};
use crate::store::{AppSummary, ShareRow, Totals};
use crate::viewmodel::{Range, day_start_ms};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);
const HOUR_MS: i64 = 3_600_000;

struct Fixture {
    path: PathBuf,
    store: Store,
    identity: Identity,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for ext in ["db", "db-wal", "db-shm"] {
            let _ = std::fs::remove_file(self.path.with_extension(ext));
        }
    }
}

fn fixture() -> Fixture {
    let path = std::env::temp_dir().join(format!(
        "gtt-peer-{}-{}.db",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let store = Store::open(&path).unwrap();
    let identity = ensure_identity(&store, Some("test-mac")).unwrap();
    Fixture {
        path,
        store,
        identity,
    }
}

fn ev(
    key: &str,
    app: &str,
    model: Option<&str>,
    ts: Option<i64>,
    out: u64,
    cost: f64,
) -> UsageEvent {
    UsageEvent {
        dedup_key: key.into(),
        app: app.into(),
        model: model.map(str::to_string),
        ts_start: ts,
        input_tokens: 100 + out,
        output_tokens: out,
        reasoning_tokens: out / 5,
        cache_read_tokens: out / 2,
        cache_write_5m_tokens: out / 3,
        cache_write_1h_tokens: out / 7,
        unclassified_tokens: out % 11,
        cost_usd: Some(cost),
        cost_source: Some(CostSource::Computed),
        provenance: Provenance::LocalJsonl,
        credits: out.is_multiple_of(3).then_some(out as f64 / 10.0),
        active_ms: Some(out as i64),
        duration_ms: (!out.is_multiple_of(4)).then_some(out as i64 * 11),
        session_id: Some(format!("secret-session-{key}")),
        project: Some("/Users/someone/secret-project".into()),
        raw_ref: Some("/Users/someone/.claude/secret.jsonl:42".into()),
        ..Default::default()
    }
}

/// ~45 local days, three tools, several models (incl. empty names falling
/// back through MODEL_EXPR), events today and in the future, one undated row.
fn fill(store: &Store) {
    let mut n = 0u64;
    let tools = ["claude", "codex", "cursor"];
    let models = [Some("opus"), Some("sonnet"), Some("gpt"), None, Some("")];
    for d in 0..45i64 {
        for (i, app) in tools.iter().enumerate() {
            for (j, m) in models.iter().enumerate() {
                if (d + i as i64 + j as i64) % 3 == 0 {
                    continue;
                }
                n += 1;
                // spread over all 24 hours, so local and UTC days disagree
                let ts =
                    day_start_ms(d) + HOUR_MS * ((n * 7 % 24) as i64) + (n as i64 % 59) * 60_000;
                let mut e = ev(&format!("k{n}"), app, *m, Some(ts), n * 7, n as f64 * 0.013);
                if m.is_none() {
                    e.request_model = Some("aliasy".into());
                }
                store.upsert_event(&e).unwrap();
            }
        }
    }
    store
        .upsert_event(&ev("nots", "claude", Some("opus"), None, 999, 1.5))
        .unwrap();
    store
        .upsert_event(&ev(
            "fut",
            "claude",
            Some("sonnet"),
            Some(now_ms() + HOUR_MS),
            42,
            0.2,
        ))
        .unwrap();
}

fn full_summary(f: &Fixture) -> Summary {
    query_summary(f, &SummaryQuery::default())
}

fn query_summary(f: &Fixture, q: &SummaryQuery) -> Summary {
    let v = respond(&f.path, &f.identity, 1, &Request::Summary(q.clone()));
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}: {v}"))
}

fn add(t: &mut Totals, g: &Group) {
    t.events += g.events;
    t.input_tokens += g.input_tokens;
    t.output_tokens += g.output_tokens;
    t.reasoning_tokens += g.reasoning_tokens;
    t.cache_read_tokens += g.cache_read_tokens;
    t.cache_write_tokens += g.cache_write_tokens;
    t.unclassified_tokens += g.unclassified_tokens;
    t.credits += g.credits;
    t.cost_usd += g.cost_usd;
    t.active_ms += g.active_ms;
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn assert_totals(a: &Totals, b: &Totals, what: &str) {
    assert_eq!(
        (
            a.events,
            a.input_tokens,
            a.output_tokens,
            a.reasoning_tokens,
            a.cache_read_tokens,
            a.cache_write_tokens,
            a.unclassified_tokens,
            a.active_ms
        ),
        (
            b.events,
            b.input_tokens,
            b.output_tokens,
            b.reasoning_tokens,
            b.cache_read_tokens,
            b.cache_write_tokens,
            b.unclassified_tokens,
            b.active_ms
        ),
        "{what}"
    );
    assert!(close(a.cost_usd, b.cost_usd), "{what} cost");
    assert!(close(a.credits, b.credits), "{what} credits");
}

/// Re-bucket peer groups into the viewer's local frame and fold one range,
/// the way a viewer would merge a peer into its cube.
fn fold(
    summary: &Summary,
    off_ms: i64,
    range: Range,
) -> (
    Totals,
    BTreeMap<String, AppSummary>,
    BTreeMap<String, ShareRow>,
) {
    let lo = range.start_ms();
    let hi = range.end_ms();
    let bounded = lo.is_some() || hi.is_some();
    let mut span = Totals::default();
    let mut apps = BTreeMap::new();
    let mut models = BTreeMap::new();
    for day in &summary.days {
        for g in &day.groups {
            let keep = match day.local_day(g, off_ms) {
                None => !bounded,
                Some(d) => {
                    let start = d * DAY_MS - off_ms;
                    lo.is_none_or(|l| start >= l) && hi.is_none_or(|h| start < h)
                }
            };
            if !keep {
                continue;
            }
            add(&mut span, g);
            let a = apps.entry(g.app.clone()).or_insert_with(|| AppSummary {
                app: g.app.clone(),
                events: 0,
                input_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                unclassified_tokens: 0,
                credits: 0.0,
                cost_usd: 0.0,
            });
            a.events += g.events;
            a.input_tokens += g.input_tokens;
            a.output_tokens += g.output_tokens;
            a.cost_usd += g.cost_usd;
            let m = models.entry(g.model.clone()).or_insert_with(|| ShareRow {
                name: g.model.clone(),
                events: 0,
                tokens: 0,
                cost_usd: 0.0,
            });
            m.events += g.events;
            m.tokens += g.headline_tokens();
            m.cost_usd += g.cost_usd;
        }
    }
    (span, apps, models)
}

#[test]
fn rebucketed_summary_equals_cube_overview() {
    let f = fixture();
    fill(&f.store);
    let off = crate::cube::current_offset_ms();
    if off % HOUR_MS != 0 {
        // Hour buckets cannot split a :30/:45 local midnight (documented).
        return;
    }
    let summary = full_summary(&f);
    assert_eq!(summary.meta.status, "ok", "{:?}", summary.meta.warnings);
    let cube = Cube::build(&f.store).unwrap();
    let ranges = [
        Range::Today,
        Range::Week,
        Range::Month,
        Range::All,
        Range::custom(day_start_ms(20), day_start_ms(10) + DAY_MS),
        Range::custom(day_start_ms(3), day_start_ms(0)),
    ];
    for range in ranges {
        let parts = cube.overview_parts(range, None, None).unwrap();
        let (span, apps, models) = fold(&summary, off, range);
        assert_totals(&span, &parts.span, &format!("{range:?} span"));
        let key = |a: &AppSummary| {
            (
                a.events,
                a.input_tokens,
                a.output_tokens,
                (a.cost_usd * 1e6).round() as u64,
            )
        };
        let cube_apps: BTreeMap<_, _> = parts
            .by_app
            .iter()
            .map(|a| (a.app.clone(), key(a)))
            .collect();
        let peer_apps: BTreeMap<_, _> = apps.values().map(|a| (a.app.clone(), key(a))).collect();
        assert_eq!(cube_apps, peer_apps, "{range:?} by_app");
        let key = |m: &ShareRow| (m.events, m.tokens, (m.cost_usd * 1e6).round() as u64);
        let cube_models: BTreeMap<_, _> = parts
            .by_model
            .iter()
            .map(|m| (m.name.clone(), key(m)))
            .collect();
        let peer_models: BTreeMap<_, _> =
            models.values().map(|m| (m.name.clone(), key(m))).collect();
        assert_eq!(cube_models, peer_models, "{range:?} by_model");
    }
    // Unbounded totals include the undated row; `all` agrees too.
    let (all, _, _) = fold(&summary, off, Range::All);
    assert_totals(
        &all,
        &cube.overview_parts(Range::All, None, None).unwrap().all,
        "all",
    );
    assert!(
        summary
            .days
            .iter()
            .any(|d| d.day == UNDATED && d.start_ms.is_none())
    );
}

#[test]
fn rebucketing_for_other_offsets_matches_sql_local_days() {
    let f = fixture();
    fill(&f.store);
    let summary = full_summary(&f);
    for off in [0, 8 * HOUR_MS, -5 * HOUR_MS, 14 * HOUR_MS, -12 * HOUR_MS] {
        let mut peer: BTreeMap<i64, (u64, u64, u64)> = BTreeMap::new();
        for day in &summary.days {
            for g in &day.groups {
                if let Some(d) = day.local_day(g, off) {
                    let e = peer.entry(d).or_default();
                    e.0 += g.events;
                    e.1 += g.headline_tokens();
                    e.2 += g.duration_ms;
                }
            }
        }
        let mut st = f
            .store
            .conn()
            .prepare(&format!(
                "SELECT (ts_start + ?1) / {DAY_MS} AS d, COUNT(*),
                        SUM(input_tokens+output_tokens+cache_read_tokens+unclassified_tokens),
                        COALESCE(SUM(duration_ms),0)
                 FROM usage_events WHERE ts_start IS NOT NULL GROUP BY d"
            ))
            .unwrap();
        let sql: BTreeMap<i64, (u64, u64, u64)> = st
            .query_map([off], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    (
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)? as u64,
                    ),
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(peer, sql, "offset {off}");
    }
    // An event at 23:30 UTC is "tomorrow" at +08:00 and "today" at -05:00.
    let f = fixture();
    let t = 20_000 * DAY_MS + 23 * HOUR_MS + 30 * 60_000;
    f.store
        .upsert_event(&ev("edge", "codex", Some("gpt"), Some(t), 10, 0.1))
        .unwrap();
    let s = full_summary(&f);
    let (day, g) = (&s.days[0], &s.days[0].groups[0]);
    assert_eq!((day.day.as_str(), g.hour), ("2024-10-04", Some(23)));
    assert_eq!(day.local_day(g, 8 * HOUR_MS), Some(20_001));
    assert_eq!(day.local_day(g, -5 * HOUR_MS), Some(20_000));
    assert_eq!(day.local_day(g, 0), Some(20_000));
}

#[test]
fn fingerprints_track_updates_deletes_and_drive_incremental_pulls() {
    let f = fixture();
    let day = |d: i64, h: i64| 20_000 * DAY_MS + d * DAY_MS + h * HOUR_MS;
    for (i, d) in [0, 0, 1, 2].iter().enumerate() {
        f.store
            .upsert_event(&ev(
                &format!("e{i}"),
                "claude",
                Some("opus"),
                Some(day(*d, 3 + i as i64)),
                10 + i as u64,
                0.5,
            ))
            .unwrap();
    }
    let hello: Hello =
        serde_json::from_value(respond(&f.path, &f.identity, 1, &Request::Hello)).unwrap();
    assert_eq!(hello.fingerprints.len(), 3);
    assert_eq!(hello.device_id, f.identity.device_id);
    let known = hello.fingerprints.clone();
    let full = full_summary(&f);
    let from_full: BTreeMap<_, _> = full
        .days
        .iter()
        .map(|d| (d.day.clone(), d.fingerprint.clone()))
        .collect();
    assert_eq!(
        known, from_full,
        "hello and summary fingerprint the same content"
    );

    // Nothing changed → nothing resent.
    let q = SummaryQuery {
        from: None,
        known: known.clone(),
    };
    let s = query_summary(&f, &q);
    assert!(s.days.is_empty() && s.removed.is_empty());
    assert_eq!(s.unchanged, 3);

    // An in-place update of the same row (a re-priced or upgraded upsert —
    // same rowid, so a rowid watermark would miss it) changes only its day.
    f.store
        .conn()
        .execute(
            "UPDATE usage_events SET cost_usd=0.75 WHERE dedup_key='e2'",
            [],
        )
        .unwrap();
    // A model-only re-attribution with identical numbers changes it too.
    f.store
        .conn()
        .execute(
            "UPDATE usage_events SET model='sonnet' WHERE dedup_key='e3'",
            [],
        )
        .unwrap();
    // Deleting a whole day reports it removed.
    f.store
        .conn()
        .execute(
            "DELETE FROM usage_events WHERE dedup_key IN ('e0','e1')",
            [],
        )
        .unwrap();
    let s = query_summary(&f, &q);
    let changed: Vec<&str> = s.days.iter().map(|d| d.day.as_str()).collect();
    assert_eq!(changed, ["2024-10-05", "2024-10-06"]);
    assert_eq!(s.removed, ["2024-10-04"]);
    assert_eq!(s.unchanged, 0);
    assert_eq!(s.days[1].groups[0].model, "sonnet");
    for d in &s.days {
        assert_ne!(Some(&d.fingerprint), known.get(&d.day));
        assert_eq!(d.fingerprint, fingerprint(&d.groups));
    }

    // `from` scopes both the days returned and what can be reported removed.
    let q = SummaryQuery {
        from: Some("2024-10-06".into()),
        known: known.clone(),
    };
    let s = query_summary(&f, &q);
    assert_eq!(
        s.days.iter().map(|d| d.day.as_str()).collect::<Vec<_>>(),
        ["2024-10-06"]
    );
    assert!(s.removed.is_empty());
}

#[test]
fn summary_query_round_trips_and_rejects_garbage() {
    let q = SummaryQuery {
        from: Some("2025-01-02".into()),
        known: BTreeMap::from([
            ("2025-01-02".into(), "0123456789abcdef".into()),
            (UNDATED.into(), "fedcba9876543210".into()),
        ]),
    };
    assert_eq!(SummaryQuery::parse(&q.to_query()), Some(q));
    assert_eq!(
        SummaryQuery::parse("known=2025-01-02%3A0123456789abcdef")
            .unwrap()
            .known
            .len(),
        1
    );
    for bad in [
        "from=2025-02-30",
        "from=yesterday",
        "known=2025-01-02:XYZ",
        "known=../etc:0123456789abcdef",
        "since=1",
        "known=2025-01-02",
    ] {
        assert_eq!(SummaryQuery::parse(bad), None, "{bad}");
    }
}

#[test]
fn output_never_carries_projects_sessions_or_raw_refs() {
    let f = fixture();
    fill(&f.store);
    f.store
        .upsert_event(&ev(
            "weird",
            "claude",
            Some("/Users/someone/private/model"),
            Some(now_ms()),
            5,
            0.1,
        ))
        .unwrap();
    f.store
        .insert_quota(&QuotaSnapshot {
            app: "codex".into(),
            account: Some("someone@example.com".into()),
            captured_at: now_ms(),
            window_kind: "weekly".into(),
            used: None,
            limit_value: None,
            used_percent: Some(40.0),
            resets_at: None,
            raw_json: Some("{\"token\":\"secret\"}".into()),
        })
        .unwrap();
    f.store
        .conn()
        .execute(
            "INSERT OR REPLACE INTO sources(source,enabled,last_error,rows_ingested) VALUES('codex',1,'/Users/someone/err',3)",
            [],
        )
        .unwrap();
    let mut all = String::new();
    for r in [
        Request::Hello,
        Request::Summary(SummaryQuery::default()),
        Request::Quotas,
        Request::Sources,
    ] {
        let v = respond(&f.path, &f.identity, 1, &r);
        assert_ne!(v["status"], "unavailable", "{v}");
        all.push_str(&v.to_string());
    }
    for secret in ["secret", "/Users", "someone", "example.com", "jsonl"] {
        assert!(!all.contains(secret), "{secret} leaked");
    }
    let s = full_summary(&f);
    assert_eq!(s.meta.status, "partial");
    assert!(s.meta.warnings.contains(&"identifier_redacted".to_string()));
    let quotas: Quotas =
        serde_json::from_value(respond(&f.path, &f.identity, 1, &Request::Quotas)).unwrap();
    assert_eq!(
        quotas.quotas[0].account_key.as_deref().map(str::len),
        Some(16)
    );
    assert_eq!(quotas.quotas[0].value_semantics, "allowance_percent");
    let sources: Sources =
        serde_json::from_value(respond(&f.path, &f.identity, 1, &Request::Sources)).unwrap();
    assert!(
        sources
            .sources
            .iter()
            .any(|s| s.source == "codex" && s.has_error)
    );
}

#[test]
fn identity_is_created_once_on_the_write_path() {
    let f = fixture();
    let again = ensure_identity(&f.store, None).unwrap();
    assert_eq!(again, f.identity);
    assert_eq!(again.device_name, "test-mac");
    assert!(valid_device_id(&again.device_id));
    let renamed = ensure_identity(&f.store, Some("  Studio\u{7}\n ")).unwrap();
    assert_eq!(
        (renamed.device_id.as_str(), renamed.device_name.as_str()),
        (f.identity.device_id.as_str(), "Studio")
    );
    assert_ne!(new_device_id(), new_device_id());
}

#[test]
fn unknown_versions_and_missing_ledgers_fail_with_fixed_codes() {
    let f = fixture();
    let v = respond(&f.path, &f.identity, 2, &Request::Hello);
    assert_eq!(v["error"]["code"], "unsupported_protocol_version");
    assert_eq!(v["status"], "unavailable");
    let missing = f.path.with_file_name("gtt-peer-does-not-exist.db");
    let v = respond(&missing, &f.identity, 1, &Request::Sources);
    assert_eq!(v["error"]["code"], "ledger_missing");
    assert!(!missing.exists(), "the read path never creates a ledger");
}

fn start_server(f: &Fixture) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let state = ServerState {
        db: f.path.clone(),
        identity: f.identity.clone(),
    };
    std::thread::spawn(move || server::serve(&listener, state));
    addr
}

fn raw(addr: std::net::SocketAddr, request: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .unwrap();
    let _ = s.write_all(request);
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

#[test]
fn server_serves_get_and_rejects_everything_else() {
    let f = fixture();
    fill(&f.store);
    let addr = start_server(&f);

    let (status, hello) = server::pull(&addr.to_string(), 1, "hello", "").unwrap();
    assert_eq!(status, 200);
    assert_eq!(hello["device_name"], "test-mac");
    let known = hello["fingerprints"].as_object().unwrap();
    let q = SummaryQuery {
        from: None,
        known: known
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
            .collect(),
    };
    let (status, s) =
        server::pull(&format!("http://{addr}/"), 1, "summary", &q.to_query()).unwrap();
    assert_eq!(status, 200);
    assert_eq!(s["days"].as_array().unwrap().len(), 0);
    assert_eq!(s["unchanged"].as_u64().unwrap() as usize, known.len());
    for ep in ["quotas", "sources"] {
        assert_eq!(server::pull(&addr.to_string(), 1, ep, "").unwrap().0, 200);
    }

    let (status, v) = server::pull(&addr.to_string(), 2, "hello", "").unwrap();
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (400, Some("unsupported_protocol_version"))
    );
    let (status, v) = server::pull(&addr.to_string(), 1, "events", "").unwrap();
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (404, Some("unknown_endpoint"))
    );
    let (status, v) = server::pull(&addr.to_string(), 1, "summary", "known=nope").unwrap();
    assert_eq!(
        (status, v["error"]["code"].as_str()),
        (400, Some("invalid_request"))
    );

    for method in ["POST", "PUT", "DELETE", "HEAD", "OPTIONS"] {
        let r = raw(
            addr,
            format!("{method} /peer/v1/hello HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes(),
        );
        assert!(r.starts_with("HTTP/1.1 405 "), "{method}: {r}");
        assert!(r.contains("Allow: GET\r\n") && r.contains("method_not_allowed"));
    }
    let r = raw(
        addr,
        b"GET /peer/v1/hello HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(r.starts_with("HTTP/1.1 413 "), "{r}");
    let r = raw(
        addr,
        b"GET /peer/v1/hello HTTP/1.1\r\nHost: evil.example:4319\r\n\r\n",
    );
    assert!(
        r.starts_with("HTTP/1.1 403 ") && r.contains("host_not_allowed"),
        "{r}"
    );
    let r = raw(
        addr,
        b"GET /peer/v1/hello HTTP/1.1\r\nHost: localhost:4319\r\n\r\n",
    );
    assert!(r.starts_with("HTTP/1.1 200 "), "{r}");

    let mut huge = b"GET /peer/v1/hello HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Pad: ".to_vec();
    huge.extend(std::iter::repeat_n(b'a', server::MAX_HEAD + 10));
    huge.extend_from_slice(b"\r\n\r\n");
    let r = raw(addr, &huge);
    assert!(
        r.starts_with("HTTP/1.1 431 ") && r.contains("request_too_large"),
        "{r}"
    );
    // A head that never ends is cut off once it exceeds the limit too.
    let r = raw(addr, &vec![b'G'; server::MAX_HEAD + 4096]);
    assert!(r.starts_with("HTTP/1.1 431 "), "{r}");
}

#[test]
fn allowed_hosts_are_ip_literals_or_localhost() {
    use super::server::allowed_host;
    for ok in [
        "127.0.0.1",
        "127.0.0.1:4319",
        "[::1]:4319",
        "::1",
        "LOCALHOST",
        "localhost:1",
        "192.168.1.20:4319",
    ] {
        assert!(allowed_host(ok), "{ok}");
    }
    for bad in [
        "",
        "evil.example",
        "evil.example:4319",
        "127.0.0.1.nip.io",
        "mac.local:4319",
    ] {
        assert!(!allowed_host(bad), "{bad}");
    }
}
