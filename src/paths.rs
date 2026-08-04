// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Filesystem path resolvers for Catenary's base directories.
//!
//! Catenary keeps its data across several XDG base directories, each chosen for
//! its durability semantics:
//!
//! - [`state_dir`] — durable, per-host state (the Unix socket).
//! - [`runtime_dir`] — ephemeral, tmpfs-backed runtime files (the `state.json`
//!   snapshot).
//! - [`cache_dir`] — regenerable, high-volume telemetry (the JSONL firehose).
//! - [`data_dir`] — regenerable installed artifacts (the managed server home,
//!   `catenary/servers/<name>/<version>/`).
//! - [`home_dir`] — the user's home, the base every host-CLI integration
//!   artifact hangs off (`~/.claude`, `~/.gemini`, `~/.config/opencode`).
//!
//! Every resolver here reads a `CATENARY_*_DIR` override **first**, on every
//! platform, and only then falls back to the `dirs` crate. That ordering is what
//! makes the whole surface testable: `isolate_env` (tests/common/mod.rs) points
//! each override at a distinct subdir of a tempdir, so a subprocess test can
//! never write the operator's real state — and because the bases stay distinct,
//! code that writes under the *wrong* base is caught rather than silently
//! absorbed.
//!
//! This module owns the **only** blessed `dirs::*` base-dir calls in the
//! codebase; `clippy.toml`'s `disallowed-methods` gate denies them everywhere
//! else (bug 149), so a new home- or base-rooted path cannot re-enter without
//! an override behind it. It likewise owns the only environment read of `HOME`
//! (misc 229) — clippy cannot key a denial on an *argument*, so that half of the
//! class is held by a source-scan pin test in this module's tests, and
//! [`compress_home`] (the one `~`-compressing display helper) reads home through
//! [`home_dir`] like everything else.
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
#[allow(
    clippy::disallowed_methods,
    reason = "blessed base-dir resolver: `CATENARY_STATE_DIR` is resolved first, which is what makes the state base testable"
)]
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
#[allow(
    clippy::disallowed_methods,
    reason = "blessed base-dir resolver: `CATENARY_RUNTIME_DIR` is resolved first, which is what makes the runtime base testable"
)]
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("CATENARY_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(dirs::runtime_dir)
        .unwrap_or_else(state_dir)
}

/// Resolve the Catenary config base directory.
///
/// The base directory that *holds* the `catenary/` subdirectory — callers
/// join `catenary/config.toml` (or similar) to reach actual config files.
/// Note the distinction between `CATENARY_CONFIG_DIR` (this resolver — the
/// base directory) and `CATENARY_CONFIG` (a separate mechanism that names an
/// explicit config *file* appended as an additional layer after the user
/// layer; it injects but does not suppress).
///
/// Resolution order:
/// 1. `CATENARY_CONFIG_DIR` environment variable (cross-platform override).
/// 2. `dirs::config_dir()` (`XDG_CONFIG_HOME` on Linux).
/// 3. [`state_dir`] as a fallback when no config dir is configured. The
///    state directory is durable (survives reboots), which matches the
///    durability expectation for a user config file; `/tmp` would lose the
///    config on reboot, making the fallback useless in practice.
#[must_use]
#[allow(
    clippy::disallowed_methods,
    reason = "blessed base-dir resolver: `CATENARY_CONFIG_DIR` is resolved first, which is what makes the config base testable"
)]
pub fn config_dir() -> PathBuf {
    std::env::var_os("CATENARY_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
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
#[allow(
    clippy::disallowed_methods,
    reason = "blessed base-dir resolver: `CATENARY_CACHE_DIR` is resolved first, which is what makes the cache base testable"
)]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("CATENARY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::cache_dir)
        .unwrap_or_else(state_dir)
}

/// Resolve the Catenary data directory.
///
/// Home for regenerable *installed artifacts* — the managed server home
/// (`catenary/servers/<name>/<version>/`, see
/// [`crate::managed_home::ManagedHome`]) lives here. Like the cache, its
/// contents are regenerable (a recipe reinstall recreates any server), but they
/// are deliberately **not** cache: a routine cache purge must not delete a
/// pinned language server out from under its users.
///
/// Resolution order:
/// 1. `CATENARY_DATA_DIR` environment variable (cross-platform override).
/// 2. `dirs::data_dir()` (`XDG_DATA_HOME` on Linux).
/// 3. [`state_dir`] as a fallback when no data dir is configured.
#[must_use]
#[allow(
    clippy::disallowed_methods,
    reason = "blessed base-dir resolver: `CATENARY_DATA_DIR` is resolved first, which is what makes the data base testable"
)]
pub fn data_dir() -> PathBuf {
    std::env::var_os("CATENARY_DATA_DIR")
        .map(PathBuf::from)
        .or_else(dirs::data_dir)
        .unwrap_or_else(state_dir)
}

/// Resolve the user's home directory.
///
/// Not an XDG base, but the same kind of resolution problem: it is the root
/// every host-CLI integration artifact hangs off — Claude Code's `~/.claude`,
/// Antigravity's `~/.gemini/config/plugins/catenary`, OpenCode's
/// `~/.config/opencode/plugin` — as well as the reference point for `~`
/// expansion and `~`-compressed display. So it gets a `CATENARY_*` override for
/// exactly the reason the XDG bases have one.
///
/// **Bug 149.** `isolate_env` deliberately does *not* redirect `$HOME` (other
/// tooling inside a test subprocess legitimately needs the real one), so a
/// seam that resolved a *write* target straight through `dirs::home_dir()`
/// escaped test isolation and rewrote the operator's real
/// `~/.gemini/…/rules/catenary.md`. Routing every home-rooted path through this
/// resolver — and denying bare `dirs::home_dir()` elsewhere via `clippy.toml`'s
/// `disallowed-methods` — closes the class: `CATENARY_HOME_DIR` is the one lever
/// that moves them all.
///
/// Resolution order:
/// 1. `CATENARY_HOME_DIR` environment variable (cross-platform override); an
///    empty value reads as unset, since an empty home would silently reroot
///    every host artifact onto a relative path.
/// 2. `dirs::home_dir()` (`$HOME` on Unix, `%USERPROFILE%` on Windows).
///
/// `None` when neither resolves — callers treat a homeless host as "nothing
/// installed" rather than guessing a location.
#[must_use]
#[allow(
    clippy::disallowed_methods,
    reason = "the one blessed `dirs::home_dir()`: the `CATENARY_HOME_DIR` override layered over it here is what makes home-rooted writes testable (bug 149)"
)]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("CATENARY_HOME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

/// Render `path` with a leading `~` when it lies under the user's home.
///
/// The **one** `~`-compressing display helper (misc 229). CLI receipts, the
/// `cwd:` anchor lines, the config file's `[roots] pinned` entries and the TUI
/// all render home-rooted paths this way, and they must agree byte for byte —
/// three hand-copied definitions used to, and had already drifted.
///
/// Home itself renders as the bare `~`; a path beneath it as `~/<rel>`; a path
/// outside home (or a homeless host) as the plain absolute form.
///
/// Resolution goes through [`home_dir`], so `CATENARY_HOME_DIR` moves the
/// compression along with every other home-rooted path.
#[must_use]
pub fn compress_home(path: &Path) -> String {
    let Some(home) = home_dir() else {
        return path.display().to_string();
    };
    let Ok(rel) = path.strip_prefix(&home) else {
        return path.display().to_string();
    };
    if rel.as_os_str().is_empty() {
        // `~` alone for the home directory itself; `~/rel` otherwise.
        return "~".to_string();
    }
    format!("~/{}", rel.display())
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

/// Canonicalize `path` as far as the filesystem allows, keeping the tail that
/// does not exist yet — the spelling rule for paths that may not exist.
///
/// `Path::canonicalize` is all-or-nothing: it resolves symlinks correctly but
/// FAILS outright on a path whose leaf (or whose parent chain) is not there yet,
/// leaving callers to fall back to the raw spelling. That fallback is a
/// correctness hole wherever the resolved path is used as a KEY, because it
/// silently produces a different spelling for a not-yet-existing path than the
/// same path gets once it exists:
///
/// ```text
///   /var/folders/T/repo/src/new.rs   (absent)  → canonicalize fails → raw
///   /var/folders/T/repo/src/new.rs   (present) → /private/var/folders/T/repo/src/new.rs
/// ```
///
/// This resolves the **nearest existing ancestor** — which is what carries the
/// symlinks — and re-appends the remaining components verbatim, so both readings
/// agree. `.`/`..` are folded lexically first, so a `..` segment cannot escape
/// the ancestor walk or survive into the tail.
///
/// Where every component exists this is exactly `canonicalize`. Where nothing
/// resolves (a relative path with no existing base, a permission error) it
/// degrades to the lexically-normalized input — never an error, because every
/// caller is on a best-effort path where a hard failure would be worse than an
/// unresolved spelling.
///
/// Used by the durable debt ledger ([`crate::lock`]), whose leaves are keyed by
/// the edited path: misc 230 books write targets BEFORE the write runs, so
/// booking a not-yet-existing file and consulting it after it exists must land
/// on one spelling or the debt splits (the macOS `/tmp` → `/private/tmp` red).
#[must_use]
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    let normalized = normalize_lexical(path);
    // Walk up to the nearest ancestor that resolves; everything below it is the
    // lexical tail. `ancestors()` yields the path itself first, so a fully
    // existing path canonicalizes on the first step.
    for ancestor in normalized.ancestors() {
        let Ok(canonical) = ancestor.canonicalize() else {
            continue;
        };
        return normalized.strip_prefix(ancestor).map_or_else(
            |_| canonical.clone(),
            |tail| {
                if tail.as_os_str().is_empty() {
                    canonical.clone()
                } else {
                    canonical.join(tail)
                }
            },
        );
    }
    normalized
}

/// Fold `.` and `..` components without touching the filesystem.
///
/// A leading `..` on a relative path is preserved (there is nothing above it to
/// pop); `..` directly under the root is dropped, matching POSIX.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut parts: Vec<std::path::Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match parts.last() {
                Some(std::path::Component::Normal(_)) => {
                    parts.pop();
                }
                Some(std::path::Component::RootDir | std::path::Component::Prefix(_)) => {}
                _ => parts.push(component),
            },
            other => parts.push(other),
        }
    }
    parts.iter().collect()
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

/// Subtree under [`worktrees_dir`] that holds durable "feats" worktrees.
///
/// `<state_dir>/catenary/worktrees/feats/`. A feats worktree is a deliberate,
/// long-lived parallel checkout for a disjoint line of work (misc 151), created
/// only via `catenary worktree add` — never nagged, never auto-disposed, removed
/// only explicitly. Each lives at `feats/<repo-basename>/<branch>/`
/// ([`feat_worktree_dir`]). One `worktrees/` root with `agents/` vs `feats/`
/// beneath it keeps the is-this-ours guard a single prefix check per class.
#[must_use]
pub fn feats_worktrees_dir() -> PathBuf {
    worktrees_dir().join("feats")
}

/// Directory for a single durable feats worktree under [`feats_worktrees_dir`].
///
/// `<state_dir>/catenary/worktrees/feats/<repo-basename>/<branch>/`. The
/// `repo_basename` is the source repo's directory name (a collision across two
/// repos of the same basename is refused with a rename hint rather than
/// uglifying the common case); `branch` slashes map to nested directories, so a
/// `feature/auth` branch lands at `feats/<repo>/feature/auth/`.
#[must_use]
pub fn feat_worktree_dir(repo_basename: &str, branch: &str) -> PathBuf {
    let mut dir = feats_worktrees_dir().join(repo_basename);
    for segment in branch.split('/').filter(|s| !s.is_empty()) {
        dir = dir.join(segment);
    }
    dir
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
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── canonicalize_lenient (misc 230 follow-up) ──────────────────────────

    #[cfg(unix)]
    #[test]
    fn canonicalize_lenient_resolves_a_symlinked_prefix_for_an_absent_leaf() {
        // The whole point: plain `canonicalize` fails on a path whose leaf does
        // not exist, so callers fell back to the RAW spelling — a different key
        // than the same path gets once it exists. Both readings must agree.
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("src")).expect("mk src");
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("mk symlink");

        let absent = alias.join("src/ghost.rs");
        assert!(!absent.exists(), "the leaf must be absent for this case");
        assert!(
            absent.canonicalize().is_err(),
            "plain canonicalize must fail here — that is the hole being closed"
        );

        let resolved = canonicalize_lenient(&absent);
        let expected = real
            .canonicalize()
            .expect("canon real")
            .join("src/ghost.rs");
        assert_eq!(
            resolved, expected,
            "the symlinked prefix resolves and the absent tail is kept"
        );

        // …and once the file exists, plain canonicalize agrees with what the
        // lenient form already answered. This equality IS the invariant.
        std::fs::write(&absent, b"x").expect("create the leaf");
        assert_eq!(
            absent.canonicalize().expect("canon once present"),
            resolved,
            "the absent and present readings must be the same spelling"
        );
    }

    #[test]
    fn canonicalize_lenient_matches_canonicalize_when_everything_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("present.rs");
        std::fs::write(&file, b"x").expect("write");
        assert_eq!(
            canonicalize_lenient(&file),
            file.canonicalize().expect("canon"),
            "where every component exists this is exactly canonicalize"
        );
    }

    #[test]
    fn canonicalize_lenient_keeps_a_deep_absent_tail() {
        // Not just the leaf: whole directory chains that do not exist yet
        // (`mkdir -p`-style targets) keep their tail below the resolved base.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canon base");
        let deep = dir.path().join("a/b/c/d.rs");
        assert_eq!(canonicalize_lenient(&deep), base.join("a/b/c/d.rs"));
    }

    #[test]
    fn canonicalize_lenient_folds_dot_and_dotdot_before_resolving() {
        // `..` must not survive into the tail — it would key a spelling no
        // consult would ever reproduce.
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canon base");
        std::fs::create_dir_all(dir.path().join("src")).expect("mk src");
        let noisy = dir.path().join("src/../src/./ghost.rs");
        assert_eq!(canonicalize_lenient(&noisy), base.join("src/ghost.rs"));
    }

    #[test]
    fn canonicalize_lenient_degrades_to_the_lexical_form_when_nothing_resolves() {
        // A relative path with no existing base has nothing to resolve; the
        // contract is "never an error", so it comes back lexically normalized.
        assert_eq!(
            canonicalize_lenient(Path::new("no/such/./base/../leaf.rs")),
            PathBuf::from("no/such/leaf.rs")
        );
    }

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
    fn feats_worktrees_dir_is_the_feats_subtree() {
        let dir = feats_worktrees_dir();
        assert!(
            dir.starts_with(worktrees_dir()),
            "feats subtree must live under the worktrees root",
        );
        assert!(
            dir.ends_with("catenary/worktrees/feats"),
            "feats subtree must be `<state>/catenary/worktrees/feats`, got {}",
            dir.display(),
        );
    }

    #[test]
    fn feat_worktree_dir_nests_branch_slashes() {
        let dir = feat_worktree_dir("OmniDSP", "feature/accelerate");
        assert!(
            dir.starts_with(feats_worktrees_dir()),
            "feat worktree dir must live under the feats subtree",
        );
        // `feats/<repo>/<branch-with-slashes-nested>`.
        assert!(
            dir.ends_with("feats/OmniDSP/feature/accelerate"),
            "unexpected feat leaf path: {}",
            dir.display(),
        );
    }

    #[test]
    fn feat_worktree_dir_flat_branch() {
        let dir = feat_worktree_dir("OmniDSP", "accelerate");
        assert!(dir.ends_with("feats/OmniDSP/accelerate"));
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

    // ── compress_home (the one `~`-compressing helper, misc 229) ──

    #[test]
    fn compress_home_renders_home_itself_as_bare_tilde() {
        let Some(home) = home_dir() else {
            return; // homeless host — the compression is a no-op by contract
        };
        assert_eq!(compress_home(&home), "~");
    }

    #[test]
    fn compress_home_renders_a_path_under_home_relative() {
        let Some(home) = home_dir() else {
            return;
        };
        assert_eq!(
            compress_home(&home.join("Projects/Widget")),
            "~/Projects/Widget"
        );
    }

    #[test]
    fn compress_home_leaves_a_path_outside_home_absolute() {
        assert_eq!(compress_home(Path::new("/srv/project")), "/srv/project");
    }

    // ── The raw-`$HOME` gate (misc 229) ───────────────────────────

    /// No production code may read `HOME` from the environment directly — every
    /// home-rooted path resolves through [`home_dir`].
    ///
    /// Bug 149 closed the `dirs::*` route with `clippy.toml`'s
    /// `disallowed-methods` gate, but a raw `std::env::var`/`var_os` of `HOME`
    /// walks straight past it: `std::env::var` is legitimately everywhere and
    /// clippy cannot key a denial on an *argument*. So this class gets a source
    /// scan instead. What it protects is the *next* home-rooted write: built on
    /// a raw read it escapes both `CATENARY_HOME_DIR` and `isolate_env`
    /// silently, and a test rewrites the operator's real `~/.claude` (the
    /// bug-109/149 family).
    ///
    /// `#[cfg(test)]` code is exempt — a test may legitimately read the real
    /// `HOME` — and the scan finds those regions structurally: a column-0
    /// `#[cfg(test)]` opens a region that ends at its item's column-0 closing
    /// delimiter (rustfmt puts every top-level item's closer there). An
    /// *indented* `#[cfg(test)]` (a test-only method inside an `impl`) is
    /// deliberately not exempt — the scan errs toward flagging, never toward
    /// waving code through.
    #[test]
    fn production_code_reads_home_only_through_the_paths_resolver() {
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&src_root, &mut files);
        assert!(
            files.len() > 50,
            "the source walk found only {} file(s) under {} — the walk is \
             broken, not the tree",
            files.len(),
            src_root.display(),
        );

        // This file is the blessed home: the resolver, and this scan's needles.
        let blessed = src_root.join("paths.rs");
        let mut offenders = Vec::new();
        for file in files.iter().filter(|f| **f != blessed) {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            for (line_no, line) in production_lines(&text) {
                if line.trim_start().starts_with("//") {
                    continue; // prose about the class, not a read
                }
                if line.contains("var(\"HOME\")") || line.contains("var_os(\"HOME\")") {
                    offenders.push(format!("{}:{line_no}", file.display()));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "raw `$HOME` reads in production code — route them through \
             `crate::paths::home_dir()`, whose `CATENARY_HOME_DIR` override is \
             what keeps home-rooted paths inside test isolation (misc 229):\n  {}",
            offenders.join("\n  "),
        );
    }

    /// Every `.rs` file under `dir`, recursively.
    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// The `(1-based line number, line)` pairs of `source` that live outside a
    /// top-level `#[cfg(test)]` item.
    fn production_lines(source: &str) -> Vec<(usize, &str)> {
        let lines: Vec<&str> = source.lines().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if lines[i] != "#[cfg(test)]" {
                out.push((i + 1, lines[i]));
                i += 1;
                continue;
            }
            i += 1;
            // Further attributes / doc comments before the item head.
            while i < lines.len() && (lines[i].starts_with("#[") || lines[i].starts_with("//")) {
                let one_line = lines[i].starts_with("//") || lines[i].trim_end().ends_with(']');
                i += 1;
                if !one_line {
                    // A wrapped attribute: run to its column-0 `)]`.
                    while i < lines.len() && !closes_at_column_zero(lines[i]) {
                        i += 1;
                    }
                    i += 1;
                }
            }
            // The item itself: a one-liner ends at its `;`, a block at its
            // column-0 closing delimiter.
            let Some(head) = lines.get(i) else { break };
            let is_block = !head.trim_end().ends_with(';');
            i += 1;
            if is_block {
                while i < lines.len() && !closes_at_column_zero(lines[i]) {
                    i += 1;
                }
                i += 1;
            }
        }
        out
    }

    /// Whether `line` opens a top-level item's closer (`}`, `];`, `)]`) —
    /// rustfmt puts it at column 0.
    fn closes_at_column_zero(line: &str) -> bool {
        line.starts_with('}') || line.starts_with(']') || line.starts_with(')')
    }
}
