// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The daemon intent marker (workstream "pulse").
//!
//! A tiny file at `runtime_dir()/daemon.intent` records a maintainer-declared
//! lifecycle intent that the bridge's indefinite retry loops consult each tick:
//!
//! - **absent** — no declared intent: the bridge may connect *or spawn* the
//!   daemon (the ordinary crash-recovery path).
//! - **`stop`** — the daemon is deliberately down: the bridge waits
//!   connect-only, never spawning, until a `catenary start` clears the marker.
//! - **`quit`** — bridges should end their sessions: the one marker-sanctioned
//!   self-exit (the bridge otherwise never kills itself — the host owns the
//!   stdio link, and self-exit destroys it from the wrong side).
//!
//! # Format
//!
//! Two lines: the mode word (`stop` or `quit`) and an RFC3339 timestamp
//! recording when the intent was declared. Writes are atomic
//! (write-temp-then-rename in the marker's directory), so a reader never sees a
//! torn marker. Unknown or corrupt content reads as `None` — crash semantics:
//! a damaged marker must fail toward availability (connect-or-spawn), never
//! toward a stranded or self-exiting bridge.
//!
//! This module owns the marker's format and filesystem contract; the verbs
//! that write `stop`/`quit` in production land in a later pulse ticket.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::debug;

use crate::source::Source;

/// File name of the marker under [`crate::paths::runtime_dir`].
const MARKER_FILE_NAME: &str = "daemon.intent";

/// A maintainer-declared daemon lifecycle intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// The daemon is deliberately stopped: bridges wait connect-only and never
    /// spawn until the marker clears.
    Stop,
    /// Bridges should end their sessions: the one marker-sanctioned self-exit.
    Quit,
}

impl Intent {
    /// The canonical mode word written as the marker's first line.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Quit => "quit",
        }
    }
}

/// Returns the marker path: `runtime_dir()/daemon.intent`.
#[must_use]
pub fn marker_path() -> PathBuf {
    crate::paths::runtime_dir().join(MARKER_FILE_NAME)
}

/// Reads the current intent, or `None` when the marker is absent, unreadable,
/// or carries an unknown mode word (crash semantics — fail toward
/// availability).
#[must_use]
pub fn read() -> Option<Intent> {
    read_at(&marker_path())
}

/// Atomically writes `intent` (mode word + RFC3339 timestamp) to the marker.
///
/// # Errors
///
/// Returns an error if the marker's directory cannot be created or the
/// write-temp-then-rename sequence fails.
pub fn write(intent: Intent) -> Result<()> {
    write_at(&marker_path(), intent)
}

/// Removes the marker; an already-absent marker is not an error.
///
/// # Errors
///
/// Returns an error if the marker exists but cannot be removed.
pub fn clear() -> Result<()> {
    clear_at(&marker_path())
}

/// Path-parameterized [`read`] (unit-test seam).
fn read_at(path: &Path) -> Option<Intent> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            debug!(
                source = Source::DaemonLifecycle.as_str(),
                path = %path.display(),
                error = %e,
                "daemon.intent unreadable — treating as absent",
            );
            return None;
        }
    };
    let mode = content.lines().next().unwrap_or_default().trim();
    match mode {
        "stop" => Some(Intent::Stop),
        "quit" => Some(Intent::Quit),
        other => {
            debug!(
                source = Source::DaemonLifecycle.as_str(),
                path = %path.display(),
                mode = other,
                "daemon.intent carries an unknown mode — treating as absent",
            );
            None
        }
    }
}

/// Path-parameterized [`write`] (unit-test seam).
fn write_at(path: &Path, intent: Intent) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("daemon.intent path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create daemon.intent directory: {}", dir.display()))?;

    // Atomic replace: write the full content to a temp file in the same
    // directory, then rename over the marker. A reader observes either the old
    // marker or the new one, never a torn write.
    let content = format!("{}\n{}\n", intent.as_str(), chrono::Utc::now().to_rfc3339());
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create daemon.intent temp file in {}", dir.display()))?;
    std::io::Write::write_all(&mut tmp, content.as_bytes())
        .context("write daemon.intent temp file")?;
    tmp.persist(path)
        .with_context(|| format!("rename daemon.intent into place: {}", path.display()))?;
    Ok(())
}

/// Path-parameterized [`clear`] (unit-test seam).
fn clear_at(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("remove daemon.intent marker: {}", path.display()))
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::{Intent, clear_at, read_at, write_at};

    fn marker_in(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("daemon.intent")
    }

    /// Both modes round-trip through write → read.
    #[test]
    fn write_read_round_trips_both_modes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = marker_in(dir.path());

        write_at(&path, Intent::Stop).expect("write stop");
        assert_eq!(read_at(&path), Some(Intent::Stop));

        write_at(&path, Intent::Quit).expect("write quit");
        assert_eq!(read_at(&path), Some(Intent::Quit));
    }

    /// The on-disk format is pinned: the mode word on line one, an RFC3339
    /// timestamp on line two, trailing newline.
    #[test]
    fn format_is_mode_line_then_rfc3339_timestamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = marker_in(dir.path());
        write_at(&path, Intent::Stop).expect("write");

        let content = std::fs::read_to_string(&path).expect("read marker");
        let mut lines = content.lines();
        assert_eq!(lines.next(), Some("stop"), "line one is the mode word");
        let ts = lines.next().expect("line two present");
        chrono::DateTime::parse_from_rfc3339(ts).expect("line two parses as RFC3339");
        assert_eq!(lines.next(), None, "exactly two lines");
        assert!(content.ends_with('\n'), "trailing newline");
    }

    /// A write renames over an existing marker (atomic replace) and leaves no
    /// temp-file debris behind.
    #[test]
    fn write_renames_over_existing_marker_without_debris() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = marker_in(dir.path());

        write_at(&path, Intent::Stop).expect("first write");
        write_at(&path, Intent::Quit).expect("rename-over write");
        assert_eq!(read_at(&path), Some(Intent::Quit), "the new mode won");

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("dir entry").file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("daemon.intent")],
            "only the marker remains — no temp-file debris",
        );
    }

    /// An absent marker reads as `None`.
    #[test]
    fn absent_marker_reads_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_at(&marker_in(dir.path())), None);
    }

    /// Unknown mode words and corrupt content read as `None` — crash
    /// semantics, failing toward availability.
    #[test]
    fn corrupt_content_reads_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = marker_in(dir.path());

        std::fs::write(&path, "halt\n2026-07-17T00:00:00Z\n").expect("unknown mode");
        assert_eq!(read_at(&path), None, "unknown mode word is None");

        std::fs::write(&path, "").expect("empty file");
        assert_eq!(read_at(&path), None, "empty marker is None");

        std::fs::write(&path, [0xFF, 0xFE, 0x00]).expect("non-UTF-8 bytes");
        assert_eq!(read_at(&path), None, "non-UTF-8 marker is None");
    }

    /// The mode word is trimmed and only the first line is consulted.
    #[test]
    fn mode_word_is_trimmed_first_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = marker_in(dir.path());
        std::fs::write(&path, "  quit  \nnot-a-timestamp\nextra\n").expect("write");
        assert_eq!(read_at(&path), Some(Intent::Quit));
    }

    /// `clear` removes the marker and tolerates an already-absent one.
    #[test]
    fn clear_removes_marker_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = marker_in(dir.path());

        write_at(&path, Intent::Stop).expect("write");
        clear_at(&path).expect("clear");
        assert_eq!(read_at(&path), None, "cleared marker reads as absent");

        clear_at(&path).expect("clear on absent marker is Ok");
    }
}
