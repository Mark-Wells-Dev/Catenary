// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Per-session notification routing for multi-session daemon mode.
//!
//! [`NotificationRouter`] is a [`Sink`] that replaces the single
//! [`super::notification_queue::NotificationQueueSink`] when Catenary runs as
//! a daemon. Events above the severity threshold are routed to per-session
//! queues based on `session_id` from the tracing span hierarchy. LSP-scoped
//! events (those with a `server` field but no `session_id`) broadcast to all
//! sessions that have interacted with the affected server (server affinity).
//!
//! Server affinity is recorded as a side effect of event processing: any event
//! carrying both `session_id` and `server` registers the association. This
//! means LSP protocol events emitted within an MCP connection task (which has
//! `session_id` in its span) automatically build affinity without invasive
//! threading through tool servers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use super::{LogEvent, Notification, NotificationKey, Severity, Sink};

/// Maximum queued notifications per session before drop-oldest overflow.
const CAP: usize = 100;

/// Per-session notification state.
struct SessionQueue {
    queue: VecDeque<Notification>,
    seen: HashSet<NotificationKey>,
    dropped: u32,
}

impl SessionQueue {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            seen: HashSet::new(),
            dropped: 0,
        }
    }

    fn enqueue(&mut self, event: &LogEvent<'_>) {
        let key = NotificationKey::from_event(event);
        if !self.seen.insert(key) {
            return;
        }
        if self.queue.len() >= CAP {
            let _ = self.queue.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.queue.push_back(Notification {
            severity: event.severity,
            message: event.message.clone(),
            timestamp: chrono::Utc::now(),
        });
    }

    fn drain(&mut self) -> (Vec<Notification>, u32) {
        let out: Vec<Notification> = self.queue.drain(..).collect();
        let dropped = self.dropped;
        self.dropped = 0;
        (out, dropped)
    }
}

/// Multi-session notification routing sink.
///
/// Routes events to per-session queues based on `session_id`. LSP-scoped
/// events broadcast to sessions with server affinity. Events with neither
/// `session_id` nor `server` are silently dropped (no routing information).
///
/// Affinity is recorded automatically: any event carrying both `session_id`
/// and `server` fields registers the session → server association,
/// regardless of severity level.
pub struct NotificationRouter {
    threshold: Severity,
    state: Mutex<RouterState>,
}

struct RouterState {
    /// Per-session notification queues.
    queues: HashMap<String, SessionQueue>,
    /// Session → set of LSP server names that session has interacted with.
    affinity: HashMap<String, HashSet<String>>,
}

impl NotificationRouter {
    /// Create a new router with the given severity threshold for enqueuing.
    #[must_use]
    pub fn new(threshold: Severity) -> Self {
        Self {
            threshold,
            state: Mutex::new(RouterState {
                queues: HashMap::new(),
                affinity: HashMap::new(),
            }),
        }
    }

    /// Register a session for notification routing.
    ///
    /// Creates an empty queue. Calling this for an already-registered
    /// session is a no-op (preserves existing queue and dedup set).
    pub fn register_session(&self, session_id: &str) {
        let mut state = self.lock();
        state
            .queues
            .entry(session_id.to_string())
            .or_insert_with(SessionQueue::new);
    }

    /// Remove a session and its affinity tracking.
    ///
    /// Drops any undelivered notifications. Safe to call for
    /// already-removed or never-registered sessions.
    pub fn remove_session(&self, session_id: &str) {
        let mut state = self.lock();
        state.queues.remove(session_id);
        state.affinity.remove(session_id);
    }

    /// Drain a session's notification queue.
    ///
    /// Returns notifications in FIFO order plus an overflow sentinel if
    /// entries were dropped. Returns an empty vec for unknown sessions.
    #[must_use]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "MutexGuard must outlive the queue borrow"
    )]
    pub fn drain(&self, session_id: &str) -> Vec<Notification> {
        let mut state = self.lock();
        let Some(queue) = state.queues.get_mut(session_id) else {
            return Vec::new();
        };
        let (mut out, dropped) = queue.drain();
        if dropped > 0 {
            out.push(Notification {
                severity: Severity::Info,
                message: format!("{dropped} notifications dropped"),
                timestamp: chrono::Utc::now(),
            });
        }
        out
    }

    /// Record that a session has interacted with an LSP server.
    ///
    /// Used for broadcasting LSP-scoped events (server crash, init failure)
    /// to affected sessions. Idempotent.
    pub fn record_affinity(&self, session_id: &str, server: &str) {
        let mut state = self.lock();
        state
            .affinity
            .entry(session_id.to_string())
            .or_default()
            .insert(server.to_string());
    }

    /// Number of registered sessions (test accessor).
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.lock().queues.len()
    }

    /// Number of queued notifications for a session (test accessor).
    #[must_use]
    pub fn queue_len(&self, session_id: &str) -> usize {
        self.lock()
            .queues
            .get(session_id)
            .map_or(0, |q| q.queue.len())
    }

    /// Returns the set of servers a session has affinity for (test accessor).
    #[must_use]
    pub fn affinity_for(&self, session_id: &str) -> HashSet<String> {
        self.lock()
            .affinity
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Broadcast an event to all sessions with affinity for the given server.
    fn broadcast_to_affinity(state: &mut RouterState, server: &str, event: &LogEvent<'_>) {
        // Collect matching session IDs first to avoid double borrow.
        let matching: Vec<String> = state
            .affinity
            .iter()
            .filter(|(_, servers)| servers.contains(server))
            .map(|(sid, _)| sid.clone())
            .collect();

        for sid in &matching {
            if let Some(queue) = state.queues.get_mut(sid) {
                queue.enqueue(event);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RouterState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Sink for NotificationRouter {
    #[allow(
        clippy::suspicious_operation_groupings,
        reason = "false positive: event.severity < self.threshold is correct"
    )]
    fn handle(&self, event: &LogEvent<'_>) {
        let has_session = event.session_id.is_some();
        let has_server = event.server.is_some();

        // Fast path: no routing information — skip the lock.
        if !has_session && !has_server {
            return;
        }

        // Below threshold and no affinity to record — skip the lock.
        // Affinity requires both session_id and server.
        if event.severity < self.threshold && !(has_session && has_server) {
            return;
        }

        let mut state = self.lock();

        // Record affinity at any severity: any event with both session_id
        // and server means the session is interacting with that server.
        if let (Some(session_id), Some(server)) = (&event.session_id, &event.server) {
            state
                .affinity
                .entry(session_id.clone())
                .or_default()
                .insert(server.clone());
        }

        // Only enqueue notifications above the threshold.
        if event.severity < self.threshold {
            return;
        }

        if let Some(session_id) = &event.session_id {
            // Session-scoped: route to that session's queue.
            if let Some(queue) = state.queues.get_mut(session_id) {
                queue.enqueue(event);
            }
        } else if let Some(server) = &event.server {
            // LSP-scoped: broadcast to all sessions with affinity.
            Self::broadcast_to_affinity(&mut state, server, event);
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use super::NotificationRouter;
    use crate::logging::{LogEvent, Severity, Sink};

    fn make_event<'a>(
        severity: Severity,
        message: &str,
        server: Option<&str>,
        session_id: Option<&str>,
    ) -> LogEvent<'a> {
        LogEvent {
            severity,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: server.map(str::to_string),
            client: None,
            parent_id: None,
            source: None,
            language: None,
            scope_root: None,
            payload: None,
            session_id: session_id.map(str::to_string),
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn session_scoped_event_routes_correctly() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("session-a");
        router.register_session("session-b");

        router.handle(&make_event(
            Severity::Warn,
            "tool error",
            None,
            Some("session-a"),
        ));

        assert_eq!(router.queue_len("session-a"), 1);
        assert_eq!(router.queue_len("session-b"), 0);

        let drained = router.drain("session-a");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].message, "tool error");
    }

    #[test]
    fn lsp_event_broadcasts_to_affinity() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("session-a");
        router.register_session("session-b");

        // Both sessions interact with rust-analyzer.
        router.record_affinity("session-a", "rust-analyzer");
        router.record_affinity("session-b", "rust-analyzer");

        // Server crash event (no session_id, has server).
        router.handle(&make_event(
            Severity::Error,
            "server crashed",
            Some("rust-analyzer"),
            None,
        ));

        assert_eq!(router.queue_len("session-a"), 1);
        assert_eq!(router.queue_len("session-b"), 1);
    }

    #[test]
    fn lsp_event_skips_unrelated_session() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("session-a");
        router.register_session("session-c");

        router.record_affinity("session-a", "rust-analyzer");
        router.record_affinity("session-c", "pyright");

        // rust-analyzer crash — should reach session-a but not session-c.
        router.handle(&make_event(
            Severity::Error,
            "server crashed",
            Some("rust-analyzer"),
            None,
        ));

        assert_eq!(router.queue_len("session-a"), 1);
        assert_eq!(router.queue_len("session-c"), 0);
    }

    #[test]
    fn drain_returns_queued_notifications() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("s1");

        router.handle(&make_event(Severity::Warn, "one", None, Some("s1")));
        router.handle(&make_event(Severity::Error, "two", None, Some("s1")));
        router.handle(&make_event(
            Severity::Warn,
            "three",
            Some("srv"),
            Some("s1"),
        ));

        let drained = router.drain("s1");
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].message, "one");
        assert_eq!(drained[1].message, "two");
        assert_eq!(drained[2].message, "three");

        // Queue is empty after drain.
        assert!(router.drain("s1").is_empty());
    }

    #[test]
    fn affinity_cleaned_on_disconnect() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("s1");
        router.record_affinity("s1", "rust-analyzer");

        assert!(!router.affinity_for("s1").is_empty());
        assert_eq!(router.session_count(), 1, "session should be registered");

        router.remove_session("s1");

        assert!(router.affinity_for("s1").is_empty());
        assert_eq!(router.session_count(), 0);

        // Broadcast after removal: no crash, no delivery.
        router.handle(&make_event(
            Severity::Error,
            "crash",
            Some("rust-analyzer"),
            None,
        ));
    }

    #[test]
    fn single_file_server_affinity() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("s1");
        router.register_session("s2");

        // Only s1 touches the markdown server.
        router.record_affinity("s1", "marksman");

        router.handle(&make_event(
            Severity::Warn,
            "marksman offline",
            Some("marksman"),
            None,
        ));

        assert_eq!(router.queue_len("s1"), 1);
        assert_eq!(router.queue_len("s2"), 0);
    }

    #[test]
    fn below_threshold_not_enqueued() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("s1");

        router.handle(&make_event(Severity::Info, "routine", None, Some("s1")));
        assert_eq!(router.queue_len("s1"), 0);
    }

    #[test]
    fn automatic_affinity_from_events() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("s1");

        // An info-level LSP event with session_id + server records affinity
        // even though it's below the notification threshold.
        router.handle(&make_event(
            Severity::Info,
            "textDocument/hover",
            Some("rust-analyzer"),
            Some("s1"),
        ));

        assert!(router.affinity_for("s1").contains("rust-analyzer"));

        // Now a server crash broadcasts to s1.
        router.handle(&make_event(
            Severity::Error,
            "crashed",
            Some("rust-analyzer"),
            None,
        ));
        assert_eq!(router.queue_len("s1"), 1);
    }

    #[test]
    fn dedup_within_session() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("s1");

        router.handle(&make_event(
            Severity::Warn,
            "server crashed 3 times",
            Some("ra"),
            Some("s1"),
        ));
        router.handle(&make_event(
            Severity::Warn,
            "server crashed 5 times",
            Some("ra"),
            Some("s1"),
        ));

        assert_eq!(router.queue_len("s1"), 1);
    }

    #[test]
    fn unregistered_session_event_dropped() {
        let router = NotificationRouter::new(Severity::Warn);

        // Event for unknown session: silently dropped.
        router.handle(&make_event(Severity::Warn, "orphan", None, Some("unknown")));
        assert_eq!(router.drain("unknown").len(), 0);
    }

    #[test]
    fn drain_unknown_session_returns_empty() {
        let router = NotificationRouter::new(Severity::Warn);
        assert!(router.drain("nonexistent").is_empty());
    }

    #[test]
    fn drain_includes_overflow_sentinel() {
        let router = NotificationRouter::new(Severity::Warn);
        router.register_session("s1");

        // Fill past CAP (100) with distinct messages. Dedup keys include
        // `server`, so varying it ensures each event is unique.
        for i in 0..=super::CAP {
            router.handle(&make_event(
                Severity::Warn,
                "overflow",
                Some(&format!("server-{i}")),
                Some("s1"),
            ));
        }

        let drained = router.drain("s1");
        // CAP events kept + 1 overflow sentinel appended by drain.
        assert_eq!(
            drained.len(),
            super::CAP + 1,
            "should have CAP events + overflow sentinel"
        );
        let sentinel = &drained[super::CAP];
        assert!(
            sentinel.message.contains("dropped"),
            "sentinel should mention dropped count, got: {:?}",
            sentinel.message
        );
    }
}
