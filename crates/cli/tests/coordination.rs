use rusqlite::{Connection, params};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "gtt-coordination-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn db(&self) -> PathBuf {
        self.0.join("ledger.db")
    }
    fn seed(&self) -> Connection {
        std::fs::create_dir_all(&self.0).unwrap();
        let conn = Connection::open(self.db()).unwrap();
        conn.execute_batch("CREATE TABLE sources(source TEXT,enabled INTEGER,last_synced_at INTEGER,last_error TEXT,files_seen INTEGER,rows_ingested INTEGER);
            CREATE TABLE usage_events(app TEXT,provider_id TEXT,model TEXT,ts_start INTEGER,duration_ms INTEGER,ttft_ms INTEGER);
            CREATE TABLE quota_snapshots(id INTEGER PRIMARY KEY,app TEXT,account TEXT,captured_at INTEGER,window_kind TEXT,used REAL,limit_value REAL,used_percent REAL,resets_at INTEGER,raw_json TEXT);").unwrap();
        let now = globaltokentracker_core::store::now_ms();
        conn.execute(
            "INSERT INTO sources VALUES('codex',1,?1,'private-path-and-token',2,3)",
            [now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events VALUES('codex','p','model-a',?1,100,10)",
            [now - 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_events VALUES('zcode','p','model-b',?1,200,20)",
            [now - 1],
        )
        .unwrap();
        conn.execute("INSERT INTO quota_snapshots VALUES(1,'codex','private-account',?1,'daily',1,10,10,NULL,'private-raw-token')",[now]).unwrap();
        conn
    }
    fn call(&self, args: &[&str]) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_globaltokentracker-cli"))
            .arg("--db")
            .arg(self.db())
            .arg("coordination")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stderr.is_empty());
        serde_json::from_slice(&out.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.0.is_dir() {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

#[test]
fn capabilities_and_missing_snapshot_have_no_creation_side_effects() {
    let f = Fixture::new();
    let c = f.call(&["capabilities"]);
    assert_eq!(c["protocol_version"], 1);
    assert_eq!(c["features"]["implicit_refresh"], false);
    assert!(c["sources"].as_array().unwrap().len() >= 16);
    assert_eq!(f.call(&["snapshot"])["error"]["code"], "ledger_missing");
    assert!(!f.0.exists());
}

#[test]
fn version_negotiation_rejects_before_opening_database() {
    let f = Fixture::new();
    assert_eq!(
        f.call(&["snapshot", "--protocol-version", "2"])["error"]["code"],
        "unsupported_protocol_version"
    );
    assert!(!f.0.exists());
}

#[test]
fn read_only_snapshot_is_sanitized_and_source_filtered() {
    let f = Fixture::new();
    let conn = f.seed();
    drop(conn);
    let before = std::fs::read(f.db()).unwrap();
    let s = f.call(&["snapshot", "--source", "codex", "--format", "json"]);
    assert_eq!(s["status"], "ok");
    assert_eq!(s["models"].as_array().unwrap().len(), 1);
    assert_eq!(s["quotas"][0]["remaining_percent"], 90.0);
    assert_eq!(s["sources"][0]["has_error"], true);
    let text = s.to_string();
    for secret in [
        "private-account",
        "private-raw-token",
        "private-path-and-token",
    ] {
        assert!(!text.contains(secret));
    }
    assert_eq!(std::fs::read(f.db()).unwrap(), before);
    let conn = Connection::open(f.db()).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='schema_migrations'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn old_or_empty_schema_returns_explicit_partial_or_unavailable() {
    let f = Fixture::new();
    std::fs::create_dir_all(&f.0).unwrap();
    let conn = Connection::open(f.db()).unwrap();
    assert_eq!(f.call(&["snapshot"])["status"], "unavailable");
    conn.execute_batch(
        "CREATE TABLE sources(source,enabled,last_synced_at,last_error,files_seen,rows_ingested);",
    )
    .unwrap();
    let s = f.call(&["snapshot"]);
    assert_eq!(s["status"], "partial");
    assert!(
        s["warnings"]
            .as_array()
            .unwrap()
            .contains(&Value::from("models_schema_unavailable"))
    );
}

#[test]
fn bounded_results_report_truncation_and_invalid_filter_is_rejected() {
    let f = Fixture::new();
    let conn = f.seed();
    assert_eq!(
        f.call(&["snapshot", "--model-limit", "1"])["truncated"][0],
        "models"
    );
    assert_eq!(
        f.call(&["snapshot", "--model-limit", "0"])["error"]["code"],
        "invalid_options"
    );
    assert_eq!(
        f.call(&["snapshot", "--source", "../arbitrary"])["error"]["code"],
        "unknown_source"
    );
    drop(conn);
}

#[test]
fn quota_semantics_fail_closed_for_age_clock_reset_and_non_allowance() {
    let f = Fixture::new();
    let conn = f.seed();
    let now = globaltokentracker_core::store::now_ms();
    let cases = [
        ("weekly", now - 600_000, Some(20.0), None),
        ("monthly", now + 600_000, Some(20.0), None),
        ("api_pool", now, Some(20.0), Some(now - 1)),
        ("auto_pool", now, Some(101.0), None),
        ("credits", now, None, None),
        ("session_ctx", now, Some(20.0), None),
    ];
    for (i, (window, captured, pct, reset)) in cases.iter().enumerate() {
        conn.execute(
            "INSERT INTO quota_snapshots VALUES(?1,'codex',NULL,?2,?3,9,10,?4,?5,NULL)",
            params![10 + i as i64, captured, window, pct, reset],
        )
        .unwrap();
    }
    let s = f.call(&["snapshot"]);
    for q in s["quotas"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|q| q["window_kind"] != "daily")
    {
        assert!(q["remaining_percent"].is_null(), "{q}");
    }
    let credits = s["quotas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["window_kind"] == "credits")
        .unwrap();
    assert_eq!(credits["remaining_balance"], 9.0);
    drop(conn);
}

#[test]
fn latest_quota_tie_uses_id_and_dangerous_identifiers_are_redacted() {
    let f = Fixture::new();
    let conn = f.seed();
    let now = globaltokentracker_core::store::now_ms();
    conn.execute("INSERT INTO quota_snapshots VALUES(2,'codex','private-account',?1,'daily',2,10,20,NULL,'raw')",[now-1]).unwrap();
    conn.execute("UPDATE quota_snapshots SET captured_at=?1", [now - 1])
        .unwrap();
    conn.execute("INSERT INTO usage_events VALUES('codex','https://secret.example/key','C:/private/config',?1,10,1)",[now-1]).unwrap();
    let s = f.call(&["snapshot"]);
    assert_eq!(s["quotas"][0]["remaining_percent"], 80.0);
    assert!(!s.to_string().contains("secret.example"));
    assert!(!s.to_string().contains("private/config"));
    assert_eq!(s["status"], "partial");
    drop(conn);
}

#[test]
fn locked_database_returns_machine_error_without_raw_diagnostics() {
    let f = Fixture::new();
    let conn = f.seed();
    conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
    assert_eq!(f.call(&["snapshot"])["error"]["code"], "ledger_busy");
    conn.execute_batch("ROLLBACK").unwrap();
    drop(conn);
}

#[test]
fn refresh_requires_scope_and_unknown_scope_does_not_create_database() {
    let f = Fixture::new();
    assert_eq!(
        f.call(&["refresh", "--source", "unknown"])["error"]["code"],
        "invalid_refresh_scope"
    );
    assert!(!f.0.exists());
    let out = Command::new(env!("CARGO_BIN_EXE_globaltokentracker-cli"))
        .args(["coordination", "refresh"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn disabled_refresh_never_discovers_files_or_polls_and_is_throttled() {
    let f = Fixture::new();
    let store = globaltokentracker_core::Store::open(&f.db()).unwrap();
    drop(store);
    let conn = Connection::open(f.db()).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO sources(source,enabled) VALUES('codex',0)",
        [],
    )
    .unwrap();
    let r = f.call(&["refresh", "--source", "codex", "--quota"]);
    assert_eq!(r["sources"][0]["status"], "disabled");
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM usage_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        f.call(&["refresh", "--source", "codex"])["error"]["code"],
        "refresh_throttled"
    );
    drop(conn);
}

#[test]
fn lock_holder_process_fixture() {
    if let Ok(path) = std::env::var("GTT_TEST_LOCK_PATH") {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        file.try_lock().unwrap();
        std::fs::write(format!("{path}.ready"), b"ready").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(20));
        drop(file);
    }
}

#[test]
fn cross_process_lock_survives_expired_metadata_and_releases_after_process_death() {
    let f = Fixture::new();
    let store = globaltokentracker_core::Store::open(&f.db()).unwrap();
    drop(store);
    let conn = Connection::open(f.db()).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO sources(source,enabled) VALUES('codex',0)",
        [],
    )
    .unwrap();
    let lock = f
        .db()
        .canonicalize()
        .unwrap()
        .with_extension("coordination-refresh.lock");
    let ready = PathBuf::from(format!("{}.ready", lock.display()));
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_holder_process_fixture", "--nocapture"])
        .env("GTT_TEST_LOCK_PATH", &lock)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !ready.exists() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let is_ready = ready.exists();
    let result = if is_ready {
        Some(f.call(&["refresh", "--source", "codex"]))
    } else {
        None
    };
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(is_ready, "lock holder failed to start");
    assert_eq!(result.unwrap()["error"]["code"], "refresh_busy");
    assert_eq!(
        f.call(&["refresh", "--source", "codex"])["sources"][0]["status"],
        "disabled"
    );
    drop(conn);
}

#[test]
fn refresh_does_not_replace_a_corrupt_ledger_with_an_existing_backup() {
    let f = Fixture::new();
    let store = globaltokentracker_core::Store::open(&f.db()).unwrap();
    store.maybe_backup().unwrap();
    drop(store);
    let corrupt = b"deliberately-invalid-ledger";
    std::fs::write(f.db(), corrupt).unwrap();
    assert_eq!(
        f.call(&["refresh", "--source", "codex"])["error"]["code"],
        "refresh_failed"
    );
    assert_eq!(std::fs::read(f.db()).unwrap(), corrupt);
}
