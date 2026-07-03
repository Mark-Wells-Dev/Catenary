// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! In-memory editing state manager.
//!
//! Tracks which agents are in editing mode and accumulates modified file
//! paths during editing sessions. State is per-session lifetime — no
//! database persistence needed since the session owns the [`super::session::Session`]
//! which owns the `EditingManager`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

/// Computes the editing state key from session and agent identifiers.
///
/// The key is `"{session_id}\0{agent_id}"` when a non-empty session ID is
/// present, falling back to just `agent_id` for backward compatibility
/// with hosts that don't provide a session ID.
fn editing_key(session_id: Option<&str>, agent_id: &str) -> String {
    match session_id {
        Some(sid) if !sid.is_empty() => format!("{sid}\0{agent_id}"),
        _ => agent_id.to_string(),
    }
}

/// Per-agent editing accumulator.
///
/// Holds both the accumulated covered file paths and the count of files
/// skipped during accumulation because they lacked LSP coverage (outside
/// tracked roots). Keeping the filtered count alongside the file set — rather
/// than as a single session-global counter — means a per-agent drain reports
/// the requesting agent's own skipped-no-coverage count, never another agent's
/// (bug 37).
#[derive(Default)]
struct EditingState {
    /// Accumulated covered file paths for this agent.
    files: Vec<PathBuf>,
    /// Files skipped during accumulation for lack of LSP coverage.
    filtered: usize,
}

/// In-memory editing state manager.
///
/// Owns editing state for a single Catenary session.
/// [`super::hook_router::HookRouter`] (which has the real `agent_id` from
/// the host CLI) accesses this through [`super::session::Session`].
///
/// State is keyed by a composite of `(session_id, agent_id)` to prevent
/// cross-session collisions when multiple host CLI sessions share a
/// workspace and route hooks to the same Catenary instance.
#[derive(Default)]
pub struct EditingManager {
    /// Active editing sessions: composite key → per-agent accumulator.
    state: Mutex<HashMap<String, EditingState>>,
}

impl EditingManager {
    /// Creates a new `EditingManager` with no active editing sessions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters editing mode for an agent.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is already in editing mode.
    pub fn start_editing(&self, session_id: Option<&str>, agent_id: &str) -> Result<()> {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.contains_key(&key) {
            return Err(anyhow!("agent is already in editing mode"));
        }
        state.insert(key, EditingState::default());
        drop(state);
        Ok(())
    }

    /// Returns `true` if any agent in this session has an active editing
    /// accumulator.
    ///
    /// Session-wide (not keyed by agent): drives the snapshot session board's
    /// `editing` status (observability ticket 05). An accumulator is present
    /// from `start_editing` until `drain_all_and_clear` / `done_editing`, even
    /// before any file is added.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Returns `true` if the agent is currently in editing mode.
    #[must_use]
    pub fn is_editing(&self, session_id: Option<&str>, agent_id: &str) -> bool {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key)
    }

    /// Returns `true` if the agent has accumulated any files during editing.
    #[must_use]
    pub fn has_files(&self, session_id: Option<&str>, agent_id: &str) -> bool {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .is_some_and(|state| !state.files.is_empty())
    }

    /// Returns a snapshot of the accumulated file paths without draining them.
    ///
    /// Unlike [`drain_files`](Self::drain_files) this leaves the editing state
    /// intact — used to render the boundary-block message, which lists the
    /// tracked files while the set must remain for `catenary diagnostics`.
    #[must_use]
    pub fn files(&self, session_id: Option<&str>, agent_id: &str) -> Vec<PathBuf> {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|state| state.files.clone())
            .unwrap_or_default()
    }

    /// Returns the count of files skipped during accumulation for lack of LSP
    /// coverage, without draining the editing state.
    ///
    /// Companion to [`files`](Self::files): together they let the
    /// `pre-tool/editing-stop` prepare hook *snapshot* the accumulated set —
    /// file list plus filtered count — into the handoff without clearing the
    /// accumulator. The clear is deferred to the consume step (drain-on-consume,
    /// bug 32), so a failed `catenary diagnostics` attempt that never consumes
    /// leaves the set intact for a retry.
    #[must_use]
    pub fn filtered(&self, session_id: Option<&str>, agent_id: &str) -> usize {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map_or(0, |state| state.filtered)
    }

    /// Accumulates a modified file path for an agent in editing mode.
    ///
    /// Idempotent — duplicate paths are not added.
    pub fn add_file(&self, session_id: Option<&str>, agent_id: &str, path: PathBuf) {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.get_mut(&key)
            && !entry.files.contains(&path)
        {
            entry.files.push(path);
        }
    }

    /// Returns and clears accumulated file paths for an agent.
    pub fn drain_files(&self, session_id: Option<&str>, agent_id: &str) -> Vec<PathBuf> {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .get_mut(&key)
            .map(|entry| std::mem::take(&mut entry.files))
            .unwrap_or_default()
    }

    /// Exits editing mode for an agent, removing the entry entirely.
    pub fn done_editing(&self, session_id: Option<&str>, agent_id: &str) {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.remove(&key);
    }

    /// Records that a file was skipped during accumulation because it
    /// lacked LSP coverage (outside tracked workspace roots).
    ///
    /// Counted per `(session_id, agent_id)` so the drain reports the
    /// requesting agent's own skipped count (bug 37). A no-op if the agent
    /// is not in editing mode — the accumulation path only calls this for an
    /// agent already known to be editing.
    pub fn increment_filtered(&self, session_id: Option<&str>, agent_id: &str) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            entry.filtered += 1;
        }
    }

    /// Drains accumulated file paths for a single agent and removes its
    /// editing entry. Returns the agent's file list and its own filtered
    /// count (files skipped during accumulation for lack of LSP coverage).
    ///
    /// Scoped to one `(session_id, agent_id)` key so one agent's
    /// `catenary diagnostics` does not consume a sibling agent's accumulated
    /// set when both share a Catenary session (bug 37). This is the drain the
    /// `pre-tool/editing-stop` hook uses: the hook carries the real
    /// `agent_id` from the host CLI.
    pub fn drain_and_clear(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
    ) -> (Vec<PathBuf>, usize) {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .remove(&key)
            .map(|entry| (entry.files, entry.filtered))
            .unwrap_or_default()
    }

    /// Drops the named file paths from an agent's accumulated set, leaving any
    /// unlisted files in place. Returns the paths actually removed (a path not
    /// in the set is a no-op).
    ///
    /// The per-file counterpart to [`drain_and_clear`](Self::drain_and_clear):
    /// `catenary diagnostics <paths>` pays the editing debt for exactly those
    /// files (ws37 ticket 02, decision 3) — diagnosing a file pays its debt
    /// regardless of clean/dirty — leaving the rest of the bucket armed. It
    /// drops only listed files (never `clear_all`, bug 37); when that empties
    /// the file list the whole `(session_id, agent_id)` entry is removed, so a
    /// bucket a scoped pull drained looks identical to a bare drain (`is_active`
    /// / `is_editing` go false, and the caller releases the guardrail). Matching
    /// is by exact path equality; the caller resolves relative paths to absolute
    /// before dispatch.
    pub fn drop_files(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
        files: &[PathBuf],
    ) -> Vec<PathBuf> {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = state.get_mut(&key) else {
            return Vec::new();
        };
        let mut removed = Vec::new();
        entry.files.retain(|p| {
            if files.contains(p) {
                removed.push(p.clone());
                false
            } else {
                true
            }
        });
        if entry.files.is_empty() {
            state.remove(&key);
        }
        removed
    }

    /// Clears all editing state. Returns the number of entries removed.
    ///
    /// Used by `SessionStart` cleanup to clear stale state when the
    /// agent's context is reset.
    pub fn clear_all(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = state.len();
        state.clear();
        count
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn start_editing_enters_mode() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("should succeed");
        assert!(em.is_editing(None, "agent-a"));
    }

    #[test]
    fn start_editing_already_editing_errors() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("first call");
        let err = em
            .start_editing(None, "agent-a")
            .expect_err("should error on duplicate");
        assert!(
            err.to_string().contains("already in editing mode"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn is_editing_false_when_not_started() {
        let em = EditingManager::new();
        assert!(!em.is_editing(None, "agent-a"));
    }

    #[test]
    fn add_file_accumulates() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.add_file(None, "", PathBuf::from("/src/main.rs"));
        em.add_file(None, "", PathBuf::from("/src/lib.rs"));
        let files = em.drain_files(None, "");
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn add_file_deduplicates() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.add_file(None, "", PathBuf::from("/src/main.rs"));
        em.add_file(None, "", PathBuf::from("/src/main.rs"));
        let files = em.drain_files(None, "");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn add_file_ignored_when_not_editing() {
        let em = EditingManager::new();
        em.add_file(None, "ghost", PathBuf::from("/src/main.rs"));
        assert!(em.drain_files(None, "ghost").is_empty());
    }

    #[test]
    fn has_files_tracks_accumulation() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        assert!(!em.has_files(None, ""), "no files yet");
        em.add_file(None, "", PathBuf::from("/src/main.rs"));
        assert!(em.has_files(None, ""), "file added");
    }

    #[test]
    fn has_files_false_when_not_editing() {
        let em = EditingManager::new();
        assert!(!em.has_files(None, "ghost"));
    }

    #[test]
    fn files_snapshots_without_draining() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.add_file(None, "", PathBuf::from("/src/main.rs"));
        em.add_file(None, "", PathBuf::from("/src/lib.rs"));

        let snapshot = em.files(None, "");
        assert_eq!(
            snapshot,
            vec![PathBuf::from("/src/main.rs"), PathBuf::from("/src/lib.rs")],
        );
        // Snapshot did not drain — the files are still tracked.
        assert!(em.has_files(None, ""), "files() must not drain");
        assert_eq!(em.drain_files(None, "").len(), 2);
    }

    #[test]
    fn files_empty_when_not_editing() {
        let em = EditingManager::new();
        assert!(em.files(None, "ghost").is_empty());
    }

    #[test]
    fn filtered_reads_count_without_draining() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.add_file(None, "", PathBuf::from("/src/main.rs"));
        em.increment_filtered(None, "");
        em.increment_filtered(None, "");

        assert_eq!(em.filtered(None, ""), 2);
        // Reading the filtered count must not drain the file set.
        assert!(em.has_files(None, ""), "filtered() must not drain");
        // It remains readable on a repeat call (no consume).
        assert_eq!(em.filtered(None, ""), 2);
    }

    #[test]
    fn filtered_zero_when_not_editing() {
        let em = EditingManager::new();
        assert_eq!(em.filtered(None, "ghost"), 0);
    }

    #[test]
    fn drain_files_clears() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.add_file(None, "", PathBuf::from("/src/main.rs"));
        let first = em.drain_files(None, "");
        assert_eq!(first.len(), 1);
        let second = em.drain_files(None, "");
        assert!(second.is_empty());
    }

    #[test]
    fn done_editing_removes_entry() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.done_editing(None, "agent-a");
        assert!(!em.is_editing(None, "agent-a"));
        // Can re-enter after done
        em.start_editing(None, "agent-a").expect("re-enter");
        assert!(em.is_editing(None, "agent-a"));
    }

    #[test]
    fn drain_and_clear_collects_and_clears() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.add_file(None, "agent-a", PathBuf::from("/src/main.rs"));
        em.add_file(None, "agent-a", PathBuf::from("/src/lib.rs"));
        em.increment_filtered(None, "agent-a");

        let (files, filtered) = em.drain_and_clear(None, "agent-a");
        assert_eq!(files.len(), 2);
        assert_eq!(filtered, 1);
        assert!(!em.is_editing(None, "agent-a"));

        // Empty when nothing is editing
        let (files, filtered) = em.drain_and_clear(None, "agent-a");
        assert!(files.is_empty());
        assert_eq!(filtered, 0);
    }

    #[test]
    fn drop_files_pays_partial_debt() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.add_file(None, "agent-a", PathBuf::from("/src/a.rs"));
        em.add_file(None, "agent-a", PathBuf::from("/src/b.rs"));

        let removed = em.drop_files(None, "agent-a", &[PathBuf::from("/src/a.rs")]);
        assert_eq!(removed, vec![PathBuf::from("/src/a.rs")]);
        assert_eq!(
            em.files(None, "agent-a"),
            vec![PathBuf::from("/src/b.rs")],
            "the unlisted file survives a scoped drop"
        );
        assert!(
            em.has_files(None, "agent-a"),
            "partial pay leaves the bucket armed"
        );
    }

    #[test]
    fn drop_files_emptying_bucket_removes_entry() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.add_file(None, "agent-a", PathBuf::from("/src/a.rs"));

        let removed = em.drop_files(None, "agent-a", &[PathBuf::from("/src/a.rs")]);
        assert_eq!(removed, vec![PathBuf::from("/src/a.rs")]);
        assert!(
            !em.is_editing(None, "agent-a"),
            "draining the last file removes the entry (mirrors a bare drain)"
        );
        assert!(!em.is_active(), "no accumulator remains");
    }

    #[test]
    fn drop_files_unedited_path_is_noop() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.add_file(None, "agent-a", PathBuf::from("/src/a.rs"));

        let removed = em.drop_files(None, "agent-a", &[PathBuf::from("/src/unedited.rs")]);
        assert!(removed.is_empty(), "a path not in the set drops nothing");
        assert_eq!(
            em.files(None, "agent-a"),
            vec![PathBuf::from("/src/a.rs")],
            "the debt set is unchanged by a no-op drop"
        );
    }

    #[test]
    fn drop_files_missing_entry_is_noop() {
        let em = EditingManager::new();
        let removed = em.drop_files(None, "agent-a", &[PathBuf::from("/src/a.rs")]);
        assert!(removed.is_empty(), "no entry → nothing to drop");
    }

    #[test]
    fn is_active_tracks_any_accumulator() {
        let em = EditingManager::new();
        assert!(!em.is_active(), "no accumulator yet");
        em.start_editing(Some("s1"), "agent").expect("start");
        assert!(em.is_active(), "active after start, even with no files");
        let (_, _) = em.drain_and_clear(Some("s1"), "agent");
        assert!(!em.is_active(), "inactive after drain_and_clear");

        // done_editing on the only entry also clears activity.
        em.start_editing(None, "a").expect("start");
        assert!(em.is_active());
        em.done_editing(None, "a");
        assert!(!em.is_active());
    }

    #[test]
    fn clear_all_empties_state() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start a");
        em.start_editing(None, "agent-b").expect("start b");
        em.add_file(None, "agent-a", PathBuf::from("/src/main.rs"));
        let count = em.clear_all();
        assert_eq!(count, 2);
        assert!(!em.is_editing(None, "agent-a"));
        assert!(!em.is_editing(None, "agent-b"));
    }

    // ── Session-scoped key tests ───────────────────────────────────────

    #[test]
    fn different_sessions_same_agent_id_are_independent() {
        let em = EditingManager::new();
        em.start_editing(Some("session-a"), "")
            .expect("session A start");
        em.start_editing(Some("session-b"), "")
            .expect("session B start");

        assert!(em.is_editing(Some("session-a"), ""));
        assert!(em.is_editing(Some("session-b"), ""));

        em.done_editing(Some("session-a"), "");
        assert!(!em.is_editing(Some("session-a"), ""));
        assert!(em.is_editing(Some("session-b"), ""));
    }

    #[test]
    fn session_scoped_file_accumulation() {
        let em = EditingManager::new();
        em.start_editing(Some("s1"), "").expect("start");
        em.start_editing(Some("s2"), "").expect("start");

        em.add_file(Some("s1"), "", PathBuf::from("/a.rs"));
        em.add_file(Some("s2"), "", PathBuf::from("/b.rs"));

        let s1_files = em.drain_files(Some("s1"), "");
        assert_eq!(s1_files, vec![PathBuf::from("/a.rs")]);

        let s2_files = em.drain_files(Some("s2"), "");
        assert_eq!(s2_files, vec![PathBuf::from("/b.rs")]);
    }

    /// Two agents within ONE session (a subagent and the main agent, which
    /// share `session_id` and differ only by `agent_id`) accumulate into
    /// separate buckets. Draining one agent's bucket must leave the other's
    /// files — and filtered count — intact. Regression guard for bug 37,
    /// where the diagnostics drain flattened every agent's bucket.
    #[test]
    fn agent_scoped_drain_within_one_session() {
        let em = EditingManager::new();
        em.start_editing(Some("S"), "subA").expect("subA start");
        em.start_editing(Some("S"), "").expect("main start");

        // Distinct covered files per agent.
        em.add_file(Some("S"), "subA", PathBuf::from("/a.rs"));
        em.add_file(Some("S"), "", PathBuf::from("/b.rs"));
        // Distinct skipped-no-coverage counts per agent.
        em.increment_filtered(Some("S"), "subA");
        em.increment_filtered(Some("S"), "subA");
        em.increment_filtered(Some("S"), "");

        // Drain only the subagent's bucket.
        let (sub_files, sub_filtered) = em.drain_and_clear(Some("S"), "subA");
        assert_eq!(sub_files, vec![PathBuf::from("/a.rs")]);
        assert_eq!(sub_filtered, 2, "filtered attributed to subA, not shared");
        assert!(
            !em.is_editing(Some("S"), "subA"),
            "subA entry removed after its drain"
        );

        // The main agent's bucket survives untouched.
        assert!(
            em.is_editing(Some("S"), ""),
            "main agent still editing after subA drained"
        );
        assert_eq!(
            em.files(Some("S"), ""),
            vec![PathBuf::from("/b.rs")],
            "main agent's file set survives subA's drain"
        );
        let (main_files, main_filtered) = em.drain_and_clear(Some("S"), "");
        assert_eq!(main_files, vec![PathBuf::from("/b.rs")]);
        assert_eq!(main_filtered, 1, "main agent keeps its own filtered count");
    }

    #[test]
    fn none_session_does_not_collide_with_some_session() {
        let em = EditingManager::new();
        em.start_editing(None, "agent").expect("no session");
        em.start_editing(Some("sess"), "agent")
            .expect("with session");

        assert!(em.is_editing(None, "agent"));
        assert!(em.is_editing(Some("sess"), "agent"));

        em.done_editing(None, "agent");
        assert!(!em.is_editing(None, "agent"));
        assert!(em.is_editing(Some("sess"), "agent"));
    }

    #[test]
    fn editing_key_format() {
        assert_eq!(editing_key(None, "agent"), "agent");
        assert_eq!(editing_key(Some(""), "agent"), "agent");
        assert_eq!(editing_key(Some("sess-1"), ""), "sess-1\0");
        assert_eq!(editing_key(Some("sess-1"), "sub"), "sess-1\0sub");
    }
}
