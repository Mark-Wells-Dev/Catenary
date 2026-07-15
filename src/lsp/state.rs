// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Server state and progress tracking types.

use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tracing::debug;

/// Token type for progress tracking.
pub type ProgressToken = String;

/// State of an active progress operation.
#[derive(Debug, Clone)]
pub struct ProgressState {
    /// The title of the progress operation.
    pub title: String,
    /// The optional progress message.
    pub message: Option<String>,
    /// The optional progress percentage (0-100).
    pub percentage: Option<u32>,
    /// When the operation started.
    pub started: Instant,
}

/// Server lifecycle state.
///
/// A single enum that tracks the server from spawn through shutdown.
/// Carries data where needed (`Busy` holds the in-flight progress count).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerLifecycle {
    /// Spawned, init handshake not complete.
    Initializing,
    /// Init complete, server unproven. Diagnostics path blocks,
    /// tool requests proceed (self-testing).
    Probing,
    /// Proven healthy, idle, accepts requests.
    Healthy,
    /// Progress work **announced but not yet started**: the server has
    /// requested one or more work-done tokens via
    /// `window/workDoneProgress/create` and opened no `$/progress` `begin`
    /// bracket for any of them yet. Carries the outstanding created-token count
    /// (always >= 1). This is the spec-defined pending half — the LSP-native
    /// analog of an unconsumed input queue (misc 200): an announced token is
    /// unfinished work the settle seams must hold on, distinct from `Busy` so
    /// the evidence surfaces can say "announced, not yet started" rather than
    /// overloading `Busy`. Rides the same `Stuck` ceiling as a hung `Busy`
    /// bracket — no new clock.
    Pending(u32),
    /// Server declared active via progress tokens. Carries
    /// `in_progress_count` (always >= 1).
    Busy(u32),
    /// Health probe failed or init error. Shut down.
    Failed,
    /// Connection lost / process died.
    Dead,
}

impl ServerLifecycle {
    /// Returns whether the server is in a terminal state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed | Self::Dead)
    }

    /// Returns the display state string for TUI/CLI.
    #[must_use]
    pub const fn display_state(&self) -> &str {
        match self {
            Self::Initializing | Self::Probing => "initializing",
            Self::Healthy => "ready",
            // Announced-but-not-started work still reads as "busy" to the agent:
            // the server has unfinished work coming (misc 200). The non-lossy
            // snapshot string ([`Self::lifecycle_str`]) keeps it distinct.
            Self::Pending(_) | Self::Busy(_) => "busy",
            Self::Failed | Self::Dead => "dead",
        }
    }

    /// Returns the non-lossy lifecycle string for the `state.json` snapshot.
    ///
    /// Unlike [`Self::display_state`] — which collapses `Probing` into
    /// `"initializing"` and both terminal states into `"dead"` (the ws25
    /// "stuck initializing" bug) — this returns the six distinct variants so a
    /// stuck `probing` is observable. `Busy(_)` maps to `"busy"` regardless of
    /// the in-flight count, which the snapshot carries separately. `Pending(_)`
    /// is distinct from `"busy"` so an announced-but-not-started token
    /// (misc 200) is observable as "announced, not yet started".
    #[must_use]
    pub const fn lifecycle_str(&self) -> &str {
        match self {
            Self::Initializing => "initializing",
            Self::Probing => "probing",
            Self::Healthy => "healthy",
            Self::Pending(_) => "pending",
            Self::Busy(_) => "busy",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }
}

impl Serialize for ServerLifecycle {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.display_state())
    }
}

/// Detailed status for a single LSP server.
#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    /// The language ID this server handles.
    pub language: String,
    /// Server name (binary / config entry name).
    pub server_name: String,
    /// Scope kind string ("workspace", "root", etc.).
    pub scope_kind: String,
    /// Scope root path (empty string for scopeless variants).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope_root: String,
    /// Current server lifecycle state.
    pub state: ServerLifecycle,
    /// Active progress title, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_title: Option<String>,
    /// Active progress message, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_message: Option<String>,
    /// Active progress percentage, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_percentage: Option<u32>,
    /// Seconds since spawn.
    pub uptime_secs: u64,
}

/// Manages progress state for a single LSP client.
///
/// The single work-done-token registry (misc 200). It folds two sources into
/// one lifecycle: tokens **announced** via `window/workDoneProgress/create`
/// (held in `created`, the "announced, not yet started" set) and tokens
/// **begun** via a `$/progress` `begin` (held in `active_progress`). A token
/// rides the lifecycle Created → Begun → Ended: `create` inserts into
/// `created`; a `begin` promotes it out of `created` into `active_progress`
/// (and arms `begin` independently, so a sloppy server that begins a token it
/// never created still counts); an `end` retires it from `active_progress`.
/// Outstanding = created ∪ begun; the server holds settle while any token is
/// outstanding.
#[derive(Debug, Default)]
pub struct ProgressTracker {
    active_progress: HashMap<ProgressToken, ProgressState>,
    /// Tokens the server announced via `window/workDoneProgress/create` but
    /// has not yet opened a `$/progress` `begin` bracket for — the pending
    /// "announced, not yet started" half (misc 200). A `begin` moves a token
    /// out of this set into `active_progress`; an `end` on a
    /// never-begun token drops it from here.
    created: HashSet<ProgressToken>,
    /// Last broadcast title (used to deduplicate monitor output).
    last_broadcast_title: Option<String>,
    /// Last broadcast percentage (used to deduplicate monitor output).
    last_broadcast_percentage: Option<u32>,
}

impl ProgressTracker {
    /// Creates a new `ProgressTracker`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a token **announced** via `window/workDoneProgress/create`.
    ///
    /// Records the token in the created set (the "announced, not yet started"
    /// half). Idempotent, and a no-op if the token has already begun — a
    /// begin that raced ahead of the create ack already owns the token.
    pub fn create(&mut self, token: &str) {
        if self.active_progress.contains_key(token) {
            return;
        }
        self.created.insert(token.to_string());
    }

    /// Number of tokens announced but not yet begun (the pending half).
    #[must_use]
    pub fn created_count(&self) -> u32 {
        u32::try_from(self.created.len()).unwrap_or(u32::MAX)
    }

    /// Number of tokens with an open `$/progress` `begin` bracket (the busy
    /// half).
    #[must_use]
    pub fn begun_count(&self) -> u32 {
        u32::try_from(self.active_progress.len()).unwrap_or(u32::MAX)
    }

    /// Whether any token is outstanding — created ∪ begun. The server holds
    /// settle while this is `true` (misc 200).
    #[must_use]
    pub fn has_outstanding(&self) -> bool {
        !self.active_progress.is_empty() || !self.created.is_empty()
    }

    /// Update state from a progress notification.
    ///
    /// `token` is the canonicalized progress token (string form).
    /// `value` is the raw `WorkDoneProgress` payload from `$/progress`.
    pub fn update(&mut self, token: &str, value: &Value) {
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => {
                // The token has started: promote it out of the announced set
                // into the begun map. A begin on a token never created still
                // arms here (sloppy servers, misc 200) — the remove is a no-op.
                self.created.remove(token);
                self.active_progress.insert(
                    token.to_string(),
                    ProgressState {
                        title: value
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        message: value
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        percentage: value
                            .get("percentage")
                            .and_then(Value::as_u64)
                            .and_then(|n| u32::try_from(n).ok()),
                        started: Instant::now(),
                    },
                );
            }
            Some("report") => {
                if let Some(state) = self.active_progress.get_mut(token) {
                    if let Some(msg) = value.get("message").and_then(Value::as_str) {
                        state.message = Some(msg.to_string());
                    }
                    if let Some(pct) = value
                        .get("percentage")
                        .and_then(Value::as_u64)
                        .and_then(|n| u32::try_from(n).ok())
                    {
                        state.percentage = Some(pct);
                    }
                }
            }
            Some("end") => {
                self.active_progress.remove(token);
                // An `end` on a token that was announced but never begun
                // retires it from the pending set too (misc 200).
                self.created.remove(token);
            }
            other => {
                debug!("Unknown progress kind: {:?}", other);
            }
        }
    }

    /// Returns true if server is busy with any progress operations.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        !self.active_progress.is_empty()
    }

    /// Returns the most significant active progress (longest running or lowest percentage).
    #[must_use]
    pub fn primary_progress(&self) -> Option<&ProgressState> {
        self.active_progress
            .values()
            .min_by_key(|p| p.percentage.unwrap_or(0))
    }

    /// Returns `true` if the primary progress has changed since the last broadcast.
    ///
    /// Compares title and percentage only — per-file message changes are not
    /// considered meaningful for monitor output, since LSP servers like
    /// rust-analyzer send a notification for every individual file scanned.
    /// Updates the cached state when returning `true`.
    pub fn broadcast_changed(&mut self) -> bool {
        let (title, pct) = self
            .primary_progress()
            .map_or((None, None), |p| (Some(p.title.clone()), p.percentage));
        if title == self.last_broadcast_title && pct == self.last_broadcast_percentage {
            return false;
        }
        self.last_broadcast_title = title;
        self.last_broadcast_percentage = pct;
        true
    }

    /// Clear all progress (e.g., on reconnect or server death). Drops both the
    /// begun brackets and the announced-but-not-started tokens (misc 200).
    pub fn clear(&mut self) {
        self.active_progress.clear();
        self.created.clear();
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_progress_begin_end() {
        let mut tracker = ProgressTracker::new();
        assert!(!tracker.is_busy());

        // Begin progress
        let begin = json!({"kind": "begin", "title": "Indexing", "message": "src/main.rs", "percentage": 0});
        tracker.update("indexing", &begin);

        assert!(tracker.is_busy());
        let primary = tracker.primary_progress().expect("active progress");
        assert_eq!(primary.title, "Indexing");
        assert_eq!(primary.message, Some("src/main.rs".to_string()));
        assert_eq!(primary.percentage, Some(0));

        // End progress
        let end = json!({"kind": "end"});
        tracker.update("indexing", &end);

        assert!(!tracker.is_busy());
    }

    #[test]
    fn test_progress_report() {
        let mut tracker = ProgressTracker::new();

        // Begin
        let begin = json!({"kind": "begin", "title": "Indexing", "percentage": 0});
        tracker.update("indexing", &begin);

        // Report progress
        let report = json!({"kind": "report", "message": "50% done", "percentage": 50});
        tracker.update("indexing", &report);

        let primary = tracker.primary_progress().expect("active progress");
        assert_eq!(primary.percentage, Some(50));
        assert_eq!(primary.message, Some("50% done".to_string()));
    }

    #[test]
    fn test_multiple_progress_tokens() {
        let mut tracker = ProgressTracker::new();

        // Begin two progress operations
        let begin1 = json!({"kind": "begin", "title": "Indexing", "percentage": 50});
        let begin2 = json!({"kind": "begin", "title": "Analyzing", "percentage": 10});
        tracker.update("indexing", &begin1);
        tracker.update("analyzing", &begin2);

        assert!(tracker.is_busy());

        // Primary should be the one with lower percentage
        let primary = tracker.primary_progress().expect("active progress");
        assert_eq!(primary.title, "Analyzing");
        assert_eq!(primary.percentage, Some(10));

        // End one
        let end1 = json!({"kind": "end"});
        tracker.update("indexing", &end1);

        assert!(tracker.is_busy());
        let primary = tracker.primary_progress().expect("active progress");
        assert_eq!(primary.title, "Analyzing");
    }

    #[test]
    fn lifecycle_display_state() {
        assert_eq!(
            ServerLifecycle::Initializing.display_state(),
            "initializing"
        );
        assert_eq!(ServerLifecycle::Probing.display_state(), "initializing");
        assert_eq!(ServerLifecycle::Healthy.display_state(), "ready");
        assert_eq!(ServerLifecycle::Busy(1).display_state(), "busy");
        assert_eq!(ServerLifecycle::Busy(3).display_state(), "busy");
        assert_eq!(ServerLifecycle::Failed.display_state(), "dead");
        assert_eq!(ServerLifecycle::Dead.display_state(), "dead");
    }

    #[test]
    fn lifecycle_str_is_non_lossy() {
        // Unlike display_state, every variant maps to a distinct string and
        // Probing is not collapsed into "initializing".
        assert_eq!(
            ServerLifecycle::Initializing.lifecycle_str(),
            "initializing"
        );
        assert_eq!(ServerLifecycle::Probing.lifecycle_str(), "probing");
        assert_eq!(ServerLifecycle::Healthy.lifecycle_str(), "healthy");
        assert_eq!(ServerLifecycle::Busy(1).lifecycle_str(), "busy");
        assert_eq!(ServerLifecycle::Busy(7).lifecycle_str(), "busy");
        assert_eq!(ServerLifecycle::Failed.lifecycle_str(), "failed");
        assert_eq!(ServerLifecycle::Dead.lifecycle_str(), "dead");
    }

    #[test]
    fn lifecycle_is_terminal() {
        assert!(!ServerLifecycle::Initializing.is_terminal());
        assert!(!ServerLifecycle::Probing.is_terminal());
        assert!(!ServerLifecycle::Healthy.is_terminal());
        assert!(!ServerLifecycle::Busy(1).is_terminal());
        assert!(ServerLifecycle::Failed.is_terminal());
        assert!(ServerLifecycle::Dead.is_terminal());
    }

    #[test]
    fn lifecycle_serializes_to_display_state() {
        let json = serde_json::to_string(&ServerLifecycle::Healthy).expect("serialize");
        assert_eq!(json, "\"ready\"");

        let json = serde_json::to_string(&ServerLifecycle::Busy(2)).expect("serialize");
        assert_eq!(json, "\"busy\"");

        let json = serde_json::to_string(&ServerLifecycle::Dead).expect("serialize");
        assert_eq!(json, "\"dead\"");
    }

    // ── Pending lifecycle exposure (misc 200) ───────────────────────

    #[test]
    fn pending_is_distinct_in_snapshot_but_busy_to_the_agent() {
        // The non-lossy snapshot string keeps "announced, not yet started"
        // distinct from an open bracket; the coarse agent view collapses both
        // to "busy" (the server has unfinished work either way).
        assert_eq!(ServerLifecycle::Pending(1).lifecycle_str(), "pending");
        assert_eq!(ServerLifecycle::Pending(4).lifecycle_str(), "pending");
        assert_eq!(ServerLifecycle::Pending(1).display_state(), "busy");
        assert!(!ServerLifecycle::Pending(1).is_terminal());
        // Serialization rides display_state, so a Pending server reads "busy"
        // on the query surface.
        let json = serde_json::to_string(&ServerLifecycle::Pending(2)).expect("serialize");
        assert_eq!(json, "\"busy\"");
    }

    // ── Token registry: Created → Begun → Ended (misc 200) ──────────

    #[test]
    fn create_registers_the_announced_pending_token() {
        // A created-but-not-begun token is outstanding (holds settle) but is
        // NOT "busy" — busy is reserved for an open `begin` bracket.
        let mut tracker = ProgressTracker::new();
        assert!(!tracker.has_outstanding());

        tracker.create("tok-1");
        assert_eq!(tracker.created_count(), 1);
        assert_eq!(tracker.begun_count(), 0);
        assert!(tracker.has_outstanding(), "an announced token holds settle");
        assert!(!tracker.is_busy(), "announced is not begun");
    }

    #[test]
    fn begin_promotes_a_created_token_out_of_pending() {
        // The spec-proper sequence: create then begin. The begin moves the
        // token from the announced set into the begun map — busy, not pending.
        let mut tracker = ProgressTracker::new();
        tracker.create("tok-1");
        assert_eq!(tracker.created_count(), 1);

        tracker.update(
            "tok-1",
            &json!({"kind": "begin", "title": "Initializing workspace"}),
        );
        assert_eq!(tracker.created_count(), 0, "no longer merely announced");
        assert_eq!(tracker.begun_count(), 1, "now a live bracket");
        assert!(tracker.has_outstanding());
        assert!(tracker.is_busy());
    }

    #[test]
    fn end_retires_the_token_and_releases() {
        // Full cycle create → begin → end drops the token entirely.
        let mut tracker = ProgressTracker::new();
        tracker.create("tok-1");
        tracker.update("tok-1", &json!({"kind": "begin", "title": "Checking"}));
        tracker.update("tok-1", &json!({"kind": "end"}));
        assert_eq!(tracker.created_count(), 0);
        assert_eq!(tracker.begun_count(), 0);
        assert!(
            !tracker.has_outstanding(),
            "a fully-retired token releases settle"
        );
    }

    #[test]
    fn end_on_a_created_never_begun_token_releases() {
        // A server that announces then retires a token WITHOUT ever beginning
        // (a cancelled/aborted announcement) drops it from the pending set.
        let mut tracker = ProgressTracker::new();
        tracker.create("tok-1");
        assert!(tracker.has_outstanding());
        tracker.update("tok-1", &json!({"kind": "end"}));
        assert!(
            !tracker.has_outstanding(),
            "an end on a never-begun token still retires it"
        );
    }

    #[test]
    fn begin_arms_independently_of_create() {
        // Sloppy servers send `$/progress begin` on a token they never
        // announced via create. That coverage must not regress — the begin
        // arms the busy state on its own.
        let mut tracker = ProgressTracker::new();
        tracker.update(
            "never-created",
            &json!({"kind": "begin", "title": "Indexing"}),
        );
        assert_eq!(tracker.created_count(), 0);
        assert_eq!(tracker.begun_count(), 1);
        assert!(tracker.is_busy());
    }

    #[test]
    fn create_is_idempotent() {
        let mut tracker = ProgressTracker::new();
        tracker.create("tok-1");
        tracker.create("tok-1");
        assert_eq!(tracker.created_count(), 1, "create is idempotent");
    }

    #[test]
    fn a_redundant_create_does_not_resurrect_a_begun_token() {
        // A begin that already owns the token means a later, redundant create
        // (a create ack racing behind the begin) must not put it back in the
        // pending set.
        let mut tracker = ProgressTracker::new();
        tracker.update("tok", &json!({"kind": "begin", "title": "x"}));
        tracker.create("tok");
        assert_eq!(tracker.created_count(), 0, "a begun token is not pending");
        assert_eq!(tracker.begun_count(), 1);
    }

    #[test]
    fn clear_drops_both_created_and_begun() {
        // Server death (on_shutdown) clears the whole registry — both halves.
        let mut tracker = ProgressTracker::new();
        tracker.create("tok-created");
        tracker.update("tok-begun", &json!({"kind": "begin", "title": "x"}));
        assert!(tracker.has_outstanding());

        tracker.clear();
        assert_eq!(tracker.created_count(), 0);
        assert_eq!(tracker.begun_count(), 0);
        assert!(!tracker.has_outstanding());
    }

    #[test]
    fn mixed_created_and_begun_are_both_counted() {
        let mut tracker = ProgressTracker::new();
        tracker.create("announced");
        tracker.update("live", &json!({"kind": "begin", "title": "x"}));
        assert_eq!(tracker.created_count(), 1);
        assert_eq!(tracker.begun_count(), 1);
        assert!(tracker.has_outstanding());
        // Retiring the begun one leaves the announced one still holding.
        tracker.update("live", &json!({"kind": "end"}));
        assert_eq!(tracker.begun_count(), 0);
        assert_eq!(tracker.created_count(), 1);
        assert!(
            tracker.has_outstanding(),
            "the still-announced token holds after the other ends"
        );
    }
}
