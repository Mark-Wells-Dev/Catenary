// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Out-of-tree agent worktree creation for Claude Code's `WorktreeCreate` hook.
//!
//! Claude Code lets a plugin own worktree creation: the `WorktreeCreate` hook
//! receives a JSON payload on stdin and must print the absolute path of the
//! created worktree on stdout; a failure or an empty path fails worktree
//! creation (the host contract). Catenary uses this to relocate every subagent
//! worktree **out of the source repo tree**, under [`paths::agents_worktrees_dir`]
//! (`<state_dir>/catenary/worktrees/agents/<session_id>/<segment>/` — the durable
//! state base, since a dirty agent worktree can hold the only copy of unlanded
//! work; misc 150).
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
//! where the directory lives. The crash-safety backstop for the rare case a
//! directory lingers after its git linkage is already gone sweeps **both**
//! locations at the start of every create: [`prune_agent_orphans`] over the
//! nested agents subtree (removing paired sidecars and emptied session parents)
//! and [`prune_orphans`] over the legacy cache location (stragglers from older
//! builds). Cheap, self-contained, and at exactly the cadence the directory
//! grows, so orphans never accumulate past the next spawn.
//!
//! ## Creation sidecar
//!
//! After a successful `git worktree add`, a [`WorktreeMeta`] sidecar is written
//! as a `<worktree-dir>.meta.json` **sibling** (never inside the worktree, so
//! `git status` stays pristine). It is the durable half of the daemon's worktree
//! registry: the daemon rehydrates the identity→path map by scanning the agents
//! subtree ([`scan_sidecars`]) on restart, so nothing durable is lost.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use crate::paths;
use crate::source::Source;

/// Worktree class recorded in the sidecar and the daemon registry.
///
/// Only `"agent"` exists today (misc 150); durable "feats" worktrees (misc 151)
/// will add a second class. Stored as a plain string so an unknown future class
/// round-trips through an older reader.
pub const WORKTREE_CLASS_AGENT: &str = "agent";

/// Durable creation metadata written beside each agent worktree as a
/// `<worktree-dir>.meta.json` **sibling** (never inside the worktree, so
/// `git status` stays pristine).
///
/// The durable half of the worktree registry (misc 150): it lets the daemon
/// rehydrate the identity→path map on restart by scanning the agents subtree,
/// and is the record misc 151's disposal will consume. One registration, two
/// consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeMeta {
    /// Canonical worktree directory (the tracked-root value the daemon keys on).
    pub worktree: PathBuf,
    /// The source repo the worktree was cut from (`git rev-parse --show-toplevel`).
    pub source_repo: PathBuf,
    /// `HEAD` commit at `git worktree add` time — the exact base the disposal
    /// clean-check compares `HEAD` against (misc 151), so it is recorded, not
    /// heuristic.
    pub base_commit: String,
    /// The branch created for the worktree (`git worktree add -b <branch>`).
    pub branch: String,
    /// The `WorktreeCreate` payload `name` verbatim (`agent-<id>` for a subagent,
    /// a user string for a `--worktree` session). A convention, not a contract —
    /// stored raw so a convention change is caught by the cwd fallback, never
    /// silently mis-parsed.
    pub name: String,
    /// The agent id parsed from `name` (`agent-<id>` → `<id>`), or `None` for a
    /// `--worktree` session whose `name` carries no agent identity.
    pub agent_id: Option<String>,
    /// The session that spawned the worktree (the parent session for a subagent).
    pub session_id: String,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Worktree class ([`WORKTREE_CLASS_AGENT`]).
    pub class: String,
}

/// Create an out-of-tree agent worktree from a `WorktreeCreate` payload.
///
/// Returns the [`WorktreeMeta`] describing the created worktree; the caller
/// prints `meta.worktree` to stdout (the host contract) and forwards `meta` to
/// the daemon for registration. Steps:
///
/// 1. Resolve the payload's `cwd` (Claude Code sends the repo root), falling back
///    to the process working directory.
/// 2. Detect the VCS at `cwd` ([`detect_vcs`]): git proceeds; a non-git working
///    copy (`.svn`/`.hg`/`.jj`) or an unversioned directory fails with an honest,
///    VCS-named line — never a raw git error.
/// 3. Resolve the source repo with `git rev-parse --show-toplevel`.
/// 4. Prune orphaned worktrees under the agents subtree and the legacy cache
///    location ([`prune_agent_orphans`] / [`prune_orphans`]).
/// 5. `git -C <repo> worktree add -b <branch> <agents-subtree path>`.
/// 6. Copy `.worktreeinclude`-matched local config into the worktree
///    ([`copy_worktree_includes`]).
/// 7. Write the durable sidecar ([`write_sidecar`], best-effort).
///
/// # Errors
///
/// Returns an error — the loud, nonzero-exit failure the host contract requires
/// — when no repo can be resolved (missing/invalid `cwd`, a non-git or
/// unversioned `cwd`, or `cwd` outside any git working tree) or when
/// `git worktree add` fails.
pub fn create_from_payload(payload: &Value) -> Result<WorktreeMeta> {
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
    // and run at exactly the cadence the worktrees dir grows. Sweep BOTH the new
    // agents subtree and the legacy cache location (stragglers from older builds).
    let pruned = prune_agent_orphans(&paths::agents_worktrees_dir())
        + prune_orphans(&paths::legacy_cache_worktrees_dir());
    if pruned > 0 {
        debug!(
            source = Source::HookDispatch.as_str(),
            pruned, "pruned orphaned agent worktrees before create",
        );
    }

    let unique_id = short_id();
    let raw_name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let agent_id = raw_name.and_then(parse_agent_id).map(str::to_string);
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let segment = worktree_segment(agent_id.as_deref(), raw_name, &unique_id);
    let worktree = paths::agent_worktree_dir(&session_id, &segment);
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

    // The base commit the disposal clean-check compares against (misc 151):
    // `HEAD` right after creation. Best-effort — an empty base is a keep-forever
    // conservative default, never a wrong deletion.
    let base_commit = git_head(&repo).unwrap_or_default();

    // Canonicalize so the registry value matches the daemon's canonicalized
    // mount key; the dir exists now, so canonicalization succeeds.
    let worktree = worktree.canonicalize().unwrap_or(worktree);

    let meta = WorktreeMeta {
        worktree,
        source_repo: repo,
        base_commit,
        branch,
        name: raw_name.unwrap_or("").to_string(),
        agent_id,
        session_id,
        created_at: crate::state_snapshot::now_iso(),
        class: WORKTREE_CLASS_AGENT.to_string(),
    };

    // The durable half of the registry — a sidecar beside (never inside) the
    // worktree. Best-effort: a sidecar write failure degrades to the in-memory
    // registration only (lost on daemon restart), never a failed creation.
    if let Err(e) = write_sidecar(&meta) {
        warn!(
            source = Source::HookDispatch.as_str(),
            worktree = %meta.worktree.display(),
            error = %e,
            "cannot write worktree sidecar; registration will not survive daemon restart",
        );
    }

    Ok(meta)
}

/// Parse the bare agent id out of a `WorktreeCreate` payload `name`.
///
/// `agent-<id>` → `Some("<id>")`; any other shape (a user-chosen `--worktree`
/// name) → `None`. The `agent-` prefix is a host convention, not a contract
/// (misc 150) — a convention change simply yields `None` and the worktree keys
/// by its verbatim name, caught downstream by the cwd fallback.
#[must_use]
pub fn parse_agent_id(name: &str) -> Option<&str> {
    name.strip_prefix("agent-").filter(|id| !id.is_empty())
}

/// The path segment (`agents/<session_id>/<segment>`) for a new worktree.
///
/// The bare agent id when the `name` parses as `agent-<id>`, else the `name`
/// verbatim (the `--worktree` case), else the generated `unique_id` (no usable
/// name at all).
fn worktree_segment(agent_id: Option<&str>, raw_name: Option<&str>, unique_id: &str) -> String {
    agent_id.or(raw_name).unwrap_or(unique_id).to_string()
}

/// The sidecar path for a worktree: `<worktree-dir>.meta.json`, a sibling of the
/// worktree directory (never inside it).
#[must_use]
pub fn sidecar_path(worktree: &Path) -> PathBuf {
    let leaf = worktree
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    worktree.with_file_name(format!("{leaf}.meta.json"))
}

/// Write a worktree's sidecar JSON beside its directory.
///
/// # Errors
///
/// Returns any filesystem or serialization error; the caller treats it as
/// best-effort (the in-memory registration still stands).
pub fn write_sidecar(meta: &WorktreeMeta) -> Result<()> {
    let path = sidecar_path(&meta.worktree);
    let json = serde_json::to_string_pretty(meta).context("serialize worktree sidecar")?;
    std::fs::write(&path, json).with_context(|| format!("write sidecar {}", path.display()))?;
    Ok(())
}

/// Read the `HEAD` commit of `repo` (`git -C <repo> rev-parse HEAD`).
///
/// `None` on any git failure (a bare/unborn `HEAD`, a git error). The caller
/// records the empty base as a keep-forever conservative default.
fn git_head(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!commit.is_empty()).then_some(commit)
}

/// Scan the agents subtree for sidecars, returning every readable
/// [`WorktreeMeta`] (daemon-startup registry rehydration, misc 150).
///
/// The layout is `agents/<session_id>/<segment>.meta.json`; each session dir is
/// read one level deep for `*.meta.json` files. Best-effort throughout: an
/// unreadable dir or a malformed sidecar is skipped, never fatal. A missing
/// `agents_root` (no worktree ever created) yields an empty vec.
#[must_use]
pub fn scan_sidecars(agents_root: &Path) -> Vec<WorktreeMeta> {
    let mut metas = Vec::new();
    let Ok(sessions) = std::fs::read_dir(agents_root) else {
        return metas;
    };
    for session in sessions.flatten() {
        let session_dir = session.path();
        if !session_dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&session_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".meta.json"))
            {
                continue;
            }
            if let Ok(contents) = std::fs::read_to_string(&path)
                && let Ok(meta) = serde_json::from_str::<WorktreeMeta>(&contents)
            {
                metas.push(meta);
            }
        }
    }
    metas
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

/// Remove orphaned worktree directories directly under `root` whose git linkage
/// is dead (the flat, one-level scheme of the legacy cache location).
///
/// `git worktree prune` semantics: an entry is orphaned when its `.git` pointer
/// names a `<repo>/.git/worktrees/<name>` metadata directory that no longer
/// exists (git deregistered it), or when it carries no `.git` pointer at all (a
/// partial/interrupted create). A live worktree — whose metadata directory still
/// exists — is always kept. Returns the number of directories removed.
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

/// Remove orphaned agent worktrees under the nested `agents/<session_id>/<segment>`
/// scheme, sweeping paired sidecars and rmdir'ing emptied session parents.
///
/// The agents subtree is two levels deep: each session dir holds worktree dirs
/// (`<segment>/`) alongside their sidecars (`<segment>.meta.json`). A worktree
/// dir with dead git linkage (see [`prune_orphans`]) is removed, and — since the
/// sidecar is the transaction record — its paired `<segment>.meta.json` is
/// removed with it. A session dir left empty by the sweep is rmdir'd so a dead
/// session's subtree collapses. Returns the number of worktree directories
/// removed.
///
/// Best-effort and idempotent, mirroring [`prune_orphans`]: any unreadable/
/// unremovable entry is left in place; a missing `agents_root` prunes nothing.
#[must_use]
pub fn prune_agent_orphans(agents_root: &Path) -> usize {
    let Ok(sessions) = std::fs::read_dir(agents_root) else {
        return 0;
    };
    let mut removed = 0;
    for session in sessions.flatten() {
        let session_dir = session.path();
        if !session_dir.is_dir() {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&session_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && linkage_dead(&path) && std::fs::remove_dir_all(&path).is_ok() {
                    // The sidecar is the transaction record: remove it with the
                    // dead worktree it described (best-effort).
                    let _ = std::fs::remove_file(sidecar_path(&path));
                    removed += 1;
                }
            }
        }
        // Collapse an emptied session parent (a dead session's subtree).
        if std::fs::read_dir(&session_dir).is_ok_and(|mut it| it.next().is_none()) {
            let _ = std::fs::remove_dir(&session_dir);
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
    use std::path::{Path, PathBuf};

    use super::{
        ForeignVcs, VcsPosture, WorktreeMeta, branch_name, copy_worktree_includes, detect_vcs,
        linkage_dead, parse_agent_id, prune_agent_orphans, prune_orphans, scan_sidecars,
        sidecar_path, worktree_segment, write_sidecar,
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

    // ── Path scheme + sidecar ──────────────────────────────────────────────

    #[test]
    fn parse_agent_id_strips_prefix() {
        assert_eq!(
            parse_agent_id("agent-ad9dee0ad90513642"),
            Some("ad9dee0ad90513642")
        );
        assert_eq!(parse_agent_id("feature-auth"), None);
        assert_eq!(parse_agent_id("agent-"), None, "bare prefix has no id");
    }

    #[test]
    fn worktree_segment_prefers_agent_then_name_then_unique() {
        assert_eq!(worktree_segment(Some("abc"), Some("agent-abc"), "u"), "abc");
        assert_eq!(
            worktree_segment(None, Some("feature-auth"), "u"),
            "feature-auth"
        );
        assert_eq!(worktree_segment(None, None, "u"), "u");
    }

    #[test]
    fn sidecar_path_is_a_sibling() {
        let wt = Path::new("/state/catenary/worktrees/agents/sess/abc");
        assert_eq!(
            sidecar_path(wt),
            Path::new("/state/catenary/worktrees/agents/sess/abc.meta.json"),
            "sidecar is a sibling of the worktree dir, never inside it",
        );
    }

    /// Build a `WorktreeMeta` for a worktree dir under an agents subtree.
    fn meta_for(worktree: &Path) -> WorktreeMeta {
        WorktreeMeta {
            worktree: worktree.to_path_buf(),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "agent-abc".to_string(),
            name: "agent-abc".to_string(),
            agent_id: Some("abc".to_string()),
            session_id: "sess-1".to_string(),
            created_at: "2026-07-06T00:00:00.000Z".to_string(),
            class: super::WORKTREE_CLASS_AGENT.to_string(),
        }
    }

    #[test]
    fn sidecar_write_and_scan_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let agents = tmp.path().join("agents");
        let wt = agents.join("sess-1").join("abc");
        std::fs::create_dir_all(&wt).expect("mkdir worktree");
        let meta = meta_for(&wt);
        write_sidecar(&meta).expect("write sidecar");

        // The sidecar lands beside the worktree dir.
        assert!(sidecar_path(&wt).exists(), "sidecar written as a sibling");

        // A fresh scan recovers exactly the registration.
        let scanned = scan_sidecars(&agents);
        assert_eq!(scanned.len(), 1, "one sidecar recovered: {scanned:?}");
        assert_eq!(scanned[0].worktree, wt);
        assert_eq!(scanned[0].session_id, "sess-1");
        assert_eq!(scanned[0].agent_id.as_deref(), Some("abc"));
        assert_eq!(scanned[0].base_commit, "deadbeef");
    }

    #[test]
    fn scan_sidecars_missing_root_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(scan_sidecars(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn prune_agent_orphans_sweeps_dead_and_paired_sidecar_and_empty_parent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repos = tmp.path().join("repos");
        let agents = tmp.path().join("agents");
        std::fs::create_dir_all(&repos).expect("mkdir repos");

        // Session A: one live worktree (kept) + its sidecar.
        let live = agents.join("sess-a").join("live");
        std::fs::create_dir_all(&live).expect("mkdir live");
        let meta = repos.join("repo/.git/worktrees/live");
        std::fs::create_dir_all(&meta).expect("mkdir meta");
        std::fs::write(live.join(".git"), format!("gitdir: {}\n", meta.display()))
            .expect("write live .git");
        std::fs::write(sidecar_path(&live), "{}").expect("write live sidecar");

        // Session B: one dead worktree + its sidecar; the ONLY entries in the
        // session dir, so the parent collapses after the sweep.
        let dead = agents.join("sess-b").join("dead");
        std::fs::create_dir_all(&dead).expect("mkdir dead");
        std::fs::write(
            dead.join(".git"),
            format!(
                "gitdir: {}\n",
                repos.join("gone/.git/worktrees/x").display()
            ),
        )
        .expect("write dead .git");
        std::fs::write(sidecar_path(&dead), "{}").expect("write dead sidecar");

        let removed = prune_agent_orphans(&agents);
        assert_eq!(removed, 1, "only the dead worktree is pruned");
        assert!(live.exists(), "the live worktree is kept");
        assert!(sidecar_path(&live).exists(), "the live sidecar is kept");
        assert!(!dead.exists(), "the dead worktree is removed");
        assert!(
            !sidecar_path(&dead).exists(),
            "the dead worktree's paired sidecar is removed with it",
        );
        assert!(
            !agents.join("sess-b").exists(),
            "the emptied session parent is rmdir'd",
        );
        assert!(
            agents.join("sess-a").exists(),
            "a session parent with a live worktree survives",
        );
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
