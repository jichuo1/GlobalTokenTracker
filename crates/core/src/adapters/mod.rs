//! SourceAdapter — one per AI coding tool (spec §6).
//!
//! Two ingestion shapes:
//! - **Jsonl**: append-only log files; the engine owns byte cursors and calls
//!   `parse_jsonl` with only the NEW byte segment. `dedup_key`s must be stable
//!   across scans (message ids / byte offsets), since a streaming record may be
//!   completed by a later segment — the store's completeness-UPSERT merges them.
//! - **Sqlite**: read-only databases; the adapter keeps its own high-water mark
//!   in `sync_cursors.last_byte_offset` (reused as "max rowid/ts seen").

use crate::model::{QuotaSnapshot, UsageEvent};
use crate::store::Store;
use anyhow::Result;
use std::path::PathBuf;

pub mod antigravity;
pub mod claude;
pub mod cline;
pub mod codebuddy;
pub mod codebuddy_cli;
pub mod codex;
pub mod commandcode;
pub mod coordinator;
pub mod cursor;
pub mod devin;
pub mod dsh;
pub mod freebuff;
pub mod grok;
pub mod kimi_code;
pub mod minimax_code;
pub mod muse_code;
pub mod opencode;
pub mod qoder;
pub mod workbuddy;
pub mod zcode;

/// Coverage tier shown as a badge in the UI sources page (spec §9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Exact per-request token data.
    Precise,
    /// Partial / estimated.
    Estimate,
    /// Metadata only (sessions exist but no billed usage).
    Metadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Jsonl,
    Sqlite,
}

#[derive(Debug, Clone)]
pub struct SourceItem {
    /// Stable cursor key (usually the canonical file path).
    pub key: String,
    pub path: PathBuf,
    pub kind: SourceKind,
}

#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub events: Vec<UsageEvent>,
    pub quotas: Vec<QuotaSnapshot>,
    /// Records deliberately skipped (e.g. ZCode cross-provider rows that other
    /// adapters already count — counted, never silently dropped, spec §6.4).
    pub skipped: u64,
    /// Bytes actually consumed (≤ segment len; tail after last '\n' stays
    /// unconsumed so a half-written line is picked up next round).
    pub consumed: u64,
    /// Adapter-private resume state persisted into `sync_cursors.adapter_state`
    /// (e.g. Codex cumulative counters needed to delta future segments).
    pub new_state: Option<String>,
    pub notes: Vec<String>,
}

pub trait SourceAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn capability(&self) -> Capability;

    /// Files/databases this adapter ingests; empty when the tool is absent.
    fn discover(&self) -> Result<Vec<SourceItem>>;

    /// Top-level directories a live watcher should subscribe to (recursive).
    /// Wider than `discover` parents on purpose: brand-new session files/dirs
    /// must fire events too. May include dirs that don't exist yet — the
    /// watcher filters on `is_dir`.
    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![]
    }

    /// Parse the JSONL segment `data` starting at absolute `from` offset.
    /// `prior_state` is the adapter's resume blob saved on the previous scan.
    fn parse_jsonl(
        &self,
        _item: &SourceItem,
        _from: u64,
        _data: &[u8],
        _prior_state: Option<&str>,
    ) -> Result<ScanOutcome> {
        Ok(ScanOutcome::default())
    }

    /// Scan a SQLite source (adapter-managed high-water mark via `store`).
    fn scan_sqlite(&self, _item: &SourceItem, _store: &Store) -> Result<ScanOutcome> {
        Ok(ScanOutcome::default())
    }
}

/// Global manual-refresh generation. Incremented by [`forget_scan_memos`] so
/// content-fingerprint / file-metadata fast-paths re-check sources on manual
/// refresh (including equal-size, same-timestamp rewrites).
static SCAN_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn scan_generation() -> u64 {
    SCAN_GENERATION.load(std::sync::atomic::Ordering::Relaxed)
}

/// All adapters, ordered by spec priority. P0 set first.
/// Force adapters to recheck content fingerprints on manual refresh.
pub fn forget_scan_memos() {
    SCAN_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    codebuddy::forget_scan_memo();
}

/// Watch roots grouped by adapter id (only directories that currently exist).
/// Used by live file watchers to route filesystem events to only the affected
/// adapter(s) instead of rescanning all 20 adapters on every write.
pub fn watch_roots_by_adapter() -> Vec<(&'static str, Vec<PathBuf>)> {
    registry()
        .into_iter()
        .filter_map(|a| {
            let roots: Vec<PathBuf> = a.watch_roots().into_iter().filter(|r| r.is_dir()).collect();
            (!roots.is_empty()).then_some((a.id(), roots))
        })
        .collect()
}

/// Cheap freshness token for a read-only SQLite source database and its
/// `-wal` sidecar. Uses `(len, mtime_ns)` for both files and additionally
/// reads the 96-byte SQLite header (`change_counter`, `version_valid_for`) and
/// 24-byte WAL salt header only when a file was modified within the last 2 s.
///
/// Skipping `File::open` on quiescent databases avoids dozens of `CreateFileW`
/// syscalls (and AV open-hooks) per idle scan, while recently-modified DBs
/// (active sessions and sub-millisecond unit tests) still get exact header
/// change detection even on equal-size rollback-journal commits.
pub(crate) fn sqlite_change_token(db: &std::path::Path) -> String {
    use std::fmt::Write as _;
    use std::io::Read as _;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    const RECENT_WINDOW: Duration = Duration::from_secs(2);
    let now = SystemTime::now();
    let is_recent = |modified: Option<SystemTime>| -> bool {
        match modified {
            Some(t) => now
                .duration_since(t)
                .map_or(true, |age| age < RECENT_WINDOW),
            None => true,
        }
    };

    let mut out = String::with_capacity(96);
    let _ = write!(out, "sq2:{}", scan_generation());

    if let Ok(m) = std::fs::metadata(db) {
        let modified = m.modified().ok();
        let mtime = modified
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        if is_recent(modified) {
            let mut hdr = [0u8; 96];
            let n = std::fs::File::open(db)
                .and_then(|mut f| f.read(&mut hdr))
                .unwrap_or(0);
            let cc = if n >= 28 {
                u32::from_be_bytes([hdr[24], hdr[25], hdr[26], hdr[27]])
            } else {
                0
            };
            let vvf = if n >= 96 {
                u32::from_be_bytes([hdr[92], hdr[93], hdr[94], hdr[95]])
            } else {
                0
            };
            let _ = write!(out, "|{}:{}:{cc:x}:{vvf:x}", m.len(), mtime);
        } else {
            let _ = write!(out, "|{}:{}", m.len(), mtime);
        }
    } else {
        out.push_str("|none");
    }

    let mut wal = db.as_os_str().to_os_string();
    wal.push("-wal");
    let wal_path = std::path::Path::new(&wal);
    if let Ok(m) = std::fs::metadata(wal_path)
        && m.len() > 0
    {
        let modified = m.modified().ok();
        let mtime = modified
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        if is_recent(modified) {
            let mut hdr = [0u8; 24];
            let n = std::fs::File::open(wal_path)
                .and_then(|mut f| f.read(&mut hdr))
                .unwrap_or(0);
            let (s1, s2) = if n >= 24 {
                (
                    u32::from_be_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]),
                    u32::from_be_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]),
                )
            } else {
                (0, 0)
            };
            let _ = write!(out, "|w:{}:{}:{s1:x}:{s2:x}", m.len(), mtime);
        } else {
            let _ = write!(out, "|w:{}:{}", m.len(), mtime);
        }
    }
    out
}

pub fn registry() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(claude::Claude),
        Box::new(codex::Codex),
        Box::new(devin::Devin),
        Box::new(cursor::Cursor),
        Box::new(qoder::Qoder),
        Box::new(opencode::OpenCode),
        Box::new(zcode::ZCode),
        Box::new(grok::Grok),
        Box::new(workbuddy::WorkBuddy),
        Box::new(codebuddy::CodeBuddyIde::new()),
        Box::new(codebuddy_cli::CodeBuddyCli),
        Box::new(minimax_code::MiniMaxCode),
        Box::new(kimi_code::KimiCode),
        Box::new(cline::Cline),
        Box::new(commandcode::CommandCode),
        Box::new(antigravity::Antigravity),
        Box::new(dsh::Dsh),
        Box::new(freebuff::Freebuff),
        Box::new(muse_code::MuseCode),
        Box::new(coordinator::Coordinator),
    ]
}

/// Split `data` at the last newline: returns (complete segment, consumed len).
/// A file being appended may end mid-line — the incomplete tail is left for
/// the next scan by reporting `consumed` < data.len().
pub(crate) fn complete_lines(data: &[u8]) -> (&[u8], u64) {
    match data.iter().rposition(|&b| b == b'\n') {
        Some(i) => (&data[..=i], (i + 1) as u64),
        None if data.is_empty() => (data, 0),
        // No newline at all: a single still-growing line — consume nothing.
        None => (&data[..0], 0),
    }
}
