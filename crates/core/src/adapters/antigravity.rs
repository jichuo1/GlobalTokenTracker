//! Antigravity adapter — Google's agentic coding tool in all its shells: the
//! Antigravity 2.0 app, the IDE extension and the `agy` CLI. They share one
//! on-disk format, so one adapter covers them.
//!
//! ## Where the data is
//!
//! Every conversation is its own SQLite database, `<uuid>.db`, in any of
//! `<base>/antigravity-cli/conversations/`, `<base>/antigravity/conversations/`,
//! `<base>/antigravity-ide/conversations/` and `<base>/antigravity/`, where
//! `<base>` is `~/.gemini` (or `$GEMINI_CLI_HOME/.gemini`). Older IDE builds
//! stored opaque `.pb` files instead; those carry no readable usage and are
//! ignored. The `conversation_summaries.db` that 1.2.x drops beside them is an
//! index, not a conversation (it has no `gen_metadata`) and is skipped.
//!
//! ## What is in a conversation database
//!
//! Not documented by Google — the layout below was reverse-engineered by the
//! community (tokscale `antigravity_cli.rs`, CodexBar `docs/antigravity.md`,
//! tokscale issue #1184 / PR #1327 / issue #1610) and verified directly against
//! real local Antigravity 2.0 / CLI databases and `transcript.jsonl`; field
//! numbers are protobuf tags.
//!
//! - `gen_metadata(idx, data, size)` — one row per model generation, `data` is
//!   a protobuf. `#1` = chat model message:
//!   - `#3` protobuf `Model` enum integer (same integer repeated in `#4.#1`).
//!   - `#4` usage: `#1` protobuf `Model` enum integer (NOT tokens — e.g. `1318`
//!     for `MODEL_PLACEHOLDER_M318`, `1535` for `MODEL_PLACEHOLDER_M535`; early
//!     community notes mistook `1132` = `M132` for fixed system-prompt tokens);
//!     `#2` newly processed (non-cached) input tokens; `#3` total output tokens
//!     (`#9 + #10`); `#5` cache read; `#9` thinking/reasoning tokens; `#10`
//!     visible response / tool-call output tokens; `#11` the response id.
//!   - `#11` `time_to_first_token` (`google.protobuf.Duration` `{seconds, nanos}`):
//!     client-measured TTFT from request start to first streamed chunk.
//!   - `#12` `streaming_duration` (`google.protobuf.Duration` `{seconds, nanos}`):
//!     client-measured streaming duration from first chunk to completion;
//!     `#11 + #12` equals the total call duration (`steps.#7 - steps.#1`).
//!   - `#19` machine model id (`gemini-3-flash-a`, `gemini-4-neon`, …;
//!     sometimes absent or the routing label `gemini-default`), `#20` key-value
//!     metadata map (repeated `{#1 key, #2 value}`, including `model_enum`),
//!     `#21` display label (`Gemini 3.5 Flash (High)`).
//!   - `#9.#4` a `{seconds, nanos}` timestamp — agy ≤ 1.1.17 only.
//! - `steps(idx, step_type, metadata)` — for `step_type = 15` (model turns)
//!   `metadata.#1` is the wall-clock `created_at` `{seconds, nanos}`, `#6` is
//!   `first_token_at`, `#7` (echoed in `#8`/`#32`) is `completed_at`; `#9.#11`
//!   repeats the response id and `#20.#3` is the `gen_metadata.idx`. agy ≥
//!   1.1.18 dates generations *only* here.
//! - `trajectory_metadata_blob(id, data)` — `#2` session created-at,
//!   `#1.#1` the workspace as a `file://` URI.
//!
//! ## Mapping
//!
//! `input = #2` (already excludes cache); `output = #3` (falling back to
//! `#9 + #10` when `#3` is absent) because the provider bills thinking as
//! output and the price maths only multiplies `output_tokens`;
//! `reasoning = #9` is the informational subset (same convention as
//! Codex/Claude). Antigravity records no cache-write figure.
//! `duration_ms = #11 + #12` and `ttft_ms = #11` come directly from
//! `gen_metadata` (measured); when those duration fields are absent, `ts_end`
//! from `steps.#7` (and `ttft_ms` from `steps.#6 - steps.#1`) let the store
//! derive bounded timestamp-estimated speed instead.
//!
//! Events are deduplicated on the response id alone, not per file: `/fork`
//! and importing an IDE conversation into the CLI copy the earlier
//! generations into a new database, and those tokens were spent once. The
//! conversation that reported a turn first keeps it (later copies are skipped
//! rather than re-written), so attribution stays put across rescans.
//!
//! ## Freshness
//!
//! Each database is re-read whenever its size/mtime (or its `-wal`'s)
//! changed; unchanged files cost two `stat`s. Reads are read-only; a WAL
//! database left without sidecars by a clean close is retried `immutable=1`.

use super::{Capability, ScanOutcome, SourceAdapter, SourceItem, SourceKind};
use crate::model::{Provenance, UsageEvent, apps};
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::ValueRef};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct Antigravity;

/// Index database next to the conversations — never holds usage.
const SUMMARIES_DB: &str = "conversation_summaries.db";
/// The router's placeholder for "some Gemini"; names no concrete model.
const ROUTING_LABEL: &str = "gemini-default";
/// Earliest plausible turn time (Antigravity did not exist before).
const MIN_TS_MS: i64 = 1_577_836_800_000; // 2020-01-01
/// Tolerated clock skew for a time read from disk.
const SKEW_MS: i64 = 60 * 60 * 1000;
/// Maximum plausible measured duration for a single model call (24 hours),
/// guarding against negative twos-complement varints or corrupt blobs.
const MAX_DURATION_SECS: u64 = 24 * 60 * 60;

impl SourceAdapter for Antigravity {
    fn id(&self) -> &'static str {
        apps::GEMINI_ANTIGRAVITY
    }
    fn display_name(&self) -> &'static str {
        "Antigravity"
    }
    fn capability(&self) -> Capability {
        Capability::Precise
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        watch_roots_under(&base_dir())
    }

    fn discover(&self) -> Result<Vec<SourceItem>> {
        Ok(discover_in(&base_dir()))
    }

    fn scan_sqlite(&self, item: &SourceItem, store: &Store) -> Result<ScanOutcome> {
        let fp = super::sqlite_change_token(&item.path);
        if let Some(meta) = store.load_cursor_meta(&item.key)?
            && meta.fingerprint.as_deref() == Some(fp.as_str())
        {
            return Ok(ScanOutcome::default());
        }
        let cur = store.load_cursor(&item.key)?;
        let scan_gen = super::scan_generation();
        let resume = cur
            .state
            .as_deref()
            .and_then(|s| parse_cursor_state(s, scan_gen));
        let conn = open_db(&item.path)?;
        let (out, rows, max_gen_idx, max_step_idx) = read_conversation(&conn, item, store, resume)?;
        // Fingerprint taken BEFORE the read: a write racing the read moves the
        // file past it, so the next pass simply reads again.
        let state = format!("v2:{scan_gen}:{max_gen_idx}:{max_step_idx}");
        store.save_cursor_opaque(self.id(), &item.key, rows, Some(&fp), 0, Some(&state))?;
        Ok(out)
    }
}

fn parse_cursor_state(s: &str, expected_gen: u64) -> Option<(i64, i64)> {
    let mut parts = s.split(':');
    if parts.next()? != "v2" {
        return None;
    }
    let gen_id: u64 = parts.next()?.parse().ok()?;
    if gen_id != expected_gen {
        return None;
    }
    let max_gen: i64 = parts.next()?.parse().ok()?;
    let max_step: i64 = parts.next()?.parse().ok()?;
    (max_gen >= 0).then_some((max_gen, max_step.max(0)))
}

// ------------------------------------------------------------------ discovery

/// `~/.gemini`, or `$GEMINI_CLI_HOME/.gemini` when that override is set (the
/// Gemini CLI's own convention — Antigravity shares the directory).
fn base_dir() -> PathBuf {
    let env = std::env::var("GEMINI_CLI_HOME").ok();
    base_dir_from(env.as_deref(), &dirs::home_dir().unwrap_or_default())
}

fn base_dir_from(gemini_cli_home: Option<&str>, home: &Path) -> PathBuf {
    match gemini_cli_home.map(str::trim).filter(|s| !s.is_empty()) {
        Some(h) => Path::new(h).join(".gemini"),
        None => home.join(".gemini"),
    }
}

fn roots_under(base: &Path) -> Vec<PathBuf> {
    vec![
        base.join("antigravity-cli").join("conversations"),
        base.join("antigravity").join("conversations"),
        base.join("antigravity-ide").join("conversations"),
        base.join("antigravity"),
    ]
}

/// Live-watch directories: subscribe only to `conversations/` subdirectories
/// unless legacy direct `<uuid>.db` files actually live at `<base>/antigravity`.
/// Watching `<base>/antigravity` recursively when unused floods the watcher
/// with `brain/`, `presence/`, `daemon/`, `persistent_terminals/`, and
/// `conversation_summaries.db-wal` chatter.
fn watch_roots_under(base: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        base.join("antigravity-cli").join("conversations"),
        base.join("antigravity").join("conversations"),
        base.join("antigravity-ide").join("conversations"),
    ];
    let legacy = base.join("antigravity");
    let has_legacy_db = std::fs::read_dir(&legacy).is_ok_and(|rd| {
        rd.flatten().any(|e| {
            let p = e.path();
            p.extension().is_some_and(|x| x.eq_ignore_ascii_case("db"))
                && p.file_name().is_none_or(|n| n != SUMMARIES_DB)
                && p.is_file()
        })
    });
    if has_legacy_db {
        roots.push(legacy);
    }
    roots
}

/// Immediate `*.db` entries of every root, deduplicated and sorted.
fn discover_in(base: &Path) -> Vec<SourceItem> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for root in roots_under(base) {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let is_db = p.extension().is_some_and(|x| x.eq_ignore_ascii_case("db"));
            let is_index = p.file_name().is_some_and(|n| n == SUMMARIES_DB);
            if !is_db || is_index || !p.is_file() {
                continue;
            }
            let key = p.to_string_lossy().to_string();
            if seen.insert(key.clone()) {
                items.push(SourceItem {
                    key,
                    path: p,
                    kind: SourceKind::Sqlite,
                });
            }
        }
    }
    items.sort_by(|a, b| a.key.cmp(&b.key));
    items
}

// -------------------------------------------------------------------- opening

fn open_with(path: &Path, immutable: bool) -> rusqlite::Result<Connection> {
    let p = path.to_string_lossy().replace('\\', "/");
    let uri = if immutable {
        format!("file:{p}?immutable=1")
    } else {
        format!("file:{p}?mode=ro")
    };
    let conn = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(1500))?;
    // Force the header/WAL to be read now so an unusable open fails here.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })?;
    Ok(conn)
}

/// A cleanly closed WAL database has no `-wal`/`-shm`; a read-only handle
/// cannot create them and SQLite then refuses to open it. With no live WAL
/// there is nothing to miss, so an immutable open is safe. A non-empty `-wal`
/// means a writer is active — the error is real and is reported.
fn open_db(path: &Path) -> Result<Connection> {
    match open_with(path, false) {
        Ok(c) => Ok(c),
        Err(first) => {
            let mut wal = path.as_os_str().to_os_string();
            wal.push("-wal");
            let live_wal = std::fs::metadata(Path::new(&wal)).is_ok_and(|m| m.len() > 0);
            if live_wal {
                return Err(first.into());
            }
            open_with(path, true).map_err(|_| first.into())
        }
    }
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

/// A BLOB (or a TEXT holding raw bytes) column as bytes.
fn bytes_of(v: ValueRef<'_>) -> Option<Vec<u8>> {
    match v {
        ValueRef::Blob(b) | ValueRef::Text(b) => Some(b.to_vec()),
        _ => None,
    }
}

// ------------------------------------------------------------------- protobuf

/// Minimal protobuf wire reader — enough for "first occurrence of field N".
enum Wire<'a> {
    Varint(u64),
    Len(&'a [u8]),
    Skip,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut out = 0u64;
        let mut shift = 0u32;
        loop {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            out |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(out);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn advance(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len())?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Some(s)
    }

    /// Next `(field, value)`, or `None` at the end / on malformed input (never
    /// resyncs: a corrupt blob yields what precedes the damage).
    fn next_field(&mut self) -> Option<(u64, Wire<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.varint()?;
        let wire = match tag & 7 {
            0 => Wire::Varint(self.varint()?),
            1 => {
                self.advance(8)?;
                Wire::Skip
            }
            2 => {
                let n = usize::try_from(self.varint()?).ok()?;
                Wire::Len(self.advance(n)?)
            }
            5 => {
                self.advance(4)?;
                Wire::Skip
            }
            _ => return None,
        };
        Some((tag >> 3, wire))
    }
}

fn msg(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut r = Reader::new(buf);
    while let Some((f, w)) = r.next_field() {
        if let (true, Wire::Len(b)) = (f == field, w) {
            return Some(b);
        }
    }
    None
}

fn varint(buf: &[u8], field: u64) -> Option<u64> {
    let mut r = Reader::new(buf);
    while let Some((f, w)) = r.next_field() {
        if let (true, Wire::Varint(v)) = (f == field, w) {
            return Some(v);
        }
    }
    None
}

fn string(buf: &[u8], field: u64) -> Option<&str> {
    msg(buf, field).and_then(|b| std::str::from_utf8(b).ok())
}

/// A string field, blank counted as absent.
fn text(buf: &[u8], field: u64) -> Option<&str> {
    string(buf, field).map(str::trim).filter(|s| !s.is_empty())
}

/// `{#1 seconds, #2 nanos}` → epoch ms; out-of-range nanos mean "malformed".
fn timestamp_ms(buf: &[u8]) -> Option<i64> {
    let secs = i64::try_from(varint(buf, 1)?).ok()?;
    let nanos = i64::try_from(varint(buf, 2).unwrap_or(0)).ok()?;
    if !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    secs.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

/// `google.protobuf.Duration` (`{#1 seconds, #2 nanos}`) → nanoseconds.
/// Proto3 omits zero fields, so an empty buffer encodes 0 ns. Malformed wire
/// bytes, out-of-range nanos, or implausible seconds return `None`.
fn duration_ns(buf: &[u8]) -> Option<u64> {
    let mut secs = 0u64;
    let mut nanos = 0u64;
    let mut r = Reader::new(buf);
    while r.pos < r.buf.len() {
        let (f, w) = r.next_field()?;
        match (f, w) {
            (1, Wire::Varint(v)) => secs = v,
            (2, Wire::Varint(v)) => nanos = v,
            _ => {}
        }
    }
    if secs > MAX_DURATION_SECS || nanos >= 1_000_000_000 {
        return None;
    }
    secs.checked_mul(1_000_000_000)?.checked_add(nanos)
}

/// Convert a positive nanosecond duration to rounded milliseconds (at least 1 ms
/// when non-zero; `None` when zero).
fn ns_to_ms(ns: u64) -> Option<i64> {
    if ns == 0 {
        return None;
    }
    let ms = (ns.saturating_add(500_000) / 1_000_000).max(1);
    i64::try_from(ms).ok()
}

/// Look up `key` inside a repeated protobuf map field `{#1 key, #2 value}`.
fn kv_text<'a>(buf: &'a [u8], field: u64, key: &str) -> Option<&'a str> {
    let mut r = Reader::new(buf);
    while let Some((f, w)) = r.next_field() {
        if let (true, Wire::Len(entry)) = (f == field, w)
            && string(entry, 1) == Some(key)
        {
            return text(entry, 2);
        }
    }
    None
}

// --------------------------------------------------------------------- models

/// Antigravity machine id → the price-book key of the model it serves.
///
/// Ids are opaque and were mapped by the community from the app's own model
/// registry (Antigravity Context Window Monitor `models.ts`, via tokscale's
/// alias table). Display labels are deliberately not used as keys — they are
/// renamed and localized server-side. Tier suffixes (`-high`/`-low`/`-medium`,
/// `-thinking`) share one price, so they collapse into the base model.
fn canonical_model(raw: &str) -> Option<&'static str> {
    Some(match raw.trim().to_ascii_lowercase().as_str() {
        "gemini-pro-default"
        | "gemini-pro-agent"
        | "gemini-3.1-pro-high"
        | "gemini-3.1-pro-low"
        | "model_placeholder_m16"
        | "model_placeholder_m36"
        | "model_placeholder_m37" => "gemini-3.1-pro",
        "gemini-3-pro-high" | "gemini-3-pro-low" => "gemini-3-pro",
        // The 4.x family:
        "gemini-4-neon"
        | "gemini-4-neon-xhigh"
        | "gemini-4-neon-high"
        | "gemini-4-neon-medium"
        | "gemini-4-neon-low"
        | "model_placeholder_m535" => "gemini-4-neon",
        "gemini-4-carbon"
        | "gemini-4-carbon-xhigh"
        | "gemini-4-carbon-high"
        | "gemini-4-carbon-medium"
        | "gemini-4-carbon-low"
        | "model_placeholder_m427" => "gemini-4-carbon",
        "gemini-4-hydrogen"
        | "gemini-4-hydrogen-xhigh"
        | "gemini-4-hydrogen-high"
        | "gemini-4-hydrogen-medium"
        | "gemini-4-hydrogen-low"
        | "model_placeholder_m528" => "gemini-4-hydrogen",
        // The 3.8 Flash family:
        "gemini-3.8-flash"
        | "gemini-3.8-flash-n"
        | "gemini-3.8-flash-high"
        | "gemini-3.8-flash-medium"
        | "gemini-3.8-flash-low"
        | "model_placeholder_m318"
        | "model_placeholder_m319"
        | "model_placeholder_m320" => "gemini-3.8-flash",
        // The 3.7 Flash family:
        "gemini-3.7-flash"
        | "gemini-3.7-flash-high"
        | "gemini-3.7-flash-medium"
        | "gemini-3.7-flash-low"
        | "gemini-3.7-flash-thinking" => "gemini-3.7-flash",
        // The 3.6 Flash family:
        "gemini-3.6-flash"
        | "gemini-3.6-flash-high"
        | "gemini-3.6-flash-medium"
        | "gemini-3.6-flash-low" => "gemini-3.6-flash",
        // The 3.5 Flash family: the `-a`/`-b`/`-agent` ids are retired
        // predecessors of "Gemini 3.5 Flash (High)".
        "gemini-3-flash-a"
        | "gemini-3-flash-b"
        | "gemini-3-flash-agent"
        | "gemini-3.5-flash-high"
        | "gemini-3.5-flash-medium"
        | "gemini-3.5-flash-low"
        | "gemini-3.5-flash-extra-low"
        | "model_placeholder_m20"
        | "model_placeholder_m132"
        | "model_placeholder_m133"
        | "model_placeholder_m187" => "gemini-3.5-flash",
        "gemini-3-flash"
        | "gemini-3-flash-c"
        | "model_placeholder_m18"
        | "model_placeholder_m47"
        | "model_placeholder_m84" => "gemini-3-flash-preview",
        // The 3.1 Flash / Lite family:
        "gemini-3.1-flash" | "gemini-3.1-flash-lite" => "gemini-3.1-flash-lite",
        // The 2.5 Flash / Pro family:
        "gemini-2.5-flash" | "gemini-2.5-flash-lite" => "gemini-2.5-flash",
        "gemini-2.5-pro" => "gemini-2.5-pro",
        "claude-opus-4-6-thinking" | "claude-opus-4.6-thinking" | "model_placeholder_m26" => {
            "claude-opus-4-6"
        }
        "claude-sonnet-4-6-thinking" | "claude-sonnet-4.6-thinking" | "model_placeholder_m35" => {
            "claude-sonnet-4-6"
        }
        "gpt-oss-120b-medium" | "model_openai_gpt_oss_120b_medium" => "gpt-oss-120b",
        _ => return None,
    })
}

/// Display label → model, for rows that carry neither a usable `#19` nor a
/// sibling to borrow one from. Only labels seen in real databases.
fn label_model(label: &str) -> Option<&'static str> {
    match label.trim() {
        "Gemini 4 Neon (Low)"
        | "Gemini 4 Neon (Medium)"
        | "Gemini 4 Neon (High)"
        | "Gemini 4 Neon (XHigh)" => Some("gemini-4-neon"),
        "Gemini 4 Carbon (Low)"
        | "Gemini 4 Carbon (Medium)"
        | "Gemini 4 Carbon (High)"
        | "Gemini 4 Carbon (XHigh)" => Some("gemini-4-carbon"),
        "Gemini 4 Hydrogen (Low)"
        | "Gemini 4 Hydrogen (Medium)"
        | "Gemini 4 Hydrogen (High)"
        | "Gemini 4 Hydrogen (XHigh)" => Some("gemini-4-hydrogen"),
        "Gemini 3.8 Flash (Low)" | "Gemini 3.8 Flash (Medium)" | "Gemini 3.8 Flash (High)" => {
            Some("gemini-3.8-flash")
        }
        "Gemini 3.7 Flash (Low)" | "Gemini 3.7 Flash (Medium)" | "Gemini 3.7 Flash (High)" => {
            Some("gemini-3.7-flash")
        }
        "Gemini 3.6 Flash (Low)" | "Gemini 3.6 Flash (Medium)" | "Gemini 3.6 Flash (High)" => {
            Some("gemini-3.6-flash")
        }
        "Gemini 3.5 Flash (Low)" | "Gemini 3.5 Flash (Medium)" | "Gemini 3.5 Flash (High)" => {
            Some("gemini-3.5-flash")
        }
        "Gemini 3.1 Pro (High)" | "Gemini 3.1 Pro (Low)" => Some("gemini-3.1-pro"),
        _ => None,
    }
}

fn is_routing(model: &str) -> bool {
    model.trim().eq_ignore_ascii_case(ROUTING_LABEL)
}

fn provider_of(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    if m.contains("gemini") {
        Some("google")
    } else if m.contains("claude") {
        Some("anthropic")
    } else if m.contains("gpt-oss") {
        Some("openai")
    } else {
        None
    }
}

/// Model attribution recovered from the conversation as a whole, for rows whose
/// `#19` is missing (continuation/tool turns) or only the routing label.
#[derive(Default)]
struct SessionModels {
    /// `#21` label → the concrete id seen next to it. A label seen with two
    /// *different* models is dropped: ambiguous evidence is no evidence.
    by_label: HashMap<String, String>,
    /// The one concrete id of the whole conversation — only when every label
    /// in it was identified by some row (otherwise an unlabelled row could
    /// silently inherit the wrong model after a model switch).
    sole: Option<String>,
}

impl SessionModels {
    fn from_rows<'a>(blobs: impl Iterator<Item = &'a [u8]>) -> Self {
        let mut by_label: HashMap<&str, Option<&str>> = HashMap::new();
        let mut distinct: HashSet<&str> = HashSet::new();
        let mut unresolved: Vec<&str> = Vec::new();
        for blob in blobs {
            let Some(chat) = msg(blob, 1) else { continue };
            let label = text(chat, 21);
            let concrete = text(chat, 19).filter(|m| !is_routing(m));
            let Some(model) = concrete else {
                unresolved.extend(label);
                continue;
            };
            distinct.insert(model);
            if let Some(label) = label {
                by_label
                    .entry(label)
                    .and_modify(|seen| {
                        if let Some(prev) = *seen {
                            let same = prev == model
                                || canonical_model(prev).unwrap_or(prev)
                                    == canonical_model(model).unwrap_or(model);
                            if !same {
                                *seen = None;
                            }
                        }
                    })
                    .or_insert(Some(model));
            }
        }
        let by_label: HashMap<String, String> = by_label
            .into_iter()
            .filter_map(|(l, m)| Some((l.to_string(), m?.to_string())))
            .collect();
        let all_labels_known = unresolved.iter().all(|l| by_label.contains_key(*l));
        let sole = match (distinct.len(), all_labels_known) {
            (1, true) => distinct.iter().next().map(|m| (*m).to_string()),
            _ => None,
        };
        Self { by_label, sole }
    }

    fn recover(&self, chat: &[u8]) -> Option<&str> {
        match text(chat, 21) {
            // A label never seen beside a concrete id is positive evidence of
            // a model this file never names — do not fall through to a guess.
            Some(label) => self.by_label.get(label).map(String::as_str),
            None => self.sole.as_deref(),
        }
    }
}

// -------------------------------------------------------------------- reading

/// Session-level facts from `trajectory_metadata_blob`.
struct Trajectory {
    created_ms: Option<i64>,
    workspace: Option<String>,
}

fn read_trajectory(conn: &Connection) -> Trajectory {
    let blob = has_table(conn, "trajectory_metadata_blob")
        .then(|| {
            conn.query_row(
                "SELECT data FROM trajectory_metadata_blob LIMIT 1",
                [],
                |r| Ok(bytes_of(r.get_ref(0)?)),
            )
            .ok()
            .flatten()
        })
        .flatten();
    let Some(blob) = blob else {
        return Trajectory {
            created_ms: None,
            workspace: None,
        };
    };
    Trajectory {
        created_ms: msg(&blob, 2).and_then(timestamp_ms).filter(|&ms| ms > 0),
        workspace: msg(&blob, 1)
            .and_then(|folder| string(folder, 1))
            .and_then(file_uri_to_path),
    }
}

/// Wall-clock timestamps of a model turn from `steps(step_type = 15)`:
/// `#1` created_at, `#6` first_token_at, `#7` (or `#8`/`#32`) completed_at.
#[derive(Clone, Copy)]
struct StepTiming {
    start_ms: i64,
    first_token_ms: Option<i64>,
    end_ms: Option<i64>,
}

/// Wall-clock times of model turns, keyed by response id and by generation
/// index. agy ≥ 1.1.18 keeps them nowhere else.
#[derive(Default)]
struct StepTimes {
    by_response: HashMap<String, StepTiming>,
    by_gen_idx: HashMap<i64, StepTiming>,
    max_idx: i64,
}

fn read_step_times(conn: &Connection, min_idx: Option<i64>) -> StepTimes {
    let mut out = StepTimes::default();
    if !has_table(conn, "steps") {
        return out;
    }
    let sql = if min_idx.is_some() {
        "SELECT idx, metadata FROM steps WHERE step_type = 15 AND metadata IS NOT NULL AND idx >= ?1"
    } else {
        "SELECT idx, metadata FROM steps WHERE step_type = 15 AND metadata IS NOT NULL"
    };
    let Ok(mut st) = conn.prepare(sql) else {
        return out;
    };
    let parsed: Vec<(Option<i64>, Option<Vec<u8>>)> = match min_idx {
        Some(m) => st
            .query_map([m], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, bytes_of(r.get_ref(1)?)))
            })
            .ok()
            .into_iter()
            .flat_map(|it| it.flatten())
            .collect(),
        None => st
            .query_map([], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, bytes_of(r.get_ref(1)?)))
            })
            .ok()
            .into_iter()
            .flat_map(|it| it.flatten())
            .collect(),
    };
    for (step_idx, blob) in parsed {
        if let Some(i) = step_idx {
            out.max_idx = out.max_idx.max(i);
        }
        let Some(blob) = blob else { continue };
        let Some(start_ms) = msg(&blob, 1).and_then(timestamp_ms).filter(|&m| m > 0) else {
            continue;
        };
        let first_token_ms = msg(&blob, 6)
            .and_then(timestamp_ms)
            .filter(|&m| m >= start_ms);
        let end_ms = msg(&blob, 7)
            .or_else(|| msg(&blob, 8))
            .or_else(|| msg(&blob, 32))
            .and_then(timestamp_ms)
            .filter(|&m| m >= start_ms);
        let timing = StepTiming {
            start_ms,
            first_token_ms,
            end_ms,
        };
        if let Some(id) = msg(&blob, 9).and_then(|m| text(m, 11)) {
            out.by_response.entry(id.to_string()).or_insert(timing);
        }
        if let Some(idx) = msg(&blob, 20)
            .and_then(|m| varint(m, 3))
            .and_then(|i| i64::try_from(i).ok())
        {
            out.by_gen_idx.entry(idx).or_insert(timing);
        }
    }
    out
}

fn file_mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(0))
}

/// `file:///C:/x` → `C:\x`; `file:///home/x` → `/home/x`; `file://host/s/x`
/// → `\\host\s\x`. Percent escapes are decoded (CJK paths arrive encoded).
/// `file://localhost/…` is the explicit-host spelling of a local path
/// (RFC 8089; macOS APIs emit it) — never a UNC share.
fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest
        .strip_prefix("localhost")
        .filter(|r| r.starts_with('/'))
        .unwrap_or(rest);
    let decoded = percent_decode(rest);
    let b = decoded.as_bytes();
    let path = if b.first() == Some(&b'/') {
        if b.len() >= 3 && b[2] == b':' {
            decoded[1..].replace('/', "\\") // /C:/x → C:\x
        } else {
            decoded
        }
    } else {
        format!("//{decoded}").replace('/', "\\") // UNC
    };
    (!path.is_empty()).then_some(path)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Read one conversation database into events. Returns `(outcome, total_rows,
/// max_gen_idx, max_step_idx)`. When `resume = Some((prev_gen_idx, prev_step_idx))`
/// and the database only grew, only `idx >= prev_gen_idx` rows and recent steps
/// are fetched instead of re-reading megabytes of historical blobs.
fn read_conversation(
    conn: &Connection,
    item: &SourceItem,
    store: &Store,
    resume: Option<(i64, i64)>,
) -> Result<(ScanOutcome, u64, i64, i64)> {
    let mut out = ScanOutcome::default();
    if !has_table(conn, "gen_metadata") {
        return Ok((out, 0, -1, 0));
    }
    let session_id = item.path.file_stem().map_or_else(
        || "unknown".to_string(),
        |s| s.to_string_lossy().to_string(),
    );

    let (total_rows, db_max_idx): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(MAX(idx), -1) FROM gen_metadata",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let incremental = resume.filter(|&(prev_gen, _)| prev_gen >= 0 && db_max_idx >= prev_gen);

    let mut rows: Vec<(Option<i64>, Vec<u8>)> = Vec::new();
    {
        if let Some((prev_gen, _)) = incremental {
            let mut st =
                conn.prepare("SELECT idx, data FROM gen_metadata WHERE idx >= ?1 ORDER BY idx")?;
            let it = st.query_map([prev_gen], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, bytes_of(r.get_ref(1)?)))
            })?;
            for row in it {
                if let Ok((idx, Some(data))) = row {
                    rows.push((idx, data));
                }
            }
        } else {
            let mut st = conn.prepare("SELECT idx, data FROM gen_metadata ORDER BY idx")?;
            let it = st.query_map([], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, bytes_of(r.get_ref(1)?)))
            })?;
            for row in it {
                // A damaged row must not sink the rest of the conversation.
                if let Ok((idx, Some(data))) = row {
                    rows.push((idx, data));
                }
            }
        }
    }
    let n_rows = total_rows.max(0) as u64;

    let traj = read_trajectory(conn);
    let mut steps = read_step_times(
        conn,
        incremental.map(|(_, prev_step)| prev_step.saturating_sub(2)),
    );
    let mut models = SessionModels::from_rows(rows.iter().map(|(_, d)| d.as_slice()));
    let mut loaded_full_models = incremental.is_none();
    let mut loaded_full_steps = incremental.is_none();
    let fallback_ts = traj.created_ms.unwrap_or_else(|| file_mtime_ms(&item.path));
    let now = crate::store::now_ms();
    let believable = |ms: i64| ms >= MIN_TS_MS && ms <= now + SKEW_MS;

    let mut seen: HashSet<String> = HashSet::new();
    for (idx, blob) in &rows {
        let Some(chat) = msg(blob, 1) else { continue };
        let Some(usage) = msg(chat, 4) else { continue };

        // `usage.#1` is the protobuf `Model` enum integer (equals `chat.#3`),
        // NOT input tokens. `#2` is non-cached input, `#5` cache read, `#3`
        // total output, `#9` thinking (reasoning) output, `#10` visible
        // response / tool-call output (`#9 + #10 == #3`).
        let input = varint(usage, 2).unwrap_or(0);
        let cache_read = varint(usage, 5).unwrap_or(0);
        let thinking = varint(usage, 9).unwrap_or(0);
        let response_out = varint(usage, 10).unwrap_or(0);
        let sum_out = thinking.saturating_add(response_out);
        let output = varint(usage, 3).filter(|&v| v > 0).unwrap_or(sum_out);
        let reasoning = thinking.min(output);
        let response_id = text(usage, 11).map(str::to_string);
        if input == 0 && cache_read == 0 && output == 0 {
            // Clean up legacy phantom rows that earlier versions ingested by
            // counting `usage.#1` (the Model enum integer) as input tokens.
            if let Some(key) = response_id
                .as_deref()
                .map(|id| format!("agy:{id}"))
                .or_else(|| idx.map(|i| format!("agy:{session_id}:{i}")))
            {
                let _ = store.conn().execute(
                    "DELETE FROM usage_events WHERE app = ?1 AND dedup_key = ?2 \
                     AND output_tokens = 0 AND cache_read_tokens = 0 AND duration_ms IS NULL",
                    rusqlite::params![apps::GEMINI_ANTIGRAVITY, key],
                );
            }
            continue;
        }
        if let Some(id) = &response_id {
            if !seen.insert(id.clone()) {
                out.skipped += 1;
                continue;
            }
            // A fork or an imported conversation carries the earlier turns
            // along. The ledger row already belongs to the conversation that
            // first reported it; letting the copy re-write it would flip the
            // session/project back and forth on every scan of either file.
            let owner = store
                .conn()
                .query_row(
                    "SELECT session_id FROM usage_events WHERE dedup_key = ?1",
                    [format!("agy:{id}")],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .ok()
                .flatten()
                .flatten();
            if owner.is_some_and(|o| o != session_id) {
                out.skipped += 1;
                continue;
            }
        }

        // Model: own concrete id → known `model_enum` (#20) → sibling-derived →
        // label → raw `model_enum` → (routing label).
        let own = text(chat, 19);
        let model_enum = kv_text(chat, 20, "model_enum").filter(|m| !is_routing(m));
        let direct = own
            .filter(|m| !is_routing(m))
            .or_else(|| model_enum.filter(|m| canonical_model(m).is_some()));
        if direct.is_none() && models.recover(chat).is_none() && !loaded_full_models {
            // Lazy fallback: load sibling rows only when an incremental turn
            // carries a routing placeholder without a direct model id.
            loaded_full_models = true;
            if let Ok(mut st) = conn.prepare("SELECT data FROM gen_metadata")
                && let Ok(it) = st.query_map([], |r| Ok(bytes_of(r.get_ref(0)?)))
            {
                let all_blobs: Vec<Vec<u8>> = it.flatten().flatten().collect();
                models = SessionModels::from_rows(all_blobs.iter().map(Vec::as_slice));
            }
        }
        let raw = direct
            .or_else(|| models.recover(chat))
            .or_else(|| text(chat, 21).and_then(label_model))
            .or(model_enum)
            .or(own);
        let canonical = raw.and_then(canonical_model);
        let model = canonical.or(raw).map(str::to_string);
        let request_model = raw
            .filter(|r| canonical.is_some_and(|c| !c.eq_ignore_ascii_case(r)))
            .map(str::to_string);

        let lookup_step = |st: &StepTimes| {
            response_id
                .as_deref()
                .and_then(|id| st.by_response.get(id))
                .copied()
                .filter(|s| believable(s.start_ms))
                .or_else(|| {
                    idx.and_then(|i| st.by_gen_idx.get(&i))
                        .copied()
                        .filter(|s| believable(s.start_ms))
                })
        };
        let mut step = lookup_step(&steps);
        if step.is_none() && !loaded_full_steps {
            loaded_full_steps = true;
            steps = read_step_times(conn, None);
            step = lookup_step(&steps);
        }

        // Start time: per-generation stamp (agy ≤ 1.1.17) → matching model-turn
        // step (agy ≥ 1.1.18) → conversation start → file mtime.
        let ts = msg(chat, 9)
            .and_then(|g| msg(g, 4))
            .and_then(timestamp_ms)
            .filter(|&ms| believable(ms))
            .or_else(|| step.map(|s| s.start_ms))
            .unwrap_or(fallback_ts);

        // Direct client-measured durations on `gen_metadata.#1`:
        // `#11` = time_to_first_token (TTFT), `#12` = streaming_duration;
        // `#11 + #12` is total call duration (agrees with `steps.#7 - steps.#1`
        // to sub-millisecond precision on real databases).
        let ttft_ns = msg(chat, 11).and_then(duration_ns);
        let stream_ns = msg(chat, 12).and_then(duration_ns);
        let (duration_ms, mut ttft_ms) = match (ttft_ns, stream_ns) {
            (Some(t), Some(s)) => {
                let total = t.checked_add(s).and_then(ns_to_ms);
                let ttft = total.and_then(|dur| ns_to_ms(t).map(|v| v.min(dur)));
                (total, ttft)
            }
            // Only `#11` present (`#12` absent): keep TTFT without pretending
            // it is the total duration; `steps.#7` may still provide `ts_end`.
            (Some(t), None) => (None, ns_to_ms(t)),
            _ => (None, None),
        };

        let step_end_ms = step
            .and_then(|s| s.end_ms)
            .filter(|&ms| believable(ms) && ms >= ts);

        if ttft_ms.is_none()
            && let Some(s) = step
            && let Some(ft) = s.first_token_ms.filter(|&m| believable(m))
            && let Some(d) = crate::normalize::derived_duration(Some(s.start_ms), Some(ft))
        {
            let within_end = duration_ms
                .map(|dur| d <= dur)
                .or_else(|| step_end_ms.map(|end| ft <= end))
                .unwrap_or(true);
            if within_end {
                ttft_ms = Some(d);
            }
        }

        // End time: step completed_at (#7/#8/#32), else ts_start + measured
        // duration. When `duration_ms` is absent (legacy DB without #11/#12)
        // but `steps` gives both start and end, `Store`/`EventRow` use the
        // bounded `ts_end - ts_start` span and flag it as timestamp-estimated.
        let ts_end = step_end_ms.or_else(|| duration_ms.and_then(|d| ts.checked_add(d)));

        let dedup_key = match &response_id {
            Some(id) => format!("agy:{id}"),
            None => format!(
                "agy:{session_id}:{}",
                idx.unwrap_or(out.events.len() as i64)
            ),
        };
        out.events.push(UsageEvent {
            dedup_key,
            app: apps::GEMINI_ANTIGRAVITY.into(),
            session_id: Some(session_id.clone()),
            project: traj.workspace.clone(),
            provider_id: model.as_deref().and_then(provider_of).map(str::to_string),
            model,
            request_model,
            ts_start: Some(ts),
            ts_end,
            input_tokens: input,
            // Thinking is billed as output; `reasoning_tokens` is the subset.
            output_tokens: output,
            reasoning_tokens: reasoning,
            cache_read_tokens: cache_read,
            provenance: Provenance::LocalSqlite,
            duration_ms,
            ttft_ms,
            raw_ref: Some(format!(
                "{}#gen:{}",
                item.path.display(),
                idx.map_or_else(|| "?".to_string(), |i| i.to_string())
            )),
            ..Default::default()
        });
    }
    let next_step = steps
        .max_idx
        .max(incremental.map_or(0, |(_, prev_step)| prev_step));
    Ok((out, n_rows, db_max_idx, next_step))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{PriceBook, Resolution};

    // ---- tiny protobuf encoder for fixtures
    fn varint_bytes(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                return out;
            }
        }
    }
    fn f_varint(field: u64, v: u64) -> Vec<u8> {
        let mut o = varint_bytes(field << 3);
        o.extend(varint_bytes(v));
        o
    }
    fn f_len(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut o = varint_bytes(field << 3 | 2);
        o.extend(varint_bytes(payload.len() as u64));
        o.extend_from_slice(payload);
        o
    }
    fn f_ts(field: u64, secs: i64) -> Vec<u8> {
        let mut t = f_varint(1, secs as u64);
        t.extend(f_varint(2, 500_000_000));
        f_len(field, &t)
    }
    fn f_dur(field: u64, secs: u64, nanos: u64) -> Vec<u8> {
        let mut d = Vec::new();
        if secs > 0 {
            d.extend(f_varint(1, secs));
        }
        if nanos > 0 {
            d.extend(f_varint(2, nanos));
        }
        f_len(field, &d)
    }
    fn f_kv(field: u64, key: &str, val: &str) -> Vec<u8> {
        let mut kv = f_len(1, key.as_bytes());
        kv.extend(f_len(2, val.as_bytes()));
        f_len(field, &kv)
    }

    /// Recent enough to pass the plausibility window whenever the tests run.
    fn recent_secs(back: i64) -> i64 {
        crate::store::now_ms() / 1000 - back
    }

    struct Gen<'a> {
        rid: &'a str,
        /// Protobuf `Model` enum integer (`usage.#1` / `chat.#3`), never tokens.
        model_enum_id: u64,
        /// Non-cached input tokens (`usage.#2`).
        input: u64,
        cache: u64,
        /// Thinking/reasoning output tokens (`usage.#9`).
        think: u64,
        /// Visible response / tool-call output tokens (`usage.#10`).
        response: u64,
        /// Explicit total output (`usage.#3`); when `None`, encoded as `think + response`.
        total_out: Option<u64>,
        model: Option<&'a str>,
        model_enum: Option<&'a str>,
        label: Option<&'a str>,
        /// agy ≤ 1.1.17 per-generation stamp (`#9.#4`), seconds.
        stamp: Option<i64>,
        /// Client-measured TTFT (`chat.#11`) as `(secs, nanos)`.
        ttft: Option<(u64, u64)>,
        /// Client-measured streaming duration (`chat.#12`) as `(secs, nanos)`.
        stream: Option<(u64, u64)>,
    }

    impl Gen<'_> {
        fn blob(&self) -> Vec<u8> {
            let mut usage = f_varint(1, self.model_enum_id);
            usage.extend(f_varint(2, self.input));
            let tot = self
                .total_out
                .unwrap_or_else(|| self.think.saturating_add(self.response));
            if tot > 0 {
                usage.extend(f_varint(3, tot));
            }
            usage.extend(f_varint(5, self.cache));
            usage.extend(f_varint(9, self.think));
            usage.extend(f_varint(10, self.response));
            if !self.rid.is_empty() {
                usage.extend(f_len(11, self.rid.as_bytes()));
            }
            let mut chat = f_varint(3, self.model_enum_id);
            chat.extend(f_len(4, &usage));
            match self.stamp {
                Some(s) => chat.extend(f_len(9, &f_ts(4, s))),
                // modern agy: #9 carries prompt-cache metadata, no timestamp
                None => chat.extend(f_len(9, &f_len(10, b"cache metadata, not a time"))),
            }
            if let Some((s, ns)) = self.ttft {
                chat.extend(f_dur(11, s, ns));
            }
            if let Some((s, ns)) = self.stream {
                chat.extend(f_dur(12, s, ns));
            }
            if let Some(m) = self.model {
                chat.extend(f_len(19, m.as_bytes()));
            }
            if let Some(me) = self.model_enum {
                chat.extend(f_kv(20, "model_enum", me));
            }
            if let Some(l) = self.label {
                chat.extend(f_len(21, l.as_bytes()));
            }
            f_len(1, &chat)
        }
    }

    fn turn<'a>(rid: &'a str, model: Option<&'a str>) -> Gen<'a> {
        Gen {
            rid,
            model_enum_id: 1318,
            input: 900,
            cache: 5000,
            think: 200,
            response: 300,
            total_out: None,
            model,
            model_enum: None,
            label: None,
            stamp: None,
            ttft: None,
            stream: None,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gtt_agy_{tag}_{}_{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn new_db(path: &Path, with_steps: bool) -> Connection {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);",
        )
        .unwrap();
        if with_steps {
            c.execute_batch("CREATE TABLE steps (idx integer, step_type integer, metadata blob);")
                .unwrap();
        }
        c
    }

    fn put_gen(c: &Connection, idx: i64, blob: &[u8]) {
        c.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)",
            rusqlite::params![idx, blob, blob.len() as i64],
        )
        .unwrap();
    }

    fn put_traj(c: &Connection, created_secs: i64, workspace_uri: &str) {
        let mut b = f_len(1, &f_len(1, workspace_uri.as_bytes()));
        b.extend(f_ts(2, created_secs));
        c.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            [b],
        )
        .unwrap();
    }

    fn item(path: &Path) -> SourceItem {
        SourceItem {
            key: path.to_string_lossy().into(),
            path: path.to_path_buf(),
            kind: SourceKind::Sqlite,
        }
    }

    fn parses_generations_into_billable_events() {
        let dir = temp_dir("parse");
        let db = dir.join("11111111-2222-3333-4444-555555555555.db");
        let c = new_db(&db, false);
        put_traj(&c, recent_secs(3600), "file:///C:/Users/me/My%20Proj");
        let mut g1 = turn("resp-1", Some("gemini-pro-default"));
        g1.stamp = Some(recent_secs(600));
        g1.ttft = Some((3, 585_277_600));
        g1.stream = Some((7, 580_841_500));
        put_gen(&c, 0, &g1.blob());
        // Same response id again (a retried write) — counted once.
        put_gen(&c, 1, &g1.blob());
        // Zero-usage row (only model enum `#1` set, no real tokens) — dropped.
        let mut empty = turn("resp-empty", Some("gemini-pro-default"));
        (empty.input, empty.cache, empty.think, empty.response) = (0, 0, 0, 0);
        put_gen(&c, 2, &empty.blob());
        let mut g3 = turn("resp-2", Some("claude-opus-4-6-thinking"));
        (g3.think, g3.cache) = (0, 0);
        g3.stamp = Some(recent_secs(300));
        put_gen(&c, 3, &g3.blob());
        drop(c);

        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 2, "{:?}", out.events);
        assert_eq!(out.skipped, 1);

        let e = &out.events[0];
        assert_eq!(e.dedup_key, "agy:resp-1");
        assert_eq!(e.app, "gemini_antigravity");
        assert_eq!(
            e.session_id.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(e.project.as_deref(), Some(r"C:\Users\me\My Proj"));
        // input = #2 only (`#1` is the Model enum integer, never tokens).
        assert_eq!(e.input_tokens, 900);
        assert_eq!(e.cache_read_tokens, 5000);
        // thinking (#9) is billed as output (#3 = #9 + #10), and stays visible as the subset.
        assert_eq!(e.output_tokens, 500);
        assert_eq!(e.reasoning_tokens, 200);
        // machine id → price-book model, raw id kept for audit.
        assert_eq!(e.model.as_deref(), Some("gemini-3.1-pro"));
        assert_eq!(e.request_model.as_deref(), Some("gemini-pro-default"));
        assert_eq!(e.provider_id.as_deref(), Some("google"));
        let start = g1.stamp.unwrap() * 1000 + 500;
        assert_eq!(e.ts_start, Some(start));
        assert_eq!(e.ttft_ms, Some(3_585));
        assert_eq!(e.duration_ms, Some(11_166));
        assert_eq!(e.ts_end, Some(start + 11_166));
        assert_eq!(e.provenance, Provenance::LocalSqlite);

        let e2 = &out.events[1];
        assert_eq!(e2.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(e2.provider_id.as_deref(), Some("anthropic"));
        assert_eq!(e2.reasoning_tokens, 0);
        assert_eq!(e2.output_tokens, 300);
        assert_eq!(e2.duration_ms, None);
        assert_eq!(e2.ttft_ms, None);
        assert_eq!(e2.ts_end, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn modern_turns_are_dated_from_the_steps_table() {
        let dir = temp_dir("steps");
        let db = dir.join("modern.db");
        let created = recent_secs(3 * 86_400);
        let (t_a, t_b) = (recent_secs(7200), recent_secs(60));
        let c = new_db(&db, true);
        put_traj(&c, created, "file:///home/me/proj");
        // No `#9.#4` on either row (agy ≥ 1.1.18).
        put_gen(&c, 3, &turn("resp-a", Some("gemini-3-flash-a")).blob());
        put_gen(&c, 7, &turn("", Some("gemini-3-flash-a")).blob()); // no response id → idx join
        let step = |secs: i64, rid: Option<&str>, gen_idx: Option<u64>| {
            let mut m = f_ts(1, secs);
            if let Some(r) = rid {
                m.extend(f_len(9, &f_len(11, r.as_bytes())));
            }
            if let Some(i) = gen_idx {
                m.extend(f_len(20, &f_varint(3, i)));
            }
            m
        };
        for (i, ty, meta) in [
            (1, 15, step(t_a, Some("resp-a"), Some(3))),
            (2, 15, step(t_b, None, Some(7))),
            // A non-model step must never date a generation.
            (3, 9, step(recent_secs(10), Some("resp-a"), Some(3))),
        ] {
            c.execute(
                "INSERT INTO steps (idx, step_type, metadata) VALUES (?1, ?2, ?3)",
                rusqlite::params![i, ty, meta],
            )
            .unwrap();
        }
        drop(c);

        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[0].ts_start, Some(t_a * 1000 + 500)); // by response id
        assert_eq!(out.events[1].ts_start, Some(t_b * 1000 + 500)); // by gen idx
        assert_eq!(out.events[1].dedup_key, "agy:modern:7");
        assert_eq!(out.events[0].model.as_deref(), Some("gemini-3.5-flash"));
        assert_eq!(out.events[0].project.as_deref(), Some("/home/me/proj"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn falls_back_to_conversation_start_when_nothing_dates_a_turn() {
        let dir = temp_dir("fallback");
        let db = dir.join("plain.db");
        let created = recent_secs(86_400);
        let c = new_db(&db, false);
        put_traj(&c, created, "file:///x");
        put_gen(&c, 0, &turn("r", Some("gemini-3-flash-a")).blob());
        drop(c);
        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events[0].ts_start, Some(created * 1000 + 500));
        // An absurd stamp (year 2000) is not believed either.
        let db2 = dir.join("skewed.db");
        let c = new_db(&db2, false);
        put_traj(&c, created, "file:///x");
        let mut g = turn("r2", Some("gemini-3-flash-a"));
        g.stamp = Some(946_684_800);
        put_gen(&c, 0, &g.blob());
        drop(c);
        let out = Antigravity.scan_sqlite(&item(&db2), &store).unwrap();
        assert_eq!(out.events[0].ts_start, Some(created * 1000 + 500));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn missing_model_ids_are_recovered_from_siblings_only() {
        let dir = temp_dir("models");
        let db = dir.join("m.db");
        let c = new_db(&db, false);
        let mut labelled = turn("r1", Some("gemini-pro-agent"));
        labelled.label = Some("Gemini 3.1 Pro (High)");
        put_gen(&c, 0, &labelled.blob());
        // Continuation turn: label only, no `#19` → borrows the sibling's id.
        let mut cont = turn("r2", None);
        cont.label = Some("Gemini 3.1 Pro (High)");
        put_gen(&c, 1, &cont.blob());
        // Routing label with a label nobody identified → stays as the router
        // name (unpriced later), never a guess from another row.
        let mut routed = turn("r3", Some("gemini-default"));
        routed.label = Some("Some Brand New Model");
        put_gen(&c, 2, &routed.blob());
        // Routing label whose display label is a known tier → concrete model.
        let mut known = turn("r4", Some("gemini-default"));
        known.label = Some("Gemini 3.5 Flash (Medium)");
        put_gen(&c, 3, &known.blob());
        drop(c);
        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        let m: Vec<_> = out.events.iter().map(|e| e.model.as_deref()).collect();
        assert_eq!(
            m,
            [
                Some("gemini-3.1-pro"),
                Some("gemini-3.1-pro"),
                Some("gemini-default"),
                Some("gemini-3.5-flash")
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn a_sole_model_serves_unlabelled_rows_but_a_model_switch_blocks_it() {
        // One concrete id, every label identified → the unlabelled row inherits it.
        let blobs_ok = [
            turn("a", Some("gemini-pro-default")).blob(),
            turn("b", None).blob(),
        ];
        let sm = SessionModels::from_rows(blobs_ok.iter().map(Vec::as_slice));
        assert_eq!(
            sm.recover(msg(&blobs_ok[1], 1).unwrap()),
            Some("gemini-pro-default")
        );
        // A label that no row ever identified proves the file ran a model it
        // never names → no fallback for anyone.
        let mut orphan = turn("c", None);
        orphan.label = Some("Mystery");
        let blobs_bad = [turn("a", Some("gemini-pro-default")).blob(), orphan.blob()];
        let sm = SessionModels::from_rows(blobs_bad.iter().map(Vec::as_slice));
        assert_eq!(sm.recover(msg(&blobs_bad[1], 1).unwrap()), None);
        assert_eq!(sm.sole, None);
    }

    fn unchanged_databases_are_skipped_and_growth_is_picked_up() {
        let dir = temp_dir("fp");
        let db = dir.join("g.db");
        let c = new_db(&db, false);
        put_traj(&c, recent_secs(100), "file:///x");
        put_gen(&c, 0, &turn("r1", Some("gemini-pro-default")).blob());
        drop(c);
        let store = Store::open_memory().unwrap();
        let it = item(&db);
        assert_eq!(
            Antigravity.scan_sqlite(&it, &store).unwrap().events.len(),
            1
        );
        // Nothing changed → not even opened.
        assert!(
            Antigravity
                .scan_sqlite(&it, &store)
                .unwrap()
                .events
                .is_empty()
        );

        std::thread::sleep(std::time::Duration::from_millis(30));
        let c = Connection::open(&db).unwrap();
        put_gen(&c, 1, &turn("r2", Some("gemini-pro-default")).blob());
        drop(c);
        // Re-read: the old generation comes back too (the ledger UPSERT makes
        // that idempotent) alongside the new one.
        let out = Antigravity.scan_sqlite(&it, &store).unwrap();
        let ids: Vec<_> = out.events.iter().map(|e| e.dedup_key.as_str()).collect();
        assert_eq!(ids, ["agy:r1", "agy:r2"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn a_forked_copy_never_steals_or_double_counts_a_turn() {
        let dir = temp_dir("fork");
        let (a, b) = (dir.join("orig.db"), dir.join("fork.db"));
        let c = new_db(&a, false);
        put_gen(
            &c,
            0,
            &turn("shared-resp", Some("gemini-pro-default")).blob(),
        );
        drop(c);
        // The fork carries the shared turn plus one of its own.
        let c = new_db(&b, false);
        put_gen(
            &c,
            0,
            &turn("shared-resp", Some("gemini-pro-default")).blob(),
        );
        put_gen(&c, 1, &turn("fork-only", Some("gemini-pro-default")).blob());
        drop(c);

        let store = Store::open_memory().unwrap();
        let ingest = |events: &[UsageEvent]| {
            for e in events {
                store.upsert_event(e).unwrap();
            }
        };
        let ea = Antigravity.scan_sqlite(&item(&a), &store).unwrap();
        ingest(&ea.events);
        let eb = Antigravity.scan_sqlite(&item(&b), &store).unwrap();
        ingest(&eb.events);
        // The copy contributes only what is new to the ledger.
        let keys: Vec<_> = eb.events.iter().map(|e| e.dedup_key.as_str()).collect();
        assert_eq!(keys, ["agy:fork-only"]);
        assert_eq!(eb.skipped, 1);

        // The original grows: it re-reads its own turn, which stays its own.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let c = Connection::open(&a).unwrap();
        put_gen(&c, 1, &turn("orig-2", Some("gemini-pro-default")).blob());
        drop(c);
        let ea = Antigravity.scan_sqlite(&item(&a), &store).unwrap();
        ingest(&ea.events);
        let owner: String = store
            .conn()
            .query_row(
                "SELECT session_id FROM usage_events WHERE dedup_key='agy:shared-resp'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, "orig");
        let n: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM usage_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3); // shared, fork-only, orig-2 — nothing counted twice
        std::fs::remove_dir_all(&dir).ok();
    }

    fn databases_without_gen_metadata_are_ignored() {
        let dir = temp_dir("idx");
        let db = dir.join("idx.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE summaries (id text, title text);")
            .unwrap();
        drop(c);
        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert!(out.events.is_empty());
        // Remembered: the second pass does not reopen it.
        assert!(store.load_cursor(&item(&db).key).unwrap().state.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn a_wal_database_left_without_sidecars_still_reads() {
        let dir = temp_dir("wal");
        let db = dir.join("w.db");
        let c = new_db(&db, false);
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        put_gen(&c, 0, &turn("wal-1", Some("gemini-pro-default")).blob());
        drop(c); // clean close checkpoints and removes -wal/-shm
        let mut wal = db.as_os_str().to_os_string();
        wal.push("-wal");
        assert!(!Path::new(&wal).exists(), "fixture must have no sidecars");
        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn corrupt_blobs_degrade_to_nothing() {
        let dir = temp_dir("bad");
        let db = dir.join("bad.db");
        let c = new_db(&db, false);
        put_gen(
            &c,
            0,
            &[
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            ],
        );
        put_gen(&c, 1, &[0x0a, 0x7f, 0x01]); // length runs past the buffer
        put_gen(&c, 2, &[]);
        put_gen(&c, 3, &turn("ok", Some("gemini-pro-default")).blob());
        drop(c);
        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].dedup_key, "agy:ok");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn discovery_covers_all_roots_and_skips_the_index() {
        let base = temp_dir("disc");
        let mk = |rel: &str, name: &str| {
            let d = base.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(name), b"x").unwrap();
        };
        mk("antigravity-cli/conversations", "a.db");
        mk("antigravity/conversations", "b.db");
        mk("antigravity-ide/conversations", "d.db");
        mk("antigravity", "c.db");
        mk("antigravity", SUMMARIES_DB);
        mk("antigravity", "notes.txt");
        mk("antigravity/conversations", "old.pb");
        mk("antigravity", "c.db-wal");
        let names: Vec<_> = discover_in(&base)
            .iter()
            .map(|i| i.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 4, "{names:?}");
        for n in ["a.db", "b.db", "c.db", "d.db"] {
            assert!(names.contains(&n.to_string()), "{n} in {names:?}");
        }
        assert!(discover_in(&base.join("nothing-here")).is_empty());
        std::fs::remove_dir_all(&base).ok();
    }

    fn gemini_cli_home_relocates_the_base_directory() {
        let home = Path::new("/home/u");
        assert_eq!(base_dir_from(None, home), home.join(".gemini"));
        assert_eq!(base_dir_from(Some("  "), home), home.join(".gemini"));
        assert_eq!(
            base_dir_from(Some("/data/g"), home),
            Path::new("/data/g").join(".gemini")
        );
    }

    fn workspace_uris_become_native_paths() {
        assert_eq!(
            file_uri_to_path("file:///C:/Users/me/proj").as_deref(),
            Some(r"C:\Users\me\proj")
        );
        assert_eq!(
            file_uri_to_path("file:///d%3A/%E9%A1%B9%E7%9B%AE").as_deref(),
            Some(r"d:\项目")
        );
        assert_eq!(
            file_uri_to_path("file:///home/me/p").as_deref(),
            Some("/home/me/p")
        );
        assert_eq!(
            file_uri_to_path("file://srv/share/x").as_deref(),
            Some(r"\\srv\share\x")
        );
        assert_eq!(file_uri_to_path("https://example.com"), None);
        // macOS: explicit localhost host and percent-encoded spaces.
        assert_eq!(
            file_uri_to_path("file://localhost/Users/me/My%20Project").as_deref(),
            Some("/Users/me/My Project")
        );
        assert_eq!(
            file_uri_to_path("file:///Users/me/%E9%A1%B9%E7%9B%AE").as_deref(),
            Some("/Users/me/项目")
        );
    }

    #[test]
    fn every_alias_target_is_priced_by_the_seed_book() {
        let store = Store::open_memory().unwrap();
        let book = PriceBook::load(&store).unwrap();
        let ids = [
            "gemini-pro-default",
            "gemini-pro-agent",
            "gemini-3.1-pro-high",
            "gemini-3-pro-low",
            "gemini-3-flash-a",
            "gemini-3-flash-b",
            "gemini-3-flash-agent",
            "gemini-3.5-flash-low",
            "gemini-3.5-flash-extra-low",
            "gemini-3-flash",
            "MODEL_PLACEHOLDER_M26",
            "MODEL_PLACEHOLDER_M35",
            "claude-opus-4-6-thinking",
            "claude-sonnet-4-6-thinking",
            "MODEL_OPENAI_GPT_OSS_120B_MEDIUM",
            "gpt-oss-120b-medium",
            "gemini-3.8-flash",
            "gemini-3.8-flash-n",
            "gemini-3.8-flash-high",
            "MODEL_PLACEHOLDER_M318",
            "MODEL_PLACEHOLDER_M319",
            "MODEL_PLACEHOLDER_M320",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
        ];
        for id in ids {
            let target = canonical_model(id).unwrap_or_else(|| panic!("{id} has no alias"));
            match book.resolve(target, Some(id), None) {
                Resolution::Priced(..) => {}
                Resolution::Unpriced => panic!("{id} → {target} is not in the seed price book"),
            }
        }
        // The router's placeholder must stay unpriced rather than be guessed.
        assert!(canonical_model("gemini-default").is_none());
        assert!(matches!(
            book.resolve("gemini-default", None, None),
            Resolution::Unpriced
        ));
    }

    fn measured_durations_power_all_five_speed_metrics() {
        let dir = temp_dir("speed");
        let db = dir.join("speed.db");
        let t0 = recent_secs(120);
        let c = new_db(&db, true);
        put_traj(&c, recent_secs(3600), "file:///C:/proj");

        // Turn 0: 4000 input, 600 output (400 think + 200 response), TTFT 2s, stream 3s → total 5s.
        let mut g0 = turn("sp-0", Some("gemini-4-neon"));
        g0.input = 4000;
        g0.think = 400;
        g0.response = 200;
        g0.ttft = Some((2, 0));
        g0.stream = Some((3, 0));
        put_gen(&c, 0, &g0.blob());

        // Turn 1: single-chunk call (TTFT 1s, stream 0s) → duration == ttft, excluded from decode.
        let mut g1 = turn("sp-1", Some("gemini-4-neon"));
        g1.input = 1000;
        g1.think = 50;
        g1.response = 50;
        g1.ttft = Some((1, 0));
        g1.stream = Some((0, 0));
        put_gen(&c, 1, &g1.blob());

        let step = |start: i64, first: i64, end: i64, rid: &str, gen_idx: u64| {
            let mut m = f_ts(1, start);
            m.extend(f_ts(6, first));
            m.extend(f_ts(7, end));
            m.extend(f_len(9, &f_len(11, rid.as_bytes())));
            m.extend(f_len(20, &f_varint(3, gen_idx)));
            m
        };
        for (i, meta) in [
            (1, step(t0, t0 + 2, t0 + 5, "sp-0", 0)),
            (2, step(t0 + 10, t0 + 11, t0 + 11, "sp-1", 1)),
        ] {
            c.execute(
                "INSERT INTO steps (idx, step_type, metadata) VALUES (?1, 15, ?2)",
                rusqlite::params![i, meta],
            )
            .unwrap();
        }
        drop(c);

        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[0].duration_ms, Some(5_000));
        assert_eq!(out.events[0].ttft_ms, Some(2_000));
        assert_eq!(out.events[0].ts_start, Some(t0 * 1000 + 500));
        assert_eq!(out.events[0].ts_end, Some((t0 + 5) * 1000 + 500));
        assert_eq!(out.events[1].duration_ms, Some(1_000));
        assert_eq!(out.events[1].ttft_ms, Some(1_000));

        for ev in &out.events {
            store.upsert_event(ev).unwrap();
        }
        let by_app = store.speed_by_app(None, None, None, None).unwrap();
        let agy = by_app
            .iter()
            .find(|r| r.name == apps::GEMINI_ANTIGRAVITY)
            .unwrap();
        assert_eq!(agy.speed_ms, 6_000);
        assert_eq!(agy.speed_est_ms, 0);
        // Output: (600 + 100) / 6s
        assert!((agy.output_tps().unwrap() - (700.0 / 6.0)).abs() < 1e-6);
        // Input: (4000 + 1000) / 6s
        assert!((agy.input_tps().unwrap() - (5000.0 / 6.0)).abs() < 1e-6);
        // Total: (5000 + 700) / 6s
        assert!((agy.total_tps().unwrap() - (5700.0 / 6.0)).abs() < 1e-6);
        // Prefill: (4000 + 1000) / (2s + 1s)
        assert!((agy.prefill_tps().unwrap() - (5000.0 / 3.0)).abs() < 1e-6);
        // Decode: only sp-0 has eff > ttft (600 / 3s = 200 tok/s)
        assert!((agy.decode_tps().unwrap() - 200.0).abs() < 1e-6);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn step_timestamps_provide_estimated_speed_fallback_when_gen_durations_absent() {
        let dir = temp_dir("step_est");
        let db = dir.join("legacy.db");
        let t0 = recent_secs(300);
        let c = new_db(&db, true);
        put_traj(&c, recent_secs(3600), "file:///proj");

        // No #11/#12 on gen_metadata → steps provide ts_start/ts_end/ttft_ms.
        put_gen(&c, 0, &turn("est-0", Some("gemini-3.8-flash-n")).blob());
        // Only #11 without #12 → keeps ttft_ms, leaves duration_ms None so step end supplies estimated span.
        let mut partial = turn("est-1", Some("gemini-3.8-flash-n"));
        partial.ttft = Some((1, 500_000_000));
        put_gen(&c, 1, &partial.blob());

        let step = |start: i64, first: i64, end: i64, rid: &str, gen_idx: u64| {
            let mut m = f_ts(1, start);
            m.extend(f_ts(6, first));
            m.extend(f_ts(7, end));
            m.extend(f_len(9, &f_len(11, rid.as_bytes())));
            m.extend(f_len(20, &f_varint(3, gen_idx)));
            m
        };
        for (i, meta) in [
            (1, step(t0, t0 + 2, t0 + 6, "est-0", 0)),
            (2, step(t0 + 20, t0 + 22, t0 + 25, "est-1", 1)),
        ] {
            c.execute(
                "INSERT INTO steps (idx, step_type, metadata) VALUES (?1, 15, ?2)",
                rusqlite::params![i, meta],
            )
            .unwrap();
        }
        drop(c);

        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[0].duration_ms, None);
        assert_eq!(out.events[0].ttft_ms, Some(2_000));
        assert_eq!(out.events[0].ts_start, Some(t0 * 1000 + 500));
        assert_eq!(out.events[0].ts_end, Some((t0 + 6) * 1000 + 500));
        // Partial #11 preserves #11 TTFT (1500ms) and still uses step ts_end.
        assert_eq!(out.events[1].duration_ms, None);
        assert_eq!(out.events[1].ttft_ms, Some(1_500));
        assert_eq!(out.events[1].ts_end, Some((t0 + 25) * 1000 + 500));

        for ev in &out.events {
            store.upsert_event(ev).unwrap();
        }
        let rows = store.by_model(None, None, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        // Both spans are timestamp-estimated: 6s + 5s = 11s.
        assert_eq!(rows[0].speed_ms, 11_000);
        assert_eq!(rows[0].speed_est_ms, 11_000);
        assert_eq!(rows[0].ttft_ms, 3_500);
        assert_eq!(rows[0].decode_ms, 7_500);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn corrupt_duration_blobs_degrade_cleanly() {
        let dir = temp_dir("bad_dur");
        let db = dir.join("bad_dur.db");
        let c = new_db(&db, false);
        put_traj(&c, recent_secs(100), "file:///x");

        let mut usage = f_varint(1, 1318);
        usage.extend(f_varint(2, 500));
        usage.extend(f_varint(3, 100));
        usage.extend(f_len(11, b"bad-dur-1"));
        let mut chat = f_len(4, &usage);
        // Negative seconds (twos-complement u64::MAX) in #11, out-of-range nanos in #12.
        chat.extend(f_dur(11, u64::MAX, 0));
        chat.extend(f_dur(12, 1, 1_000_000_000));
        chat.extend(f_len(19, b"gemini-3.8-flash-n"));
        put_gen(&c, 0, &f_len(1, &chat));

        // Truncated wire bytes in #11/#12.
        let mut usage2 = f_varint(1, 1318);
        usage2.extend(f_varint(2, 500));
        usage2.extend(f_varint(3, 100));
        usage2.extend(f_len(11, b"bad-dur-2"));
        let mut chat2 = f_len(4, &usage2);
        chat2.extend(f_len(11, &[0x08, 0x80]));
        chat2.extend(f_len(12, &[0xff, 0xff]));
        chat2.extend(f_len(19, b"gemini-3.8-flash-n"));
        put_gen(&c, 1, &f_len(1, &chat2));
        drop(c);

        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 2);
        for ev in &out.events {
            assert_eq!(ev.duration_ms, None);
            assert_eq!(ev.ttft_ms, None);
            assert_eq!(ev.ts_end, None);
            assert_eq!(ev.input_tokens, 500);
            assert_eq!(ev.output_tokens, 100);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn phantom_model_enum_only_rows_are_skipped_and_cleaned() {
        let dir = temp_dir("phantom");
        let db = dir.join("sess-p.db");
        let c = new_db(&db, false);
        put_traj(&c, recent_secs(100), "file:///x");

        // Failed/aborted turn: only Model enum `#1 = 1318`, zero tokens, no response_id.
        let mut phantom = turn("", None);
        phantom.model_enum_id = 1318;
        (
            phantom.input,
            phantom.cache,
            phantom.think,
            phantom.response,
        ) = (0, 0, 0, 0);
        phantom.ttft = Some((0, 0));
        phantom.stream = Some((0, 0));
        put_gen(&c, 11, &phantom.blob());

        // Real prompt-only turn: non-zero input (`#2`) and response_id, zero output.
        let mut real = turn("resp-prompt-only", Some("gemini-3.8-flash-n"));
        (real.input, real.cache, real.think, real.response) = (117_349, 0, 0, 0);
        real.ttft = Some((12, 100_000_000));
        real.stream = Some((0, 934_000_000));
        put_gen(&c, 89, &real.blob());
        drop(c);

        let store = Store::open_memory().unwrap();
        // Seed a legacy phantom row (as earlier builds would have written it).
        store
            .upsert_event(&UsageEvent {
                dedup_key: "agy:sess-p:11".into(),
                app: apps::GEMINI_ANTIGRAVITY.into(),
                session_id: Some("sess-p".into()),
                input_tokens: 1318,
                ..Default::default()
            })
            .unwrap();

        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].dedup_key, "agy:resp-prompt-only");
        assert_eq!(out.events[0].input_tokens, 117_349);
        assert_eq!(out.events[0].duration_ms, Some(13_034));
        assert_eq!(out.events[0].ttft_ms, Some(12_100));

        // Legacy phantom row was removed from the ledger.
        let remaining: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM usage_events WHERE dedup_key = 'agy:sess-p:11'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn model_enum_metadata_and_gemini_4_aliases_recover_models() {
        let dir = temp_dir("enum_meta");
        let db = dir.join("enums.db");
        let c = new_db(&db, false);
        put_traj(&c, recent_secs(100), "file:///x");

        // Neither `#19` nor `#21` present, only `#20.model_enum`.
        let mut r0 = turn("e0", None);
        r0.model_enum = Some("MODEL_PLACEHOLDER_M318");
        put_gen(&c, 0, &r0.blob());

        let mut r1 = turn("e1", None);
        r1.model_enum = Some("MODEL_OPENAI_GPT_OSS_120B_MEDIUM");
        put_gen(&c, 1, &r1.blob());

        let mut r2 = turn("e2", Some("gemini-4-neon-xhigh"));
        r2.model_enum = Some("MODEL_PLACEHOLDER_M535");
        put_gen(&c, 2, &r2.blob());

        let mut r3 = turn("e3", None);
        r3.label = Some("Gemini 4 Carbon (High)");
        put_gen(&c, 3, &r3.blob());

        let r4 = turn("e4", Some("gpt-oss-120b-medium"));
        put_gen(&c, 4, &r4.blob());

        let mut r5 = turn("e5", None);
        r5.model_enum = Some("MODEL_PLACEHOLDER_M528");
        put_gen(&c, 5, &r5.blob());
        drop(c);

        let store = Store::open_memory().unwrap();
        let out = Antigravity.scan_sqlite(&item(&db), &store).unwrap();
        let models: Vec<_> = out
            .events
            .iter()
            .map(|e| {
                (
                    e.model.as_deref(),
                    e.request_model.as_deref(),
                    e.provider_id.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            models,
            [
                (
                    Some("gemini-3.8-flash"),
                    Some("MODEL_PLACEHOLDER_M318"),
                    Some("google")
                ),
                (
                    Some("gpt-oss-120b"),
                    Some("MODEL_OPENAI_GPT_OSS_120B_MEDIUM"),
                    Some("openai")
                ),
                (
                    Some("gemini-4-neon"),
                    Some("gemini-4-neon-xhigh"),
                    Some("google")
                ),
                (Some("gemini-4-carbon"), None, Some("google")),
                (
                    Some("gpt-oss-120b"),
                    Some("gpt-oss-120b-medium"),
                    Some("openai")
                ),
                (
                    Some("gemini-4-hydrogen"),
                    Some("MODEL_PLACEHOLDER_M528"),
                    Some("google")
                ),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn incremental_rescan_reads_only_tail_and_recovers_sibling_models() {
        let dir = temp_dir("incr");
        let db = dir.join("sess-incr.db");
        let c = new_db(&db, false);
        put_traj(&c, recent_secs(200), "file:///work/proj");
        put_gen(&c, 0, &turn("t0", Some("gemini-4-neon")).blob());
        put_gen(&c, 1, &turn("t1", Some("gemini-4-neon")).blob());
        put_gen(&c, 2, &turn("t2", Some("gemini-4-neon")).blob());
        drop(c);

        let store = Store::open_memory().unwrap();
        let it = item(&db);
        let first = Antigravity.scan_sqlite(&it, &store).unwrap();
        assert_eq!(first.events.len(), 3);
        for ev in &first.events {
            store.upsert_event(ev).unwrap();
        }
        // Unchanged DB returns immediately.
        assert!(
            Antigravity
                .scan_sqlite(&it, &store)
                .unwrap()
                .events
                .is_empty()
        );

        // Append a turn with only the routing label; incremental scan should
        // read only idx >= 2 (t2, t3) and lazily load sibling models to recover t3.
        let c = Connection::open(&db).unwrap();
        let mut t3 = turn("t3", Some(ROUTING_LABEL));
        t3.label = None;
        put_gen(&c, 3, &t3.blob());
        drop(c);

        let second = Antigravity.scan_sqlite(&it, &store).unwrap();
        let ids: Vec<_> = second.events.iter().map(|e| e.dedup_key.as_str()).collect();
        assert_eq!(ids, ["agy:t2", "agy:t3"]);
        assert_eq!(second.events[1].model.as_deref(), Some("gemini-4-neon"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn watch_roots_under_skips_parent_when_only_summaries_db_exists() {
        let base = temp_dir("watchroots");
        std::fs::create_dir_all(base.join("antigravity/conversations")).unwrap();
        std::fs::write(base.join("antigravity").join(SUMMARIES_DB), b"idx").unwrap();
        let roots = watch_roots_under(&base);
        assert!(!roots.contains(&base.join("antigravity")));
        assert!(roots.contains(&base.join("antigravity/conversations")));

        // Legacy direct `<uuid>.db` activates watching the parent too.
        std::fs::write(base.join("antigravity/legacy.db"), b"db").unwrap();
        let roots2 = watch_roots_under(&base);
        assert!(roots2.contains(&base.join("antigravity")));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn generation_parsing_and_turn_dating() {
        parses_generations_into_billable_events();
        modern_turns_are_dated_from_the_steps_table();
        falls_back_to_conversation_start_when_nothing_dates_a_turn();
    }

    #[test]
    fn model_recovery_and_enum_aliases() {
        missing_model_ids_are_recovered_from_siblings_only();
        a_sole_model_serves_unlabelled_rows_but_a_model_switch_blocks_it();
        model_enum_metadata_and_gemini_4_aliases_recover_models();
    }

    #[test]
    fn incremental_scanning_and_fork_isolation() {
        unchanged_databases_are_skipped_and_growth_is_picked_up();
        a_forked_copy_never_steals_or_double_counts_a_turn();
        incremental_rescan_reads_only_tail_and_recovers_sibling_models();
    }

    #[test]
    fn malformed_wal_and_non_generation_databases() {
        databases_without_gen_metadata_are_ignored();
        a_wal_database_left_without_sidecars_still_reads();
        corrupt_blobs_degrade_to_nothing();
    }

    #[test]
    fn discovery_watch_roots_and_paths() {
        discovery_covers_all_roots_and_skips_the_index();
        gemini_cli_home_relocates_the_base_directory();
        workspace_uris_become_native_paths();
        watch_roots_under_skips_parent_when_only_summaries_db_exists();
    }

    #[test]
    fn speed_metrics_fallbacks_and_corrupt_durations() {
        measured_durations_power_all_five_speed_metrics();
        step_timestamps_provide_estimated_speed_fallback_when_gen_durations_absent();
        corrupt_duration_blobs_degrade_cleanly();
    }
}
