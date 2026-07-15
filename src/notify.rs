// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Desktop notification support.
//!
//! Fires OS-level desktop notifications for error-severity tracing events.
//! Best-effort, non-blocking: failures are silently ignored. Suppressed
//! when `CATENARY_NOTIFY` is set to an off-spelling (`0`/`off`/`false`/`no`) —
//! the isolation tripwire `isolate_env` sets on every test subprocess (bug 111).
//!
//! Two integration points:
//!
//! - [`DesktopNotificationSink`] — a [`crate::logging::Sink`] registered
//!   on [`crate::logging::LoggingServer`]. Fires for `error!()` events
//!   with per-daemon-lifetime debounce.
//! - Hook CLI — installs a minimal tracing subscriber with only this sink
//!   so `error!()` events (e.g., daemon unreachable) fire OS notifications
//!   even when the daemon isn't running.
//!
//! [`UnreachableStamp`] is the cross-process onset dedup for the
//! "daemon unreachable" interrupt (bug 111): a stranded socket makes every
//! short-lived hook process see the same failure, so the stamp — a marker under
//! `runtime_dir` keyed to the socket's filesystem identity — lets the first hook
//! fire one notification and every later hook stay silent until a NEW strand
//! (a different socket identity) or a successful daemon bind (which clears it).

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::logging::{LogEvent, Severity, Sink};

/// Whether desktop notifications are enabled.
///
/// Reads `CATENARY_NOTIFY` once at first call. Defaults to enabled. Any of the
/// off-spellings (`0`, `off`, `false`, `no`, case-insensitive) suppress every
/// desktop notification for the lifetime of the process, regardless of which
/// Catenary surface fires it (daemon sink, hook sink, or the direct
/// [`notify_desktop`] path). This is the isolation tripwire (bug 111): every
/// subprocess launched under `isolate_env` inherits `CATENARY_NOTIFY=off`, so a
/// test can never reach the real desktop bus no matter what error path it walks
/// — `isolate_env` redirects the XDG file bases but not the D-Bus session, so
/// this env gate is the only thing standing between an isolated test's `error!()`
/// and the maintainer's screen.
fn is_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("CATENARY_NOTIFY").map_or(true, |v| !is_off_spelling(&v)))
}

/// Whether `value` is one of the recognized "disabled" spellings for
/// `CATENARY_NOTIFY` (`0`, `off`, `false`, `no`, case-insensitive).
///
/// `isolate_env` sets `CATENARY_NOTIFY=off`; earlier call sites used `0`. Both
/// (and the other common falsey spellings) suppress, so neither a stale test
/// harness nor a hand-set shell var can leak a notification to the desktop.
fn is_off_spelling(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no"
    )
}

/// Fire an OS-level desktop notification.
///
/// Best-effort, non-blocking. Spawns a platform-specific subprocess
/// and does not wait for it to complete. Silently ignores all failures.
/// No-op when `CATENARY_NOTIFY` is off (`0`/`off`/`false`/`no`) or during
/// `#[cfg(test)]`.
pub fn notify_desktop(title: &str, body: &str) {
    // Test seam: record the notification *intent* to a file, before the OS
    // suppression gate. An isolated integration test runs with `CATENARY_NOTIFY`
    // off (no real desktop) yet still needs to *count* how many notifications
    // fired across N short-lived hook processes — the storm-dies proof (bug 111).
    // The record captures intent regardless of the OS gate, so the count is
    // observable without ever reaching the desktop bus.
    record_notify_intent(title, body);

    // Suppress real OS notifications during unit tests.
    if cfg!(test) {
        return;
    }
    if !is_enabled() {
        return;
    }
    let _ = send_notification(title, body);
}

/// Append a one-line record of a notification intent to `CATENARY_NOTIFY_LOG`
/// when that env var names a file; a no-op otherwise.
///
/// Test-only observability (bug 111): the file is an append-mostly tally of every
/// notification the process *would* fire, so a test can spawn N hook subprocesses
/// against a stranded socket and assert the tally holds exactly one line. Writes
/// are `O_APPEND` opens so concurrent processes never clobber each other. Never
/// set in production — only the test harness points it at a tempdir.
fn record_notify_intent(title: &str, body: &str) {
    use std::io::Write as _;
    let Some(path) = std::env::var_os("CATENARY_NOTIFY_LOG") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{title}\t{body}");
    }
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
/// `[notifications] desktop = false` in user config) or when `CATENARY_NOTIFY`
/// is set to an off-spelling (`0`/`off`/`false`/`no`) in the environment.
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
    /// Pass the resolved `desktop` config value. An off-spelled
    /// `CATENARY_NOTIFY` env var (`0`/`off`/`false`/`no`) still overrides to
    /// disabled regardless of this flag.
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

/// Cross-process onset dedup for the "daemon unreachable" desktop interrupt
/// (bug 111).
///
/// A failed daemon boot (or a `SIGKILL`ed daemon) can strand the IPC socket:
/// the file exists but nothing listens, so every subsequent connect fails. The
/// hook CLI is one short-lived process per tool call, so an in-process debounce
/// (like [`DesktopNotificationSink`]'s) cannot span invocations — without this
/// stamp, N tool calls in a session each fire the same interrupt (the 26-notification
/// storm the bug reports).
///
/// The stamp is a small marker file under `runtime_dir` (the ephemeral, tmpfs
/// tier — correct for a "notified already" flag that must not survive a reboot).
/// Its content is the stranded socket's **filesystem identity** — its inode
/// folded with the full mtime and ctime (see [`socket_identity`]). On each
/// unreachable sighting the hook compares the live socket's identity to the
/// stamp:
///
/// - No stamp, or a stamp for a *different* identity → this is a fresh onset;
///   fire the interrupt and (re)write the stamp. A new daemon that bound and then
///   died leaves a new socket inode, so its strand is a genuinely new event that
///   earns its own single interrupt.
/// - Stamp matches the live socket identity → the same unchanged failure; stay
///   silent.
///
/// A successful daemon bind [`clear`](Self::clear)s the stamp, so the next strand
/// notifies fresh.
pub struct UnreachableStamp {
    path: PathBuf,
}

impl UnreachableStamp {
    /// The marker's location: `<runtime_dir>/catenary/daemon-unreachable.stamp`.
    ///
    /// Lives beside the `state.json` snapshot under the ephemeral runtime tier.
    #[must_use]
    fn default_path() -> PathBuf {
        crate::paths::runtime_dir()
            .join("catenary")
            .join("daemon-unreachable.stamp")
    }

    /// A stamp at the default `runtime_dir` location.
    #[must_use]
    pub fn new() -> Self {
        Self {
            path: Self::default_path(),
        }
    }

    /// A stamp at an explicit path (tests isolate it under a tempdir).
    #[must_use]
    pub const fn at(path: PathBuf) -> Self {
        Self { path }
    }

    /// Decide whether an unreachable sighting for `socket` should fire a fresh
    /// interrupt, stamping the socket's identity when it should.
    ///
    /// Returns `true` exactly once per socket-identity onset: the first call
    /// for a given filesystem identity returns `true` and records it; every later
    /// call that observes the same identity returns `false`. A `socket` with no
    /// identity (already unlinked between the connect attempt and this check)
    /// returns `false` — there is nothing to strand a notification on.
    ///
    /// Best-effort: an unreadable or unwritable stamp errs toward notifying
    /// (returns `true`) rather than silencing a genuine onset — a lost interrupt
    /// is worse than a duplicate one.
    #[must_use]
    pub fn should_notify(&self, socket: &Path) -> bool {
        let Some(identity) = socket_identity(socket) else {
            return false;
        };
        if self.matches(&identity) {
            return false;
        }
        self.write(&identity);
        true
    }

    /// Whether the stamp on disk records `identity`.
    fn matches(&self, identity: &str) -> bool {
        std::fs::read_to_string(&self.path).is_ok_and(|s| s.trim() == identity)
    }

    /// Atomically write `identity` into the stamp (rename-over — the house
    /// pattern from `config::mutate::write_document`), so a concurrent reader
    /// never observes a torn marker. Best-effort: a failure leaves the prior
    /// stamp (or none) and the next hook re-evaluates.
    fn write(&self, identity: &str) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut tmp = self.path.as_os_str().to_os_string();
        tmp.push(format!(".tmp.{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        if std::fs::write(&tmp, identity).is_ok() && std::fs::rename(&tmp, &self.path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Clear the stamp, so the next strand notifies fresh.
    ///
    /// Called by the daemon the moment it binds its sockets successfully: a live
    /// daemon means the socket is reachable again, so the prior "unreachable"
    /// onset is resolved. Best-effort — a missing stamp is already the cleared
    /// state.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Default for UnreachableStamp {
    fn default() -> Self {
        Self::new()
    }
}

/// The stranded socket's filesystem identity as a stable string, or `None` when
/// the socket has no metadata (already gone).
///
/// Keyed on `(inode, mtime)`: a fresh daemon unlinks the old socket and binds a
/// new one, minting a new inode, so a new strand is a distinct identity even if
/// it reuses the same path. Several fields are folded together — inode plus the
/// full mtime and ctime (seconds *and* nanoseconds) — so that even a same-second
/// inode reuse (common on tmpfs, where a just-unlinked inode can be handed back
/// immediately) still resolves to a distinct identity: `ctime` advances on the
/// inode metadata change, and the nanosecond stamps discriminate within a second.
fn socket_identity(socket: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(socket).ok()?;
        Some(format!(
            "{}:{}.{}:{}.{}",
            meta.ino(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        ))
    }
    #[cfg(not(unix))]
    {
        let meta = std::fs::metadata(socket).ok()?;
        let modified = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        Some(format!("{}:{}", meta.len(), modified.as_nanos()))
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
            scope_root: None,
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

    #[test]
    fn off_spellings_recognized() {
        for off in [
            "0", "off", "OFF", "Off", "false", "FALSE", "no", "No", "  off  ",
        ] {
            assert!(
                is_off_spelling(off),
                "{off:?} should suppress notifications"
            );
        }
    }

    #[test]
    fn on_spellings_do_not_suppress() {
        for on in ["1", "on", "true", "yes", "", "enabled"] {
            assert!(
                !is_off_spelling(on),
                "{on:?} should NOT suppress notifications"
            );
        }
    }

    /// Create a real Unix socket file at `path` so `socket_identity` reads a
    /// stable inode/mtime. A plain file works — the stamp keys on filesystem
    /// metadata, not the socket type.
    fn touch(path: &Path) {
        std::fs::write(path, b"").expect("write socket stand-in");
    }

    #[test]
    fn stamp_first_notify_then_suppresses_matching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("catenary.sock");
        touch(&sock);
        let stamp = UnreachableStamp::at(dir.path().join("stamp"));

        // First sighting of this socket identity: fire.
        assert!(
            stamp.should_notify(&sock),
            "first unreachable sighting must notify"
        );
        // Same identity, later hooks: silent.
        assert!(
            !stamp.should_notify(&sock),
            "matching socket identity must stay silent"
        );
        assert!(
            !stamp.should_notify(&sock),
            "still silent on every later hook"
        );
    }

    #[test]
    fn stamp_changed_socket_identity_re_notifies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("catenary.sock");
        touch(&sock);
        let stamp = UnreachableStamp::at(dir.path().join("stamp"));

        assert!(stamp.should_notify(&sock), "first onset notifies");
        assert!(!stamp.should_notify(&sock), "same onset silent");

        // A new daemon binds a new socket at the same path — a distinct identity.
        // Remove and recreate, then pin the recreated file's mtime to a clearly
        // different value with `filetime` so the identity change is deterministic
        // (not dependent on the recreate landing in a different clock tick — a
        // just-freed tmpfs inode can be handed back within the same nanosecond).
        std::fs::remove_file(&sock).expect("remove socket");
        touch(&sock);
        filetime::set_file_mtime(&sock, filetime::FileTime::from_unix_time(1_000_000, 0))
            .expect("pin recreated socket mtime");

        assert!(
            stamp.should_notify(&sock),
            "a new socket identity is a fresh onset that re-notifies"
        );
    }

    #[test]
    fn stamp_clear_re_arms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("catenary.sock");
        touch(&sock);
        let stamp = UnreachableStamp::at(dir.path().join("stamp"));

        assert!(stamp.should_notify(&sock), "first onset notifies");
        assert!(!stamp.should_notify(&sock), "same onset silent");

        // A successful daemon bind clears the stamp.
        stamp.clear();

        assert!(
            stamp.should_notify(&sock),
            "after clear, the next strand notifies fresh"
        );
    }

    #[test]
    fn stamp_missing_socket_does_not_notify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stamp = UnreachableStamp::at(dir.path().join("stamp"));
        let absent = dir.path().join("nonexistent.sock");

        assert!(
            !stamp.should_notify(&absent),
            "no socket means nothing to strand a notification on"
        );
    }

    #[test]
    fn stamp_clear_is_idempotent_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stamp = UnreachableStamp::at(dir.path().join("stamp"));
        // Clearing a never-written stamp is a no-op, not an error.
        stamp.clear();
        stamp.clear();
    }
}
