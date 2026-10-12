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
use rusqlite::params;
use serde_json::Value;
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
    let (status, hello) = client::pull(
        &viewer.store,
        &addr.to_string(),
        false,
        false,
        1,
        "hello",
        "",
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (status, hello["fingerprint"].as_str()),
        (200, Some(fp.as_str()))
    );
    assert_eq!(
        client::pull(
            &viewer.store,
            &addr.to_string(),
            false,
            false,
            1,
            "summary",
            ""
        )
        .unwrap(),
        Err("not_paired")
    );

    let code = state.issue_code().unwrap();
    let peer = pairing::pair(&viewer.store, &addr.to_string(), &code, "Laptop")
        .unwrap()
        .unwrap();
    assert_eq!(peer.fingerprint, fp);
    assert_eq!(peer.name, "test-mac");
    assert_eq!(peer.device_id, server_f.identity.device_id);
    let viewers = TrustStore::open(&state.peer_dir)
        .unwrap()
        .viewers()
        .unwrap();
    assert_eq!(viewers.len(), 1);
    assert_eq!(viewers[0].name, "Laptop");

    // Pull by device name and by address; both use the pin and token.
    for target in ["test-mac", &addr.to_string()] {
        let (status, s) = client::pull(&viewer.store, target, false, false, 1, "summary", "")
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
        pairing::pair(&other.store, &addr.to_string(), &code, "Other").unwrap(),
        Err("pairing_unavailable")
    );

    // The server stores only the token's hash.
    let token = super::devices::Registry::open(&viewer.store)
        .unwrap()
        .token(&peer.device_id)
        .unwrap()
        .unwrap();
    let hash = super::trust::token_hash(&token);
    let mut stored = Vec::new();
    for entry in std::fs::read_dir(&state.peer_dir).unwrap() {
        stored.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    let stored = String::from_utf8_lossy(&stored);
    assert!(!stored.contains(&token), "token stored in clear");
    assert!(stored.contains(&hash));

    // Forgetting the device drops pin and token.
    let registry = super::devices::Registry::open(&viewer.store).unwrap();
    assert!(registry.forget("test-mac").unwrap().is_some());
    assert!(registry.token(&peer.device_id).unwrap().is_none());
    assert_eq!(
        client::pull(&viewer.store, "test-mac", false, false, 1, "summary", "").unwrap(),
        Err("not_paired")
    );
}

#[test]
fn unknown_unresolvable_target_is_not_paired() {
    // A name that is no paired device answers "not paired" whether or not
    // the network's resolver claims it (some resolve every name, CI's
    // resolve none) — it must not surface as a connection failure.
    let viewer = fixture();
    for endpoint in ["summary", "sources", "quotas"] {
        assert_eq!(
            client::pull(
                &viewer.store,
                "gone-device.invalid",
                false,
                false,
                1,
                endpoint,
                ""
            )
            .unwrap(),
            Err("not_paired")
        );
    }
}

#[test]
fn revoked_tokens_fail_immediately() {
    let server_f = fixture();
    let viewer = fixture();
    let (addr, state) = start(&server_f, true);
    let code = state.issue_code().unwrap();
    pairing::pair(&viewer.store, &addr.to_string(), &code, "Laptop")
        .unwrap()
        .unwrap();
    assert_eq!(
        client::pull(&viewer.store, "test-mac", false, false, 1, "sources", "")
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
    let (status, v) = client::pull(&viewer.store, "test-mac", false, false, 1, "sources", "")
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
        pairing::pair(&viewer.store, &addr.to_string(), &wrong, "Laptop").unwrap(),
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
        pairing::pair(&viewer.store, &addr.to_string(), &wrong, "Laptop").unwrap(),
        Err("pairing_code_reused")
    );
    // The server backs off after a failure …
    assert_eq!(
        pairing::pair(&viewer.store, &addr.to_string(), &code, "Laptop").unwrap(),
        Err("pairing_locked")
    );
    // … and the right code still works once the back-off has passed.
    std::thread::sleep(Duration::from_millis(1100));
    assert!(
        pairing::pair(&viewer.store, &addr.to_string(), &code, "Laptop")
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        pairing::pair(&viewer.store, &addr.to_string(), "12345", "Laptop").unwrap(),
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
        pairing::pair(&viewer.store, &mitm.to_string(), &code, "Laptop").unwrap(),
        Err("pairing_failed")
    );
    assert!(
        TrustStore::open(&state.peer_dir)
            .unwrap()
            .viewers()
            .unwrap()
            .is_empty()
    );
    assert!(super::devices::list(&viewer.store).unwrap().is_empty());

    // A device that does not know the code cannot pair either.
    let (fake_addr, fake) = start(&attacker_f, true);
    fake.issue_code().unwrap();
    let code = state.issue_code().unwrap();
    assert_eq!(
        pairing::pair(&viewer.store, &fake_addr.to_string(), &code, "Laptop").unwrap(),
        Err("pairing_failed")
    );

    // After a genuine pairing, a different certificate at the saved address
    // fails closed — no request (and no token) is sent.
    let code = state.issue_code().unwrap();
    let peer = pairing::pair(&viewer.store, &addr.to_string(), &code, "Laptop")
        .unwrap()
        .unwrap();
    super::devices::Registry::open(&viewer.store)
        .unwrap()
        .set_address(&peer.device_id, &fake_addr.to_string())
        .unwrap();
    for ep in ["hello", "summary"] {
        assert_eq!(
            client::pull(&viewer.store, "test-mac", false, false, 1, ep, "").unwrap(),
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

// ── discovery ─────────────────────────────────────────────────────────────

use super::discovery::{self, Sighting, Txt};

fn txt(id: char, name: &str, fp: char) -> Txt {
    Txt {
        protocol_version: 1,
        device_id: id.to_string().repeat(32),
        name: name.into(),
        fingerprint_prefix: fp.to_string().repeat(16),
    }
}

fn decode(pairs: &[(&str, &str)]) -> Result<Txt, &'static str> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Txt::decode(|k| map.get(k).cloned())
}

#[test]
fn txt_round_trips_and_rejects_untrusted_values() {
    let t = txt('a', "书房 Mac", 'b');
    let encoded = t.encode();
    let keys: Vec<&str> = encoded.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["v", "id", "name", "fp"], "nothing usage-related");
    let map: BTreeMap<String, String> = encoded.into_iter().collect();
    assert_eq!(Txt::decode(|k| map.get(k).cloned()), Ok(t.clone()));

    let id = "a".repeat(32);
    let fp = "b".repeat(16);
    let ok =
        |v: &str, i: &str, n: &str, f: &str| decode(&[("v", v), ("id", i), ("name", n), ("fp", f)]);
    assert!(ok("1", &id, "x", &fp).is_ok());
    assert_eq!(ok("0", &id, "x", &fp), Err("txt_invalid_version"));
    assert_eq!(ok("one", &id, "x", &fp), Err("txt_invalid_version"));
    assert_eq!(
        ok("1", &"A".repeat(32), "x", &fp),
        Err("txt_invalid_device_id")
    );
    assert_eq!(
        ok("1", &"a".repeat(31), "x", &fp),
        Err("txt_invalid_device_id")
    );
    assert_eq!(ok("1", &id, " \u{7} ", &fp), Err("txt_invalid_name"));
    assert_eq!(
        ok("1", &id, "x", &"b".repeat(64)),
        Err("txt_invalid_fingerprint")
    );
    assert_eq!(
        ok("1", &id, "x", "zzzzzzzzzzzzzzzz"),
        Err("txt_invalid_fingerprint")
    );
    assert_eq!(decode(&[("v", "1")]), Err("txt_invalid_device_id"));

    // Long multi-byte names are cut on a character boundary to fit a TXT string.
    let long = txt('a', &"名".repeat(80), 'b');
    let name = long.encode().remove(2).1;
    assert!(name.len() <= 200 && name.chars().all(|c| c == '名'));
    assert!(
        Txt::for_identity(&Identity {
            device_id: id.clone(),
            device_name: "x".into(),
            fingerprint: None
        })
        .is_none()
    );
}

fn sock(s: &str) -> std::net::SocketAddr {
    s.parse().unwrap()
}

fn peer_row(id: char, fp: &str) -> super::devices::PeerDevice {
    super::devices::PeerDevice {
        device_id: id.to_string().repeat(32),
        name: "paired".into(),
        fingerprint: fp.into(),
        last_address: "192.0.2.9:4319".into(),
        paired_at: 0,
        last_seen_at: None,
        last_pull_at: None,
        last_error_code: None,
        gtt_version: None,
        protocol_version: Some(1),
        enabled: true,
    }
}

#[test]
fn browse_answers_merge_across_interfaces_and_flag_mismatches() {
    let a = txt('a', "Studio", 'b');
    let sightings = vec![
        // The same device answers once per interface and family.
        Sighting {
            addresses: vec![sock("[fe80::1%4]:4319"), sock("192.168.1.20:4319")],
            txt: Ok(a.clone()),
        },
        Sighting {
            addresses: vec![sock("192.168.1.20:4319"), sock("[2001:db8::20]:4319")],
            txt: Ok(a.clone()),
        },
        Sighting {
            addresses: vec![sock("10.0.0.5:4319")],
            txt: Ok(txt('c', "Attic", 'd')),
        },
        Sighting {
            addresses: vec![sock("10.0.0.6:4319")],
            txt: Ok(txt('e', "Shed", 'f')),
        },
        Sighting {
            addresses: vec![sock("10.0.0.6:4319")],
            txt: Ok(txt('e', "Shed", '0')),
        },
        Sighting {
            addresses: vec![sock("10.0.0.7:4319")],
            txt: Err("txt_invalid_device_id"),
        },
    ];
    let peers = vec![
        peer_row('a', &"b".repeat(64)),
        peer_row('c', &"9".repeat(64)),
    ];
    let (found, invalid) = discovery::merge(&sightings, &peers);
    assert_eq!(invalid, 1);
    let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["Attic", "Shed", "Shed", "Studio"]);
    let studio = &found[3];
    assert_eq!(
        studio.addresses,
        [
            "192.168.1.20:4319",
            "[2001:db8::20]:4319",
            "[fe80::1%4]:4319"
        ],
        "deduplicated, IPv4 first, link-local last with its scope"
    );
    assert_eq!(studio.port, 4319);
    assert!(studio.paired && studio.warnings.is_empty());
    // Paired, but the advertised prefix is not the pinned certificate's.
    assert!(found[0].paired);
    assert_eq!(found[0].warnings, ["fingerprint_mismatch"]);
    // Two certificates claim one device id: both listed, both flagged.
    for shed in &found[1..3] {
        assert!(!shed.paired);
        assert_eq!(shed.warnings, ["duplicate_device_id"]);
    }
    assert_ne!(found[1].fingerprint_prefix, found[2].fingerprint_prefix);
}

#[test]
fn addresses_parse_with_default_ports_ipv6_and_scopes() {
    let r = |s: &str| client::resolve(s);
    assert_eq!(r("127.0.0.1"), Ok(sock("127.0.0.1:4319")));
    assert_eq!(r("https://127.0.0.1:9/"), Ok(sock("127.0.0.1:9")));
    assert_eq!(r("::1"), Ok(sock("[::1]:4319")));
    assert_eq!(r("[::1]"), Ok(sock("[::1]:4319")));
    assert_eq!(r("[::1]:7"), Ok(sock("[::1]:7")));
    assert_eq!(r("[fe80::1%4]:7"), Ok(sock("[fe80::1%4]:7")));
    // Interface names resolve to the numeric scope.
    let lo = if cfg!(target_os = "macos") {
        "lo0"
    } else {
        "lo"
    };
    if cfg!(unix) {
        let a = r(&format!("fe80::1%{lo}")).unwrap();
        let std::net::SocketAddr::V6(v6) = a else {
            panic!("{a}")
        };
        assert!(v6.scope_id() != 0 && v6.port() == 4319, "{a}");
        assert_eq!(r(&format!("[fe80::1%{lo}]:5")).unwrap().port(), 5);
    }
    for bad in ["", "[::1", "[::1]x", "127.0.0.1:port", ":4319"] {
        assert!(r(bad).is_err(), "{bad}");
    }
}

#[test]
fn paired_devices_are_refound_by_device_id_before_the_saved_address() {
    let server_f = fixture();
    let viewer = fixture();
    let attacker_f = fixture();
    let (addr, state) = start(&server_f, true);
    let (fake_addr, _) = start(&attacker_f, true);
    let code = state.issue_code().unwrap();
    let peer = pairing::pair(&viewer.store, &addr.to_string(), &code, "Laptop")
        .unwrap()
        .unwrap();
    assert_eq!(
        discovery::candidates(&[addr], &addr.to_string()),
        [addr.to_string()]
    );
    assert_eq!(
        discovery::candidates(&[fake_addr], "192.0.2.1:1"),
        [fake_addr.to_string(), "192.0.2.1:1".to_string()]
    );

    // DHCP moved the device: the saved address is dead, mDNS knows the new one.
    let registry = super::devices::Registry::open(&viewer.store).unwrap();
    registry
        .set_address(&peer.device_id, "127.0.0.1:9")
        .unwrap();
    let id = server_f.identity.device_id.clone();
    let asked = std::cell::RefCell::new(Vec::new());
    let locate = |device_id: &str, _: &str| {
        asked.borrow_mut().push(device_id.to_string());
        vec![addr]
    };
    let (status, _) = client::pull_via(&viewer.store, "test-mac", false, &locate, 1, "sources", "")
        .unwrap()
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(*asked.borrow(), [id]);
    // The working address is remembered.
    assert_eq!(
        registry.find("test-mac").unwrap().unwrap().last_address,
        addr.to_string()
    );

    // A different device answering on mDNS is skipped (pin check) and the
    // saved address still works.
    let liar = |_: &str, _: &str| vec![fake_addr];
    let (status, _) = client::pull_via(&viewer.store, "test-mac", false, &liar, 1, "sources", "")
        .unwrap()
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(
        registry.find("test-mac").unwrap().unwrap().last_address,
        addr.to_string()
    );
    // Only an impostor anywhere → fail closed with the mismatch.
    registry
        .set_address(&peer.device_id, &fake_addr.to_string())
        .unwrap();
    assert_eq!(
        client::pull_via(&viewer.store, "test-mac", false, &liar, 1, "sources", "").unwrap(),
        Err("peer_fingerprint_mismatch")
    );
}

/// Real multicast: advertise and browse on this host. Needs a network
/// interface with multicast (and, on macOS, local-network access).
#[test]
#[ignore = "needs multicast networking"]
fn advertisement_is_discoverable_and_withdrawn() {
    let t = txt('7', "mdns-it", '8');
    let ad = discovery::Advertisement::start(&t, sock("0.0.0.0:47399")).unwrap();
    let id = t.device_id.clone();
    let seen = discovery::browse(Duration::from_secs(5), |s| {
        s.txt.as_ref().is_ok_and(|x| x.device_id == id)
    })
    .unwrap();
    let (found, _) = discovery::merge(&seen, &[]);
    let me = found
        .iter()
        .find(|f| f.device_id == t.device_id)
        .expect("advertised");
    assert_eq!((me.name.as_str(), me.port), ("mdns-it", 47399));
    assert!(!me.addresses.is_empty());
    ad.withdraw();
}

// ── viewer storage, sync and the device dimension ─────────────────────────

use super::devices::Registry;
use super::sync::{SyncOptions, sync_all_via, sync_if_due};

fn no_locate() -> super::sync::Locate {
    Arc::new(|_: &str, _: &str| Vec::new())
}

fn quick() -> SyncOptions {
    SyncOptions {
        rediscover: false,
        device_timeout: Duration::from_secs(20),
    }
}

/// A small, distinct local dataset for viewers (so viewer ≠ server data).
fn fill_viewer(store: &Store) {
    for i in 0..6i64 {
        let ts = day_start_ms(i * 3) + HOUR_MS * (5 + i);
        store
            .upsert_event(&ev(
                &format!("v{i}"),
                "codex",
                Some("gpt"),
                Some(ts),
                1_000 + i as u64,
                0.5,
            ))
            .unwrap();
    }
}

/// Pair `viewer` with a TLS server for `server_f` and sync once.
fn pair_and_sync(server_f: &Fixture, viewer: &Fixture) -> (Arc<ServerState>, String) {
    let (addr, state) = start(server_f, true);
    let code = state.issue_code().unwrap();
    let peer = pairing::pair(&viewer.store, &addr.to_string(), &code, "Viewer")
        .unwrap()
        .unwrap();
    let report = sync_all_via(&viewer.store, &quick(), &no_locate()).unwrap();
    assert!(report.devices[0].ok, "{report:?}");
    (state, peer.device_id)
}

fn dump(store: &Store, device: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (table, order) in [
        ("peer_groups", "utc_hour, app, model"),
        ("peer_day_fps", "utc_day"),
        ("peer_quotas", "app, account_key, window_kind"),
    ] {
        let mut st = store
            .conn()
            .prepare(&format!(
                "SELECT * FROM {table} WHERE device_id=?1 ORDER BY {order}"
            ))
            .unwrap();
        let n = st.column_count();
        let rows = st
            .query_map([device], |r| {
                Ok((0..n)
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap();
        out.extend(rows.map(|r| format!("{table}:{}", r.unwrap())));
    }
    out
}

fn count(store: &Store, table: &str, device: &str) -> i64 {
    store
        .conn()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE device_id=?1"),
            [device],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn schema_v2_migrates_existing_ledgers_and_legacy_viewer_rows() {
    let f = fixture();
    fill_viewer(&f.store);
    // Make it look like a v1 ledger: no peer tables, version 1.
    f.store
        .conn()
        .execute_batch(
            "DROP TABLE peers; DROP TABLE peer_groups; DROP TABLE peer_day_fps;
             DROP TABLE peer_quotas; DELETE FROM schema_migrations WHERE version >= 2;",
        )
        .unwrap();
    let path = f.path.clone();
    let reopened = Store::open(&path).unwrap();
    let version: i64 = reopened
        .conn()
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(version, 2);
    for t in ["peers", "peer_groups", "peer_day_fps", "peer_quotas"] {
        assert_eq!(count(&reopened, t, "x"), 0, "{t} exists");
    }
    assert_eq!(
        reopened.event_count(None, None).unwrap(),
        6,
        "local rows untouched"
    );

    // Step-2 viewers kept pins + tokens in peer.db: moved into the ledger once.
    let trust = TrustStore::open(&peer_dir(&path)).unwrap();
    trust
        .insert_legacy_peer(&super::trust::LegacyPeer {
            fingerprint: "f".repeat(64),
            device_id: "d".repeat(32),
            device_name: "Old Mac".into(),
            address: "127.0.0.1:4319".into(),
            token: "t".repeat(64),
            paired_at: 7,
            last_pulled_at: Some(8),
        })
        .unwrap();
    let registry = Registry::open(&reopened).unwrap();
    let moved = registry.find("Old Mac").unwrap().unwrap();
    assert_eq!(
        (moved.fingerprint.as_str(), moved.paired_at),
        ("f".repeat(64).as_str(), 7)
    );
    assert_eq!(
        registry.token(&"d".repeat(32)).unwrap(),
        Some("t".repeat(64))
    );
    assert!(
        trust.legacy_peers().unwrap().is_empty(),
        "legacy table dropped"
    );
}

#[test]
fn sync_is_incremental_idempotent_and_replaces_or_removes_days() {
    let server_f = fixture();
    fill(&server_f.store);
    let viewer = fixture();
    let (_state, id) = pair_and_sync(&server_f, &viewer);
    let first = dump(&viewer.store, &id);
    assert!(count(&viewer.store, "peer_groups", &id) > 0);

    // Nothing changed → nothing rewritten.
    let again = sync_all_via(&viewer.store, &quick(), &no_locate()).unwrap();
    let d = &again.devices[0];
    assert!(
        d.ok && d.days_changed == 0 && d.days_removed == 0 && !d.quotas_changed,
        "{d:?}"
    );
    assert!(!again.changed);
    assert_eq!(dump(&viewer.store, &id), first);

    // The server re-prices one day and loses another (prune/re-import).
    let day = |n: i64| 20_000 * DAY_MS + n * DAY_MS;
    for (k, n) in [("x1", 0), ("x2", 0), ("x3", 1)] {
        server_f
            .store
            .upsert_event(&ev(
                k,
                "cline",
                Some("opus"),
                Some(day(n) + 3 * HOUR_MS),
                10,
                1.0,
            ))
            .unwrap();
    }
    sync_all_via(&viewer.store, &quick(), &no_locate()).unwrap();
    server_f
        .store
        .conn()
        .execute(
            "UPDATE usage_events SET cost_usd=9.5 WHERE dedup_key='x1'",
            [],
        )
        .unwrap();
    server_f
        .store
        .conn()
        .execute("DELETE FROM usage_events WHERE dedup_key='x3'", [])
        .unwrap();
    let r = sync_all_via(&viewer.store, &quick(), &no_locate()).unwrap();
    let d = &r.devices[0];
    assert_eq!((d.days_changed, d.days_removed), (1, 1), "{d:?}");
    let cost: f64 = viewer
        .store
        .conn()
        .query_row(
            "SELECT SUM(cost_usd) FROM peer_groups WHERE device_id=?1 AND utc_hour BETWEEN ?2 AND ?3",
            params![id, day(0) / HOUR_MS, day(0) / HOUR_MS + 23],
            |r| r.get(0),
        )
        .unwrap();
    assert!(close(cost, 10.5), "{cost}");
    let gone: i64 = viewer
        .store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM peer_groups WHERE device_id=?1 AND utc_hour BETWEEN ?2 AND ?3",
            params![id, day(1) / HOUR_MS, day(1) / HOUR_MS + 23],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(gone, 0);
    // Fingerprints on the viewer now equal the server's.
    let server_days: Days = serde_json::from_value(respond(
        &server_f.path,
        &server_f.identity,
        1,
        &Request::Days,
    ))
    .unwrap();
    let mut st = viewer
        .store
        .conn()
        .prepare("SELECT utc_day, fingerprint FROM peer_day_fps WHERE device_id=?1")
        .unwrap();
    let viewer_days: BTreeMap<String, String> = st
        .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(viewer_days, server_days.fingerprints);
}

#[test]
fn peer_data_is_rebucketed_into_the_viewers_frame() {
    let server_f = fixture();
    fill(&server_f.store);
    let viewer = fixture();
    let (_state, id) = pair_and_sync(&server_f, &viewer);
    let only = [id.clone()];
    for off in [0, 8 * HOUR_MS, -5 * HOUR_MS, 13 * HOUR_MS] {
        let cube = crate::cube::Cube::build_with_offset(&viewer.store, off).unwrap();
        let mut st = server_f
            .store
            .conn()
            .prepare(&format!(
                "SELECT (ts_start + ?1) / {DAY_MS} AS d, COUNT(*),
                        SUM(input_tokens+output_tokens+cache_read_tokens+unclassified_tokens)
                 FROM usage_events WHERE ts_start IS NOT NULL GROUP BY d"
            ))
            .unwrap();
        let sql: BTreeMap<i32, (u64, u64)> = st
            .query_map([off], |r| {
                Ok((
                    r.get::<_, i64>(0)? as i32,
                    (r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64),
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(cube.day_totals_for(Some(&only)), sql, "offset {off}");
    }
}

/// Field-by-field equality of overview parts (cost/credits with tolerance).
fn same_parts(a: &crate::cube::OverviewParts, b: &crate::viewmodel::OverviewVm, what: &str) {
    assert_totals(&a.today, &b.today, &format!("today {what}"));
    assert_totals(&a.span, &b.span, &format!("span {what}"));
    assert_totals(&a.all, &b.all, &format!("all {what}"));
    let apps = |v: &[AppSummary]| -> BTreeMap<String, (u64, u64)> {
        v.iter()
            .map(|x| (x.app.clone(), (x.events, x.input_tokens)))
            .collect()
    };
    assert_eq!(apps(&a.by_app), apps(&b.by_app), "by_app {what}");
    let models = |v: &[ShareRow]| -> BTreeMap<String, (u64, u64)> {
        v.iter()
            .map(|x| (x.name.clone(), (x.events, x.tokens)))
            .collect()
    };
    assert_eq!(models(&a.by_model), models(&b.by_model), "by_model {what}");
    let days = |v: &[crate::viewmodel::TrendBucket]| -> Vec<(String, u64, u64)> {
        v.iter()
            .map(|x| (x.date.clone(), x.events, x.tokens))
            .collect()
    };
    assert_eq!(days(&a.daily), days(&b.daily), "daily {what}");
}

fn sum_totals(a: &Totals, b: &Totals) -> Totals {
    let mut t = a.clone();
    t.events += b.events;
    t.input_tokens += b.input_tokens;
    t.output_tokens += b.output_tokens;
    t.reasoning_tokens += b.reasoning_tokens;
    t.cache_read_tokens += b.cache_read_tokens;
    t.cache_write_tokens += b.cache_write_tokens;
    t.unclassified_tokens += b.unclassified_tokens;
    t.credits += b.credits;
    t.cost_usd += b.cost_usd;
    t.active_ms += b.active_ms;
    t
}

#[test]
fn device_filter_reaches_every_overview_part() {
    let server_f = fixture();
    fill(&server_f.store);
    let viewer = fixture();
    fill_viewer(&viewer.store);
    let (_state, id) = pair_and_sync(&server_f, &viewer);
    if crate::cube::current_offset_ms() % HOUR_MS != 0 {
        return; // hour buckets cannot split a :30/:45 local midnight
    }
    let cube = crate::cube::Cube::build(&viewer.store).unwrap();
    let local = [cube.local_device().to_string()];
    let peer = [id.clone()];
    let server_cube = crate::cube::Cube::build(&server_f.store).unwrap();
    let ranges = [
        Range::Today,
        Range::Week,
        Range::Month,
        Range::All,
        Range::custom(day_start_ms(20), day_start_ms(10) + DAY_MS),
    ];
    for range in ranges {
        let what = format!("{range:?}");
        // This device only == the SQL path over local rows.
        let mine = cube
            .overview_parts_for(range, None, None, Some(&local))
            .unwrap();
        let sql = viewer.store.overview(range, None, None).unwrap();
        same_parts(&mine, &sql, &format!("local {what}"));
        // The peer only == the server's own view of itself.
        let theirs = cube
            .overview_parts_for(range, None, None, Some(&peer))
            .unwrap();
        let own = server_cube.overview_parts(range, None, None).unwrap();
        let own_vm = server_f
            .store
            .overview_from_cube(&server_cube, range, None, None)
            .unwrap();
        assert_totals(&theirs.span, &own.span, &format!("peer span {what}"));
        same_parts(&theirs, &own_vm, &format!("peer {what}"));
        // All devices == local + Σ peers.
        let all = cube.overview_parts_for(range, None, None, None).unwrap();
        assert_totals(
            &all.span,
            &sum_totals(&mine.span, &theirs.span),
            &format!("all span {what}"),
        );
        assert_totals(
            &all.today,
            &sum_totals(&mine.today, &theirs.today),
            &format!("all today {what}"),
        );
        assert_totals(
            &all.all,
            &sum_totals(&mine.all, &theirs.all),
            &format!("all all {what}"),
        );
        let events =
            |p: &crate::cube::OverviewParts| -> u64 { p.daily.iter().map(|b| b.events).sum() };
        assert_eq!(
            events(&all),
            events(&mine) + events(&theirs),
            "trend {what}"
        );
        // Per-app filter composes with the device filter.
        let codex = ["codex".to_string()];
        let c = cube
            .overview_parts_for(range, Some(&codex), None, Some(&peer))
            .unwrap();
        let s = server_cube
            .overview_parts(range, Some(&codex), None)
            .unwrap();
        assert_totals(&c.span, &s.span, &format!("codex peer {what}"));
        // Nobody selected → zeros.
        let none = cube
            .overview_parts_for(range, None, None, Some(&[]))
            .unwrap();
        assert_eq!(none.span.events, 0);
        // Per-device totals add up.
        let per = cube.device_totals(range, None, None).unwrap();
        assert_eq!(per.len(), 2);
        assert_totals(
            &per[0].span,
            &mine.span,
            &format!("device_totals local {what}"),
        );
        assert_totals(
            &per[1].span,
            &theirs.span,
            &format!("device_totals peer {what}"),
        );
    }
    // Heat: all = local + peer, day by day.
    let (h_all, h_mine, h_peer) = (
        cube.heat_for(None, None, None),
        cube.heat_for(None, None, Some(&local)),
        cube.heat_for(None, None, Some(&peer)),
    );
    for ((a, m), p) in h_all.iter().zip(&h_mine).zip(&h_peer) {
        assert_eq!(
            (a.events, a.tokens),
            (m.events + p.events, m.tokens + p.tokens),
            "{}",
            a.date
        );
    }
    assert_eq!(
        h_mine,
        viewer.store.overview(Range::All, None, None).unwrap().heat
    );
    // Detail counts stay local.
    assert_eq!(cube.event_count(None, None), 6);

    // A window the cube declines (sub-day edges) takes the SQL path for this
    // device and the hour-level fold for peers — still local + Σ peers.
    let window = Range::custom(
        day_start_ms(5) + 3 * HOUR_MS,
        day_start_ms(2) + 17 * HOUR_MS,
    );
    assert!(cube.overview_parts_for(window, None, None, None).is_none());
    let vm = viewer
        .store
        .overview_from_cube_for(&cube, window, None, None, None)
        .unwrap();
    let local_sql = viewer
        .store
        .totals(window.start_ms(), window.end_ms(), None, None)
        .unwrap();
    let peer_sql = server_f
        .store
        .totals(window.start_ms(), window.end_ms(), None, None)
        .unwrap();
    assert_totals(
        &vm.span,
        &sum_totals(&local_sql, &peer_sql),
        "fallback span",
    );
    let peer_only = viewer
        .store
        .overview_from_cube_for(&cube, window, None, None, Some(&peer))
        .unwrap();
    assert_totals(&peer_only.span, &peer_sql, "fallback peer span");
    assert_eq!(vm.devices.len(), 2);
    assert!(vm.devices[0].local && !vm.devices[1].local);
    assert_totals(&vm.devices[1].span, &peer_sql, "fallback device span");
}

#[test]
fn without_peers_the_device_dimension_changes_nothing() {
    let f = fixture();
    fill(&f.store);
    let cube = crate::cube::Cube::build(&f.store).unwrap();
    let local = [cube.local_device().to_string()];
    for range in [Range::Today, Range::Week, Range::Month, Range::All] {
        let before = cube.overview_parts(range, None, None);
        let mine = cube.overview_parts_for(range, None, None, Some(&local));
        assert_eq!(format!("{before:?}"), format!("{mine:?}"), "{range:?}");
    }
    assert_eq!(
        serde_json::to_string(&cube.heat(None, None)).unwrap(),
        serde_json::to_string(&cube.heat_for(None, None, Some(&local))).unwrap()
    );
    assert_eq!(cube.device_ids(), local);
}

#[test]
fn forgetting_a_device_removes_all_of_its_rows_and_its_token() {
    let server_f = fixture();
    fill(&server_f.store);
    server_f
        .store
        .insert_quota(&QuotaSnapshot {
            app: "codex".into(),
            account: Some("a@example.com".into()),
            captured_at: now_ms(),
            window_kind: "weekly".into(),
            used: None,
            limit_value: None,
            used_percent: Some(10.0),
            resets_at: None,
            raw_json: None,
        })
        .unwrap();
    let viewer = fixture();
    let (_state, id) = pair_and_sync(&server_f, &viewer);
    for t in ["peer_groups", "peer_day_fps", "peer_quotas"] {
        assert!(count(&viewer.store, t, &id) > 0, "{t}");
    }
    let registry = Registry::open(&viewer.store).unwrap();
    assert!(registry.forget(&id).unwrap().is_some());
    for t in ["peers", "peer_groups", "peer_day_fps", "peer_quotas"] {
        assert_eq!(count(&viewer.store, t, &id), 0, "{t}");
    }
    assert!(registry.token(&id).unwrap().is_none());
    let cube = crate::cube::Cube::build(&viewer.store).unwrap();
    assert_eq!(cube.device_ids().len(), 1);
}

#[test]
fn quotas_merge_to_the_freshest_report_per_account_never_summed() {
    let server_f = fixture();
    let viewer = fixture();
    let quota = |account: &str, kind: &str, pct: f64, at: i64| QuotaSnapshot {
        app: "codex".into(),
        account: Some(account.into()),
        captured_at: at,
        window_kind: kind.into(),
        used: None,
        limit_value: None,
        used_percent: Some(pct),
        resets_at: None,
        raw_json: None,
    };
    let now = now_ms();
    // Same account on both: the peer's report is newer.
    viewer
        .store
        .insert_quota(&quota("me@x.com", "weekly", 20.0, now - 60_000))
        .unwrap();
    server_f
        .store
        .insert_quota(&quota("me@x.com", "weekly", 35.0, now - 1_000))
        .unwrap();
    // Only local / only peer.
    viewer
        .store
        .insert_quota(&quota("me@x.com", "5h_block", 50.0, now))
        .unwrap();
    server_f
        .store
        .insert_quota(&quota("other@x.com", "weekly", 70.0, now))
        .unwrap();
    let (_state, id) = pair_and_sync(&server_f, &viewer);
    let merged = super::merged::merged_quotas(&viewer.store).unwrap();
    assert_eq!(merged.len(), 3);
    let me = crate::coordination::opaque("me@x.com");
    let weekly = merged
        .iter()
        .find(|q| q.window_kind == "weekly" && q.account_key.as_deref() == Some(&me))
        .unwrap();
    assert_eq!(
        (weekly.used_percent, weekly.device_id.as_str(), weekly.local),
        (Some(35.0), id.as_str(), false)
    );
    let block = merged.iter().find(|q| q.window_kind == "5h_block").unwrap();
    assert!(block.local && block.used_percent == Some(50.0));
}

#[test]
fn overlapping_sources_are_flagged_not_deduplicated() {
    let server_f = fixture();
    fill(&server_f.store);
    let viewer = fixture();
    // The viewer scans the very same (synced) logs.
    fill(&viewer.store);
    let (_state, id) = pair_and_sync(&server_f, &viewer);
    let warnings = super::merged::overlaps(&viewer.store).unwrap();
    assert!(!warnings.is_empty());
    for w in &warnings {
        assert_eq!(w.code, "possible_shared_source");
        assert!(w.days >= super::merged::OVERLAP_MIN_DAYS);
        assert!([&w.device_a, &w.device_b].contains(&&id));
    }
    // Still counted twice: all devices = local + peer.
    let cube = crate::cube::Cube::build(&viewer.store).unwrap();
    let all = cube
        .overview_parts_for(Range::All, None, None, None)
        .unwrap();
    assert_eq!(
        all.all.events,
        2 * viewer.store.event_count(None, None).unwrap()
    );

    // Distinct data → no warning.
    let other = fixture();
    fill_viewer(&other.store);
    pair_and_sync(&server_f, &other);
    assert!(super::merged::overlaps(&other.store).unwrap().is_empty());
}

#[test]
fn peer_data_is_never_reshared() {
    let server_f = fixture();
    fill(&server_f.store);
    let viewer = fixture();
    fill_viewer(&viewer.store);
    let serve = |r: Request| respond(&viewer.path, &viewer.identity, 1, &r);
    let before: Vec<String> = [
        Request::Days,
        Request::Summary(SummaryQuery::default()),
        Request::Quotas,
    ]
    .into_iter()
    .map(|r| {
        let mut v = serve(r);
        v["captured_at"] = Value::Null;
        v.to_string()
    })
    .collect();
    let (_state, id) = pair_and_sync(&server_f, &viewer);
    assert!(count(&viewer.store, "peer_groups", &id) > 0);
    let after: Vec<String> = [
        Request::Days,
        Request::Summary(SummaryQuery::default()),
        Request::Quotas,
    ]
    .into_iter()
    .map(|r| {
        let mut v = serve(r);
        v["captured_at"] = Value::Null;
        v.to_string()
    })
    .collect();
    assert_eq!(before, after, "the serving path reads local tables only");
    assert!(
        !after[1].contains("claude"),
        "server-only apps never appear"
    );
}

#[test]
fn one_failing_device_does_not_block_the_others() {
    let a = fixture();
    fill(&a.store);
    let b = fixture();
    fill_viewer(&b.store);
    let viewer = fixture();
    let (_sa, id_a) = pair_and_sync(&a, &viewer);
    let (_sb, id_b) = pair_and_sync(&b, &viewer);
    // Device A goes away (dead address, nothing on mDNS).
    Registry::open(&viewer.store)
        .unwrap()
        .set_address(&id_a, "127.0.0.1:9")
        .unwrap();
    let r = sync_all_via(&viewer.store, &quick(), &no_locate()).unwrap();
    let by_id = |id: &str| {
        r.devices
            .iter()
            .find(|d| d.device_id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by_id(&id_a).error_code.as_deref(), Some("connect_failed"));
    assert!(by_id(&id_b).ok);
    let devices = super::devices::list(&viewer.store).unwrap();
    let a_row = devices.iter().find(|d| d.device_id == id_a).unwrap();
    assert_eq!(a_row.last_error_code.as_deref(), Some("connect_failed"));
    assert!(
        devices
            .iter()
            .find(|d| d.device_id == id_b)
            .unwrap()
            .last_error_code
            .is_none()
    );
    // Its earlier data stays until it is forgotten; disabling hides it.
    assert!(count(&viewer.store, "peer_groups", &id_a) > 0);
    Registry::open(&viewer.store)
        .unwrap()
        .set_enabled(&id_a, false)
        .unwrap();
    let cube = crate::cube::Cube::build(&viewer.store).unwrap();
    assert!(!cube.device_ids().contains(&id_a));
    // The schedule hook only lets one sync through per interval.
    assert!(
        sync_if_due(&viewer.store, Duration::from_secs(3600), &quick())
            .unwrap()
            .is_some()
    );
    assert!(
        sync_if_due(&viewer.store, Duration::from_secs(3600), &quick())
            .unwrap()
            .is_none()
    );
}
