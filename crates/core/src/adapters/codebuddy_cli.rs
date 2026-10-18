//! CodeBuddy Code's local Agent workflow traces. One aggregate per trace;
//! spans, heartbeats and lastCall counters are never added to that aggregate.
//! Multi-model traces retain unknown model attribution instead of guessing.

use super::{Capability, ScanOutcome, SourceAdapter, SourceItem, SourceKind};
use crate::{
    Provenance, UsageEvent,
    model::apps,
    normalize::{epoch_ms, num, text},
    store::Store,
};
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;

pub struct CodeBuddyCli;

impl SourceAdapter for CodeBuddyCli {
    fn id(&self) -> &'static str {
        apps::CODEBUDDY_CLI
    }
    fn display_name(&self) -> &'static str {
        "CodeBuddy CLI"
    }
    fn capability(&self) -> Capability {
        Capability::Estimate
    }
    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![crate::sync::home(".codebuddy")]
    }
    fn discover(&self) -> Result<Vec<SourceItem>> {
        Ok(
            crate::sync::collect_files(&crate::sync::home(".codebuddy/traces"), "json", 6)
                .into_iter()
                .map(|path| SourceItem {
                    key: path.to_string_lossy().into_owned(),
                    path,
                    kind: SourceKind::Sqlite,
                })
                .collect(),
        )
    }
    fn scan_sqlite(&self, item: &SourceItem, store: &Store) -> Result<ScanOutcome> {
        let bytes = super::codebuddy::read_json(&item.path)?;
        let hash = super::codebuddy::fingerprint(&bytes);
        if store.load_cursor(&item.key)?.state.as_deref() == Some(hash.as_str()) {
            return Ok(ScanOutcome::default());
        }
        // Partial JSON remains retryable; a file can be finalized in place.
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            return Ok(ScanOutcome::default());
        };
        let trace = &v["trace"];
        let mut out = ScanOutcome::default();
        if let (Some(id), Some(start), Some(end)) = (
            text(&trace["traceId"]),
            epoch_ms(&trace["startedAt"]),
            epoch_ms(&trace["endedAt"]),
        ) {
            let model = &trace["modelInfo"];
            let total = num(&trace["totalTokens"]);
            let input = model["totalInputTokens"].as_u64();
            let output = model["totalOutputTokens"].as_u64();
            let cached = num(&model["totalCachedTokens"]);
            let classified = input
                .zip(output)
                .filter(|(i, o)| i.checked_add(*o) == Some(total) && cached <= *i);
            let (input_tokens, output_tokens, unclassified_tokens) =
                classified.map_or((0, 0, total), |(i, o)| (i - cached, o, 0));
            if total > 0 {
                out.events.push(UsageEvent {
                    dedup_key: format!("codebuddy_cli:trace:{id}"),
                    app: apps::CODEBUDDY_CLI.into(),
                    session_id: Some(id),
                    model: model["models"]
                        .as_array()
                        .filter(|a| a.len() == 1)
                        .and_then(|a| text(&a[0])),
                    ts_start: Some(start),
                    ts_end: (end >= start).then_some(end),
                    duration_ms: trace["duration"].as_i64().filter(|d| *d > 0),
                    input_tokens,
                    output_tokens,
                    cache_read_tokens: if classified.is_some() { cached } else { 0 },
                    unclassified_tokens,
                    status: text(&trace["status"]),
                    provenance: Provenance::LocalJsonl,
                    raw_ref: Some(item.path.to_string_lossy().into_owned()),
                    ..Default::default()
                });
            }
        }
        store.save_cursor(
            self.id(),
            &item.key,
            &item.path,
            bytes.len() as u64,
            0,
            Some(&hash),
        )?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
