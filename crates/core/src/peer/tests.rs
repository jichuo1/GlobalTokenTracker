use super::keys::{DeviceKey, peer_dir};
use super::net::{Stream, Wire};
use super::server::{self, ServerState};
use super::trust::TrustStore;
use super::*;
use super::{client, pairing};
use crate::Cube;
use crate::model::{CostSource, Provenance, QuotaSnapshot, UsageEvent};
use crate::store::{AppSummary, ShareRow, Totals};
use crate::viewmodel::{Range, day_start_ms};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SEQ: AtomicU64 = AtomicU64::new(0);
const HOUR_MS: i64 = 3_600_000;

struct Fixture {
    path: PathBuf,
    store: Store,
    identity: Identity,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn fixture() -> Fixture {
    // One directory per fixture: the peer directory sits next to the ledger.
    let path = std::env::temp_dir()
        .join(format!(
            "gtt-peer-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
        .join("ledger.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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
    let days: Days =
        serde_json::from_value(respond(&f.path, &f.identity, 1, &Request::Days)).unwrap();
    assert_eq!(days.fingerprints.len(), 3);
    assert_eq!(days.device_id, f.identity.device_id);
    let known = days.fingerprints.clone();
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
        Request::Days,
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

/// Start a server for `f` on an ephemeral loopback port. `tls` = with the
/// fixture's device key (created on first use).
fn start(f: &Fixture, tls: bool) -> (std::net::SocketAddr, Arc<ServerState>) {
    let key = tls.then(|| DeviceKey::load_or_create(&peer_dir(&f.path)).unwrap());
    let state =
        Arc::new(ServerState::new(f.path.clone(), f.identity.clone(), key.as_ref()).unwrap());
    let listener = server::bind("127.0.0.1:0", tls).unwrap();
    let addr = listener.local_addr().unwrap();
    let serving = Arc::clone(&state);
    std::thread::spawn(move || server::serve(&listener, serving));
    (addr, state)
}

fn raw(addr: std::net::SocketAddr, request: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let _ = s.write_all(request);
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

/// Plaintext GET → (status, JSON body).
fn get(addr: std::net::SocketAddr, target: &str, token: Option<&str>) -> (u16, Value) {
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    let r = raw(
        addr,
        format!("GET {target} HTTP/1.1\r\nHost: {addr}\r\n{auth}\r\n").as_bytes(),
    );
    let (head, body) = r.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap())
}

fn code_of(v: &Value) -> Option<&str> {
    v["error"]["code"].as_str()
}

#[test]
fn server_serves_get_and_rejects_everything_else() {
    let f = fixture();
    fill(&f.store);
    let (addr, _) = start(&f, false);
    let token = pairing::new_token().unwrap();
    TrustStore::open(&peer_dir(&f.path))
        .unwrap()
        .add_viewer("local", &token, now_ms())
        .unwrap();
    let tok = Some(token.as_str());

    // `hello` is public and minimal: identity only, nothing from the ledger.
    let (status, hello) = get(addr, "/peer/v1/hello", None);
    assert_eq!(status, 200);
    assert_eq!(hello["device_name"], "test-mac");
    assert!(hello.get("fingerprints").is_none() && hello.get("schema_version").is_none());

    // Everything else needs the bearer token.
    for ep in ["days", "summary", "quotas", "sources"] {
        let (status, v) = get(addr, &format!("/peer/v1/{ep}"), None);
        assert_eq!((status, code_of(&v)), (401, Some("auth_required")), "{ep}");
        let (status, v) = get(addr, &format!("/peer/v1/{ep}"), Some(&"0".repeat(64)));
        assert_eq!((status, code_of(&v)), (401, Some("auth_invalid")), "{ep}");
        assert_eq!(get(addr, &format!("/peer/v1/{ep}"), tok).0, 200, "{ep}");
    }
    let r = raw(
        addr,
        format!("GET /peer/v1/days HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes(),
    );
    assert!(r.contains("WWW-Authenticate: Bearer\r\n"), "{r}");

    let (_, days) = get(addr, "/peer/v1/days", tok);
    let known = days["fingerprints"].as_object().unwrap();
    let q = SummaryQuery {
        from: None,
        known: known
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().into()))
            .collect(),
    };
    let (status, s) = get(addr, &format!("/peer/v1/summary?{}", q.to_query()), tok);
    assert_eq!(status, 200);
    assert_eq!(s["days"].as_array().unwrap().len(), 0);
    assert_eq!(s["unchanged"].as_u64().unwrap() as usize, known.len());

    let (status, v) = get(addr, "/peer/v2/hello", None);
    assert_eq!(
        (status, code_of(&v)),
        (400, Some("unsupported_protocol_version"))
    );
    let (status, v) = get(addr, "/peer/v1/events", tok);
    assert_eq!((status, code_of(&v)), (404, Some("unknown_endpoint")));
    let (status, v) = get(addr, "/peer/v1/summary?known=nope", tok);
    assert_eq!((status, code_of(&v)), (400, Some("invalid_request")));
    // Pairing never runs without TLS.
    let r = raw(
        addr,
        format!("GET /peer/v1/pair?name=x HTTP/1.1\r\nHost: {addr}\r\nUpgrade: gtt-pair/1\r\n\r\n")
            .as_bytes(),
    );
    assert!(
        r.starts_with("HTTP/1.1 403 ") && r.contains("tls_required"),
        "{r}"
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
fn plaintext_is_refused_off_loopback() {
    assert!(server::bind("0.0.0.0:0", false).is_err());
    assert!(server::bind("127.0.0.1:0", false).is_ok());
    assert!(server::bind("0.0.0.0:0", true).is_ok());
    // A plaintext state handed a non-loopback listener refuses to serve.
    let f = fixture();
    let state = Arc::new(ServerState::new(f.path.clone(), f.identity.clone(), None).unwrap());
    let listener = TcpListener::bind("0.0.0.0:0").unwrap();
    assert!(server::serve(&listener, state).is_err());
    // …and the client never sends plaintext (or a token) off loopback.
    let target = client::resolve("192.0.2.1:4319").unwrap();
    assert_eq!(
        client::connect(target, None).err(),
        Some("plaintext_not_allowed")
    );
}

#[test]
fn device_key_is_stable_and_owner_only() {
    let f = fixture();
    let dir = peer_dir(&f.path);
    let a = DeviceKey::load_or_create(&dir).unwrap();
    let b = DeviceKey::load_or_create(&dir).unwrap();
    assert_eq!(a.fingerprint, b.fingerprint);
    assert_eq!(a.fingerprint.len(), 64);
    TrustStore::open(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        for file in ["device-key.der", "device-cert.der", "peer.db"] {
            assert_eq!(mode(&dir.join(file)), 0o600, "{file}");
        }
    }
}

#[test]
fn pairing_happy_path_pins_and_authorizes() {
    let server_f = fixture();
    fill(&server_f.store);
    let viewer = fixture();
    let (addr, state) = start(&server_f, true);
    let fp = state.identity.fingerprint.clone().unwrap();

    // Before pairing: only `hello`, which proves the certificate it claims.
    let (status, hello) = client::pull(&viewer.path, &addr.to_string(), false, 1, "hello", "")
        .unwrap()
        .unwrap();
    assert_eq!(
        (status, hello["fingerprint"].as_str()),
        (200, Some(fp.as_str()))
    );
    assert_eq!(
        client::pull(&viewer.path, &addr.to_string(), false, 1, "summary", "").unwrap(),
        Err("not_paired")
    );

    let code = state.issue_code().unwrap();
    let peer = pairing::pair(&viewer.path, &addr.to_string(), &code, "Laptop")
        .unwrap()
        .unwrap();
    assert_eq!(peer.fingerprint, fp);
    assert_eq!(peer.device_name, "test-mac");
    assert_eq!(peer.device_id, server_f.identity.device_id);
    let viewers = TrustStore::open(&state.peer_dir)
        .unwrap()
        .viewers()
        .unwrap();
    assert_eq!(viewers.len(), 1);
    assert_eq!(viewers[0].name, "Laptop");

    // Pull by device name and by address; both use the pin and token.
    for target in ["test-mac", &addr.to_string()] {
        let (status, s) = client::pull(&viewer.path, target, false, 1, "summary", "")
            .unwrap()
            .unwrap();
        assert_eq!(status, 200, "{s}");
        assert!(!s["days"].as_array().unwrap().is_empty());
    }
    let seen = TrustStore::open(&state.peer_dir)
        .unwrap()
        .viewers()
        .unwrap();
    assert!(seen[0].last_seen_at.is_some());

    // Single use: the same code cannot pair a second viewer.
    let other = fixture();
    assert_eq!(
        pairing::pair(&other.path, &addr.to_string(), &code, "Other").unwrap(),
        Err("pairing_unavailable")
    );

    // The server stores only the token's hash.
    let hash = super::trust::token_hash(&peer.token);
    let mut stored = Vec::new();
    for entry in std::fs::read_dir(&state.peer_dir).unwrap() {
        stored.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    let stored = String::from_utf8_lossy(&stored);
    assert!(!stored.contains(&peer.token), "token stored in clear");
    assert!(stored.contains(&hash));

    // Forgetting the device drops pin and token.
    let trust = TrustStore::open(&peer_dir(&viewer.path)).unwrap();
    assert!(trust.forget("test-mac").unwrap().is_some());
    assert_eq!(
        client::pull(&viewer.path, "test-mac", false, 1, "summary", "").unwrap(),
        Err("not_paired")
    );
}

#[test]
fn revoked_tokens_fail_immediately() {
    let server_f = fixture();
    let viewer = fixture();
    let (addr, state) = start(&server_f, true);
    let code = state.issue_code().unwrap();
    pairing::pair(&viewer.path, &addr.to_string(), &code, "Laptop")
        .unwrap()
        .unwrap();
    assert_eq!(
        client::pull(&viewer.path, "test-mac", false, 1, "sources", "")
            .unwrap()
            .unwrap()
            .0,
        200
    );
    let revoked = TrustStore::open(&state.peer_dir)
        .unwrap()
        .revoke("Laptop", now_ms())
        .unwrap()
        .unwrap();
    assert!(revoked.revoked_at.is_some());
    let (status, v) = client::pull(&viewer.path, "test-mac", false, 1, "sources", "")
        .unwrap()
        .unwrap();
    assert_eq!((status, code_of(&v)), (401, Some("auth_invalid")));
    // Revoking again finds nothing active.
    assert!(
        TrustStore::open(&state.peer_dir)
            .unwrap()
            .revoke("Laptop", now_ms())
            .unwrap()
            .is_none()
    );
}

#[test]
fn wrong_code_fails_backs_off_and_is_never_retried() {
    let server_f = fixture();
    let viewer = fixture();
    let (addr, state) = start(&server_f, true);
    let code = state.issue_code().unwrap();
    let wrong = pairing::test_support::wrong_code(&code);
    assert_eq!(
        pairing::pair(&viewer.path, &addr.to_string(), &wrong, "Laptop").unwrap(),
        Err("pairing_failed")
    );
    assert!(
        TrustStore::open(&state.peer_dir)
            .unwrap()
            .viewers()
            .unwrap()
            .is_empty()
    );
    // The viewer refuses to reuse a code that failed.
    assert_eq!(
        pairing::pair(&viewer.path, &addr.to_string(), &wrong, "Laptop").unwrap(),
        Err("pairing_code_reused")
    );
    // The server backs off after a failure …
    assert_eq!(
        pairing::pair(&viewer.path, &addr.to_string(), &code, "Laptop").unwrap(),
        Err("pairing_locked")
    );
    // … and the right code still works once the back-off has passed.
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        pairing::pair(&viewer.path, &addr.to_string(), &code, "Laptop")
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        pairing::pair(&viewer.path, &addr.to_string(), "12345", "Laptop").unwrap(),
        Err("invalid_code")
    );
}

#[test]
fn codes_expire_are_single_use_and_burn_after_five_failures() {
    let t0 = Instant::now();
    let mut p = pairing::Pairing::default();
    assert_eq!(p.begin(t0), Err("pairing_unavailable"));
    let code = p.issue(t0).unwrap();
    assert!(pairing::valid_code(&code));
    // One run at a time.
    assert_eq!(p.begin(t0), Ok(code.clone()));
    assert_eq!(p.begin(t0), Err("pairing_busy"));
    p.finish(true, t0);
    assert_eq!(p.begin(t0), Err("pairing_unavailable"), "single use");

    p.issue(t0).unwrap();
    assert_eq!(p.begin(t0 + pairing::CODE_TTL), Err("pairing_code_expired"));
    assert_eq!(p.begin(t0 + pairing::CODE_TTL), Err("pairing_unavailable"));

    // Each failure locks for 1, 2, 4, 8, 16 s; the fifth burns the code.
    p.issue(t0).unwrap();
    let mut now = t0;
    let mut waits = Vec::new();
    for _ in 0..pairing::MAX_FAILURES {
        assert!(p.begin(now).is_ok());
        p.finish(false, now);
        assert_eq!(p.begin(now), Err("pairing_locked"));
        let mut wait = Duration::ZERO;
        while p.locked(now + wait) {
            wait += Duration::from_millis(250);
        }
        waits.push(wait.as_secs());
        now += wait;
    }
    assert_eq!(waits, [1, 2, 4, 8, 16]);
    assert_eq!(p.begin(now), Err("pairing_unavailable"), "burned");
    // A new code starts clean.
    p.issue(now).unwrap();
    assert!(p.begin(now).is_ok());
}

/// Accept one TLS connection with `attacker`'s certificate and relay the
/// pairing exchange, message for message, to the real server — what a
/// network interceptor without the code can do at best.
fn relay(attacker: &DeviceKey, real: std::net::SocketAddr) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let config = attacker.server_config().unwrap();
    std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let conn = rustls::ServerConnection::new(config).unwrap();
        let mut down = Wire::new(Stream::TlsServer(Box::new(rustls::StreamOwned::new(
            conn, tcp,
        ))));
        let verifier = super::keys::PinVerifier::new(None);
        let mut up = client::connect(real, Some(&verifier)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let head = down.read_until(b"\r\n\r\n", 8192, deadline).unwrap();
        up.stream.write_all(&head).unwrap();
        up.stream.write_all(b"\r\n\r\n").unwrap();
        let head = up.read_until(b"\r\n\r\n", 8192, deadline).unwrap();
        down.stream.write_all(&head).unwrap();
        down.stream.write_all(b"\r\n\r\n").unwrap();
        // ns0 S→V, nv0 V→S, then per round c V→S, c S→V, n V→S, n S→V.
        let mut script = vec![true, false];
        for _ in 0..6 {
            script.extend([false, true, false, true]);
        }
        script.push(true);
        for from_server in script {
            let (src, dst) = if from_server {
                (&mut up, &mut down)
            } else {
                (&mut down, &mut up)
            };
            let Ok(line) = src.read_until(b"\n", 4096, deadline) else {
                break;
            };
            let _ = dst.stream.write_all(&line);
            let _ = dst.stream.write_all(b"\n");
            let _ = dst.stream.flush();
        }
        down.stream.close(0);
        up.stream.close(0);
    });
    addr
}

#[test]
fn an_interceptor_certificate_can_neither_pair_nor_pull() {
    let server_f = fixture();
    let viewer = fixture();
    let attacker_f = fixture();
    let (addr, state) = start(&server_f, true);
    let attacker = DeviceKey::load_or_create(&peer_dir(&attacker_f.path)).unwrap();
    assert_ne!(
        attacker.fingerprint,
        state.identity.fingerprint.clone().unwrap()
    );

    // A relay that terminates TLS with its own certificate cannot complete
    // pairing even when it forwards every message faithfully.
    let code = state.issue_code().unwrap();
    let mitm = relay(&attacker, addr);
    assert_eq!(
        pairing::pair(&viewer.path, &mitm.to_string(), &code, "Laptop").unwrap(),
        Err("pairing_failed")
    );
    assert!(
        TrustStore::open(&state.peer_dir)
            .unwrap()
            .viewers()
            .unwrap()
            .is_empty()
    );
    assert!(
        TrustStore::open(&peer_dir(&viewer.path))
            .unwrap()
            .peers()
            .unwrap()
            .is_empty()
    );

    // A device that does not know the code cannot pair either.
    let (fake_addr, fake) = start(&attacker_f, true);
    fake.issue_code().unwrap();
    let code = state.issue_code().unwrap();
    assert_eq!(
        pairing::pair(&viewer.path, &fake_addr.to_string(), &code, "Laptop").unwrap(),
        Err("pairing_failed")
    );

    // After a genuine pairing, a different certificate at the saved address
    // fails closed — no request (and no token) is sent.
    let code = state.issue_code().unwrap();
    let mut peer = pairing::pair(&viewer.path, &addr.to_string(), &code, "Laptop")
        .unwrap()
        .unwrap();
    peer.address = fake_addr.to_string();
    TrustStore::open(&peer_dir(&viewer.path))
        .unwrap()
        .save_peer(&peer)
        .unwrap();
    for ep in ["hello", "summary"] {
        assert_eq!(
            client::pull(&viewer.path, "test-mac", false, 1, ep, "").unwrap(),
            Err("peer_fingerprint_mismatch"),
            "{ep}"
        );
    }
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
