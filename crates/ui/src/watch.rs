//! Live source watching: one persistent `notify` recursive watcher over adapter
//! `watch_roots`, debounced so a burst of log writes collapses into one
//! targeted rescan of only the affected adapter(s). Armed only in 仅文件变更 mode
//! (`refresh_secs == 0`) — in timer mode per-file writes must not defeat the
//! configured cadence, so the periodic Tick is the sole refresh source there.

use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows_reactor::CancellationToken;

/// Persistent recursive watcher reused across `Msg::WatchFired` turns.
/// Holding one OS watcher for the life of a watch generation avoids destroying
/// and recreating `ReadDirectoryChangesW` threads and directory handles on
/// every write, and routes events to only the adapter(s) whose roots matched.
pub struct SourceWatcher {
    _watcher: notify::RecommendedWatcher,
    rx: Mutex<Receiver<notify::Result<notify::Event>>>,
    roots: Vec<(&'static str, Vec<String>)>,
    watched_count: usize,
}

impl SourceWatcher {
    /// Open one watcher over all existing `(adapter_id, roots)` pairs.
    /// Returns `None` if no root is watchable.
    pub fn open(entries: Vec<(&'static str, Vec<PathBuf>)>) -> Option<Arc<Self>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .ok()?;

        let mut unique_roots: Vec<PathBuf> = Vec::new();
        let mut routed: Vec<(&'static str, Vec<String>)> = Vec::new();
        for (id, roots) in entries {
            let mut norm_roots = Vec::new();
            for r in roots {
                if !r.is_dir() {
                    continue;
                }
                if !unique_roots.contains(&r) {
                    unique_roots.push(r.clone());
                }
                norm_roots.push(norm_prefix(&r));
            }
            if !norm_roots.is_empty() {
                routed.push((id, norm_roots));
            }
        }

        let mut watched_count = 0usize;
        for root in &unique_roots {
            if watcher.watch(root, RecursiveMode::Recursive).is_ok() {
                watched_count += 1;
            }
        }
        if watched_count == 0 {
            return None;
        }
        Some(Arc::new(Self {
            _watcher: watcher,
            rx: Mutex::new(rx),
            roots: routed,
            watched_count,
        }))
    }

    pub fn watched_count(&self) -> usize {
        self.watched_count
    }

    /// Block until any non-ignorable change lands, drain ~1.2 s of trailing
    /// writes (capped at 8 s), and return the matched adapter ids.
    ///
    /// - `Some(ids)` with non-empty `ids`: scan only those adapters.
    /// - `Some(vec![])`: a real path changed that didn't match a known prefix
    ///   (e.g. symlink alias) — caller should run a full scan.
    /// - `None`: watcher disconnected or cancelled.
    pub fn wait_next(&self, token: &CancellationToken) -> Option<Vec<&'static str>> {
        let rx = self
            .rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut hits: Vec<PathBuf> = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Ok(ev)) => hits.extend(ev.paths.into_iter().filter(|p| !ignorable(p))),
                Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => return None,
                Err(RecvTimeoutError::Timeout) if token.is_cancelled() => return None,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if !hits.is_empty() {
                break;
            }
        }
        // Drain trailing events so a burst of writes = one rescan; cap total
        // debounce at 8 s so a continuously-writing tool doesn't starve the UI.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while let Ok(Ok(ev)) = rx.recv_timeout(Duration::from_millis(1200)) {
            if token.is_cancelled() {
                return None;
            }
            if std::time::Instant::now() > deadline {
                break;
            }
            hits.extend(ev.paths.into_iter().filter(|p| !ignorable(p)));
        }
        if token.is_cancelled() {
            return None;
        }
        let matched = match_adapters(&self.roots, &hits);
        if crate::diag_enabled() {
            let mut disp: Vec<String> = hits.iter().map(|p| p.display().to_string()).collect();
            disp.sort();
            disp.dedup();
            crate::diag!(
                "[watch] {} path(s) -> {:?}: {}",
                disp.len(),
                matched,
                disp.join(" | ")
            );
        }
        Some(matched)
    }
}

/// Normalize a path into a forward-slash string (lowercased on Windows) for
/// reliable prefix matching and noise filtering.
fn norm_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s);
    if cfg!(windows) {
        s.to_ascii_lowercase()
    } else {
        s.to_string()
    }
}

fn norm_prefix(p: &Path) -> String {
    let mut s = norm_path(p);
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// Sidecar, temporary, index, and high-churn non-usage paths that must never
/// trigger a rescan.
///
/// Crucially: our own read-only WAL opens update `*-shm`, so letting `-shm`
/// through would cause an infinite self-rescan loop. `*-wal` stays watched on
/// real usage DBs (except known index DBs like `conversation_summaries.db-wal`
/// and `session-index.db-wal`).
pub(crate) fn ignorable(p: &Path) -> bool {
    let s = norm_path(p);
    if s.ends_with("-shm")
        || s.ends_with("-journal")
        || p.extension().is_some_and(|ext| {
            ext.eq_ignore_ascii_case("tmp")
                || ext.eq_ignore_ascii_case("lock")
                || ext.eq_ignore_ascii_case("part")
        })
    {
        return true;
    }
    let file_name = s.rsplit('/').next().unwrap_or("");
    if file_name.starts_with("conversation_summaries.db")
        || file_name.starts_with("session-index.db")
    {
        return true;
    }
    // Muse tool output spools — never contain `session.jsonl`.
    if s.contains("/tool-outputs/") || s.ends_with("/tool-outputs") || s.contains("/.spool/") {
        return true;
    }
    // Antigravity non-conversation runtime chatter (if legacy parent root is watched).
    if s.contains("/.gemini/antigravity/") && !s.contains("/.gemini/antigravity/conversations/") {
        for noise in [
            "/brain/",
            "/cache/",
            "/presence/",
            "/daemon/",
            "/persistent_terminals/",
            "/crashes/",
            "/annotations/",
            "/knowledge/",
            "/sidecar_data/",
            "/plugin_data/",
            "/builtin/",
            "/bin/",
            "/lib/",
        ] {
            if s.contains(noise) {
                return true;
            }
        }
    }
    false
}

fn match_adapters(roots: &[(&'static str, Vec<String>)], paths: &[PathBuf]) -> Vec<&'static str> {
    let mut matched: Vec<&'static str> = Vec::new();
    for path in paths {
        let norm = norm_path(path);
        for (id, prefixes) in roots {
            let hit = prefixes.iter().any(|pref| {
                norm == *pref
                    || norm
                        .strip_prefix(pref.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            });
            if hit && !matched.contains(id) {
                matched.push(id);
            }
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignorable_filters_wal_shm_indexes_and_chatter_but_keeps_real_sources() {
        assert!(ignorable(Path::new(
            r"C:\Users\u\.gemini\antigravity\conversations\a.db-shm"
        )));
        assert!(ignorable(Path::new(
            r"C:\Users\u\.gemini\antigravity\conversation_summaries.db-wal"
        )));
        assert!(ignorable(Path::new(
            r"C:\Users\u\.gemini\antigravity\brain\abc\scratch\x.py"
        )));
        assert!(ignorable(Path::new(
            r"C:\Users\u\.gemini\antigravity\presence\heartbeat.json"
        )));
        assert!(ignorable(Path::new(
            r"C:\Users\u\.local\share\muse\sessions\2026\11\20\s1\tool-outputs\out.txt"
        )));
        assert!(ignorable(Path::new(
            r"C:\Users\u\.local\share\muse\sessions\session-index.db-wal"
        )));
        assert!(ignorable(Path::new(
            r"C:\Users\u\.codex\sessions\s1.jsonl.tmp"
        )));

        // Real usage sources must pass through.
        assert!(!ignorable(Path::new(
            r"C:\Users\u\.gemini\antigravity\conversations\a.db-wal"
        )));
        assert!(!ignorable(Path::new(
            r"C:\Users\u\.gemini\antigravity\conversations\a.db"
        )));
        assert!(!ignorable(Path::new(
            r"C:\Users\u\.claude\projects\C--brain-cache\sess.jsonl"
        )));
        assert!(!ignorable(Path::new(
            r"C:\Users\u\.local\share\muse\sessions\2026\11\20\s1\session.jsonl"
        )));
        assert!(!ignorable(Path::new(
            r"C:\Users\u\.codex\sessions\2026\rollout.jsonl"
        )));
    }

    #[test]
    fn match_adapters_routes_paths_to_owning_adapters_only() {
        let roots = vec![
            (
                "claude",
                vec![norm_prefix(Path::new(r"C:\Users\u\.claude\projects"))],
            ),
            (
                "codex",
                vec![norm_prefix(Path::new(r"C:\Users\u\.codex\sessions"))],
            ),
            (
                "gemini_antigravity",
                vec![norm_prefix(Path::new(
                    r"C:\Users\u\.gemini\antigravity\conversations",
                ))],
            ),
        ];
        assert_eq!(
            match_adapters(
                &roots,
                &[PathBuf::from(r"C:\Users\u\.claude\projects\p1\a.jsonl")]
            ),
            vec!["claude"]
        );
        assert_eq!(
            match_adapters(
                &roots,
                &[
                    PathBuf::from(r"c:\users\u\.gemini\antigravity\conversations\x.db-wal"),
                    PathBuf::from(r"C:\Users\u\.codex\sessions\2026\a.jsonl"),
                ]
            ),
            vec!["gemini_antigravity", "codex"]
        );
        // Prefix boundary: `.codex\sessions-old` must not match `.codex\sessions`.
        assert_eq!(
            match_adapters(
                &roots,
                &[PathBuf::from(r"C:\Users\u\.codex\sessions-old\a.jsonl")]
            ),
            Vec::<&str>::new()
        );
    }
}
