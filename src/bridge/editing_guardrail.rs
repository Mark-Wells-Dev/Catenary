// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Cross-session per-root editing guardrail.
//!
//! Prevents concurrent editing across daemon sessions in the same
//! workspace root. When one session enters editing mode
//! (`start_editing`) for a root, other sessions are blocked with an
//! actionable message recommending git worktrees.
//!
//! Read operations (grep, glob, hover, definition, references) are
//! unaffected — only `start_editing` is gated.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Builds the guidance message for a blocked editing attempt.
fn guidance(root: &Path) -> String {
    let dir_name = root
        .file_name()
        .unwrap_or(root.as_os_str())
        .to_string_lossy();
    format!(
        "Another session is editing {}.\n\
         You can continue editing files not in {dir_name}.\n\
         Consider if a git worktree is appropriate.",
        root.display(),
    )
}

/// Cross-session per-root editing guardrail.
///
/// Shared via `Arc` between all per-session [`super::session::Session`]
/// instances in a daemon. In single-session mode, this type is not
/// instantiated — the guardrail field on `Session` is `None`.
pub struct EditingGuardrail {
    /// Root → `session_id` of the session currently editing.
    locks: Mutex<HashMap<PathBuf, String>>,
}

impl Default for EditingGuardrail {
    fn default() -> Self {
        Self::new()
    }
}

impl EditingGuardrail {
    /// Creates a new guardrail with no active locks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Attempts to acquire the editing lock for a single root.
    ///
    /// Returns `Ok(())` if the root is unlocked or already locked by
    /// the same session (idempotent re-entry).
    ///
    /// # Errors
    ///
    /// Returns a guidance message if the root is locked by another session.
    pub fn try_acquire(&self, root: &Path, session_id: &str) -> Result<(), String> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if locks.get(root).is_some_and(|holder| holder != session_id) {
            return Err(guidance(root));
        }
        locks.insert(root.to_path_buf(), session_id.to_string());
        drop(locks);
        Ok(())
    }

    /// Releases the editing lock for a root if held by this session.
    pub fn release(&self, root: &Path, session_id: &str) {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if locks.get(root).is_some_and(|holder| holder == session_id) {
            locks.remove(root);
        }
    }

    /// Releases all editing locks held by a session.
    ///
    /// Called on session disconnect to prevent stuck locks when a
    /// session crashes or is killed while in editing mode.
    pub fn release_all(&self, session_id: &str) {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, holder| holder != session_id);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn editing_guardrail_single_session_succeeds() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("should succeed");
    }

    #[test]
    fn editing_guardrail_concurrent_same_root_blocked() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("session A acquires");
        let err = g
            .try_acquire(Path::new("/foo"), "session-b")
            .expect_err("session B should be blocked");
        assert!(
            err.contains("Another session is editing /foo"),
            "should identify the locked root, got: {err}",
        );
    }

    #[test]
    fn editing_guardrail_concurrent_different_roots() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("session A acquires /foo");
        g.try_acquire(Path::new("/bar"), "session-b")
            .expect("session B acquires /bar");
    }

    #[test]
    fn editing_guardrail_done_editing_releases_lock() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("acquire");
        g.release(Path::new("/foo"), "session-a");
        g.try_acquire(Path::new("/foo"), "session-b")
            .expect("session B should succeed after release");
    }

    #[test]
    fn editing_guardrail_disconnect_releases_lock() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("acquire");
        g.try_acquire(Path::new("/bar"), "session-a")
            .expect("acquire");

        // Simulate disconnect.
        g.release_all("session-a");

        g.try_acquire(Path::new("/foo"), "session-b")
            .expect("should succeed after disconnect");
        g.try_acquire(Path::new("/bar"), "session-b")
            .expect("should succeed after disconnect");
    }

    #[test]
    fn editing_guardrail_same_session_re_enters() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("first acquire");
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("idempotent re-entry should succeed");
    }

    #[test]
    fn editing_guardrail_read_operations_unblocked() {
        // The guardrail only gates `start_editing` — read operations
        // (grep, glob, hover, definition, references) never call the
        // guardrail. Verify that acquiring a lock does not prevent
        // a different session from acquiring a *different* root,
        // demonstrating that the lock is per-root, not global.
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("session A edits /foo");
        // Session B can still operate on other roots.
        g.try_acquire(Path::new("/bar"), "session-b")
            .expect("session B can work in /bar");
    }

    #[test]
    fn editing_guardrail_guidance_message_content() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/projects/my-app"), "session-a")
            .expect("acquire");
        let err = g
            .try_acquire(Path::new("/projects/my-app"), "session-b")
            .expect_err("should be blocked");
        assert!(
            err.contains("Another session is editing /projects/my-app"),
            "should identify locked root, got: {err}",
        );
        assert!(
            err.contains("not in my-app"),
            "should use dir name in guidance, got: {err}",
        );
        assert!(
            err.contains("git worktree"),
            "should mention worktree, got: {err}",
        );
    }

    #[test]
    fn editing_guardrail_release_wrong_session_is_noop() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("acquire");
        // Release by a different session should not affect the lock.
        g.release(Path::new("/foo"), "session-b");
        g.try_acquire(Path::new("/foo"), "session-b")
            .expect_err("lock should still be held by session-a");
    }

    #[test]
    fn editing_guardrail_release_all_only_affects_target() {
        let g = EditingGuardrail::new();
        g.try_acquire(Path::new("/foo"), "session-a")
            .expect("a acquires /foo");
        g.try_acquire(Path::new("/bar"), "session-b")
            .expect("b acquires /bar");

        g.release_all("session-a");

        // /foo is now free, /bar still held by session-b.
        g.try_acquire(Path::new("/foo"), "session-c")
            .expect("/foo should be free");
        g.try_acquire(Path::new("/bar"), "session-c")
            .expect_err("/bar should still be held by session-b");
    }
}
