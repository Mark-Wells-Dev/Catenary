// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Out-of-tree agent worktree creation for Claude Code's `WorktreeCreate` hook.
//!
//! Claude Code lets a plugin own worktree creation: the `WorktreeCreate` hook
//! receives a JSON payload on stdin and must print the absolute path of the
//! created worktree on stdout; a failure or an empty path fails worktree
//! creation (the host contract). Catenary uses this to relocate every subagent
//! worktree **out of the source repo tree**, under [`paths::worktrees_dir`]
//! (`<cache_dir>/catenary/worktrees/`).
//!
//! Relocation is bug 53's structural fix: a worktree nested *inside* a tracked
//! root is a second copy of the project that gitignore-blind server discovery
//! (rust-analyzer's cargo walk) indexes a second time, polluting the parent
//! root's index. A worktree that lives outside the repo can never be reached by
//! that downward walk — no per-server exclude configuration required (misc 144).
//!
//! A git worktree tracks its upstream repo through a `.git` **file**
//! (`gitdir: <repo>/.git/worktrees/<name>`), not its filesystem location, so the
//! subagent mount predicate ([`crate::router`]) and the deletion watch
//! ([`crate::worktree_watch`]) both work unchanged for an out-of-tree worktree.
//!
//! ## Orphan prune
//!
//! Claude Code cleans up git worktrees itself with `git worktree remove` (the
//! `WorktreeRemove` hook does not fire for git worktrees), which removes both the
//! worktree directory and its `.git/worktrees/<name>` metadata regardless of
//! where the directory lives. [`prune_orphans`] is the crash-safety backstop for
//! the rare case a directory lingers after its git linkage is already gone: it
//! runs `git worktree prune` semantics over the cache dir. It is invoked at the
//! start of every create (cheap, self-contained, and at exactly the cadence the
//! directory grows), so orphans never accumulate past the next spawn.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tracing::debug;

use crate::paths;
use crate::source::Source;

/// Create an out-of-tree agent worktree from a `WorktreeCreate` payload.
///
/// Returns the absolute path of the created worktree (the value the hook prints
/// to stdout). Steps:
///
/// 1. Resolve the source repo from the payload's `cwd` (Claude Code sends the
///    session working directory), falling back to the process working
///    directory, then to `git rev-parse --show-toplevel`.
/// 2. Prune orphaned cache-dir worktrees ([`prune_orphans`]).
/// 3. `git -C <repo> worktree add -b <branch> <cache-dir path>`.
///
/// # Errors
///
/// Returns an error — the loud, nonzero-exit failure the host contract requires
/// — when no repo can be resolved (missing/invalid `cwd`, or `cwd` outside any
/// git working tree) or when `git worktree add` fails.
pub fn create_from_payload(payload: &Value) -> Result<PathBuf> {
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow!("no `cwd` in payload and no process working directory"))?;

    let repo = repo_toplevel(&cwd)
        .with_context(|| format!("cannot resolve a git repository from {}", cwd.display()))?;

    // Prune before adding — cheap (a readdir + a stat per entry), self-contained,
    // and run at exactly the cadence the worktrees dir grows.
    let pruned = prune_orphans(&paths::worktrees_dir());
    if pruned > 0 {
        debug!(
            source = Source::HookDispatch.as_str(),
            pruned, "pruned orphaned cache-dir worktrees before create",
        );
    }

    let unique_id = short_id();
    let worktree = paths::agent_worktree_dir(&repo, &unique_id);
    let branch = branch_name(payload, &unique_id);

    // Ensure the worktrees root exists; `git worktree add` creates the leaf.
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktrees dir {}", parent.display()))?;
    }

    git_worktree_add(&repo, &worktree, &branch)?;

    Ok(worktree)
}

/// Resolve the toplevel of the git working tree containing `cwd`.
///
/// Runs `git -C <cwd> rev-parse --show-toplevel`. Errors when `cwd` is not
/// inside a git working tree (the loud missing-repo failure the hook contract
/// requires).
fn repo_toplevel(cwd: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("run `git rev-parse --show-toplevel`")?;
    if !output.status.success() {
        bail!(
            "`git rev-parse --show-toplevel` failed for {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        bail!(
            "`git rev-parse --show-toplevel` returned no path for {}",
            cwd.display(),
        );
    }
    Ok(PathBuf::from(path))
}

/// Run `git -C <repo> worktree add -b <branch> <worktree>`.
///
/// The new branch (created from the repo's `HEAD`) gives the lead's
/// landing-protocol cleanup (`git worktree remove` + `git branch -D`) a branch
/// to delete.
fn git_worktree_add(repo: &Path, worktree: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "-b", branch])
        .arg(worktree)
        .output()
        .context("run `git worktree add`")?;
    if !output.status.success() {
        bail!(
            "`git worktree add` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    debug!(
        source = Source::HookDispatch.as_str(),
        repo = %repo.display(),
        worktree = %worktree.display(),
        branch,
        "created out-of-tree agent worktree",
    );
    Ok(())
}

/// Choose the branch name for the new worktree.
///
/// Follows a payload-supplied name when present (Claude Code's
/// `worktree-agent-…` convention), else generates a unique `catenary-wt-<id>`.
/// The docs do not pin down the payload's name field, so several candidate
/// spellings are accepted leniently; the full payload is debug-logged at the
/// hook boundary so the real field surfaces on the first live run.
fn branch_name(payload: &Value, unique_id: &str) -> String {
    for key in ["branch", "branch_name", "worktree_name", "name"] {
        if let Some(name) = payload.get(key).and_then(Value::as_str) {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    format!("catenary-wt-{unique_id}")
}

/// A short unique id for the worktree directory and default branch name.
fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Remove orphaned worktree directories under `root` whose git linkage is dead.
///
/// `git worktree prune` semantics for the cache dir: an entry is orphaned when
/// its `.git` pointer names a `<repo>/.git/worktrees/<name>` metadata directory
/// that no longer exists (git deregistered it), or when it carries no `.git`
/// pointer at all (a partial/interrupted create). A live worktree — whose
/// metadata directory still exists — is always kept. Returns the number of
/// directories removed.
///
/// Best-effort and idempotent: a directory that cannot be read or removed is
/// left in place (a concurrent create may be sweeping the same root). A missing
/// `root` (no worktree ever created) prunes nothing.
#[must_use]
pub fn prune_orphans(root: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && linkage_dead(&path) && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Whether a cache-dir worktree directory's git linkage is dead.
///
/// Reads the worktree's `.git` file (`gitdir: <metadata>`); the linkage is dead
/// when the file is missing/unreadable, carries no `gitdir:` line, or names a
/// metadata directory that no longer exists on disk. A relative `gitdir:` is
/// resolved against the worktree directory (git writes an absolute path by
/// default, but tolerate both).
fn linkage_dead(worktree: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(worktree.join(".git")) else {
        return true;
    };
    let Some(gitdir) = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
    else {
        return true;
    };
    let metadata = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        worktree.join(gitdir)
    };
    !metadata.exists()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::{branch_name, linkage_dead, prune_orphans};

    #[test]
    fn branch_name_prefers_payload_name() {
        let payload = serde_json::json!({ "branch": "worktree-agent-abc" });
        assert_eq!(branch_name(&payload, "xyz"), "worktree-agent-abc");
    }

    #[test]
    fn branch_name_generates_when_absent() {
        let payload = serde_json::json!({ "session_id": "s1" });
        assert_eq!(branch_name(&payload, "xyz"), "catenary-wt-xyz");
    }

    #[test]
    fn branch_name_ignores_blank_supplied_name() {
        let payload = serde_json::json!({ "name": "   " });
        assert_eq!(branch_name(&payload, "xyz"), "catenary-wt-xyz");
    }

    #[test]
    fn linkage_dead_true_when_metadata_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                tmp.path().join("gone/worktrees/x").display()
            ),
        )
        .expect("write .git");
        assert!(linkage_dead(&wt));
    }

    #[test]
    fn linkage_dead_false_when_metadata_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let meta = tmp.path().join("repo/.git/worktrees/live");
        std::fs::create_dir_all(&meta).expect("mkdir meta");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", meta.display()))
            .expect("write .git");
        assert!(!linkage_dead(&wt));
    }

    #[test]
    fn linkage_dead_true_when_no_git_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        assert!(linkage_dead(&wt));
    }

    #[test]
    fn prune_removes_dead_keeps_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Simulated main repos (metadata) live outside the swept root so they are
        // never themselves considered worktree entries.
        let repos = tmp.path().join("repos");
        let wt_root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&repos).expect("mkdir repos");
        std::fs::create_dir_all(&wt_root).expect("mkdir wt_root");

        // Live: `.git` points at an existing metadata dir.
        let meta = repos.join("repo/.git/worktrees/live");
        std::fs::create_dir_all(&meta).expect("mkdir meta");
        let live = wt_root.join("live-wt");
        std::fs::create_dir_all(&live).expect("mkdir live");
        std::fs::write(live.join(".git"), format!("gitdir: {}\n", meta.display()))
            .expect("write live .git");

        // Dead: `.git` points at a metadata dir that does not exist.
        let dead = wt_root.join("dead-wt");
        std::fs::create_dir_all(&dead).expect("mkdir dead");
        std::fs::write(
            dead.join(".git"),
            format!(
                "gitdir: {}\n",
                repos.join("gone/.git/worktrees/x").display()
            ),
        )
        .expect("write dead .git");

        // Malformed: no `.git` file at all (interrupted create).
        let malformed = wt_root.join("malformed-wt");
        std::fs::create_dir_all(&malformed).expect("mkdir malformed");

        let removed = prune_orphans(&wt_root);
        assert_eq!(removed, 2, "the dead and malformed entries are pruned");
        assert!(live.exists(), "the live worktree is kept");
        assert!(!dead.exists(), "the dead worktree is removed");
        assert!(!malformed.exists(), "the malformed entry is removed");
    }

    #[test]
    fn prune_missing_root_is_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(prune_orphans(&tmp.path().join("does-not-exist")), 0);
    }
}
