// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Runtime regeneration of the Antigravity rules file (teaching-surface ticket
//! 12).
//!
//! Antigravity's `always_on` rules file is re-injected into the model context
//! every conversation turn, so a file rewritten at hook time is the live,
//! compaction-proof delivery channel: when the Antigravity `PreInvocation` hook
//! fires (detection = the host's own hook firing), Catenary regenerates the
//! *installed* copy of its own rules file to the live workspace-invariant teaching
//! surface ([`crate::cli::teaching::context_file_body`]).
//!
//! Whether Antigravity re-reads the rules file from disk *per turn* is
//! **unconfirmed** — the host's agent source is opaque and no primary source
//! documents the cadence. The rewrite is correct either way: if the file is
//! re-read per turn, each turn sees the live surface; if it is cached per
//! conversation, the rewrite still lands by the next conversation start — strictly
//! better than install-time-only content.
//!
//! The rewrite is:
//! - **Location-resolved the way `catenary install` does** — the Antigravity plugin
//!   dir (`~/.gemini/config/plugins/catenary`). A missing location (plugin not
//!   installed) is skipped silently.
//! - **Link-install guarded** — a rewrite must never dirty a developer's git
//!   worktree (a dev plugin install is a symlink into the repo). The guard skips
//!   when the install dir is a symlink, or when the resolved target's ancestry
//!   contains a `.git` dir/file (a git worktree).
//! - **Hash-gated and atomic** — render, read, compare; only on a difference is a
//!   temp file written and renamed into place (same dir, atomic). The no-op path
//!   (`PreInvocation` fires per model call) is one render + one read + compare.
//! - **Fail-open** — every error path is swallowed at `debug` level. The hook's
//!   primary job (injection / response) must never be blocked by the rewrite.
//!
//! The shipped file remains the cold bootstrap (build-time content unchanged); the
//! runtime-rewritten installed file diverges from it by design, carrying a
//! generation stamp so `catenary doctor` accepts it (see
//! [`crate::cli::teaching::is_runtime_stamped`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A resolved runtime-rewrite target: the installed file to rewrite plus whether
/// it is a developer link/symlink install (which the rewrite must leave alone).
struct RewriteTarget {
    /// The installed context/rules file to regenerate.
    path: PathBuf,
    /// The install is a developer link/symlink (skip the rewrite).
    is_dev_link: bool,
}

/// Regenerate the installed Antigravity rules file to the live surface (fail-open).
///
/// Called from the Antigravity `PreInvocation` hook, which fires per model call —
/// the hash gate keeps the no-op path a render + read + compare. Any error is
/// swallowed at `debug` level so the hook's first-sighting injection is never
/// blocked.
pub(crate) fn regenerate_antigravity_rules() {
    if let Err(e) = try_regenerate(
        antigravity_rewrite_target(),
        crate::cli::teaching::antigravity_rules_file,
    ) {
        tracing::debug!(host = "antigravity", error = %format!("{e:#}"), "context-file regeneration failed");
    }
}

/// The shared regenerate flow: resolve → link guard → hash-gated atomic write.
///
/// `content_fn` is deferred behind the link guard so a developer link/worktree
/// install pays no render cost. A `None` target (host not installed) is a silent
/// no-op.
fn try_regenerate(
    target: Option<RewriteTarget>,
    content_fn: impl FnOnce() -> String,
) -> Result<()> {
    let Some(target) = target else {
        // Host not installed — nothing to regenerate, no signal.
        return Ok(());
    };

    // Link-install guard: never rewrite a developer's git worktree. A dev plugin
    // install is a symlink into the Catenary repo, so a naive rewrite would dirty
    // the worktree and trip the shipped-file freshness test locally. Copy installs
    // (no symlink, no `.git` ancestor) rewrite freely.
    if target.is_dev_link || resolves_into_git_worktree(&target.path) {
        tracing::debug!(
            path = %target.path.display(),
            "context-file rewrite skipped: developer link/worktree install"
        );
        return Ok(());
    }

    if write_if_changed(&target.path, &content_fn())? {
        tracing::debug!(path = %target.path.display(), "regenerated context file to the live surface");
    }
    Ok(())
}

/// Whether `path` (after symlink resolution) lies inside a git worktree — a
/// `.git` dir or file in its ancestry.
///
/// The primary link-install detection. Canonicalizing first follows a link
/// install's symlink into the developer repo, where an ancestor `.git` (a dir in
/// a normal clone, a *file* in a linked worktree) is found and the rewrite is
/// skipped. A copy install under `~/.gemini/config/plugins/...` has no `.git`
/// ancestor and rewrites freely. Conservative by construction: a home dir that is itself a git
/// checkout (dotfile repos) reads as a worktree and is left alone — exactly the
/// bias the guard wants, since rewriting there would dirty that repo too.
fn resolves_into_git_worktree(path: &Path) -> bool {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    resolved.ancestors().any(|dir| dir.join(".git").exists())
}

/// Hash-gated atomic write: rewrite `target` to `content` only when they differ.
///
/// Reads the current file and compares; on a match returns `Ok(false)` with no
/// write (the hot, no-op path). On a difference writes a temp file in the same
/// directory and renames it into place — atomic on one filesystem — returning
/// `Ok(true)`. The temp name carries the pid so concurrent hooks never collide,
/// and a failed rename cleans the temp up.
fn write_if_changed(target: &Path, content: &str) -> Result<bool> {
    if std::fs::read_to_string(target).ok().as_deref() == Some(content) {
        return Ok(false);
    }

    let parent = target
        .parent()
        .context("context file has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create directory {}", parent.display()))?;

    let tmp = parent.join(format!(".catenary-context-{}.tmp", std::process::id()));
    std::fs::write(&tmp, content).with_context(|| format!("write temp {}", tmp.display()))?;

    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::Error::new(e).context(format!("rename temp into {}", target.display())));
    }
    Ok(true)
}

/// Resolve the installed Antigravity rules file to rewrite, the way `catenary
/// install` resolves it (`~/.gemini/config/plugins/catenary/rules/catenary.md`).
///
/// Returns `None` when the plugin is not installed. A symlinked plugin dir is a
/// developer install and is flagged as a dev link so the rewrite skips it.
///
/// The home comes from [`crate::paths::home_dir`], **not** `dirs::home_dir()`
/// (bug 149): this function names a file Catenary *writes*, and `isolate_env`
/// does not redirect `$HOME`, so resolving through the OS answer let an
/// agy-format subprocess test rewrite the operator's real rules file. The
/// resolver's `CATENARY_HOME_DIR` override is what keeps the rewrite inside the
/// tempdir under test; production, with no override set, resolves exactly as
/// before.
fn antigravity_rewrite_target() -> Option<RewriteTarget> {
    let home = crate::paths::home_dir()?;
    let plugin_dir = home.join(".gemini/config/plugins/catenary");
    let is_symlink = plugin_dir.is_symlink();
    if !plugin_dir.is_dir() && !is_symlink {
        // Antigravity plugin not installed — nothing to regenerate.
        return None;
    }
    Some(RewriteTarget {
        path: plugin_dir.join("rules/catenary.md"),
        is_dev_link: is_symlink,
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn git_worktree_guard_detects_git_dir() {
        // A normal clone marks its root with a `.git` directory.
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect("mk .git dir");
        let nested = repo.path().join("plugins/catenary-antigravity/rules");
        std::fs::create_dir_all(&nested).expect("mk nested");
        let target = nested.join("catenary.md");
        std::fs::write(&target, "x").expect("write target");
        assert!(
            resolves_into_git_worktree(&target),
            "a `.git` dir ancestor is a worktree — rewrite must skip"
        );
    }

    #[test]
    fn git_worktree_guard_detects_git_file() {
        // A linked git worktree marks its root with a `.git` *file*, not a dir.
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join(".git"), "gitdir: /elsewhere\n").expect("write .git file");
        let target = repo.path().join("catenary.md");
        std::fs::write(&target, "x").expect("write target");
        assert!(
            resolves_into_git_worktree(&target),
            "a `.git` file ancestor is a worktree — rewrite must skip"
        );
    }

    #[test]
    fn git_worktree_guard_false_outside_repo() {
        // A copy install with no `.git` ancestor rewrites freely.
        let plain = tempfile::tempdir().expect("tempdir");
        let target = plain.path().join("catenary.md");
        std::fs::write(&target, "x").expect("write target");
        assert!(
            !resolves_into_git_worktree(&target),
            "no `.git` ancestor → free to rewrite"
        );
    }

    #[test]
    fn write_if_changed_writes_then_noops_then_rewrites() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Parent does not exist yet — the write must create it.
        let target = dir.path().join("sub/context.md");

        assert!(
            write_if_changed(&target, "alpha\n").expect("first write"),
            "first write happens"
        );
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "alpha\n");

        assert!(
            !write_if_changed(&target, "alpha\n").expect("no-op"),
            "identical content is a no-op (hash gate hit)"
        );

        assert!(
            write_if_changed(&target, "beta\n").expect("rewrite"),
            "changed content rewrites"
        );
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "beta\n");

        // The atomic rename leaves no temp file behind.
        let leftovers = std::fs::read_dir(target.parent().expect("parent"))
            .expect("read dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains("catenary-context"))
            .count();
        assert_eq!(leftovers, 0, "no temp file left after atomic rename");
    }

    #[test]
    fn try_regenerate_none_target_is_silent_ok() {
        // Host not installed → resolver yields None → silent no-op, and the
        // content builder is never invoked.
        let res = try_regenerate(None, || {
            unreachable!("content must not be rendered when the host is uninstalled")
        });
        assert!(res.is_ok(), "a None target must be a silent Ok");
    }

    #[test]
    fn try_regenerate_skips_a_git_worktree_target_without_writing() {
        // A target resolving into a git worktree is skipped — nothing is written,
        // even though the install is not flagged a dev link (the git-ancestry
        // backstop catches copy-into-repo installs too).
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect("mk .git dir");
        let target = repo.path().join("catenary.md");

        let res = try_regenerate(
            Some(RewriteTarget {
                path: target.clone(),
                is_dev_link: false,
            }),
            || "SHOULD NOT BE WRITTEN".to_string(),
        );
        assert!(res.is_ok(), "the guard skip is a fail-open Ok");
        assert!(
            !target.exists(),
            "a git-worktree target must not be written"
        );
    }

    #[test]
    fn try_regenerate_writes_a_clean_target() {
        // A plain (non-link, non-worktree) target is rewritten to the rendered
        // content and no-ops on a second identical run.
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("catenary.md");

        let render = || "STAMPED BODY\n".to_string();
        try_regenerate(
            Some(RewriteTarget {
                path: target.clone(),
                is_dev_link: false,
            }),
            render,
        )
        .expect("write");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "STAMPED BODY\n"
        );

        // Second run with identical content: hash gate → no-op (still Ok).
        try_regenerate(
            Some(RewriteTarget {
                path: target.clone(),
                is_dev_link: false,
            }),
            render,
        )
        .expect("no-op");
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "STAMPED BODY\n"
        );
    }
}
