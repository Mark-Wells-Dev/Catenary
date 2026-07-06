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

/// Flatten a string into one filesystem-safe path component.
///
/// Every character that is not ASCII alphanumeric (path separators, `.`, `_`,
/// spaces, …) becomes `-`. Used by [`encode_cwd`] (the firehose shard key). The
/// mapping is stable but intentionally lossy — distinct inputs can collide (e.g.
/// `a/b` and `a.b`) — which is acceptable for the regenerable ephemera it keys.
fn flatten_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
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
    flatten_component(&path.to_string_lossy())
}

/// Root directory under [`cache_dir`] that holds relocated agent worktrees.
///
/// `<cache_dir>/catenary/worktrees/`. Claude Code's `WorktreeCreate` hook
/// (`catenary hook worktree-create`) creates each subagent worktree here —
/// physically *outside* the source repo tree — so gitignore-blind language
/// server discovery (rust-analyzer's cargo walk) can never descend into it, the
/// structural fix for the nested-worktree index pollution (bug 53 / misc 144).
/// The orphan-prune sweep ([`crate::worktree_create::prune_orphans`]) scans this
/// directory.
#[must_use]
pub fn worktrees_dir() -> PathBuf {
    cache_dir().join("catenary").join("worktrees")
}

/// Directory for a single relocated agent worktree under [`worktrees_dir`].
///
/// `<cache_dir>/catenary/worktrees/<flattened-repo>-<unique_id>`. The source
/// repo path is flattened to one filesystem-safe component via
/// [`flatten_component`] (the same lossy `[^a-zA-Z0-9] -> -` mapping the
/// firehose shard key uses), then suffixed with `unique_id` so concurrent
/// worktrees of the same repo never collide. The flattened repo is a human
/// label, not a reversible encoding — collisions are harmless because
/// `unique_id` disambiguates.
#[must_use]
pub fn agent_worktree_dir(repo: &Path, unique_id: &str) -> PathBuf {
    worktrees_dir().join(format!(
        "{}-{unique_id}",
        flatten_component(&repo.to_string_lossy())
    ))
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

    #[test]
    fn worktrees_dir_lives_under_cache() {
        let dir = worktrees_dir();
        assert!(
            dir.starts_with(cache_dir()),
            "worktrees dir must live under cache_dir",
        );
        assert!(
            dir.ends_with("catenary/worktrees"),
            "worktrees dir must be `<cache>/catenary/worktrees`, got {}",
            dir.display(),
        );
    }

    #[test]
    fn agent_worktree_dir_flattens_repo_and_suffixes_id() {
        let dir = agent_worktree_dir(Path::new("/home/mark/Projects/Catenary"), "abc123");
        assert!(
            dir.starts_with(worktrees_dir()),
            "agent worktree dir must live under the worktrees root",
        );
        // Final component: the flattened repo (same `[^a-zA-Z0-9] -> -` mapping
        // as `encode_cwd`) suffixed with the unique id.
        assert!(
            dir.ends_with("-home-mark-Projects-Catenary-abc123"),
            "unexpected leaf component: {}",
            dir.display(),
        );
    }
}
