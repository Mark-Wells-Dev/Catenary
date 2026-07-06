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

/// Root directory under [`state_dir`] that holds Catenary-managed worktrees.
///
/// `<state_dir>/catenary/worktrees/`. A *durable* base (not the regenerable
/// cache): a dirty agent worktree can hold the only copy of unlanded work, which
/// the disposal design refuses to auto-delete, so its home must survive a cache
/// purge (misc 150 / misc 151 layout). Agent worktrees live under the
/// [`agents_worktrees_dir`] subtree. Placing worktrees *outside* the source repo
/// tree is bug 53's structural fix — gitignore-blind server discovery
/// (rust-analyzer's cargo walk) can never descend into them.
#[must_use]
pub fn worktrees_dir() -> PathBuf {
    state_dir().join("catenary").join("worktrees")
}

/// Subtree under [`worktrees_dir`] that holds ephemeral agent worktrees.
///
/// `<state_dir>/catenary/worktrees/agents/`. Each agent worktree lives at
/// `agents/<session_id>/<segment>/` ([`agent_worktree_dir`]); the path itself is
/// the `(session, agent)` key, so a dead session's leftovers group into one
/// sweepable subtree and registry rehydration is path-derivable even with a
/// damaged sidecar. The orphan-prune sweep
/// ([`crate::worktree_create::prune_agent_orphans`]) scans this directory.
#[must_use]
pub fn agents_worktrees_dir() -> PathBuf {
    worktrees_dir().join("agents")
}

/// Legacy cache-dir worktrees root from older builds.
///
/// `<cache_dir>/catenary/worktrees/`. Pre-misc-150 builds created agent
/// worktrees here (the flattened-repo scheme). No new worktree is ever placed
/// here; it is retained solely so [`crate::worktree_create::prune_orphans`] can
/// sweep stragglers left by an older daemon.
#[must_use]
pub fn legacy_cache_worktrees_dir() -> PathBuf {
    cache_dir().join("catenary").join("worktrees")
}

/// Directory for a single agent worktree under [`agents_worktrees_dir`].
///
/// `<state_dir>/catenary/worktrees/agents/<session_id>/<segment>/`. The
/// `segment` is the bare agent id when the `WorktreeCreate` payload `name`
/// parses as `agent-<id>` (a subagent spawn), else the `name` verbatim (a
/// `--worktree` session). The identity-in-path scheme makes the directory itself
/// the `(session, agent)` key.
#[must_use]
pub fn agent_worktree_dir(session_id: &str, segment: &str) -> PathBuf {
    agents_worktrees_dir().join(session_id).join(segment)
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
    fn worktrees_dir_lives_under_state() {
        let dir = worktrees_dir();
        assert!(
            dir.starts_with(state_dir()),
            "worktrees dir must live under state_dir (durable, not the cache)",
        );
        assert!(
            dir.ends_with("catenary/worktrees"),
            "worktrees dir must be `<state>/catenary/worktrees`, got {}",
            dir.display(),
        );
    }

    #[test]
    fn agents_worktrees_dir_is_the_agents_subtree() {
        let dir = agents_worktrees_dir();
        assert!(
            dir.starts_with(worktrees_dir()),
            "agents subtree must live under the worktrees root",
        );
        assert!(
            dir.ends_with("catenary/worktrees/agents"),
            "agents subtree must be `<state>/catenary/worktrees/agents`, got {}",
            dir.display(),
        );
    }

    #[test]
    fn legacy_cache_worktrees_dir_lives_under_cache() {
        let dir = legacy_cache_worktrees_dir();
        assert!(
            dir.starts_with(cache_dir()),
            "legacy worktrees dir must live under cache_dir (the old scheme)",
        );
        assert!(
            dir.ends_with("catenary/worktrees"),
            "legacy worktrees dir must be `<cache>/catenary/worktrees`, got {}",
            dir.display(),
        );
    }

    #[test]
    fn agent_worktree_dir_is_session_then_segment() {
        let dir = agent_worktree_dir("sess-abc", "ad9dee0ad90513642");
        assert!(
            dir.starts_with(agents_worktrees_dir()),
            "agent worktree dir must live under the agents subtree",
        );
        // `agents/<session_id>/<segment>` — the identity-in-path key.
        assert!(
            dir.ends_with("agents/sess-abc/ad9dee0ad90513642"),
            "unexpected leaf path: {}",
            dir.display(),
        );
    }
}
