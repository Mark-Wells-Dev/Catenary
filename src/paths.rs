// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Filesystem path resolvers for Catenary's base directories.
//!
//! Catenary keeps its data across three XDG base directories, each chosen for
//! its durability semantics:
//!
//! - [`state_dir`] — durable, per-host state (the Unix socket).
//! - [`runtime_dir`] — ephemeral, tmpfs-backed runtime files (the `state.json`
//!   snapshot).
//! - [`cache_dir`] — regenerable, high-volume telemetry (the JSONL firehose).
//!
//! [`encode_cwd`] flattens an absolute path into a single filesystem-safe
//! directory-name component, used as the per-root shard key in the firehose tree.

use std::path::{Path, PathBuf};

/// Resolve the Catenary state directory.
///
/// Resolution order:
/// 1. `CATENARY_STATE_DIR` environment variable (cross-platform override).
/// 2. `dirs::state_dir()` (`XDG_STATE_HOME` on Linux).
/// 3. `dirs::data_local_dir()` (macOS / Windows fallback).
/// 4. `/tmp` as a last resort.
#[must_use]
pub fn state_dir() -> PathBuf {
    std::env::var_os("CATENARY_STATE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Resolve the Catenary runtime directory.
///
/// Home for ephemeral, regenerable runtime files (the daemon-owned `state.json`
/// snapshot) — tmpfs-backed and OS-cleared on logout on Linux, which is the
/// semantically-correct place for them. Unlike the socket (which lives under
/// [`state_dir`]), these files do not need to survive a logout.
///
/// Resolution order:
/// 1. `CATENARY_RUNTIME_DIR` environment variable (cross-platform override).
/// 2. `dirs::runtime_dir()` (`XDG_RUNTIME_DIR` on Linux).
/// 3. [`state_dir`] as a fallback when no runtime dir is configured (macOS /
///    Windows, or `XDG_RUNTIME_DIR` unset).
#[must_use]
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("CATENARY_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(dirs::runtime_dir)
        .unwrap_or_else(state_dir)
}

/// Resolve the Catenary cache directory.
///
/// Home for the regenerable JSONL telemetry firehose — safe to delete, never
/// holds durable state. Unlike [`state_dir`] (socket) and [`runtime_dir`] (small
/// ephemeral runtime reports), the cache dir holds high-volume, append-mostly
/// logs that can be discarded at any time without affecting correctness.
///
/// Resolution order:
/// 1. `CATENARY_CACHE_DIR` environment variable (cross-platform override).
/// 2. `dirs::cache_dir()` (`XDG_CACHE_HOME` on Linux).
/// 3. [`state_dir`] as a fallback when no cache dir is configured.
#[must_use]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("CATENARY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::cache_dir)
        .unwrap_or_else(state_dir)
}

/// Flatten an absolute path into one filesystem-safe directory-name component.
///
/// Matches the encoding Claude Code uses for `~/.claude/projects/`: every
/// character that is not ASCII alphanumeric (path separators, `.`, `_`,
/// spaces, …) becomes `-`.
///
/// `/home/mark/Projects/Catenary` → `-home-mark-Projects-Catenary`.
///
/// Used as the per-root shard key in the JSONL firehose tree. The encoding is
/// stable but intentionally lossy — it is a shard key, not a reversible
/// encoding, so distinct paths can collide (e.g. `a/b` and `a.b`), which is
/// acceptable for a regenerable cache.
#[must_use]
pub fn encode_cwd(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_matches_claude_code_form() {
        assert_eq!(
            encode_cwd(Path::new("/home/mark/Projects/Catenary")),
            "-home-mark-Projects-Catenary"
        );
    }

    #[test]
    fn encode_cwd_replaces_dots_underscores_and_preserves_dashes() {
        // `/` `.` `_` and spaces all map to `-`; existing `-` and alphanumerics
        // survive (mirrors Claude Code's `[^a-zA-Z0-9] -> -` rule).
        assert_eq!(
            encode_cwd(Path::new("/home/mark/.local/share/dot_local")),
            "-home-mark--local-share-dot-local"
        );
        assert_eq!(encode_cwd(Path::new("/p/Catenary-00")), "-p-Catenary-00");
    }

    #[test]
    fn encode_cwd_is_stable() {
        let p = Path::new("/a/b/c");
        assert_eq!(encode_cwd(p), encode_cwd(p));
    }
}
