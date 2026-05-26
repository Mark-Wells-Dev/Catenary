// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Desktop notification support.
//!
//! Fires OS-level desktop notifications for error-severity tracing events.
//! Best-effort, non-blocking: failures are silently ignored. Suppressed
//! when `CATENARY_NOTIFY=0`.
//!
//! Two integration points:
//!
//! - [`DesktopNotificationSink`] — a [`crate::logging::Sink`] registered
//!   on [`crate::logging::LoggingServer`]. Fires for `error!()` events
//!   with per-daemon-lifetime debounce.
//! - Hook CLI — installs a minimal tracing subscriber with only this sink
//!   so `error!()` events (e.g., daemon unreachable) fire OS notifications
//!   even when the daemon isn't running.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use crate::logging::{LogEvent, Severity, Sink};

/// Whether desktop notifications are enabled.
///
/// Reads `CATENARY_NOTIFY` once at first call. Defaults to enabled.
/// Set `CATENARY_NOTIFY=0` to suppress (CI/headless environments).
fn is_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("CATENARY_NOTIFY").map_or(true, |v| v != "0"))
}

/// Fire an OS-level desktop notification.
///
/// Best-effort, non-blocking. Spawns a platform-specific subprocess
/// and does not wait for it to complete. Silently ignores all failures.
/// No-op when `CATENARY_NOTIFY=0` or during `#[cfg(test)]`.
pub fn notify_desktop(title: &str, body: &str) {
    // Suppress real OS notifications during unit tests.
    if cfg!(test) {
        return;
    }
    if !is_enabled() {
        return;
    }
    let _ = send_notification(title, body);
}

#[cfg(target_os = "linux")]
fn send_notification(title: &str, body: &str) -> Option<()> {
    use std::process::{Command, Stdio};
    Command::new("notify-send")
        .arg("--app-name=Catenary")
        .arg(title)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
        .map(|_| ())
}

#[cfg(target_os = "macos")]
fn send_notification(title: &str, body: &str) -> Option<()> {
    use std::process::{Command, Stdio};
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        body.replace('\\', "\\\\").replace('"', "\\\""),
        title.replace('\\', "\\\\").replace('"', "\\\""),
    );
    Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
        .map(|_| ())
}

#[cfg(target_os = "windows")]
fn send_notification(title: &str, body: &str) -> Option<()> {
    use std::process::{Command, Stdio};
    // Use PowerShell BurntToast or built-in toast API.
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
         $xml = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $nodes = $xml.GetElementsByTagName('text'); \
         $nodes.Item(0).AppendChild($xml.CreateTextNode('{}')) > $null; \
         $nodes.Item(1).AppendChild($xml.CreateTextNode('{}')) > $null; \
         $toast = [Windows.UI.Notifications.ToastNotification]::new($xml); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Catenary').Show($toast)",
        title.replace('\'', "''"),
        body.replace('\'', "''"),
    );
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
        .map(|_| ())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn send_notification(_title: &str, _body: &str) -> Option<()> {
    None
}

/// Tracing sink that fires desktop notifications for error-severity events.
///
/// Debounces by `(title, body)` hash — each unique message fires at most
/// once per daemon lifetime. The hook CLI process is short-lived and
/// self-limiting, so debounce is not needed there (each process creates
/// a fresh sink).
///
/// Disabled when constructed with `enabled = false` (from
/// `[notifications] desktop = false` in user config) or when
/// `CATENARY_NOTIFY=0` is set in the environment.
pub struct DesktopNotificationSink {
    enabled: bool,
    fired: Mutex<HashSet<u64>>,
}

impl DesktopNotificationSink {
    /// Create a new desktop notification sink (enabled by default).
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::with_enabled(true)
    }

    /// Create a desktop notification sink with explicit enable/disable.
    ///
    /// Pass the resolved `desktop` config value. The `CATENARY_NOTIFY=0`
    /// env var still overrides to disabled regardless of this flag.
    #[must_use]
    pub fn with_enabled(enabled: bool) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            fired: Mutex::new(HashSet::new()),
        })
    }
}

impl Sink for DesktopNotificationSink {
    fn handle(&self, event: &LogEvent<'_>) {
        if !self.enabled || event.severity < Severity::Error {
            return;
        }

        let title = "Catenary";
        let body = &event.message;

        let mut hasher = DefaultHasher::new();
        title.hash(&mut hasher);
        body.hash(&mut hasher);
        let hash = hasher.finish();

        let mut fired = self
            .fired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !fired.insert(hash) {
            return;
        }
        drop(fired);

        notify_desktop(title, body);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for assertions")]
mod tests {
    use super::*;
    use crate::logging::LogEvent;

    fn make_event(severity: Severity, message: &str) -> LogEvent<'_> {
        LogEvent {
            severity,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: None,
            client: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            session_id: None,
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn notify_desktop_does_not_panic() {
        // Best-effort — just verify no panic on arbitrary strings.
        notify_desktop("test title", "test body");
        notify_desktop("", "");
        notify_desktop(
            "Catenary: hooks need update",
            "Installed hooks are outdated",
        );
    }

    #[test]
    fn sink_ignores_below_error() {
        let sink = DesktopNotificationSink::new();
        let warn = make_event(Severity::Warn, "warning message");
        let info = make_event(Severity::Info, "info message");
        let debug = make_event(Severity::Debug, "debug message");

        // These should not fire (we can't assert on OS notifications,
        // but we verify no panic and the debounce set stays empty).
        sink.handle(&warn);
        sink.handle(&info);
        sink.handle(&debug);

        let count = sink
            .fired
            .lock()
            .expect("mutex should not be poisoned")
            .len();
        assert_eq!(count, 0, "non-error events should not fire");
    }

    #[test]
    fn sink_debounce_fires_once() {
        let sink = DesktopNotificationSink::new();
        let err1 = make_event(Severity::Error, "same message");
        let err2 = make_event(Severity::Error, "same message");

        sink.handle(&err1);
        let fired_count_1 = sink
            .fired
            .lock()
            .expect("mutex should not be poisoned")
            .len();
        assert_eq!(fired_count_1, 1);

        sink.handle(&err2);
        let fired_count_2 = sink
            .fired
            .lock()
            .expect("mutex should not be poisoned")
            .len();
        assert_eq!(fired_count_2, 1, "duplicate message should not add to set");
    }

    #[test]
    fn sink_different_messages_both_fire() {
        let sink = DesktopNotificationSink::new();
        let err1 = make_event(Severity::Error, "first error");
        let err2 = make_event(Severity::Error, "second error");

        sink.handle(&err1);
        sink.handle(&err2);

        let count = sink
            .fired
            .lock()
            .expect("mutex should not be poisoned")
            .len();
        assert_eq!(count, 2, "different messages should both fire");
    }

    #[test]
    fn is_enabled_defaults_to_true() {
        // Can't test env var mutation in parallel tests, but verify
        // the function doesn't panic and returns a bool.
        let _ = is_enabled();
    }

    #[test]
    fn sink_disabled_skips_all_events() {
        let sink = DesktopNotificationSink::with_enabled(false);
        let err = make_event(Severity::Error, "should be suppressed");

        sink.handle(&err);

        let count = sink
            .fired
            .lock()
            .expect("mutex should not be poisoned")
            .len();
        assert_eq!(count, 0, "disabled sink should not fire");
    }
}
