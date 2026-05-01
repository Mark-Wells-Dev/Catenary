// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Pending CWD stash for grep/glob relative-pattern resolution.
//!
//! The `PreToolUse` hook stashes the host CLI's working directory before
//! each Catenary grep or glob call. The MCP handler reads and clears it,
//! then resolves relative patterns against the stashed path.
//!
//! A [`Condvar`] serializes overlapping stash attempts: if a previous cwd
//! is pending when a new `PreToolUse` fires, the hook blocks until the MCP
//! call consumes it (or a ~5 s timeout expires).

use std::path::PathBuf;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Timeout for blocking when a previous cwd has not been consumed.
const STASH_TIMEOUT: Duration = Duration::from_secs(5);

/// Single-slot cwd stash shared between the hook router and MCP handler.
///
/// The slot holds at most one pending cwd. The [`Condvar`] ensures that
/// a second `stash` call blocks until the first is consumed, preventing
/// cwd mismatch when tool calls overlap (e.g. subagent concurrency).
pub struct CwdStash {
    slot: Mutex<Option<PathBuf>>,
    consumed: Condvar,
}

impl Default for CwdStash {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            consumed: Condvar::new(),
        }
    }
}

impl CwdStash {
    /// Creates an empty stash.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stash a cwd for the next grep/glob MCP call.
    ///
    /// If a previous cwd is still pending, blocks until it is consumed
    /// or [`STASH_TIMEOUT`] expires. On timeout the stale entry is
    /// overwritten and a warning is logged.
    pub fn stash(&self, cwd: PathBuf) {
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
                            "cwd stash timeout \u{2014} overwriting unconsumed entry",
                        );
                    }
                    guard
                }
                Err(e) => e.into_inner().0,
            };
        }

        *slot = Some(cwd);
    }

    /// Take the stashed cwd, clearing the slot.
    ///
    /// Returns `None` if nothing was stashed (e.g. no hook fired, or
    /// the tool was invoked without host CLI integration).
    pub fn take(&self) -> Option<PathBuf> {
        let cwd = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if cwd.is_some() {
            // Wake any blocked stash() call.
            self.consumed.notify_one();
        }
        cwd
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn stash_and_take() {
        let stash = CwdStash::new();
        stash.stash(PathBuf::from("/home/user/project"));
        let cwd = stash.take();
        assert_eq!(cwd, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn take_empty_returns_none() {
        let stash = CwdStash::new();
        assert!(stash.take().is_none());
    }

    #[test]
    fn take_clears_slot() {
        let stash = CwdStash::new();
        stash.stash(PathBuf::from("/a"));
        let _ = stash.take();
        assert!(stash.take().is_none());
    }

    #[test]
    fn stash_blocks_until_consumed() {
        let stash = Arc::new(CwdStash::new());
        stash.stash(PathBuf::from("/first"));

        let stash2 = Arc::clone(&stash);
        let handle = std::thread::spawn(move || {
            // This should block until /first is consumed.
            stash2.stash(PathBuf::from("/second"));
        });

        // Give the spawned thread time to block.
        std::thread::sleep(Duration::from_millis(50));

        // Consume the first entry — unblocks the spawned thread.
        let first = stash.take();
        assert_eq!(first, Some(PathBuf::from("/first")));

        handle.join().expect("stash thread should finish");

        let second = stash.take();
        assert_eq!(second, Some(PathBuf::from("/second")));
    }

    #[test]
    fn stash_without_prior_entry_does_not_block() {
        let stash = CwdStash::new();
        // Should return immediately — no prior entry to wait on.
        stash.stash(PathBuf::from("/fast"));
        assert_eq!(stash.take(), Some(PathBuf::from("/fast")));
    }
}
