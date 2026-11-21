//! Muse adapter (Meta's terminal coding agent, `muse` binary).
//!
//! Sessions are append-only envelope JSONL at
//! `${XDG_DATA_HOME:-~/.local/share}/muse/sessions/YYYY/MM/DD/<session-id>/session.jsonl`,
//! with one log per delegated subagent under `<session>/subagent/<child-id>/`.
//! The XDG layout is used verbatim on Windows and macOS too (verified on
//! Windows with Muse Code 1.4.23; the same path is what macOS readers use).
//! `session-index.db` beside the sessions root is an index, not a usage
//! source, and is never read.
//!
//! Envelope: `{schema_version, id, stream:{kind:"session", id}, sequence,
//! recorded_at (epoch MICROseconds), payload_type, payload}`. Counted rows
//! are `payload.event` of exactly two kinds:
//!
//! - `model_completed` (payload.kind `run`) — one terminal record per model
//!   call: `{model, duration_ms, finish_reason?, usage:{input_tokens,
//!   output_tokens, reasoning_tokens, cache_read_tokens, cache_write_tokens,
//!   cached_tokens}}`. `input_tokens` INCLUDES the cached share and
//!   `cached_tokens == cache_read + cache_write` (both verified on a real
//!   corpus: 292/292 completions; the sibling review shape below spells the
//!   remainder out as `non_cached_input_tokens == input - cached` 9/9), so
//!   bare input is the remainder. `reasoning_tokens` is CONTAINED BY
//!   `output_tokens` (292/292 `reasoning <= output`), stored alongside as
//!   Codex does. No cache-write tier split is reported → 5m tier, like the
//!   Claude adapter's no-breakdown fallback. `estimated_prompt_tokens` on
//!   the same event is a pre-call estimate and never counted.
//! - `automated_review_completed` (payload.kind `approval`) — the approval
//!   auto-reviewer's own model calls, with a differently-spelled usage:
//!   `{input_tokens, cached_input_tokens, non_cached_input_tokens,
//!   output_tokens, reasoning_tokens, total_tokens}` (verified
//!   `total == input + output`, input again cache-inclusive), `model` as an
//!   OBJECT `{provider_id, model_id, reasoning_effort}`, plus `ttft_ms`,
//!   `duration_ms`, `status` and a stable `review_id`.
//!
//! Everything else that mentions tokens is a restatement and must NOT be
//! counted (summing it double-counts, verified by census): the many
//! `goal_usage_attribution` records mirror each call's numbers against a
//! billing owner (~3 per call), and the parent's `subagent.control`
//! `payload.record.usage` is a whole-attempt aggregate whose calls the child
//! session's own log already holds one by one.
//!
//! Dedup: `model_completed` has no response id inside the event; the stable
//! per-call identity is the payload's `source_run_record_id` — the run-log
//! join key a record keeps when a workflow-child run stream is mirrored into
//! a parent log, so a mirrored copy lands on the same dedup key instead of
//! counting twice. Pre-mirror hosts omit it → fall back to the envelope
//! record `id`, then to file+offset. Review calls key on `review_id`.
//!
//! Project/provider come from the file's `runtime.session.metadata` record
//! (`payload.record.workspace_root` / `provider_id`), persisted in adapter
//! state so later segments keep them. Older subagent logs carry no metadata
//! record (observed externally on 0.1.0; 1.4.23 subagents do carry one) →
//! the workspace is inherited from the parent session's log when absent.
//!
//! Models (`muse-wonder-full-1.0`, `muse-ember-1.0`, …) ship `cost: null`
//! in muse's own model catalog, so rows stay Unpriced until the shared price
//! feeds carry them — never guessed (spec §7.4).

use super::{Capability, ScanOutcome, SourceAdapter, SourceItem, SourceKind, complete_lines};
use crate::model::{Provenance, UsageEvent, apps};
use crate::normalize::{epoch_ms, input_excludes_cache, num, text};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct MuseCode;

/// Session-store roots, `${XDG_DATA_HOME}/muse/sessions` first, then the
/// default `~/.local/share/muse/sessions` (identical on every OS).
fn sessions_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        roots.push(PathBuf::from(xdg).join("muse").join("sessions"));
    }
    roots.push(crate::sync::home(".local/share/muse/sessions"));
    roots.sort();
    roots.dedup();
    roots
}

/// Workspace/provider of this log, learned from its
/// `runtime.session.metadata` record and persisted so calls in later
/// segments keep them (the metadata line sits near the file head).
#[derive(Debug, Default, Serialize, Deserialize)]
struct MuseState {
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default)]
    provider_id: Option<String>,
}

impl SourceAdapter for MuseCode {
    fn id(&self) -> &'static str {
        apps::MUSE_CODE
    }
    fn display_name(&self) -> &'static str {
        "Muse"
    }
    fn capability(&self) -> Capability {
        Capability::Precise
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        sessions_roots()
    }

    fn discover(&self) -> Result<Vec<SourceItem>> {
        let mut out = Vec::new();
        for root in sessions_roots() {
            let mut found = Vec::new();
            collect_sessions(&root, 10, &mut found);
            found.sort();
            for p in found {
                out.push(SourceItem {
                    key: p.to_string_lossy().to_string(),
                    path: p,
                    kind: SourceKind::Jsonl,
                });
            }
        }
        Ok(out)
    }

    fn parse_jsonl(
        &self,
        item: &SourceItem,
        from: u64,
        data: &[u8],
        prior_state: Option<&str>,
    ) -> Result<ScanOutcome> {
        let (seg, consumed) = complete_lines(data);
        let mut state: MuseState = prior_state
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let mut events = Vec::new();

        let mut pos = 0u64;
        for line in seg.split(|&b| b == b'\n') {
            let line_start = from + pos;
            pos += line.len() as u64 + 1;
            let line = trim_cr(line);
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let payload = &v["payload"];
            if v["payload_type"].as_str() == Some("runtime.session.metadata") {
                let record = &payload["record"];
                if state.workspace_root.is_none() {
                    state.workspace_root = text(&record["workspace_root"]);
                }
                if state.provider_id.is_none() {
                    state.provider_id = text(&record["provider_id"]);
                }
                continue;
            }
            let ev = &payload["event"];
            let built = match ev["kind"].as_str() {
                Some("model_completed") => {
                    model_completed(&v, payload, ev, item, line_start, &state)
                }
                Some("automated_review_completed") => {
                    review_completed(&v, payload, ev, item, line_start, &state)
                }
                _ => None,
            };
            if let Some(e) = built.filter(UsageEvent::is_billable) {
                events.push(e);
            }
        }

        // A legacy subagent log may never carry metadata: inherit the parent
        // session's workspace once (bounded head read), then it sticks in
        // state. Only when this pass actually produced unattributed rows.
        if state.workspace_root.is_none()
            && !events.is_empty()
            && let Some(root) = parent_workspace(&item.path)
        {
            for e in &mut events {
                if e.project.is_none() {
                    e.project = Some(root.clone());
                }
            }
            state.workspace_root = Some(root);
        }

        Ok(ScanOutcome {
            events,
            consumed,
            new_state: serde_json::to_string(&state).ok(),
            ..Default::default()
        })
    }
}

/// Dedup identity for a record: the mirror-stable run-log id, else the
/// envelope record id, else the byte position.
fn dedup_key(record_id: Option<String>, envelope: &Value, item: &SourceItem, at: u64) -> String {
    if let Some(id) = record_id {
        return format!("muse_code:{id}");
    }
    if let Some(id) = text(&envelope["id"]) {
        return format!("muse_code:rec:{id}");
    }
    format!("muse_code:{}@{at}", item.path.display())
}

/// Session id from the envelope stream; the parent directory name names the
/// same session when a record lacks the stream block.
fn session_id(envelope: &Value, item: &SourceItem) -> Option<String> {
    text(&envelope["stream"]["id"]).or_else(|| {
        item.path
            .parent()?
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
    })
}

fn model_completed(
    envelope: &Value,
    payload: &Value,
    ev: &Value,
    item: &SourceItem,
    line_start: u64,
    state: &MuseState,
) -> Option<UsageEvent> {
    let u = &ev["usage"];
    let input = num(&u["input_tokens"]);
    let cache_read = num(&u["cache_read_tokens"]);
    let cache_write = num(&u["cache_write_tokens"]);
    let model = text(&ev["model"]);
    Some(UsageEvent {
        dedup_key: dedup_key(
            text(&payload["source_run_record_id"]),
            envelope,
            item,
            line_start,
        ),
        app: apps::MUSE_CODE.into(),
        session_id: session_id(envelope, item),
        project: state.workspace_root.clone(),
        provider_id: state.provider_id.clone(),
        model: model.clone(),
        request_model: model,
        ts_start: epoch_ms(&envelope["recorded_at"]),
        input_tokens: input_excludes_cache(input, cache_read, cache_write),
        output_tokens: num(&u["output_tokens"]),
        reasoning_tokens: num(&u["reasoning_tokens"]),
        cache_read_tokens: cache_read,
        cache_write_5m_tokens: cache_write,
        provenance: Provenance::LocalJsonl,
        duration_ms: num64(&ev["duration_ms"]),
        status: text(&ev["finish_reason"]),
        raw_ref: Some(format!("{}@{line_start}", item.path.display())),
        ..Default::default()
    })
}

fn review_completed(
    envelope: &Value,
    _payload: &Value,
    ev: &Value,
    item: &SourceItem,
    line_start: u64,
    state: &MuseState,
) -> Option<UsageEvent> {
    let u = &ev["usage"];
    let input = num(&u["input_tokens"]);
    let cached = num(&u["cached_input_tokens"]);
    // `model` is an object here ({provider_id, model_id, …}); tolerate a
    // plain string from other writer versions.
    let model = text(&ev["model"]["model_id"]).or_else(|| text(&ev["model"]));
    let key = match text(&ev["review_id"]) {
        Some(id) => format!("muse_code:review:{id}"),
        None => dedup_key(None, envelope, item, line_start),
    };
    Some(UsageEvent {
        dedup_key: key,
        app: apps::MUSE_CODE.into(),
        session_id: session_id(envelope, item),
        project: state.workspace_root.clone(),
        provider_id: text(&ev["model"]["provider_id"]).or_else(|| state.provider_id.clone()),
        model: model.clone(),
        request_model: model,
        ts_start: epoch_ms(&envelope["recorded_at"]).or_else(|| epoch_ms(&ev["completed_at_ms"])),
        input_tokens: input_excludes_cache(input, cached, 0),
        output_tokens: num(&u["output_tokens"]),
        reasoning_tokens: num(&u["reasoning_tokens"]),
        cache_read_tokens: cached,
        provenance: Provenance::LocalJsonl,
        duration_ms: num64(&ev["duration_ms"]),
        ttft_ms: num64(&ev["ttft_ms"]),
        status: text(&ev["status"]),
        raw_ref: Some(format!("{}@{line_start}", item.path.display())),
        ..Default::default()
    })
}

/// Workspace of the parent session for a `<session>/subagent/<child>/` log,
/// from the parent log's own metadata record (bounded head read).
fn parent_workspace(path: &Path) -> Option<String> {
    let subagent_dir = path.parent()?.parent()?;
    if subagent_dir.file_name().is_none_or(|n| n != "subagent") {
        return None;
    }
    let session_dir = subagent_dir.parent()?;
    let raw = read_head(&session_dir.join("session.jsonl"), 1 << 20)?;
    for line in raw.split(|&b| b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(trim_cr(line)) else {
            continue;
        };
        if v["payload_type"].as_str() == Some("runtime.session.metadata") {
            return text(&v["payload"]["record"]["workspace_root"]);
        }
    }
    None
}

fn read_head(path: &Path, max: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max];
    let mut n = 0usize;
    while n < max {
        match f.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    buf.truncate(n);
    Some(buf)
}

fn num64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// Targeted walk under `sessions/`: finds `session.jsonl` while skipping
/// bulky non-transcript directories (`tool-outputs`, `.spool`, dot-dirs) and
/// recursing only into `subagent/` once inside a session directory.
fn collect_sessions(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut has_session = false;
    let mut subdirs = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Ok(ft) = entry.file_type() else { continue };
        if name == "session.jsonl" && ft.is_file() {
            out.push(entry.path());
            has_session = true;
            continue;
        }
        if !ft.is_dir() {
            continue;
        }
        if name
            .to_str()
            .is_some_and(|s| s == "tool-outputs" || s == ".spool" || s.starts_with('.'))
        {
            continue;
        }
        subdirs.push(entry.path());
    }
    for sub in subdirs {
        if has_session && sub.file_name().is_none_or(|n| n != "subagent") {
            continue;
        }
        collect_sessions(&sub, depth - 1, out);
    }
}

fn trim_cr(l: &[u8]) -> &[u8] {
    l.strip_suffix(b"\r").unwrap_or(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: i64 = 1_794_975_660_035_291; // recorded_at is epoch microseconds

    fn item() -> SourceItem {
        SourceItem {
            key: "k".into(),
            path: PathBuf::from("2026/11/18/sess-1/session.jsonl"),
            kind: SourceKind::Jsonl,
        }
    }

    fn metadata(workspace: &str) -> String {
        format!(
            r#"{{"schema_version":1,"id":"m0","stream":{{"kind":"session","id":"sess-1"}},"sequence":1,"recorded_at":{US},"payload_type":"runtime.session.metadata","payload":{{"kind":"session_metadata","record":{{"workspace_root":"{workspace}","provider_id":"meta","model_id":"muse-wonder-full-1.0"}}}}}}"#
        )
    }

    fn completed(src: Option<&str>, usage: &str) -> String {
        let src = src.map_or(String::new(), |s| {
            format!(r#","source_run_record_id":"{s}""#)
        });
        format!(
            r#"{{"schema_version":1,"id":"env-1","stream":{{"kind":"session","id":"sess-1"}},"sequence":9,"recorded_at":{US},"payload_type":"runtime.session","payload":{{"kind":"run","run_id":"run-1"{src},"event":{{"kind":"model_completed","model":"muse-wonder-full-1.0","duration_ms":4122,"finish_reason":"tool_calls","estimated_prompt_tokens":41111,"usage":{usage}}}}}}}"#
        )
    }

    fn parse_at(item: &SourceItem, lines: &[String], state: Option<&str>) -> ScanOutcome {
        let data = format!("{}\n", lines.join("\n"));
        MuseCode
            .parse_jsonl(item, 0, data.as_bytes(), state)
            .unwrap()
    }

    fn parse(lines: &[String], state: Option<&str>) -> ScanOutcome {
        parse_at(&item(), lines, state)
    }

    fn model_completed_is_normalized_and_mirrors_are_not_counted() {
        let usage = r#"{"input_tokens":40599,"output_tokens":514,"reasoning_tokens":300,"cache_read_tokens":40564,"cache_write_tokens":0,"cached_tokens":40564}"#;
        // The same call restated as goal attribution and as a whole-attempt
        // subagent aggregate: both must be ignored, never summed.
        let attribution = format!(
            r#"{{"schema_version":1,"id":"a1","stream":{{"kind":"session","id":"sess-1"}},"recorded_at":{US},"payload_type":"runtime.session","payload":{{"kind":"run","run_id":"run-1","event":{{"kind":"goal_usage_attribution","record":{{"usage_family":"provider","usage_id":"u1","quantity":{{"input_tokens":40599,"output_tokens":514,"reasoning_tokens":300,"cached_tokens":40564,"reported":true,"unit":"tokens","main_llm_steps":1}}}}}}}}}}"#
        );
        let aggregate = format!(
            r#"{{"schema_version":1,"id":"a2","stream":{{"kind":"session","id":"sess-1"}},"recorded_at":{US},"payload_type":"subagent.control.result_ready","payload":{{"kind":"subagent_control","record":{{"kind":"attempt","subagent_id":"c1","duration_ms":9,"usage":{usage}}}}}}}"#
        );
        let out = parse(
            &[
                metadata("/work/app"),
                completed(Some("src-1"), usage),
                attribution,
                aggregate,
            ],
            None,
        );
        assert_eq!(out.events.len(), 1);
        let e = &out.events[0];
        assert_eq!(e.dedup_key, "muse_code:src-1");
        assert_eq!(e.app, "muse_code");
        assert_eq!(e.session_id.as_deref(), Some("sess-1"));
        assert_eq!(e.project.as_deref(), Some("/work/app"));
        assert_eq!(e.provider_id.as_deref(), Some("meta"));
        assert_eq!(e.model.as_deref(), Some("muse-wonder-full-1.0"));
        assert_eq!(e.ts_start, Some(1_794_975_660_035), "microseconds to ms");
        assert_eq!(e.input_tokens, 35, "cache-inclusive input normalized");
        assert_eq!(e.cache_read_tokens, 40_564);
        assert_eq!((e.output_tokens, e.reasoning_tokens), (514, 300));
        assert_eq!(e.duration_ms, Some(4_122));
        assert_eq!(e.status.as_deref(), Some("tool_calls"));
    }

    fn automated_review_calls_are_counted_with_object_model() {
        let review = format!(
            r#"{{"schema_version":1,"id":"r-env","stream":{{"kind":"session","id":"sess-1"}},"recorded_at":{US},"payload_type":"runtime.session","payload":{{"kind":"approval","event":{{"kind":"automated_review_completed","review_id":"rev-1","status":"approved","duration_ms":900,"ttft_ms":123,"completed_at_ms":1794975660035,"model":{{"provider_id":"meta","model_id":"muse-ember-1.0","reasoning_effort":"low"}},"usage":{{"input_tokens":42465,"cached_input_tokens":465,"non_cached_input_tokens":42000,"output_tokens":371,"reasoning_tokens":239,"total_tokens":42836}}}}}}}}"#
        );
        let out = parse(&[metadata("/work/app"), review], None);
        assert_eq!(out.events.len(), 1);
        let e = &out.events[0];
        assert_eq!(e.dedup_key, "muse_code:review:rev-1");
        assert_eq!(e.model.as_deref(), Some("muse-ember-1.0"));
        assert_eq!(e.provider_id.as_deref(), Some("meta"));
        assert_eq!(e.input_tokens, 42_000);
        assert_eq!(e.cache_read_tokens, 465);
        assert_eq!(e.ttft_ms, Some(123));
        assert_eq!(e.status.as_deref(), Some("approved"));
    }

    fn dedup_fallbacks_state_chaining_and_half_lines() {
        let usage = r#"{"input_tokens":10,"output_tokens":2,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cached_tokens":0}"#;
        // No source id → the envelope record id keys the call.
        let out = parse(&[completed(None, usage)], None);
        assert_eq!(out.events[0].dedup_key, "muse_code:rec:env-1");
        assert_eq!(out.events[0].project, None);

        // Workspace learned in one chunk applies to calls in the next.
        let first = parse(&[metadata("/work/app")], None);
        assert!(first.events.is_empty());
        let st = first.new_state.unwrap();
        let second = parse(&[completed(Some("src-2"), usage)], Some(&st));
        assert_eq!(second.events[0].project.as_deref(), Some("/work/app"));
        // Garbage state degrades to empty instead of failing the scan.
        let third = parse(&[completed(Some("src-3"), usage)], Some("{nope"));
        assert_eq!(third.events[0].project, None);

        // A still-growing trailing line stays unconsumed.
        let data = completed(Some("src-4"), usage);
        let out = MuseCode
            .parse_jsonl(&item(), 0, data.as_bytes(), None)
            .unwrap();
        assert_eq!(out.consumed, 0);
        assert!(out.events.is_empty());
    }

    fn subagent_log_without_metadata_inherits_parent_workspace() {
        let base = std::env::temp_dir().join(format!("gtt-muse-code-test-{}", std::process::id()));
        let child_dir = base.join("sess-9/subagent/child-1");
        std::fs::create_dir_all(&child_dir).unwrap();
        std::fs::write(
            base.join("sess-9/session.jsonl"),
            format!("{}\n", metadata("/work/parent")),
        )
        .unwrap();
        let child = SourceItem {
            key: "child".into(),
            path: child_dir.join("session.jsonl"),
            kind: SourceKind::Jsonl,
        };
        let usage = r#"{"input_tokens":10,"output_tokens":2,"reasoning_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"cached_tokens":0}"#;
        let out = parse_at(&child, &[completed(Some("src-5"), usage)], None);
        assert_eq!(out.events[0].project.as_deref(), Some("/work/parent"));

        // A non-subagent log never reaches across to a sibling file.
        let plain = parse(&[completed(Some("src-6"), usage)], None);
        assert_eq!(plain.events[0].project, None);
        std::fs::remove_dir_all(&base).ok();
    }

    fn collect_sessions_skips_tool_outputs_and_spool_dirs() {
        let base = std::env::temp_dir().join(format!(
            "gtt-muse-walk-{}-{}",
            std::process::id(),
            crate::store::now_ms()
        ));
        let main = base.join("2026/11/20/s1");
        let sub = main.join("subagent/c1");
        let tools = main.join("tool-outputs/nested");
        let spool = main.join(".spool/chunks");
        for d in [&main, &sub, &tools, &spool] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("session.jsonl"), b"{}\n").unwrap();
        }
        let mut found = Vec::new();
        collect_sessions(&base, 10, &mut found);
        found.sort();
        assert_eq!(
            found,
            vec![main.join("session.jsonl"), sub.join("session.jsonl")]
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn completion_review_and_streaming_parsing() {
        model_completed_is_normalized_and_mirrors_are_not_counted();
        automated_review_calls_are_counted_with_object_model();
        dedup_fallbacks_state_chaining_and_half_lines();
    }

    #[test]
    fn subagent_workspace_inheritance_and_discovery() {
        subagent_log_without_metadata_inherits_parent_workspace();
        collect_sessions_skips_tool_outputs_and_spool_dirs();
    }
}
