//! Freebuff adapter (freebuff.com — the free, ad-funded build of Codebuff).
//!
//! **Desktop** (`Freebuff.app`, Electron + a bun "orchestrator"): one SQLite
//! database per project at
//! `~/.config/freebuff-desktop/projects/<name>-<id>/desktop-v2.db`
//! (`FREEBUFF_DESKTOP_STATE_PATH` moves the `state.json` it sits beside).
//! Each completed turn is a `messages` row whose `metrics_json` carries the
//! turn's summed provider receipts:
//! `{usage:{inputTokens, cachedInputTokens, cachedInputTokensIncluded,
//! outputTokens, reasoningOutputTokens?, totalTokens}, usageIncomplete?,
//! costUsd?}`. `inputTokens` is the provider's prompt total, cache INCLUDED
//! → normalized out. Newer writers say so with `cachedInputTokensIncluded`;
//! the hosted path (v0.0.172, verified on a real turn: 20,356 input of which
//! 20,288 cached, total 20,401 = input + output) omits the flag, so absent
//! that it is classified like ZCode (`zcode::input_includes_cache`). `reasoningOutputTokens` is a subset of `outputTokens` (OpenAI-style),
//! stored alongside as Codex does. `costUsd` is Freebuff's own figure
//! (credits × price; 0 on the free tier) → ProviderReported.
//!
//! The thread's harness decides who owns the usage: `codebuff` is Freebuff's
//! own engine and counted here; `claude-code` / `codex` threads drive the
//! user's local Claude Code / Codex, which write their usual session logs
//! (no config-dir override) — those adapters count them, so they are skipped
//! here (counted in `skipped`).
//!
//! Model: `threads.model` (the thread's current pick; a mid-thread switch
//! attributes earlier turns to the new model — the row has no per-turn model).
//! Hosted picks are stored as opaque catalog handles (`m-00032eaeec`): the
//! app derives a default row's handle as `m-` + the first 10 hex digits of
//! `freebuffLegacyModelDigest("key:" + modelId)` (two 32-bit FNV-style
//! lanes). The handle is resolved back against the known model ids; an
//! unknown one (a model newer than this list) is kept as-is.
//!
//! Threads without a folder live under `<root>/no-folder` → no project.
//!
//! High-water mark: `messages.seq` (AUTOINCREMENT), rescanned with a lookback
//! because a turn's metrics can be rewritten in place after insert
//! (context/compaction updates). Dedup keys are per (project db, seq), so the
//! overlap is idempotent.

use super::{Capability, ScanOutcome, SourceAdapter, SourceItem, SourceKind};
use crate::model::{CostSource, Provenance, UsageEvent, apps};
use crate::normalize::{fnum, input_excludes_cache, num};
use crate::store::Store;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Rows re-read below the watermark each scan (see module docs).
const SEQ_LOOKBACK: i64 = 200;

/// Harnesses whose usage the user's own Claude Code / Codex logs already hold.
const FOREIGN_HARNESSES: &[&str] = &["claude-code", "codex"];

/// Freebuff model ids (desktop v0.0.172 constants) for handle resolution.
const MODEL_IDS: &[&str] = &[
    "mimo/mimo-v2.5",
    "mimo/mimo-v2.5-pro",
    "mimo/mimo-v2.6-pro",
    "z-ai/glm-5.2",
    "z-ai/glm-5.3",
    "z-ai/glm-5.3-flash",
    "z-ai/glm-5.3-flashx",
    "z-ai/glm-5.3-prime",
    "z-ai/glm-5-turbo",
    "deepseek/deepseek-v4-flash",
    "deepseek/deepseek-v4-flash-fast",
    "deepseek/deepseek-v4.1-flash",
    "deepseek/deepseek-v4-pro",
    "minimax/minimax-m3",
    "moonshotai/kimi-k3",
    "moonshotai/kimi-k2.6",
    "moonshotai/kimi-k2.7-code",
    "crof/kimi-k3-eco",
    "openai/gpt-5.5",
    "openai/gpt-5.5-pro",
    "openai/gpt-5.4-pro",
    "openai/gpt-5.6-luna",
    "openai/gpt-5.6-luna-es",
    "openai/gpt-5.6-luna-pro",
    "openai/gpt-5.6-sol",
    "openai/gpt-5.6-sol-pro",
    "openai/gpt-5.6-terra",
    "openai/gpt-5.6-terra-pro",
    "openai/gpt-6-luna",
    "openai/gpt-6-luna-pro",
    "openai/gpt-6-sol",
    "openai/gpt-6-sol-pro",
    "openai/gpt-6.1-sol",
    "openai/gpt-6-astra",
    "openai/gpt-6-astra-pro",
    "openai/o3-pro",
    "anthropic/claude-fable-5.1",
    "anthropic/claude-opus-5.5",
    "anthropic/claude-opus-5",
    "anthropic/claude-sonnet-5",
    "anthropic/claude-opus-4.8",
    "anthropic/claude-sonnet-4.6",
    "google/gemini-3.8-flash",
    "google/gemini-3.7-flash",
    "google/gemini-3.6-flash",
    "google/gemini-3.5-flash",
    "google/gemini-3.1-pro-preview",
    "qwen/qwen3.8-max-prime",
    "qwen/qwen3.8-max-0902",
    "qwen/qwen3.8-flash",
    "qwen/qwen3.8-27b",
    "qwen/qwen3.7-max",
    "qwen/qwen3.7-plus",
    "qwen/qwen3.6-max-preview",
    "qwen/qwen3.6-plus",
    "x-ai/grok-4.7",
    "x-ai/grok-4.6",
    "x-ai/grok-4.5",
    "x-ai/grok-4.20",
    "mistralai/mistral-large",
    "mistralai/codestral-2508",
    "meta-llama/llama-4-maverick",
    "meta/muse-spark-1.2-contributor",
    "meta/muse-spark-1.3-contributor",
    "stealth/ox-alpha",
    "stealth/space-bunny-alpha",
];

/// The desktop's `freebuffLegacyModelDigest` (16 hex chars).
fn legacy_digest(model_id: &str) -> String {
    let (mut h1, mut h2) = (2_166_136_261u32, 1_523_705_862u32);
    for c in format!("freebuff-legacy-model:{model_id}").encode_utf16() {
        h1 = (h1 ^ c as u32).wrapping_mul(16_777_619);
        h2 = (h2 ^ c as u32).wrapping_mul(1_540_483_477);
    }
    format!("{h1:08x}{h2:08x}")
}

/// `m-xxxxxxxxxx` catalog handle → model id; anything else passes through.
fn resolve_model(model: &str) -> String {
    let handle = |id: &str| format!("m-{}", &legacy_digest(&format!("key:{id}"))[..10]);
    match model.starts_with("m-") {
        true => MODEL_IDS
            .iter()
            .find(|id| handle(id) == model)
            .map_or(model, |id| id)
            .to_string(),
        false => model.to_string(),
    }
}

pub struct Freebuff;

/// `~/.config/freebuff-desktop` (or the directory of an overridden state path).
fn desktop_root() -> PathBuf {
    std::env::var_os("FREEBUFF_DESKTOP_STATE_PATH")
        .map(PathBuf::from)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| crate::sync::home(".config/freebuff-desktop"))
}

fn desktop_databases(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join("projects")) else {
        return vec![];
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path().join("desktop-v2.db"))
        .filter(|p| p.is_file())
        .collect();
    out.sort();
    out
}

impl SourceAdapter for Freebuff {
    fn id(&self) -> &'static str {
        apps::FREEBUFF
    }
    fn display_name(&self) -> &'static str {
        "Freebuff"
    }
    fn capability(&self) -> Capability {
        Capability::Precise
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![desktop_root().join("projects")]
    }

    fn discover(&self) -> Result<Vec<SourceItem>> {
        Ok(desktop_databases(&desktop_root())
            .into_iter()
            .map(|p| SourceItem {
                key: p.to_string_lossy().to_string(),
                path: p,
                kind: SourceKind::Sqlite,
            })
            .collect())
    }

    fn scan_sqlite(&self, item: &SourceItem, store: &Store) -> Result<ScanOutcome> {
        let cur = store.load_cursor(&item.key)?;
        let since = (cur.offset as i64 - SEQ_LOOKBACK).max(0);
        let conn = super::opencode::open_ro(&item.path)?;
        let out = scan_desktop(&conn, &item.path, since)?;
        let max_seq = out.1;
        if max_seq as u64 > cur.offset {
            store.save_cursor(self.id(), &item.key, &item.path, max_seq as u64, 0, None)?;
        }
        Ok(out.0)
    }
}

/// Usage rows with `seq > since` → events; also returns the highest seq seen.
fn scan_desktop(
    conn: &rusqlite::Connection,
    path: &Path,
    since: i64,
) -> Result<(ScanOutcome, i64)> {
    // The project directory name (`<name>-<uuid>`) keys the dedup space: seq
    // restarts at 1 in every project database.
    let db_id = path
        .parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut st = conn.prepare(
        "SELECT m.seq, m.thread_id, m.metrics_json, m.ts,
                t.model, t.harness_id, t.byok_connection, t.project_path
         FROM messages m JOIN threads t ON t.id = m.thread_id
         WHERE m.seq > ?1 AND m.metrics_json LIKE '%\"usage\"%'
         ORDER BY m.seq",
    )?;
    let rows = st.query_map(rusqlite::params![since], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
        ))
    })?;
    let mut out = ScanOutcome::default();
    let mut max_seq = since;
    for row in rows {
        let (seq, thread, metrics, ts, model, harness, byok, project) = row?;
        max_seq = max_seq.max(seq);
        if FOREIGN_HARNESSES.contains(&harness.as_deref().unwrap_or("codebuff")) {
            out.skipped += 1;
            continue;
        }
        let Ok(m) = serde_json::from_str::<Value>(&metrics) else {
            continue;
        };
        let u = &m["usage"];
        let input = num(&u["inputTokens"]);
        let cached = num(&u["cachedInputTokens"]);
        let output = num(&u["outputTokens"]);
        if input + cached + output == 0 {
            continue;
        }
        let reasoning = num(&u["reasoningOutputTokens"]);
        let includes = u["cachedInputTokensIncluded"].as_bool() == Some(true)
            || super::zcode::input_includes_cache(
                input,
                output,
                reasoning,
                cached,
                0,
                Some(num(&u["totalTokens"])),
                None,
            );
        let input = if includes {
            input_excludes_cache(input, cached, 0)
        } else {
            input
        };
        // Freebuff's own cost; a free-tier turn without one cost nothing. A
        // BYOK turn bills the user's own key → leave it to the price book.
        let (cost_usd, cost_source) = match (fnum(&m["costUsd"]), byok.is_some()) {
            (Some(c), _) => (Some(c), Some(CostSource::ProviderReported)),
            (None, false) => (Some(0.0), Some(CostSource::ProviderReported)),
            (None, true) => (None, None),
        };
        out.events.push(UsageEvent {
            dedup_key: format!("freebuff:{db_id}:{seq}"),
            app: apps::FREEBUFF.into(),
            session_id: Some(thread),
            project: project.filter(|p| !p.ends_with("freebuff-desktop/no-folder")),
            model: model.as_deref().map(resolve_model),
            request_model: model,
            ts_start: Some(ts),
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: reasoning,
            cache_read_tokens: cached,
            cost_usd,
            cost_source,
            provenance: Provenance::LocalSqlite,
            status: (m["usageIncomplete"].as_bool() == Some(true))
                .then(|| "usage_incomplete".into()),
            raw_ref: Some(format!("{}#messages:{seq}", path.display())),
            ..Default::default()
        });
    }
    Ok((out, max_seq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fixture() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, project_path TEXT, model TEXT,
                                   harness_id TEXT, byok_connection TEXT);
             CREATE TABLE messages (seq INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT,
                                    role TEXT, metrics_json TEXT NOT NULL DEFAULT '{}', ts INTEGER);
             INSERT INTO threads VALUES
               ('t1','/p/app','deepseek/deepseek-v4-flash','codebuff',NULL),
               ('t2','/p/app','claude-opus-5-5','claude-code',NULL),
               ('t3','/p/app','gpt-6.1-sol','codebuff','conn-1');
             INSERT INTO messages (thread_id, role, metrics_json, ts) VALUES
               ('t1','user','{}',1000),
               ('t1','assistant','{\"usage\":{\"inputTokens\":120000,\"cachedInputTokens\":100000,
                 \"cachedInputTokensIncluded\":true,\"outputTokens\":900,\"reasoningOutputTokens\":300,
                 \"totalTokens\":120900},\"costUsd\":0}',2000),
               ('t2','assistant','{\"usage\":{\"inputTokens\":5,\"cachedInputTokens\":0,
                 \"outputTokens\":5,\"totalTokens\":10}}',3000),
               ('t3','assistant','{\"usage\":{\"inputTokens\":50,\"cachedInputTokens\":0,
                 \"cachedInputTokensIncluded\":true,\"outputTokens\":7,\"totalTokens\":57}}',4000);",
        )
        .unwrap();
        c
    }

    #[test]
    fn desktop_turns_are_normalized_and_foreign_harnesses_skipped() {
        let c = fixture();
        let path = Path::new("/x/projects/app-123/desktop-v2.db");
        let (out, max) = scan_desktop(&c, path, 0).unwrap();
        assert_eq!(max, 4);
        assert_eq!(
            out.skipped, 1,
            "claude-code thread belongs to the Claude adapter"
        );
        assert_eq!(out.events.len(), 2);

        let e = &out.events[0];
        assert_eq!(e.dedup_key, "freebuff:app-123:2");
        assert_eq!(e.input_tokens, 20_000, "cache-inclusive input normalized");
        assert_eq!(e.cache_read_tokens, 100_000);
        assert_eq!((e.output_tokens, e.reasoning_tokens), (900, 300));
        assert_eq!(e.model.as_deref(), Some("deepseek/deepseek-v4-flash"));
        assert_eq!(e.cost_usd, Some(0.0));

        // BYOK without a reported cost → priced from the book.
        assert_eq!(out.events[1].cost_usd, None);
        assert_eq!(out.events[1].cost_source, None);

        // The real v0.0.172 row: no inclusion flag, total = input + output.
        c.execute_batch(
            "INSERT INTO threads VALUES ('t4','/u/.config/freebuff-desktop/no-folder','m-00032eaeec','codebuff',NULL);
             INSERT INTO messages (thread_id, role, metrics_json, ts) VALUES ('t4','assistant',
               '{\"usage\":{\"inputTokens\":20356,\"cachedInputTokens\":20288,\"outputTokens\":45,
                 \"reasoningOutputTokens\":20,\"totalTokens\":20401},\"costUsd\":0}',5000);",
        )
        .unwrap();
        let (real, _) = scan_desktop(&c, path, 4).unwrap();
        let e = &real.events[0];
        assert_eq!((e.input_tokens, e.cache_read_tokens), (68, 20_288));
        assert_eq!(e.model.as_deref(), Some("mimo/mimo-v2.5"));
        assert_eq!(e.request_model.as_deref(), Some("m-00032eaeec"));
        assert_eq!(e.project, None, "no-folder threads have no project");

        // Watermark lookback re-reads nothing new beyond the max.
        let (again, _) = scan_desktop(&c, path, 5).unwrap();
        assert!(again.events.is_empty());
    }
}
