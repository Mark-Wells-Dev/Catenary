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
use std::sync::atomic::{AtomicUsize, Ordering};

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

/// In-memory editing state manager.
///
/// Owns editing state for a single Catenary session.
/// [`super::hook_router::HookRouter`] (which has the real `agent_id` from
/// the host CLI) accesses this through [`super::session::Session`].
///
/// State is keyed by a composite of `(session_id, agent_id)` to prevent
/// cross-session collisions when multiple host CLI sessions share a
/// workspace and route hooks to the same Catenary instance.
pub struct EditingManager {
    /// Active editing sessions: composite key → accumulated file paths.
    state: Mutex<HashMap<String, Vec<PathBuf>>>,
    /// Number of files skipped during accumulation because they lacked
    /// LSP coverage (outside tracked roots). Reset by
    /// [`drain_all_and_clear`](Self::drain_all_and_clear) and
    /// [`clear_all`](Self::clear_all).
    filtered_count: AtomicUsize,
}

impl Default for EditingManager {
    fn default() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            filtered_count: AtomicUsize::new(0),
        }
    }
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
        state.insert(key, Vec::new());
        drop(state);
        Ok(())
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
            .is_some_and(|files| !files.is_empty())
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
        if let Some(files) = state.get_mut(&key)
            && !files.contains(&path)
        {
            files.push(path);
        }
    }

    /// Returns and clears accumulated file paths for an agent.
    pub fn drain_files(&self, session_id: Option<&str>, agent_id: &str) -> Vec<PathBuf> {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.get_mut(&key).map(std::mem::take).unwrap_or_default()
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
    pub fn increment_filtered(&self) {
        self.filtered_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Drains accumulated file paths from all agents and clears all
    /// editing state. Returns the combined file list and the number of
    /// files that were filtered (skipped due to no LSP coverage).
    ///
    /// Used by the MCP `done_editing` tool, which does not carry an
    /// `agent_id` and cannot rely on [`active_agent`] to find the
    /// correct key.
    pub fn drain_all_and_clear(&self) -> (Vec<PathBuf>, usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let files: Vec<PathBuf> = state.values_mut().flat_map(std::mem::take).collect();
        state.clear();
        drop(state);
        let filtered = self.filtered_count.swap(0, Ordering::Relaxed);
        (files, filtered)
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
        drop(state);
        self.filtered_count.store(0, Ordering::Relaxed);
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
    fn drain_all_and_clear_collects_and_clears() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.add_file(None, "agent-a", PathBuf::from("/src/main.rs"));
        em.add_file(None, "agent-a", PathBuf::from("/src/lib.rs"));
        em.increment_filtered();

        let (files, filtered) = em.drain_all_and_clear();
        assert_eq!(files.len(), 2);
        assert_eq!(filtered, 1);
        assert!(!em.is_editing(None, "agent-a"));

        // Empty when nothing is editing
        let (files, filtered) = em.drain_all_and_clear();
        assert!(files.is_empty());
        assert_eq!(filtered, 0);
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
