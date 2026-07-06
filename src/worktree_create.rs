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
//! ## Payload schema
//!
//! Claude Code's confirmed `WorktreeCreate` payload carries `session_id`,
//! `transcript_path`, `cwd` (the repo root), `hook_event_name`, and `name` (the
//! worktree name — user-chosen for `--worktree`, generated for subagents). Only
//! `cwd` is required here; `name` is the documented branch-name field
//! ([`branch_name`]). The payload is parsed leniently (a bare [`Value`]), so
//! extra/renamed fields are tolerated and any doc drift surfaces on the first
//! live run through the hook-boundary debug log.
//!
//! ## Local config (`.worktreeinclude`)
//!
//! A fresh worktree is a clean checkout, so untracked, git-ignored local config
//! (the `.env` class) is absent. Claude Code's default `--worktree` flow copies
//! such files via a `<repo>/.worktreeinclude` file (`.gitignore` pattern syntax);
//! because this hook **replaces** that default entirely for every plugin user, it
//! reimplements the copy ([`copy_worktree_includes`]) or the host feature would
//! silently regress (misc 144).
//!
//! ## VCS detection
//!
//! `cwd` is examined for a version-control marker before any git call: `.git`
//! (a dir or a worktree file) proceeds on the git path; a non-git marker (`.svn`,
//! `.hg`, `.jj`, in `cwd` or an ancestor) fails with a single honest line that
//! names the detected VCS; no marker at all fails as "not a version-controlled
//! directory". A non-git working copy therefore never sees a raw git error
//! (misc 144 — VCS detection is in-scope, VCS *support* is not).
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
use tracing::{debug, warn};

use crate::paths;
use crate::source::Source;

/// Create an out-of-tree agent worktree from a `WorktreeCreate` payload.
///
/// Returns the absolute path of the created worktree (the value the hook prints
/// to stdout). Steps:
///
/// 1. Resolve the payload's `cwd` (Claude Code sends the repo root), falling back
///    to the process working directory.
/// 2. Detect the VCS at `cwd` ([`detect_vcs`]): git proceeds; a non-git working
///    copy (`.svn`/`.hg`/`.jj`) or an unversioned directory fails with an honest,
///    VCS-named line — never a raw git error.
/// 3. Resolve the source repo with `git rev-parse --show-toplevel`.
/// 4. Prune orphaned cache-dir worktrees ([`prune_orphans`]).
/// 5. `git -C <repo> worktree add -b <branch> <cache-dir path>`.
/// 6. Copy `.worktreeinclude`-matched local config into the worktree
///    ([`copy_worktree_includes`]).
///
/// # Errors
///
/// Returns an error — the loud, nonzero-exit failure the host contract requires
/// — when no repo can be resolved (missing/invalid `cwd`, a non-git or
/// unversioned `cwd`, or `cwd` outside any git working tree) or when
/// `git worktree add` fails.
pub fn create_from_payload(payload: &Value) -> Result<PathBuf> {
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| anyhow!("no `cwd` in payload and no process working directory"))?;

    // VCS detection precedes any git call so a non-git working copy gets an
    // honest, VCS-named refusal instead of a raw git error (misc 144, item 5).
    match detect_vcs(&cwd) {
        VcsPosture::Git => {}
        VcsPosture::Foreign(vcs) => bail!("{}", vcs.refusal()),
        VcsPosture::Unversioned => bail!(
            "{} is not a version-controlled directory (no .git, .svn, .hg, or .jj marker found)",
            cwd.display(),
        ),
    }

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

    // Reimplement Claude Code's `.worktreeinclude` copy: the hook replaces the
    // host default entirely, so untracked local config (the `.env` class) a fresh
    // checkout lacks must be carried into the worktree here (misc 144, hard
    // requirement). Best-effort — never fails a created worktree.
    copy_worktree_includes(&repo, &worktree);

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

/// A non-git version-control system Catenary's worktree hook detects and honestly
/// refuses. Catenary's hook serves git repos only (misc 144 — VCS detection is
/// in-scope, VCS *support* is not, per decision 030).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForeignVcs {
    /// Subversion — a modern working copy carries a `.svn` dir at its root.
    Svn,
    /// Mercurial — a repository carries a `.hg` dir at its root.
    Hg,
    /// Jujutsu — a repository carries a `.jj` dir at its root.
    Jj,
}

impl ForeignVcs {
    /// The short marker name for the "configure your own hook for &lt;vcs&gt;"
    /// clause.
    const fn marker(self) -> &'static str {
        match self {
            Self::Svn => "svn",
            Self::Hg => "hg",
            Self::Jj => "jj",
        }
    }

    /// The article + name for the "this is &lt;label&gt; working copy" clause.
    const fn label(self) -> &'static str {
        match self {
            Self::Svn => "an svn",
            Self::Hg => "a mercurial",
            Self::Jj => "a jujutsu",
        }
    }

    /// The single honest stderr line refusing a non-git working copy, naming the
    /// detected VCS.
    fn refusal(self) -> String {
        format!(
            "this is {} working copy; catenary's worktree hook serves git repos — \
             configure your own WorktreeCreate hook for {}",
            self.label(),
            self.marker(),
        )
    }
}

/// The version-control posture of a directory (and its ancestors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VcsPosture {
    /// A `.git` marker (dir or worktree file) — the supported relocation path.
    Git,
    /// A non-git marker — refused with [`ForeignVcs::refusal`].
    Foreign(ForeignVcs),
    /// No VCS marker in `cwd` or any ancestor.
    Unversioned,
}

/// Detect the version-control posture of `cwd` by walking it and its ancestors
/// for VCS markers.
///
/// At each level `.git` (dir **or** worktree file) wins — a git worktree's `cwd`
/// carries a `.git` file, and either shape proceeds on the git path. Otherwise a
/// non-git marker (`.svn`/`.hg`/`.jj`, laid out at the working-copy/repo root for
/// each system) yields [`VcsPosture::Foreign`]. The first level bearing any
/// marker decides; a walk to the filesystem root with none yields
/// [`VcsPosture::Unversioned`].
fn detect_vcs(cwd: &Path) -> VcsPosture {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return VcsPosture::Git;
        }
        if d.join(".svn").exists() {
            return VcsPosture::Foreign(ForeignVcs::Svn);
        }
        if d.join(".hg").exists() {
            return VcsPosture::Foreign(ForeignVcs::Hg);
        }
        if d.join(".jj").exists() {
            return VcsPosture::Foreign(ForeignVcs::Jj);
        }
        dir = d.parent();
    }
    VcsPosture::Unversioned
}

/// Copy `.worktreeinclude`-matched local config files from the source repo into
/// the freshly created worktree.
///
/// Reimplements Claude Code's default `--worktree` behavior, which this hook
/// replaces entirely: a fresh worktree is a clean checkout, so untracked,
/// git-ignored local config (`.env`, `.env.local`, `config/secrets.json`, …) is
/// absent unless copied in. Per the host docs, a `<repo>/.worktreeinclude` file
/// lists the patterns to copy in **`.gitignore` syntax** (one per line; blank
/// lines and `#` comments tolerated).
///
/// A matched file is copied into the worktree preserving its repo-relative path.
/// Any destination that **already exists** in the freshly checked-out worktree is
/// skipped, so tracked files (present after checkout) are never duplicated or
/// clobbered — matching the docs' guarantee that only files "also gitignored" are
/// copied. `.git` is never descended into.
///
/// Best-effort throughout: a missing/unreadable `.worktreeinclude` is a silent
/// no-op; a malformed pattern warns and is skipped (other patterns still apply);
/// a per-file copy failure warns and continues. None of these fail worktree
/// creation.
fn copy_worktree_includes(repo: &Path, worktree: &Path) {
    let Ok(contents) = std::fs::read_to_string(repo.join(".worktreeinclude")) else {
        return; // No `.worktreeinclude` (or unreadable) — nothing to copy.
    };

    let mut builder = ignore::gitignore::GitignoreBuilder::new(repo);
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Err(e) = builder.add_line(None, trimmed) {
            warn!(
                source = Source::HookDispatch.as_str(),
                pattern = trimmed,
                error = %e,
                ".worktreeinclude pattern is malformed; skipping it",
            );
        }
    }
    let matcher = match builder.build() {
        Ok(matcher) => matcher,
        Err(e) => {
            warn!(
                source = Source::HookDispatch.as_str(),
                error = %e,
                "cannot build .worktreeinclude matcher; skipping include processing",
            );
            return;
        }
    };

    let walker = ignore::WalkBuilder::new(repo)
        .standard_filters(false) // include hidden + git-ignored files (the point)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();

    let mut copied = 0usize;
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if !matcher.matched(path, false).is_ignore() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(repo) else {
            continue;
        };
        let dest = worktree.join(rel);
        if dest.exists() {
            // Already checked out (a tracked file) or already copied — never
            // duplicate or clobber the worktree's own copy.
            continue;
        }
        if let Some(parent) = dest.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!(
                source = Source::HookDispatch.as_str(),
                path = %rel.display(),
                error = %e,
                "cannot create parent dir for .worktreeinclude copy; skipping",
            );
            continue;
        }
        match std::fs::copy(path, &dest) {
            Ok(_) => copied += 1,
            Err(e) => warn!(
                source = Source::HookDispatch.as_str(),
                path = %rel.display(),
                error = %e,
                "cannot copy .worktreeinclude file into worktree; skipping",
            ),
        }
    }
    if copied > 0 {
        debug!(
            source = Source::HookDispatch.as_str(),
            copied, "copied .worktreeinclude local config files into new worktree",
        );
    }
}

/// Choose the branch name for the new worktree.
///
/// Follows a payload-supplied name when present, else generates a unique
/// `catenary-wt-<id>`. The confirmed `WorktreeCreate` schema names the field
/// `name` (the worktree name — user-chosen for `--worktree`, generated for
/// subagents), so it is checked **first**; the earlier lenient candidates
/// (`branch`/`branch_name`/`worktree_name`) remain as a drift net, and the full
/// payload is debug-logged at the hook boundary so any schema change still
/// surfaces on the first live run.
fn branch_name(payload: &Value, unique_id: &str) -> String {
    for key in ["name", "branch", "branch_name", "worktree_name"] {
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
    use std::path::Path;

    use super::{
        ForeignVcs, VcsPosture, branch_name, copy_worktree_includes, detect_vcs, linkage_dead,
        prune_orphans,
    };

    #[test]
    fn branch_name_prefers_payload_name() {
        let payload = serde_json::json!({ "branch": "worktree-agent-abc" });
        assert_eq!(branch_name(&payload, "xyz"), "worktree-agent-abc");
    }

    #[test]
    fn branch_name_prefers_name_over_other_candidates() {
        // The confirmed schema's `name` wins over the lenient drift-net keys.
        let payload = serde_json::json!({ "name": "feature-auth", "branch": "other" });
        assert_eq!(branch_name(&payload, "xyz"), "feature-auth");
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

    // ── .worktreeinclude copy ──────────────────────────────────────────────

    /// Build a `repo`/`worktree` pair under a fresh tempdir for the copy tests.
    fn repo_and_worktree() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::create_dir_all(&worktree).expect("mkdir worktree");
        (tmp, repo, worktree)
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir parent");
        }
        std::fs::write(path, contents).expect("write file");
    }

    #[test]
    fn copy_lands_untracked_matching_file() {
        let (_tmp, repo, worktree) = repo_and_worktree();
        write(&repo.join(".worktreeinclude"), ".env\n");
        write(&repo.join(".env"), "SECRET=1");

        copy_worktree_includes(&repo, &worktree);

        assert_eq!(
            std::fs::read_to_string(worktree.join(".env")).expect("read copied .env"),
            "SECRET=1",
        );
    }

    #[test]
    fn copy_preserves_nested_relative_path() {
        let (_tmp, repo, worktree) = repo_and_worktree();
        write(&repo.join(".worktreeinclude"), "config/secrets.json\n");
        write(&repo.join("config/secrets.json"), "{}");

        copy_worktree_includes(&repo, &worktree);

        assert_eq!(
            std::fs::read_to_string(worktree.join("config/secrets.json"))
                .expect("read copied nested file"),
            "{}",
        );
    }

    #[test]
    fn copy_skips_non_matching_file() {
        let (_tmp, repo, worktree) = repo_and_worktree();
        write(&repo.join(".worktreeinclude"), ".env\n");
        write(&repo.join("notes.txt"), "not matched");

        copy_worktree_includes(&repo, &worktree);

        assert!(
            !worktree.join("notes.txt").exists(),
            "a file matching no pattern must not be copied",
        );
    }

    #[test]
    fn copy_no_include_file_is_noop() {
        let (_tmp, repo, worktree) = repo_and_worktree();
        write(&repo.join(".env"), "SECRET=1");

        copy_worktree_includes(&repo, &worktree);

        assert!(
            !worktree.join(".env").exists(),
            "without a .worktreeinclude nothing is copied",
        );
    }

    #[test]
    fn copy_malformed_line_warns_and_others_still_copied() {
        let (_tmp, repo, worktree) = repo_and_worktree();
        // `a**b` is an invalid `**` usage — rejected by the glob compiler — while
        // `.env` on the next line stays valid and must still be copied.
        write(&repo.join(".worktreeinclude"), "a**b\n.env\n");
        write(&repo.join(".env"), "SECRET=1");

        copy_worktree_includes(&repo, &worktree);

        assert_eq!(
            std::fs::read_to_string(worktree.join(".env")).expect("read copied .env"),
            "SECRET=1",
            "a malformed pattern is skipped but valid patterns still apply",
        );
    }

    #[test]
    fn copy_skips_path_already_in_worktree() {
        let (_tmp, repo, worktree) = repo_and_worktree();
        // A pattern that matches a tracked file already checked out in the
        // worktree: the worktree copy must not be duplicated or clobbered.
        write(&repo.join(".worktreeinclude"), "tracked.txt\n");
        write(&repo.join("tracked.txt"), "repo-version");
        write(&worktree.join("tracked.txt"), "worktree-version");

        copy_worktree_includes(&repo, &worktree);

        assert_eq!(
            std::fs::read_to_string(worktree.join("tracked.txt")).expect("read worktree file"),
            "worktree-version",
            "an existing worktree path must not be clobbered",
        );
    }

    #[test]
    fn copy_ignores_comments_and_blank_lines() {
        let (_tmp, repo, worktree) = repo_and_worktree();
        write(&repo.join(".worktreeinclude"), "# a comment\n\n.env\n");
        write(&repo.join(".env"), "SECRET=1");

        copy_worktree_includes(&repo, &worktree);

        assert!(
            worktree.join(".env").exists(),
            "comments and blank lines are tolerated; the real pattern still copies",
        );
    }

    // ── VCS detection ──────────────────────────────────────────────────────

    fn dir_with_marker(marker: &str, as_file: bool) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(marker);
        if as_file {
            std::fs::write(&path, "").expect("write marker file");
        } else {
            std::fs::create_dir_all(&path).expect("mkdir marker");
        }
        tmp
    }

    #[test]
    fn detect_git_dir_proceeds() {
        let tmp = dir_with_marker(".git", false);
        assert_eq!(detect_vcs(tmp.path()), VcsPosture::Git);
    }

    #[test]
    fn detect_git_file_proceeds() {
        // A git worktree's cwd carries a `.git` **file**, not a dir.
        let tmp = dir_with_marker(".git", true);
        assert_eq!(detect_vcs(tmp.path()), VcsPosture::Git);
    }

    #[test]
    fn detect_svn_hg_jj_are_foreign() {
        for (marker, vcs) in [
            (".svn", ForeignVcs::Svn),
            (".hg", ForeignVcs::Hg),
            (".jj", ForeignVcs::Jj),
        ] {
            let tmp = dir_with_marker(marker, false);
            assert_eq!(detect_vcs(tmp.path()), VcsPosture::Foreign(vcs), "{marker}");
        }
    }

    #[test]
    fn detect_walks_ancestors() {
        // Marker at the working-copy root; cwd is a nested subdirectory.
        let tmp = dir_with_marker(".svn", false);
        let nested = tmp.path().join("crate/src");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        assert_eq!(detect_vcs(&nested), VcsPosture::Foreign(ForeignVcs::Svn));
    }

    #[test]
    fn detect_unversioned_when_no_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(detect_vcs(tmp.path()), VcsPosture::Unversioned);
    }

    #[test]
    fn refusal_names_each_detected_vcs() {
        assert!(ForeignVcs::Svn.refusal().contains("svn"));
        assert!(ForeignVcs::Hg.refusal().contains("mercurial"));
        assert!(ForeignVcs::Hg.refusal().contains("hg"));
        assert!(ForeignVcs::Jj.refusal().contains("jujutsu"));
        assert!(ForeignVcs::Jj.refusal().contains("jj"));
        // The refusal is a single line — never a raw git error.
        assert!(!ForeignVcs::Svn.refusal().contains('\n'));
        assert!(ForeignVcs::Svn.refusal().contains("git repos"));
    }
}
