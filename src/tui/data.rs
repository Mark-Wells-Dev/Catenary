// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Data abstraction layer for the TUI.
//!
//! The dashboard reads **only** the daemon-owned `state.json` snapshot — never
//! the firehose or a database (observability workstream 27, ticket 06). This
//! makes the TUI structurally unwedgeable: the consumer that wedged (~19% CPU
//! re-running `recent_scopes` SQL on every WAL change) cannot exist because the
//! TUI no longer reads the firehose at all.
//!
//! [`StateJsonDataSource`] reads + parses the snapshot file (production).
//! [`MockDataSource`] returns a fixture snapshot (testing).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::state_snapshot::Snapshot;

/// Abstraction over the dashboard's data source — a single `state.json` read.
///
/// The whole snapshot (server board, session board, alerts) is pulled in one
/// [`Self::load`] call; the [`super::App`] holds the latest snapshot and renders
/// the three boards from it. A file-watch on the snapshot drives re-loads.
pub trait DataSource {
    /// Load the current snapshot.
    ///
    /// A missing file (daemon not running) is **not** an error — it yields the
    /// default (empty) snapshot, which the caller renders as a "waiting for
    /// daemon" state. An unreadable or unparseable existing file returns an
    /// error; the caller keeps the last good snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if an existing snapshot file cannot be read or parsed.
    fn load(&self) -> Result<Snapshot>;
}

// ── state.json (production) implementation ───────────────────────────

/// Data source backed by the daemon's `state.json` snapshot.
pub struct StateJsonDataSource {
    path: PathBuf,
}

impl StateJsonDataSource {
    /// Open the snapshot at the daemon's canonical location
    /// (`runtime_dir()/catenary/state.json`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            path: default_snapshot_path(),
        }
    }

    /// Open a snapshot at an explicit path (testing / isolated runtime dirs).
    #[must_use]
    pub const fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// The snapshot file path (the file-watch target).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for StateJsonDataSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DataSource for StateJsonDataSource {
    fn load(&self) -> Result<Snapshot> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse snapshot at {}", self.path.display())),
            // Daemon not running yet — render the waiting state, not an error.
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Snapshot::default()),
            Err(e) => Err(e)
                .with_context(|| format!("failed to read snapshot at {}", self.path.display())),
        }
    }
}

/// The daemon's canonical `state.json` location.
///
/// Mirrors the writer's path in `main.rs` (`runtime_dir()/catenary/state.json`),
/// so the TUI watches the exact file the daemon overwrites on change.
#[must_use]
pub fn default_snapshot_path() -> PathBuf {
    crate::db::runtime_dir().join("catenary").join("state.json")
}

// ── Mock (testing) implementation ────────────────────────────────────

/// Data source backed by an in-memory fixture snapshot for deterministic tests.
pub struct MockDataSource {
    /// The snapshot returned from every [`DataSource::load`] call.
    pub snapshot: Snapshot,
}

impl MockDataSource {
    /// Build a mock source from a fixture snapshot.
    #[must_use]
    pub const fn new(snapshot: Snapshot) -> Self {
        Self { snapshot }
    }
}

impl DataSource for MockDataSource {
    fn load(&self) -> Result<Snapshot> {
        Ok(self.snapshot.clone())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::state_snapshot::{ServerEntry, SessionEntry, SessionStatus};

    fn fixture() -> Snapshot {
        Snapshot {
            schema: 1,
            servers: vec![ServerEntry {
                id: "rust-analyzer@/p/Catenary".to_string(),
                server: "rust-analyzer".to_string(),
                state: "probing".to_string(),
                ..ServerEntry::default()
            }],
            sessions: vec![SessionEntry {
                id: "mcp:abc".to_string(),
                status: SessionStatus::Editing,
                ..SessionEntry::default()
            }],
            ..Snapshot::default()
        }
    }

    #[test]
    fn mock_returns_fixture_snapshot() {
        let ds = MockDataSource::new(fixture());
        let snap = ds.load().expect("mock load");
        assert_eq!(snap.servers.len(), 1);
        assert_eq!(snap.servers[0].state, "probing");
        assert_eq!(snap.sessions[0].status, SessionStatus::Editing);
    }

    #[test]
    fn missing_file_yields_empty_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ds = StateJsonDataSource::with_path(dir.path().join("absent.json"));
        let snap = ds.load().expect("absent file is not an error");
        assert_eq!(snap.daemon.pid, 0);
        assert!(snap.servers.is_empty());
    }

    #[test]
    fn reads_and_parses_existing_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"schema":1,"daemon":{"pid":4242},"servers":[{"id":"ra@/p","state":"healthy"}]}"#,
        )
        .expect("write fixture");
        let ds = StateJsonDataSource::with_path(path);
        let snap = ds.load().expect("parse");
        assert_eq!(snap.daemon.pid, 4242);
        assert_eq!(snap.servers[0].id, "ra@/p");
        assert_eq!(snap.servers[0].state, "healthy");
    }

    #[test]
    fn malformed_snapshot_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not json {{{").expect("write garbage");
        let ds = StateJsonDataSource::with_path(path);
        assert!(ds.load().is_err(), "malformed existing file should error");
    }

    #[test]
    fn default_path_is_under_runtime_dir() {
        let path = default_snapshot_path();
        assert!(path.ends_with("catenary/state.json"), "got {path:?}");
    }
}
