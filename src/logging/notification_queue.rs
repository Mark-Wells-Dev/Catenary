// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Notification queue sink — severity-filtered, deduped, capped queue for
//! user-facing notifications drained into `systemMessage` at stationary
//! hook points.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use super::LogEvent;
use super::Notification;
use super::NotificationKey;
use super::Severity;
use super::Sink;

/// Maximum queued notifications before drop-oldest overflow.
const CAP: usize = 100;

/// Severity-filtered notification queue with dedup and overflow handling.
///
/// Events below `threshold` are silently dropped. Events whose
/// [`NotificationKey`] has already been seen (within the session lifetime)
/// are deduplicated. When the queue reaches [`CAP`], the oldest entry is
/// evicted and a sentinel is appended on drain.
///
/// The `seen` set persists across drains — dedup is session-scoped, not
/// drain-scoped.
pub struct NotificationQueueSink {
    threshold: Severity,
    state: Mutex<QueueState>,
}

struct QueueState {
    queue: VecDeque<Notification>,
    seen: HashSet<NotificationKey>,
    dropped: u32,
}

impl NotificationQueueSink {
    /// Create a new queue sink with the given severity threshold.
    #[must_use]
    pub fn new(threshold: Severity) -> Arc<Self> {
        Arc::new(Self {
            threshold,
            state: Mutex::new(QueueState {
                queue: VecDeque::new(),
                seen: HashSet::new(),
                dropped: 0,
            }),
        })
    }

    /// Drain the queue. Returns notifications in FIFO order plus an
    /// overflow sentinel if entries were dropped. Clears the queue and
    /// dropped counter but preserves the `seen` set (dedup persists
    /// across drains within the session lifetime).
    #[must_use]
    pub fn drain(&self) -> Vec<Notification> {
        let (mut out, dropped) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let out: Vec<Notification> = state.queue.drain(..).collect();
            let dropped = state.dropped;
            state.dropped = 0;
            drop(state);
            (out, dropped)
        };
        if dropped > 0 {
            out.push(Notification {
                severity: Severity::Info,
                message: format!("{dropped} notifications dropped"),
                timestamp: chrono::Utc::now(),
            });
        }
        out
    }

    /// Number of queued notifications (excludes any future sentinel).
    #[must_use]
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Sink for NotificationQueueSink {
    fn handle(&self, event: &LogEvent<'_>) {
        if event.severity < self.threshold {
            return;
        }
        let key = NotificationKey::from_event(event);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.seen.insert(key) {
            return;
        }
        if state.queue.len() >= CAP {
            let _ = state.queue.pop_front();
            state.dropped = state.dropped.saturating_add(1);
        }
        state.queue.push_back(Notification {
            severity: event.severity,
            message: event.message.clone(),
            timestamp: chrono::Utc::now(),
        });
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use super::CAP;
    use super::NotificationQueueSink;
    use crate::logging::LogEvent;
    use crate::logging::Severity;
    use crate::logging::Sink;

    fn make_event(
        severity: Severity,
        message: &str,
        server: Option<&str>,
        source: Option<&str>,
    ) -> LogEvent<'static> {
        LogEvent {
            severity,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: server.map(str::to_string),
            client: None,
            parent_id: None,
            source: source.map(str::to_string),
            language: None,
            payload: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn below_threshold_ignored() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        sink.handle(&make_event(Severity::Debug, "verbose", None, None));
        sink.handle(&make_event(Severity::Info, "routine", None, None));
        assert!(sink.is_empty());
    }

    #[test]
    fn at_threshold_enqueued() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        sink.handle(&make_event(Severity::Warn, "warning", None, None));
        assert_eq!(sink.len(), 1);
        let drained = sink.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].message, "warning");
        assert_eq!(drained[0].severity, Severity::Warn);
    }

    #[test]
    fn above_threshold_enqueued() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        sink.handle(&make_event(Severity::Error, "error", None, None));
        assert_eq!(sink.len(), 1);
        let drained = sink.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].severity, Severity::Error);
    }

    #[test]
    fn dedup_same_stem() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        sink.handle(&make_event(
            Severity::Warn,
            "server crashed 3 times",
            Some("ra"),
            None,
        ));
        sink.handle(&make_event(
            Severity::Warn,
            "server crashed 5 times",
            Some("ra"),
            None,
        ));
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn dedup_across_drains() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        sink.handle(&make_event(Severity::Warn, "offline", None, None));
        let first = sink.drain();
        assert_eq!(first.len(), 1);

        sink.handle(&make_event(Severity::Warn, "offline", None, None));
        let second = sink.drain();
        assert!(second.is_empty());
    }

    #[test]
    fn different_servers_not_deduped() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        sink.handle(&make_event(
            Severity::Warn,
            "server crashed",
            Some("rust-analyzer"),
            None,
        ));
        sink.handle(&make_event(
            Severity::Warn,
            "server crashed",
            Some("pylsp"),
            None,
        ));
        assert_eq!(sink.len(), 2);
    }

    #[test]
    fn overflow_drops_oldest_and_appends_sentinel() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        let servers: Vec<String> = (0..=CAP).map(|i| format!("srv-{i}")).collect();
        for s in &servers {
            sink.handle(&make_event(Severity::Warn, "event", Some(s), None));
        }
        assert_eq!(sink.len(), CAP);

        let drained = sink.drain();
        assert_eq!(drained.len(), CAP + 1);
        assert_eq!(drained[0].message, "event");
        let sentinel = &drained[CAP];
        assert_eq!(sentinel.severity, Severity::Info);
        assert_eq!(sentinel.message, "1 notifications dropped");
    }

    #[test]
    fn drain_clears_dropped_counter() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        let servers: Vec<String> = (0..=CAP).map(|i| format!("srv-{i}")).collect();
        for s in &servers {
            sink.handle(&make_event(Severity::Warn, "event", Some(s), None));
        }
        let first = sink.drain();
        assert_eq!(
            first.last().expect("sentinel").message,
            "1 notifications dropped"
        );

        sink.handle(&make_event(Severity::Warn, "fresh", Some("new-srv"), None));
        let second = sink.drain();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].message, "fresh");
    }

    #[test]
    fn drain_empty_returns_empty() {
        let sink = NotificationQueueSink::new(Severity::Warn);
        let drained = sink.drain();
        assert!(drained.is_empty());
    }

    #[test]
    fn threshold_ordering() {
        assert!(Severity::Error >= Severity::Warn);
        assert!(Severity::Warn >= Severity::Warn);
        assert!(Severity::Warn >= Severity::Info);
        assert!(Severity::Info >= Severity::Debug);
        assert!(Severity::Debug < Severity::Info);
        assert!(Severity::Info < Severity::Warn);
    }

    #[test]
    fn threshold_boundary_exact_level() {
        // Event exactly at threshold enqueues; one level below does not.
        for threshold in [Severity::Info, Severity::Warn, Severity::Error] {
            let sink = NotificationQueueSink::new(threshold);

            // At threshold: should enqueue.
            sink.handle(&make_event(threshold, "at threshold", Some("a"), None));
            assert_eq!(sink.len(), 1, "event at {threshold:?} should enqueue");

            // One level below threshold: should not enqueue.
            let below = match threshold {
                Severity::Info => Severity::Debug,
                Severity::Warn => Severity::Info,
                Severity::Error => Severity::Warn,
                Severity::Debug => unreachable!(),
            };
            sink.handle(&make_event(below, "below threshold", Some("b"), None));
            assert_eq!(
                sink.len(),
                1,
                "event at {below:?} should not enqueue when threshold is {threshold:?}"
            );

            // Drain for cleanup.
            let _ = sink.drain();
        }
    }

    #[test]
    fn dedup_with_missing_fields() {
        // Events with absent source/server/language still produce stable keys.
        let sink = NotificationQueueSink::new(Severity::Warn);

        // First event: all identity fields absent.
        sink.handle(&make_event(Severity::Warn, "something broke", None, None));
        assert_eq!(sink.len(), 1);

        // Same message, still all absent: dedup rejects.
        sink.handle(&make_event(Severity::Warn, "something broke", None, None));
        assert_eq!(
            sink.len(),
            1,
            "identical events with all-None fields should dedup"
        );
    }

    #[test]
    fn overflow_sentinel_format() {
        // Verify the exact rendered format: "[info] N notifications dropped".
        let sink = NotificationQueueSink::new(Severity::Warn);

        // Overflow by 3.
        let count = CAP + 3;
        let servers: Vec<String> = (0..count).map(|i| format!("srv-{i}")).collect();
        for s in &servers {
            sink.handle(&make_event(Severity::Warn, "event", Some(s), None));
        }

        let drained = sink.drain();
        let sentinel = drained.last().expect("should have sentinel");
        assert_eq!(sentinel.severity, Severity::Info);
        assert_eq!(sentinel.message, "3 notifications dropped");
        assert_eq!(sentinel.format(), "[info] 3 notifications dropped");
    }
}
