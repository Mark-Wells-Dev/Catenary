// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Per-session `additionalContext` queue for the parent agent (misc 151, D-1).
//!
//! The retired notification queue carried the dirty-worktree "kept" notice to
//! the **user** as a `systemMessage`; the TUI problems pane now owns the durable
//! user surface (tui-rework 03/04). But that notice's genuinely *actionable*
//! audience is the **parent agent** that spawned the worktree — it can land the
//! work or remove it. Claude Code delivers agent-facing context via hook-response
//! `additionalContext`, and the `SubagentStop` response goes to the *stopping
//! subagent*, not the parent, so the parent leg must ride a *later* parent hook
//! response.
//!
//! This queue is that side channel: [`queue`](ParentContextQueue::queue) pushes a
//! context line keyed by the parent's `session_id`; the hook-dispatch path drains
//! it ([`drain`](ParentContextQueue::drain)) on the parent's next eligible hook
//! response (`PreToolUse` / `Stop` when allowing) and emits it as
//! `hookSpecificOutput.additionalContext`. Session-scoped, drained on delivery,
//! dropped on session end ([`remove_session`](ParentContextQueue::remove_session)).
//!
//! Unlike the retired notification queue this has no severity filter, dedup, or
//! server affinity: entries are pushed explicitly by the daemon for a specific
//! parent, never derived from arbitrary tracing events.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

/// Maximum queued context lines per session before drop-oldest overflow.
///
/// A dirty-worktree notice is pushed at most once per worktree; a session
/// accumulating dozens of undelivered notices means the parent never ran a hook
/// (no delivery point). The cap bounds that pathological case.
const CAP: usize = 64;

/// Per-session queue of `additionalContext` payloads bound for the parent agent.
///
/// Shared across the daemon: the primary session owns one instance and every
/// per-session [`Session`](super::session::Session) clones the same `Arc`, so a
/// notice pushed against a parent's `session_id` from the `SubagentStop` handler
/// is visible to that parent's own hook dispatch.
#[derive(Default)]
pub struct ParentContextQueue {
    queues: Mutex<HashMap<String, VecDeque<String>>>,
}

impl ParentContextQueue {
    /// Create an empty queue.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue a context line for `session_id`'s parent agent.
    ///
    /// The oldest entry is evicted when the session's queue is at [`CAP`].
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the guard must outlive the borrowed per-session queue"
    )]
    pub fn queue(&self, session_id: &str, context: String) {
        let mut queues = self.lock();
        let queue = queues.entry(session_id.to_string()).or_default();
        if queue.len() >= CAP {
            let _ = queue.pop_front();
        }
        queue.push_back(context);
    }

    /// Drain and return `session_id`'s queued context lines in FIFO order.
    ///
    /// Returns an empty vec for a session with nothing queued (the common case,
    /// checked cheaply on every eligible hook response).
    #[must_use]
    pub fn drain(&self, session_id: &str) -> Vec<String> {
        let mut queues = self.lock();
        queues
            .get_mut(session_id)
            .map(|q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Drop a session's queue (session end). Idempotent.
    pub fn remove_session(&self, session_id: &str) {
        self.lock().remove(session_id);
    }

    /// Number of queued context lines for a session (test accessor).
    #[must_use]
    pub fn queue_len(&self, session_id: &str) -> usize {
        self.lock().get(session_id).map_or(0, VecDeque::len)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VecDeque<String>>> {
        self.queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use super::CAP;
    use super::ParentContextQueue;

    #[test]
    fn queue_then_drain_returns_fifo() {
        let q = ParentContextQueue::new();
        q.queue("sess", "first".to_string());
        q.queue("sess", "second".to_string());
        assert_eq!(q.queue_len("sess"), 2);

        let drained = q.drain("sess");
        assert_eq!(drained, vec!["first".to_string(), "second".to_string()]);
        // Drain clears the queue.
        assert_eq!(q.queue_len("sess"), 0);
        assert!(q.drain("sess").is_empty());
    }

    #[test]
    fn drain_unknown_session_is_empty() {
        let q = ParentContextQueue::new();
        assert!(q.drain("nobody").is_empty());
    }

    #[test]
    fn queues_are_session_scoped() {
        let q = ParentContextQueue::new();
        q.queue("parent-a", "for a".to_string());
        q.queue("parent-b", "for b".to_string());

        assert_eq!(q.drain("parent-a"), vec!["for a".to_string()]);
        assert_eq!(q.drain("parent-b"), vec!["for b".to_string()]);
    }

    #[test]
    fn remove_session_drops_pending() {
        let q = ParentContextQueue::new();
        q.queue("sess", "pending".to_string());
        q.remove_session("sess");
        assert!(q.drain("sess").is_empty());
        // Idempotent.
        q.remove_session("sess");
    }

    #[test]
    fn overflow_drops_oldest() {
        let q = ParentContextQueue::new();
        for i in 0..=CAP {
            q.queue("sess", format!("notice-{i}"));
        }
        assert_eq!(q.queue_len("sess"), CAP);
        let drained = q.drain("sess");
        // The oldest (notice-0) was evicted; the newest survived.
        assert_eq!(drained.first().map(String::as_str), Some("notice-1"));
        assert_eq!(
            drained.last().map(String::as_str),
            Some(&*format!("notice-{CAP}"))
        );
    }
}
