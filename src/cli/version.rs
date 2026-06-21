// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! `catenary version`: report the CLI's own version and the running daemon's.
//!
//! `catenary --version` (the clap flag) prints the binary's embedded
//! `CATENARY_VERSION` and exits inside clap, doing no I/O. This subcommand is
//! the proactive, daemon-aware view: it prints the CLI version, queries the
//! running daemon for *its* version over the IPC socket (`tool/version`), and
//! on a mismatch points at the stale daemon — the same fact the bridge's
//! `version_handshake` only surfaces as a hard error on the next connection.

use anyhow::Result;

use crate::cli::Output;

/// The CLI binary's embedded version — the same `git describe` string the
/// `--version` flag prints, so the two can never diverge.
const CLI_VERSION: &str = env!("CATENARY_VERSION");

/// Outcome of querying the running daemon for its version.
///
/// Three states, all exit 0. The distinction that matters is between *no
/// daemon* and *a daemon that won't answer*: the current running daemon
/// predates `tool/version`, so the very first `catenary version` (the
/// pre-rebuild staleness check) hits a daemon that is up but cannot answer —
/// that must read as "running, version unknown", never "not running".
enum DaemonVersion {
    /// A daemon answered with this version string.
    Reachable(String),
    /// Connected, but no valid version came back within the read timeout —
    /// a daemon that predates `tool/version` or is otherwise wedged. Covers
    /// timeout, EOF/empty, unparseable response, and a missing `version` field.
    Unresponsive,
    /// No daemon is running (connect failed — socket absent or refused).
    NotRunning,
}

/// How long to wait for the daemon's `tool/version` reply before treating it as
/// [`DaemonVersion::Unresponsive`].
///
/// Generous for a local Unix socket — a healthy daemon answers near-instantly;
/// this bound only fires for a wedged or pre-`tool/version` daemon that holds
/// the connection open without replying.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Render the `catenary version` report into lines.
///
/// Pure so the layout and the stale-daemon hint are unit-testable without a
/// live daemon. Always lists the CLI version first; appends the daemon line
/// (`daemon <V>`, the running-but-version-unknown line, or `daemon: not
/// running`); and on a CLI≠daemon mismatch appends the stale-daemon hint. No
/// stale hint for [`DaemonVersion::Unresponsive`] — its version is unknown, so
/// there is nothing to compare.
fn render(cli_version: &str, daemon: &DaemonVersion) -> Vec<String> {
    let mut lines = vec![format!("catenary {cli_version}")];
    match daemon {
        DaemonVersion::Reachable(daemon_version) => {
            lines.push(format!("daemon {daemon_version}"));
            if daemon_version != cli_version {
                lines.push(
                    "daemon is stale — run `catenary stop` (or restart it) to pick up the \
                     new build"
                        .to_string(),
                );
            }
        }
        DaemonVersion::Unresponsive => lines.push(
            "daemon: running, version unknown — predates `catenary version` or unresponsive; \
             restart it to refresh"
                .to_string(),
        ),
        DaemonVersion::NotRunning => lines.push("daemon: not running".to_string()),
    }
    lines
}

/// Query the running daemon's version over the IPC socket.
///
/// Connects to the daemon's general-purpose IPC socket and sends
/// `{"method": "tool/version"}`, mirroring how `catenary roots ls` exchanges
/// over the same socket. The three outcomes:
///
/// - **connect failure** (socket absent or refused) → [`DaemonVersion::NotRunning`].
///   This is the *only* path to `NotRunning`: a failed connect is the sole
///   signal that no daemon is up.
/// - **connected but no valid version** → [`DaemonVersion::Unresponsive`].
///   The exchange (write + read) is bounded by [`READ_TIMEOUT`]; a timeout,
///   write/read error, EOF/empty line, unparseable response, or a missing
///   `version` field all map here. A daemon predating `tool/version` is up but
///   silent or closes the connection — either way it is *running*, not absent.
/// - **valid version** → [`DaemonVersion::Reachable`].
#[cfg(unix)]
async fn query_daemon_version() -> DaemonVersion {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = crate::router::socket_path();

    // Connect failure is the sole `NotRunning` signal.
    let Ok(stream) = tokio::net::UnixStream::connect(&ipc_path).await else {
        return DaemonVersion::NotRunning;
    };

    // Connected — from here every failure is `Unresponsive` (a daemon IS up).
    // Bound the whole exchange so a wedged or pre-`tool/version` daemon that
    // holds the connection open can never hang the command.
    let exchange = async {
        let (reader, mut writer) = stream.into_split();
        let request = serde_json::json!({"method": "tool/version"});
        let mut payload = serde_json::to_string(&request).ok()?;
        payload.push('\n');
        writer.write_all(payload.as_bytes()).await.ok()?;

        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();
        // `read_line` returning 0 (EOF, empty line) yields an empty `line`,
        // which fails the parse below — folded into `Unresponsive`.
        buf_reader.read_line(&mut line).await.ok()?;

        serde_json::from_str::<serde_json::Value>(line.trim())
            .ok()?
            .get("version")
            .and_then(|s| s.as_str())
            .map(str::to_string)
    };

    match tokio::time::timeout(READ_TIMEOUT, exchange).await {
        Ok(Some(version)) => DaemonVersion::Reachable(version),
        // Inner `None` (write/read/parse/missing-field) or outer timeout: a
        // daemon is connected but did not answer with a version.
        Ok(None) | Err(_) => DaemonVersion::Unresponsive,
    }
}

/// Print the CLI version and the running daemon's version.
///
/// Always exits successfully (exit 0): a missing daemon is not an error, and a
/// version mismatch is surfaced as a hint, not a failure.
///
/// # Errors
///
/// Returns an error only if writing to `out` fails.
#[cfg(unix)]
pub async fn run_version(out: &mut Output) -> Result<()> {
    let daemon = query_daemon_version().await;
    for line in render(CLI_VERSION, &daemon) {
        out.writeln(format_args!("{line}"))?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_show_no_hint() {
        let lines = render("1.3.6", &DaemonVersion::Reachable("1.3.6".to_string()));
        assert_eq!(lines, vec!["catenary 1.3.6", "daemon 1.3.6"]);
        assert!(
            !lines.iter().any(|l| l.contains("stale")),
            "no stale hint on match: {lines:?}",
        );
    }

    #[test]
    fn mismatched_versions_show_stale_hint() {
        let lines = render(
            "1.3.7",
            &DaemonVersion::Reachable("1.3.6-3-gabc1234".to_string()),
        );
        assert_eq!(lines[0], "catenary 1.3.7");
        assert_eq!(lines[1], "daemon 1.3.6-3-gabc1234");
        assert!(
            lines[2].contains("daemon is stale"),
            "stale hint on mismatch: {lines:?}",
        );
        assert!(
            lines[2].contains("catenary stop"),
            "hint names `catenary stop`: {lines:?}",
        );
    }

    #[test]
    fn no_daemon_reports_not_running_no_hint() {
        let lines = render("1.3.6", &DaemonVersion::NotRunning);
        assert_eq!(lines, vec!["catenary 1.3.6", "daemon: not running"]);
        assert!(
            !lines.iter().any(|l| l.contains("stale")),
            "no stale hint when no daemon: {lines:?}",
        );
    }

    #[test]
    fn unresponsive_daemon_reports_running_version_unknown_no_hint() {
        let lines = render("1.3.6", &DaemonVersion::Unresponsive);
        assert_eq!(lines[0], "catenary 1.3.6");
        assert!(
            lines[1].starts_with("daemon: running, version unknown"),
            "running/version-unknown line: {lines:?}",
        );
        assert!(
            lines[1].contains("restart it"),
            "line nudges a restart: {lines:?}",
        );
        assert!(
            !lines.iter().any(|l| l.contains("stale")),
            "no stale hint when version unknown: {lines:?}",
        );
        // Only the two lines — no extra hint.
        assert_eq!(lines.len(), 2, "exactly two lines: {lines:?}");
    }
}
