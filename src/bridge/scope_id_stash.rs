// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Scope parent ID stash for hook → MCP → hook parent_id propagation.
//!
//! The `PreToolUse` hook stashes its correlation ID before each tool call.
//! The MCP handler peeks it (for the incoming `tools/call` event's
//! `parent_id`), and the `PostToolUse` hook takes it (for the hook
//! request event's `parent_id`, then clears).
//!
//! A [`Condvar`] serializes overlapping stash attempts: if a previous
//! scope ID is pending when a new `PreToolUse` fires, the hook blocks
//! until the `PostToolUse` hook clears it (or a ~5 s timeout expires).

use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Timeout for blocking when a previous scope ID has not been consumed.
const STASH_TIMEOUT: Duration = Duration::from_secs(5);

/// Single-slot scope ID stash shared between hook dispatch and MCP handler.
///
/// The slot holds at most one pending scope ID (the pre-tool hook's
/// correlation ID). The [`Condvar`] ensures that a second `stash` call
/// blocks until the first is cleared by [`Self::take`], preventing scope
/// mismatch when tool calls overlap (e.g. subagent concurrency).
pub struct ScopeIdStash {
    slot: Mutex<Option<i64>>,
    consumed: Condvar,
}

impl Default for ScopeIdStash {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            consumed: Condvar::new(),
        }
    }
}

impl ScopeIdStash {
    /// Creates an empty stash.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stash a scope parent ID for the upcoming MCP call and post-tool hook.
    ///
    /// If a previous scope ID is still pending, blocks until it is cleared
    /// by [`Self::take`] or [`STASH_TIMEOUT`] expires. On timeout the stale
    /// entry is overwritten and a warning is logged.
    pub fn stash(&self, scope_id: i64) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if slot.is_some() {
            let result = self
                .consumed
                .wait_timeout_while(slot, STASH_TIMEOUT, |s| s.is_some());

            slot = match result {
                Ok((guard, wait)) => {
                    if wait.timed_out() {
                        tracing::warn!(
                            source = crate::source::Source::HookDispatch.as_str(),
                            "scope_id stash timeout \u{2014} overwriting unconsumed entry",
                        );
                    }
                    guard
                }
                Err(e) => e.into_inner().0,
            };
        }

        *slot = Some(scope_id);
    }

    /// Read the stashed scope ID without clearing it.
    ///
    /// Returns `None` if nothing was stashed (e.g. no pre-tool hook fired,
    /// or the tool was invoked without host CLI integration).
    pub fn peek(&self) -> Option<i64> {
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take the stashed scope ID, clearing the slot.
    ///
    /// Returns `None` if nothing was stashed. Wakes any blocked
    /// [`Self::stash`] call.
    pub fn take(&self) -> Option<i64> {
        let id = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if id.is_some() {
            self.consumed.notify_one();
        }
        id
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn stash_and_peek() {
        let stash = ScopeIdStash::new();
        stash.stash(42);
        assert_eq!(stash.peek(), Some(42));
        // Peek does not consume.
        assert_eq!(stash.peek(), Some(42));
    }

    #[test]
    fn stash_and_take() {
        let stash = ScopeIdStash::new();
        stash.stash(42);
        assert_eq!(stash.take(), Some(42));
    }

    #[test]
    fn take_empty_returns_none() {
        let stash = ScopeIdStash::new();
        assert!(stash.take().is_none());
    }

    #[test]
    fn peek_empty_returns_none() {
        let stash = ScopeIdStash::new();
        assert!(stash.peek().is_none());
    }

    #[test]
    fn take_clears_slot() {
        let stash = ScopeIdStash::new();
        stash.stash(1);
        let _ = stash.take();
        assert!(stash.take().is_none());
        assert!(stash.peek().is_none());
    }

    #[test]
    fn stash_blocks_until_taken() {
        let stash = Arc::new(ScopeIdStash::new());
        stash.stash(10);

        let stash2 = Arc::clone(&stash);
        let handle = std::thread::spawn(move || {
            // This should block until 10 is taken.
            stash2.stash(20);
        });

        // Give the spawned thread time to block.
        std::thread::sleep(Duration::from_millis(50));

        // Take the first entry — unblocks the spawned thread.
        let first = stash.take();
        assert_eq!(first, Some(10));

        handle.join().expect("stash thread should finish");

        assert_eq!(stash.peek(), Some(20));
        assert_eq!(stash.take(), Some(20));
    }

    #[test]
    fn stash_without_prior_entry_does_not_block() {
        let stash = ScopeIdStash::new();
        stash.stash(99);
        assert_eq!(stash.take(), Some(99));
    }
}
