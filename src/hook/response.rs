// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Response builder for `systemMessage` content.
//!
//! [`SystemMessageBuilder`] composes two content surfaces into a single
//! string for the host CLI's `systemMessage` field:
//!
//! - **Direct** — lines built synchronously by the current hook handler
//!   (e.g., config validation warnings at `SessionStart`).
//! - **Background** — lines drained from the notification queue,
//!   accumulated since the last drain point.
//!
//! Visual separation: direct lines come first (they describe what the user
//! just triggered), followed by a header and background lines ("oh by the
//! way, these accumulated").

use crate::logging::notification_queue::NotificationQueueSink;
use crate::logging::{LoggingServer, Severity};

/// Background section header: 3 em-dashes, space, "background", space, 3 em-dashes.
const BACKGROUND_HEADER: &str = "─── background ───";

/// Builder for `systemMessage` content delivered through hook responses.
///
/// Combines direct (synchronous handler messages) and background (notification
/// queue drain) surfaces into a single string. The builder is used on the
/// server side for queue draining and on the CLI side for final composition.
///
/// # Composition rules
///
/// | Direct | Background | Output |
/// |--------|------------|--------|
/// | empty  | empty      | `None` — field omitted |
/// | present| empty      | Direct lines joined by `\n` |
/// | empty  | present    | Header + background lines |
/// | present| present    | Direct lines + separator + header + background lines |
pub struct SystemMessageBuilder {
    direct: Vec<String>,
    background: Vec<String>,
}

impl SystemMessageBuilder {
    /// Create an empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            direct: Vec::new(),
            background: Vec::new(),
        }
    }

    /// Append a line built synchronously by this handler.
    ///
    /// Rendered as `[severity] message`.
    pub fn push_direct(&mut self, severity: Severity, message: &str) {
        self.direct.push(format!("[{}] {message}", severity.tag()));
    }

    /// Drain the notification queue and any sink panic, storing rendered
    /// lines as background content.
    ///
    /// Sink panics (isolated by `catch_unwind` in the Layer) are surfaced
    /// as `[err] sink panic: <message>` so the user sees them exactly once
    /// through the same `systemMessage` channel as other notifications.
    pub fn drain_background(&mut self, queue: &NotificationQueueSink, logging: &LoggingServer) {
        if let Some(panic_msg) = logging.take_sink_panic() {
            self.background.push(format!(
                "[{}] sink panic: {panic_msg}",
                Severity::Error.tag()
            ));
        }
        for notification in &queue.drain() {
            self.background.push(notification.format());
        }
    }

    /// Add a pre-rendered background line.
    ///
    /// Used on the CLI side to reconstitute background content received
    /// from the server's IPC response.
    pub fn push_background(&mut self, line: String) {
        self.background.push(line);
    }

    /// Finalize into the `systemMessage` content string.
    ///
    /// Returns `None` if both surfaces are empty — no `systemMessage`
    /// field should be emitted.
    #[must_use]
    pub fn finish(self) -> Option<String> {
        let has_direct = !self.direct.is_empty();
        let has_background = !self.background.is_empty();

        match (has_direct, has_background) {
            (false, false) => None,
            (true, false) => Some(self.direct.join("\n")),
            (false, true) => {
                let mut out = String::from(BACKGROUND_HEADER);
                out.push('\n');
                out.push_str(&self.background.join("\n"));
                Some(out)
            }
            (true, true) => {
                let mut out = self.direct.join("\n");
                out.push_str("\n\n");
                out.push_str(BACKGROUND_HEADER);
                out.push('\n');
                out.push_str(&self.background.join("\n"));
                Some(out)
            }
        }
    }
}

impl Default for SystemMessageBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#[allow(
    clippy::panic,
    reason = "deliberate panic sinks for testing catch_unwind isolation"
)]
mod tests {
    use super::*;
    use crate::logging::notification_queue::NotificationQueueSink;
    use crate::logging::{LogEvent, LoggingServer, Severity, Sink};
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;

    /// Sink that always panics — used to test `take_sink_panic` surfacing.
    struct PanicSink(&'static str);
    impl crate::logging::Sink for PanicSink {
        fn handle(&self, _event: &LogEvent<'_>) {
            panic!("{}", self.0);
        }
    }

    /// Helper: create a `LogEvent` for feeding into sinks directly.
    fn make_event(severity: Severity, message: &str, server: Option<&str>) -> LogEvent<'static> {
        LogEvent {
            severity,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: server.map(str::to_string),
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    // ── Composition unit tests ─────────────────────────────────────────

    #[test]
    fn empty_builder_returns_none() {
        let builder = SystemMessageBuilder::new();
        assert!(builder.finish().is_none());
    }

    #[test]
    fn direct_only_no_header() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Warn, "config: invalid TOML");
        let result = builder.finish();
        assert_eq!(result.as_deref(), Some("[warn] config: invalid TOML"));
    }

    #[test]
    fn multiple_direct_lines_joined() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Error, "config: removed `inherit` field");
        builder.push_direct(Severity::Warn, "config: deprecated key");
        let result = builder.finish().expect("should have content");
        assert!(result.starts_with("[err]"));
        assert!(result.contains('\n'));
        assert!(result.contains("[warn]"));
        assert!(!result.contains("background"));
    }

    #[test]
    fn background_only_has_header() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_background("[warn] rust-analyzer offline".into());
        let result = builder.finish().expect("should have content");
        assert!(result.starts_with("─── background ───\n"));
        assert!(result.contains("[warn] rust-analyzer offline"));
    }

    #[test]
    fn direct_and_background_separated() {
        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Error, "config error");
        builder.push_background("[warn] server crashed".into());
        let result = builder.finish().expect("should have content");
        let parts: Vec<&str> = result.split("\n\n").collect();
        assert_eq!(
            parts.len(),
            2,
            "expected separator between direct and background"
        );
        assert!(parts[0].starts_with("[err]"));
        assert!(parts[1].starts_with("─── background ───"));
        assert!(parts[1].contains("[warn] server crashed"));
    }

    // ── drain_background tests ─────────────────────────────────────────

    #[test]
    fn drain_background_empties_queue() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        let logging = LoggingServer::new();

        queue.handle(&make_event(Severity::Warn, "server offline", Some("ra")));
        assert_eq!(queue.len(), 1);

        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&queue, &logging);
        assert!(queue.is_empty(), "queue should be empty after drain");

        let result = builder.finish().expect("should have background");
        assert!(result.contains("[warn] server offline"));
    }

    #[test]
    fn drain_background_empty_queue_no_content() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        let logging = LoggingServer::new();

        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&queue, &logging);
        assert!(builder.finish().is_none());
    }

    #[test]
    fn drain_background_surfaces_sink_panic() {
        let logging = LoggingServer::new();
        let queue = NotificationQueueSink::new(Severity::Warn);

        let subscriber = tracing_subscriber::registry().with(logging.clone());
        tracing::subscriber::with_default(subscriber, || {
            logging.activate(vec![Arc::new(PanicSink("connection closed"))]);
            tracing::warn!("trigger");
        });

        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&queue, &logging);
        let result = builder.finish().expect("should surface panic");
        assert!(result.contains("[err] sink panic: connection closed"));
        // take_sink_panic should be cleared after drain.
        assert!(logging.take_sink_panic().is_none());
    }

    #[test]
    fn drain_background_sink_panic_plus_notifications() {
        let logging = LoggingServer::new();
        let queue = NotificationQueueSink::new(Severity::Warn);

        // Populate queue directly.
        queue.handle(&make_event(Severity::Warn, "server offline", Some("ra")));

        // Trigger a sink panic.
        let subscriber = tracing_subscriber::registry().with(logging.clone());
        tracing::subscriber::with_default(subscriber, || {
            logging.activate(vec![Arc::new(PanicSink("db write failed"))]);
            tracing::warn!("trigger");
        });

        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&queue, &logging);
        let result = builder.finish().expect("should have content");
        // Sink panic appears first in background, then queue entries.
        assert!(result.contains("[err] sink panic: db write failed"));
        assert!(result.contains("[warn] server offline"));
    }

    // ── Integration-style: builder + queue + dispatch scenarios ─────────

    #[test]
    fn session_start_empty_queue_empty_direct_no_field() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        let logging = LoggingServer::new();

        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&queue, &logging);
        assert!(builder.finish().is_none());
    }

    #[test]
    fn session_start_direct_only() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        let logging = LoggingServer::new();

        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(Severity::Warn, "config: invalid TOML on line 12");
        builder.drain_background(&queue, &logging);
        let result = builder.finish().expect("should have direct");
        assert_eq!(result, "[warn] config: invalid TOML on line 12");
        assert!(!result.contains("background"));
    }

    #[test]
    fn session_start_direct_and_background() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        let logging = LoggingServer::new();
        queue.handle(&make_event(
            Severity::Error,
            "python-lsp crashed during previous teardown",
            Some("pylsp"),
        ));

        let mut builder = SystemMessageBuilder::new();
        builder.push_direct(
            Severity::Error,
            "config: removed `inherit` field — run `catenary doctor`",
        );
        builder.drain_background(&queue, &logging);
        let result = builder.finish().expect("should have both");
        assert!(result.starts_with("[err] config: removed `inherit` field"));
        assert!(result.contains("─── background ───"));
        assert!(result.contains("[err] python-lsp crashed"));
    }

    #[test]
    fn stop_allow_drains() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        let logging = LoggingServer::new();
        queue.handle(&make_event(Severity::Warn, "ra offline", Some("ra")));

        // Simulate allow: drain background.
        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&queue, &logging);
        let result = builder.finish().expect("should drain");
        assert!(result.contains("[warn] ra offline"));
        assert!(queue.is_empty());
    }

    #[test]
    fn stop_block_preserves_queue() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        queue.handle(&make_event(Severity::Warn, "ra offline", Some("ra")));

        // Simulate block: don't drain.
        assert_eq!(queue.len(), 1, "queue should be preserved when blocking");
    }

    #[test]
    fn dedup_persists_across_blocked_cycle() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        let logging = LoggingServer::new();

        // First cycle: enqueue, block (no drain).
        queue.handle(&make_event(Severity::Warn, "ra offline", Some("ra")));
        assert_eq!(queue.len(), 1);

        // Second cycle: same message enqueued again (dedup rejects).
        queue.handle(&make_event(Severity::Warn, "ra offline", Some("ra")));
        assert_eq!(queue.len(), 1, "dedup should reject duplicate");

        // Allow: drain.
        let mut builder = SystemMessageBuilder::new();
        builder.drain_background(&queue, &logging);
        let result = builder.finish().expect("should have one entry");
        // Header + 1 notification = 2 lines total.
        let line_count = result.lines().count();
        assert_eq!(
            line_count, 2,
            "expected header + 1 notification, got: {result}"
        );
    }

    #[test]
    fn pre_post_tool_hooks_do_not_drain() {
        let queue = NotificationQueueSink::new(Severity::Warn);
        queue.handle(&make_event(Severity::Warn, "offline", Some("ra")));
        queue.handle(&make_event(Severity::Error, "crashed", Some("pylsp")));

        // Simulate 4 PreToolUse fires — no drain, queue untouched.
        for _ in 0..4 {
            assert_eq!(queue.len(), 2, "queue should be untouched by pre/post tool");
        }
    }
}
