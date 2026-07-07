// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Guarded in-house disposal of Catenary-created worktrees (misc 151).
//!
//! Catenary owns the full worktree lifecycle: creation ([`crate::worktree_create`]),
//! mounting/unmounting (misc 150), and now disposal. The host delegates
//! hook-created worktree removal to a `WorktreeRemove` hook it never dispatches
//! (bug 71), so disposal is contractually ours and operationally nobody's — this
//! module is the "ours".
//!
//! # Safety invariant — never delete information
//!
//! A worktree is *disposable* only when it provably contains nothing. The proof
//! is per backing VCS ([`clean_reason`]; misc 148): **git** — `git status
//! --porcelain` empty **and** `HEAD` equal to the recorded base commit; **svn** —
//! `svn status` empty (svn has no local-commit class, so there is no unpushed
//! leg); **hg** — `hg status` empty **and** no draft changesets beyond the
//! recorded base changeset. Anything else — one untracked scratch file, one local
//! commit — is *dirty* and is **never** auto-deleted; it is kept and surfaced.
//! Owning creation makes the base check exact, not heuristic.
//!
//! # One guarded routine, called by every trigger
//!
//! [`dispose`] is the single implementation the four disposal triggers
//! (SubagentStop, SessionEnd, the creation-time age sweep, and the
//! `WorktreeRemove` handler) all call. Its guards are absolute: the path must lie
//! under our worktrees scheme ([`paths::worktrees_dir`]) **and** carry a sidecar,
//! or nothing is touched.
//!
//! # Filesystem procedure
//!
//! **Non-git (svn/hg) disposal is *simpler* than git** (misc 148): there is no
//! main-repo registration and no branch ref, so a clean svn checkout or hg share
//! is a plain `remove_dir_all` after the proof, then the sidecar and emptied
//! parents. (An hg share's store lives in the source repo, so deleting the
//! working dir loses nothing the clean proof did not already clear.) The
//! sidecar-as-transaction-record and remnant rules apply unchanged.
//!
//! For **git**, a worktree is four artifacts — the dir (with its `gitdir:` `.git`
//! file), the main repo's `.git/worktrees/<name>/` registration, the branch ref,
//! and our sidecar. Disposal keeps them consistent:
//!
//! - Deletion is always `git worktree remove` (never `rm -rf`, never `--force`):
//!   git removes dir + registration in one consistent operation and re-checks
//!   cleanliness itself. **Git's refusal outranks our checks** — on a refusal we
//!   stop, keep, and log ([`Disposition::Refused`]).
//! - Locks are never touched.
//! - Branch deletion takes its name from the sidecar, never a pattern, ordered
//!   after the registration is gone.
//! - The sidecar is the transaction record, deleted last. A sidecar whose
//!   worktree dir is already gone converges via the remnant rule ([`dispose`]
//!   routes a missing dir to [`Disposition::Remnant`]): prune, delete the
//!   recorded branch iff its tip still equals the recorded base, unlink a
//!   dangling recorded symlink, remove the sidecar. Every step is idempotent or
//!   refusable, so any crash point converges on the next sweep.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use tracing::debug;

use crate::paths;
use crate::source::Source;
use crate::worktree_create::{
    WORKTREE_CLASS_FEAT, WORKTREE_VCS_HG, WORKTREE_VCS_SVN, WorktreeMeta, scan_sidecars,
    sidecar_path,
};

/// Age threshold for the creation-time sweep (misc 151, trigger 3).
///
/// A *clean* agent worktree older than this is disposed, whatever session
/// created it. Conservative — a live subagent resumes long before this; a dirty
/// worktree is never swept, at any age.
pub const AGENT_DISPOSE_MAX_AGE: Duration = Duration::from_hours(24);

/// The outcome of a disposal attempt on one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Clean and provably empty: worktree dir, registration, branch, and sidecar
    /// all removed; `git worktree prune` tidied.
    Disposed,
    /// The worktree dir was already gone (a crash mid-dispose, or a manual
    /// deletion): the remnant rule converged — prune, branch delete iff
    /// `tip == base`, dangling link unlinked, sidecar swept.
    Remnant,
    /// Dirty by the safety invariant (uncommitted/untracked changes, or `HEAD`
    /// moved off the recorded base). Kept untouched; `reason` names why.
    KeptDirty {
        /// Human-readable reason the worktree was kept (for the surfacing legs).
        reason: String,
    },
    /// `git worktree remove` refused (a lock, a git-detected change our checks
    /// missed): git's refusal outranks our proof, so we stop and keep. `reason`
    /// carries git's stderr.
    Refused {
        /// git's refusal message (its stderr), for the log/ledger.
        reason: String,
    },
    /// The path is not under our worktrees scheme or carries no sidecar — never
    /// ours to touch. A hard no-op.
    NotOurs,
}

impl Disposition {
    /// Whether the worktree was left in place dirty (the surfacing triggers nag
    /// on this).
    #[must_use]
    pub const fn is_kept_dirty(&self) -> bool {
        matches!(self, Self::KeptDirty { .. })
    }

    /// Whether disposal fully removed the worktree (dir/branch/sidecar gone).
    #[must_use]
    pub const fn is_disposed(&self) -> bool {
        matches!(self, Self::Disposed | Self::Remnant)
    }
}

/// Canonical root of the worktrees scheme (`<state>/catenary/worktrees`),
/// canonicalized so it lines up with the canonical `worktree` paths the registry
/// and sidecars store.
fn scheme_root() -> PathBuf {
    let root = paths::worktrees_dir();
    root.canonicalize().unwrap_or(root)
}

/// Whether `worktree` lies under the Catenary worktrees scheme.
///
/// The first, absolute guard: a path outside `<state>/catenary/worktrees` is
/// never ours to delete, whatever a sidecar might claim.
#[must_use]
pub fn under_worktrees_scheme(worktree: &Path) -> bool {
    under_root(worktree, &scheme_root())
}

/// [`under_worktrees_scheme`] against an explicit scheme root (the test seam).
fn under_root(worktree: &Path, scheme_root: &Path) -> bool {
    let canonical = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());
    canonical.starts_with(scheme_root)
}

/// Whether `worktree` is a Catenary-managed worktree we may dispose: under the
/// scheme **and** carrying a sidecar (the transaction record).
#[must_use]
pub fn is_ours(worktree: &Path) -> bool {
    under_worktrees_scheme(worktree) && sidecar_path(worktree).exists()
}

/// Run a git command in `dir`, returning `(success, stdout_trimmed, stderr_trimmed)`.
///
/// `None` when git could not be spawned at all. Uses the user's real git
/// configuration (no config isolation) so signing/hooks behave as the user
/// expects; the operations here (`status`, `rev-parse`, `worktree remove`,
/// `branch -D`, `worktree prune`) need no committer identity.
fn git(dir: &Path, args: &[&str]) -> Option<(bool, String, String)> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Some((output.status.success(), stdout, stderr))
}

/// Run `program` (svn/hg) in `dir`, returning `(success, stdout_trimmed,
/// stderr_trimmed)` — `None` when the program could not be spawned (misc 148).
///
/// Like [`git`] but for the non-git worktree VCSes, which take the working copy
/// as the process working directory rather than a `-C` flag. Uses the user's real
/// VCS configuration (no isolation) so auth/hooks behave as expected; the
/// operations here (`status`, `log`) are read-only.
fn run_in(program: &str, dir: &Path, args: &[&str]) -> Option<(bool, String, String)> {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Some((output.status.success(), stdout, stderr))
}

/// The backing VCS of a worktree, resolved from its sidecar `vcs` tag (misc 148).
///
/// An unknown or empty tag falls back to [`Vcs::Git`] — the pre-misc-148 default,
/// consistent with [`crate::worktree_create::default_vcs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vcs {
    /// git — removed with `git worktree remove`, branch + registration cleaned.
    Git,
    /// svn — a plain directory delete after `svn status` proves it clean.
    Svn,
    /// hg — a plain directory delete after `hg status` + no-draft-beyond-base.
    Hg,
}

/// Map a [`WorktreeMeta::vcs`] tag to the disposal [`Vcs`] (git by default).
fn vcs_of(meta: &WorktreeMeta) -> Vcs {
    match meta.vcs.as_str() {
        WORKTREE_VCS_SVN => Vcs::Svn,
        WORKTREE_VCS_HG => Vcs::Hg,
        _ => Vcs::Git,
    }
}

/// Whether a [`Vcs`] is a non-git working copy (svn/hg): its disposal is a plain
/// directory delete with no registration or branch leg (misc 148).
const fn is_nongit(vcs: Vcs) -> bool {
    matches!(vcs, Vcs::Svn | Vcs::Hg)
}

/// The clean-proof reason a worktree is *not* disposable, or `None` when it is
/// provably clean — dispatched per backing VCS (misc 148).
///
/// - **git** ([`git_clean_reason`]): `git status --porcelain` empty **and** `HEAD`
///   equal to the recorded base commit.
/// - **svn** ([`svn_clean_reason`]): `svn status` empty. svn has no local-commit
///   class (commits go straight to the repository), so this is the whole proof —
///   the git/hg "unpushed"/"local commit" leg does not exist.
/// - **hg** ([`hg_clean_reason`]): `hg status` empty **and** no draft changesets
///   beyond the recorded base changeset.
///
/// Any VCS failure keeps the worktree (we cannot prove it empty).
#[must_use]
pub fn clean_reason(meta: &WorktreeMeta) -> Option<String> {
    match vcs_of(meta) {
        Vcs::Git => git_clean_reason(meta),
        Vcs::Svn => svn_clean_reason(&meta.worktree),
        Vcs::Hg => hg_clean_reason(meta),
    }
}

/// The git clean proof: `git status --porcelain` empty and `HEAD` at the recorded
/// base commit. An empty recorded base (creation could not read `HEAD`) is a
/// keep-forever conservative default.
fn git_clean_reason(meta: &WorktreeMeta) -> Option<String> {
    let worktree = &meta.worktree;
    if meta.base_commit.trim().is_empty() {
        return Some("the base commit was not recorded at creation (kept conservatively)".into());
    }
    // Proof 1: working tree pristine (changes AND untracked).
    match git(worktree, &["status", "--porcelain"]) {
        Some((true, stdout, _)) if stdout.is_empty() => {}
        Some((true, _, _)) => return Some("uncommitted or untracked changes present".into()),
        _ => return Some("`git status` could not verify the worktree is clean".into()),
    }
    // Proof 2: HEAD still at the recorded base (no local commits).
    match git(worktree, &["rev-parse", "HEAD"]) {
        Some((true, head, _)) if head == meta.base_commit => None,
        Some((true, _, _)) => Some("HEAD has moved off the recorded base (local commits)".into()),
        _ => Some("`git rev-parse HEAD` could not verify the base commit".into()),
    }
}

/// The svn clean proof (misc 148): `svn status` empty.
///
/// A non-empty `svn status` (local modifications *or* an unversioned `?` file) is
/// dirty. svn has no local-commit class — commits go straight to the repository —
/// so an empty status is the complete proof; there is no unpushed leg to check.
fn svn_clean_reason(worktree: &Path) -> Option<String> {
    match run_in("svn", worktree, &["status"]) {
        Some((true, out, _)) if out.is_empty() => None,
        Some((true, _, _)) => Some("uncommitted or unversioned changes present".into()),
        _ => Some("`svn status` could not verify the working copy is clean".into()),
    }
}

/// The hg clean proof (misc 148): `hg status` empty **and** no draft changesets
/// beyond the recorded base changeset.
///
/// Proof 1 rejects any working-copy change (modified/added/removed/missing/
/// unknown). Proof 2 rejects local commits: a `draft() and descendants(<base>)
/// and not <base>` revset that matches any changeset means the copy carries
/// unlanded commits — kept. An empty recorded base is a keep-forever conservative
/// default; any hg failure keeps the copy.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`{node}` is an hg output template, not a Rust format argument"
)]
fn hg_clean_reason(meta: &WorktreeMeta) -> Option<String> {
    let worktree = &meta.worktree;
    // Proof 1: working copy pristine.
    match run_in("hg", worktree, &["status"]) {
        Some((true, out, _)) if out.is_empty() => {}
        Some((true, _, _)) => return Some("uncommitted or unknown changes present".into()),
        _ => return Some("`hg status` could not verify the working copy is clean".into()),
    }
    // Proof 2: no draft changesets beyond the recorded base (no local commits).
    let base = meta.base_commit.trim();
    if base.is_empty() {
        return Some(
            "the base changeset was not recorded at creation (kept conservatively)".into(),
        );
    }
    let revset = format!("draft() and descendants({base}) and not {base}");
    match run_in("hg", worktree, &["log", "-r", &revset, "-T", "{node}\n"]) {
        Some((true, out, _)) if out.is_empty() => None,
        Some((true, _, _)) => {
            Some("draft changesets beyond the recorded base (local commits)".into())
        }
        _ => Some("`hg log` could not verify draft changesets against the base".into()),
    }
}

/// Whether a feats worktree carries **unpushed** commits — commits reachable from
/// `HEAD` that live on no remote-tracking ref (`rev-list HEAD --not --remotes`).
///
/// Distinct from the agent clean proof (which compares against the recorded
/// base): a durable line is expected to live on a remote, so its `rm` refuses
/// until the work is pushed. A git failure returns `true` (conservative refuse).
fn feat_unpushed(worktree: &Path) -> bool {
    match git(
        worktree,
        &["rev-list", "HEAD", "--not", "--remotes", "--count"],
    ) {
        Some((true, count, _)) => count.parse::<u64>().unwrap_or(1) > 0,
        _ => true,
    }
}

/// `git worktree remove [--force] <worktree>` from the source repo.
///
/// `Ok(())` on success; `Err(stderr)` on git's refusal (a lock, an unclean tree
/// git re-checks itself) — the caller keeps and logs. `force` is passed ONLY by
/// the `catenary worktree rm` agent-class path (the deliberate captured-work
/// assertion); auto-disposal never forces.
fn git_worktree_remove(repo: &Path, worktree: &Path, force: bool) -> Result<(), String> {
    let wt = worktree.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&wt);
    match git(repo, &args) {
        Some((true, _, _)) => Ok(()),
        Some((false, _, stderr)) if stderr.is_empty() => {
            Err("git refused to remove the worktree".into())
        }
        Some((false, _, stderr)) => Err(stderr),
        None => Err("could not run `git worktree remove`".into()),
    }
}

/// `git branch -D <branch>` from the source repo (best-effort; justified by the
/// clean proof or the `tip == base` remnant check). Logs at debug on failure.
fn git_branch_delete(repo: &Path, branch: &str) {
    if let Some((false, _, stderr)) = git(repo, &["branch", "-D", branch]) {
        debug!(
            source = Source::DaemonDispatch.as_str(),
            branch, error = %stderr,
            "worktree disposal: branch delete failed (best-effort)",
        );
    }
}

/// `git worktree prune` from the source repo (idempotent tidy; best-effort).
fn git_worktree_prune(repo: &Path) {
    let _ = git(repo, &["worktree", "prune"]);
}

/// The commit `branch` points at (`rev-parse --verify <branch>`), or `None` if
/// the branch is gone or git failed.
fn branch_tip(repo: &Path, branch: &str) -> Option<String> {
    match git(repo, &["rev-parse", "--verify", branch]) {
        Some((true, tip, _)) if !tip.is_empty() => Some(tip),
        _ => None,
    }
}

/// Remove the sidecar (the transaction record) — the last mutation, so its
/// presence always means "disposal may be incomplete; safe to re-run".
fn remove_sidecar(meta: &WorktreeMeta) {
    let _ = std::fs::remove_file(sidecar_path(&meta.worktree));
}

/// Unlink the recorded feats symlink, but ONLY when `readlink` still resolves to
/// this worktree (a user-replaced link is left alone). `dangling_only` restricts
/// the unlink to a link whose target no longer exists (the remnant rule).
fn unlink_recorded_link(meta: &WorktreeMeta, dangling_only: bool) {
    let Some(link) = &meta.link else {
        return;
    };
    let Ok(target) = std::fs::read_link(link) else {
        return; // not a symlink, or gone — nothing ours to unlink
    };
    if target != meta.worktree {
        return; // user re-pointed it elsewhere — leave it alone
    }
    if dangling_only && target.exists() {
        return; // target still present — not a dangling remnant
    }
    let _ = std::fs::remove_file(link);
}

/// Rmdir empty parent directories left by a disposed worktree, climbing until the
/// scheme root — never at or above it.
fn remove_empty_parents(worktree: &Path, scheme_root: &Path) {
    let mut cursor = worktree.parent().map(Path::to_path_buf);
    while let Some(dir) = cursor {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if !canonical.starts_with(scheme_root) || canonical.as_path() == scheme_root {
            break;
        }
        if std::fs::remove_dir(&dir).is_err() {
            break; // non-empty or gone — stop climbing
        }
        cursor = dir.parent().map(Path::to_path_buf);
    }
}

/// The guarded disposal routine — the single implementation every trigger calls.
///
/// The four disposal triggers all funnel here (`SubagentStop`, `SessionEnd`, the
/// age sweep, and `WorktreeRemove`), so the safety invariant lives in exactly one
/// place.
///
/// `host_initiated` is `true` only for the `WorktreeRemove` handler, where the
/// host has *decided* removal: the clean check stays advisory (we still refuse
/// dirty), but a kept-dirty outcome logs the divergence ("host asked, we
/// declined"). Every other trigger passes `false`.
///
/// Steps on a clean, present **git** worktree: `git worktree remove` (no
/// `--force`) → `git branch -D` (sidecar's branch) → unlink a resolving feats
/// link → remove the sidecar → `git worktree prune` → rmdir emptied parents. A
/// clean **non-git** (svn/hg) working copy is *simpler* — no registration and no
/// branch leg — so it is a plain directory delete after the proof, then sidecar +
/// emptied parents (misc 148). A missing dir routes to the remnant rule; a dirty
/// proof or a removal refusal keeps the worktree.
#[must_use]
pub fn dispose(meta: &WorktreeMeta, host_initiated: bool) -> Disposition {
    dispose_in(meta, &scheme_root(), host_initiated)
}

/// [`dispose`] against an explicit scheme root (the test seam).
fn dispose_in(meta: &WorktreeMeta, scheme_root: &Path, host_initiated: bool) -> Disposition {
    // Guard 1+2: under the scheme AND a sidecar present, or never touched.
    if !under_root(&meta.worktree, scheme_root) || !sidecar_path(&meta.worktree).exists() {
        return Disposition::NotOurs;
    }
    let vcs = vcs_of(meta);
    // Remnant rule: the dir is already gone — converge without a clean proof.
    if !meta.worktree.exists() {
        return dispose_remnant(meta, scheme_root, vcs);
    }
    // Clean proof (advisory for a host-initiated removal, but still enforced).
    if let Some(reason) = clean_reason(meta) {
        if host_initiated {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                worktree = %meta.worktree.display(),
                reason = %reason,
                "worktree-remove: host asked to remove a dirty worktree — declined, kept",
            );
        }
        return Disposition::KeptDirty { reason };
    }
    if is_nongit(vcs) {
        // A clean svn/hg working copy holds nothing to lose: a plain directory
        // delete (no registration, no branch). For an hg share the shared store
        // lives in the source repo, so this removes only the working dir + pointer.
        if let Err(e) = std::fs::remove_dir_all(&meta.worktree) {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                worktree = %meta.worktree.display(),
                error = %e,
                "worktree disposal: could not remove non-git working copy — kept",
            );
            return Disposition::Refused {
                reason: e.to_string(),
            };
        }
        unlink_recorded_link(meta, false);
        remove_sidecar(meta);
        remove_empty_parents(&meta.worktree, scheme_root);
        debug!(
            source = Source::DaemonDispatch.as_str(),
            worktree = %meta.worktree.display(),
            vcs = %meta.vcs,
            "disposed clean non-git working copy (dir + sidecar removed)",
        );
        return Disposition::Disposed;
    }
    // git's refusal outranks our checks: stop, keep, log.
    if let Err(refusal) = git_worktree_remove(&meta.source_repo, &meta.worktree, false) {
        debug!(
            source = Source::DaemonDispatch.as_str(),
            worktree = %meta.worktree.display(),
            error = %refusal,
            "worktree disposal: git worktree remove refused — kept",
        );
        return Disposition::Refused { reason: refusal };
    }
    git_branch_delete(&meta.source_repo, &meta.branch);
    unlink_recorded_link(meta, false);
    remove_sidecar(meta);
    git_worktree_prune(&meta.source_repo);
    remove_empty_parents(&meta.worktree, scheme_root);
    debug!(
        source = Source::DaemonDispatch.as_str(),
        worktree = %meta.worktree.display(),
        branch = %meta.branch,
        "disposed clean worktree (dir + branch + sidecar removed)",
    );
    Disposition::Disposed
}

/// The remnant rule: the worktree dir is gone but the sidecar remains.
///
/// For **git**, prune the dead registration and delete the recorded branch iff
/// its tip still equals the recorded base (so no local commits are lost). For
/// **non-git** (svn/hg) there is no registration and no branch leg, so the
/// remnant is just the sidecar and any dangling recorded link (misc 148). Either
/// way, unlink a dangling recorded link and remove the sidecar last. All
/// idempotent.
fn dispose_remnant(meta: &WorktreeMeta, scheme_root: &Path, vcs: Vcs) -> Disposition {
    if !is_nongit(vcs) {
        git_worktree_prune(&meta.source_repo);
        if let Some(tip) = branch_tip(&meta.source_repo, &meta.branch) {
            if !meta.base_commit.is_empty() && tip == meta.base_commit {
                git_branch_delete(&meta.source_repo, &meta.branch);
            } else {
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    branch = %meta.branch,
                    "worktree remnant: branch tip diverged from base — branch kept",
                );
            }
        }
    }
    unlink_recorded_link(meta, true);
    remove_sidecar(meta);
    remove_empty_parents(&meta.worktree, scheme_root);
    debug!(
        source = Source::DaemonDispatch.as_str(),
        worktree = %meta.worktree.display(),
        "converged worktree remnant (dir already gone)",
    );
    Disposition::Remnant
}

/// The `catenary worktree rm` **agent-class** removal: the caller's assertion
/// that the work is captured substitutes for the clean proof.
///
/// The single force-shaped path in the system, taken deliberately — this
/// replaces the raw `git worktree remove --force` of the landing workflow. The
/// worktree is removed with `--force` (overriding git's own dirty refusal),
/// branch deleted, sidecar swept, parents tidied. The caller firehose-logs the
/// captured-work assertion.
#[must_use]
pub fn remove_agent_asserted(meta: &WorktreeMeta) -> Disposition {
    remove_agent_asserted_in(meta, &scheme_root())
}

fn remove_agent_asserted_in(meta: &WorktreeMeta, scheme_root: &Path) -> Disposition {
    if !under_root(&meta.worktree, scheme_root) || !sidecar_path(&meta.worktree).exists() {
        return Disposition::NotOurs;
    }
    let vcs = vcs_of(meta);
    if !meta.worktree.exists() {
        return dispose_remnant(meta, scheme_root, vcs);
    }
    if is_nongit(vcs) {
        // The captured-work assertion substitutes for the clean proof: force a
        // plain directory delete of the non-git working copy (misc 148).
        if let Err(e) = std::fs::remove_dir_all(&meta.worktree) {
            return Disposition::Refused {
                reason: e.to_string(),
            };
        }
        unlink_recorded_link(meta, false);
        remove_sidecar(meta);
        remove_empty_parents(&meta.worktree, scheme_root);
        return Disposition::Disposed;
    }
    if let Err(refusal) = git_worktree_remove(&meta.source_repo, &meta.worktree, true) {
        return Disposition::Refused { reason: refusal };
    }
    git_branch_delete(&meta.source_repo, &meta.branch);
    unlink_recorded_link(meta, false);
    remove_sidecar(meta);
    git_worktree_prune(&meta.source_repo);
    remove_empty_parents(&meta.worktree, scheme_root);
    Disposition::Disposed
}

/// The `catenary worktree rm` **feats-class** removal: refuses dirty.
///
/// A durable line's uncommitted or unpushed work is exactly what the class
/// exists to protect — the refusal names what to clean or push first. On a clean,
/// fully-pushed worktree: `git worktree remove` (no force), branch delete, the
/// symlink unlinked only after `readlink` resolves to our worktree, sidecar
/// swept.
#[must_use]
pub fn remove_feat(meta: &WorktreeMeta) -> Disposition {
    remove_feat_in(meta, &scheme_root())
}

fn remove_feat_in(meta: &WorktreeMeta, scheme_root: &Path) -> Disposition {
    if !under_root(&meta.worktree, scheme_root) || !sidecar_path(&meta.worktree).exists() {
        return Disposition::NotOurs;
    }
    if !meta.worktree.exists() {
        // Feats are git-only (created solely via `catenary worktree add`).
        return dispose_remnant(meta, scheme_root, Vcs::Git);
    }
    // Refuse uncommitted/untracked changes first (feats clean = status empty).
    match git(&meta.worktree, &["status", "--porcelain"]) {
        Some((true, stdout, _)) if stdout.is_empty() => {}
        Some((true, _, _)) => {
            return Disposition::KeptDirty {
                reason: "the worktree has uncommitted changes — commit or discard them first"
                    .into(),
            };
        }
        _ => {
            return Disposition::KeptDirty {
                reason: "`git status` could not verify the worktree is clean".into(),
            };
        }
    }
    // Refuse unpushed commits (a durable line is expected to live on a remote).
    if feat_unpushed(&meta.worktree) {
        return Disposition::KeptDirty {
            reason: "the worktree has unpushed commits — push them first".into(),
        };
    }
    if let Err(refusal) = git_worktree_remove(&meta.source_repo, &meta.worktree, false) {
        return Disposition::Refused { reason: refusal };
    }
    git_branch_delete(&meta.source_repo, &meta.branch);
    unlink_recorded_link(meta, false);
    remove_sidecar(meta);
    git_worktree_prune(&meta.source_repo);
    remove_empty_parents(&meta.worktree, scheme_root);
    Disposition::Disposed
}

/// Whether a worktree is provably clean by the disposal invariant (no uncommitted
/// changes and `HEAD` still at the recorded base). The `worktree ls` clean/dirty
/// column for agent worktrees.
#[must_use]
pub fn is_disposable_clean(meta: &WorktreeMeta) -> bool {
    clean_reason(meta).is_none()
}

/// Whether a worktree's working tree carries uncommitted or untracked changes.
///
/// `git status --porcelain` non-empty. The `worktree ls` clean/dirty column for
/// feats worktrees, whose local commits are expected (shown via ahead/behind, not
/// as "dirty").
#[must_use]
pub fn worktree_status_dirty(worktree: &Path) -> bool {
    !matches!(git(worktree, &["status", "--porcelain"]), Some((true, s, _)) if s.is_empty())
}

/// The ahead/behind counts of a feats worktree relative to its upstream.
///
/// Runs `rev-list --left-right --count @{u}...HEAD` → `(behind, ahead)`, or
/// `None` when no upstream is configured or git fails. For the `worktree ls`
/// surface.
#[must_use]
pub fn feat_ahead_behind(worktree: &Path) -> Option<(u64, u64)> {
    let (ok, out, _) = git(
        worktree,
        &["rev-list", "--left-right", "--count", "@{u}...HEAD"],
    )?;
    if !ok {
        return None;
    }
    let mut parts = out.split_whitespace();
    let behind = parts.next()?.parse().ok()?;
    let ahead = parts.next()?.parse().ok()?;
    Some((behind, ahead))
}

/// Whether a worktree's `created_at` is older than `max_age` as of `now`.
///
/// An unparseable timestamp is treated as **not** old (never force-dispose on a
/// timestamp we cannot read); the remnant rule still converges it once its dir
/// is gone.
fn is_older_than(created_at: &str, now: SystemTime, max_age: Duration) -> bool {
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return false;
    };
    let created_secs = created.timestamp();
    let now_secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let age_secs = now_secs.saturating_sub(created_secs);
    age_secs >= 0 && u64::try_from(age_secs).unwrap_or(0) >= max_age.as_secs()
}

/// The creation-time age sweep (misc 151, trigger 3).
///
/// Scans the agents subtree's sidecars and disposes every worktree that is
/// either a remnant (dir gone — converges regardless of age) or clean **and**
/// older than `max_age` (from *any* session). Dirty worktrees are kept at any
/// age (the shared [`dispose`] enforces that). Young, present worktrees are
/// skipped. `now` is injected for testability.
///
/// Returns each `(worktree_path, Disposition)` acted on, for the caller's log.
#[must_use]
pub fn sweep_aged_agents(now: SystemTime, max_age: Duration) -> Vec<(PathBuf, Disposition)> {
    sweep_aged_agents_in(&paths::agents_worktrees_dir(), &scheme_root(), now, max_age)
}

fn sweep_aged_agents_in(
    agents_root: &Path,
    scheme_root: &Path,
    now: SystemTime,
    max_age: Duration,
) -> Vec<(PathBuf, Disposition)> {
    let mut acted = Vec::new();
    for meta in scan_sidecars(agents_root) {
        // Feats never live under agents/, but guard the class anyway.
        if meta.class == WORKTREE_CLASS_FEAT {
            continue;
        }
        let remnant = !meta.worktree.exists();
        let old = is_older_than(&meta.created_at, now, max_age);
        if remnant || old {
            let disposition = dispose_in(&meta, scheme_root, false);
            if !matches!(disposition, Disposition::NotOurs) {
                acted.push((meta.worktree.clone(), disposition));
            }
        }
    }
    acted
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "`{node}` is an hg output template, not a Rust format argument"
)]
mod tests {
    use super::*;
    use crate::worktree_create::WORKTREE_CLASS_AGENT;
    use std::process::Stdio;

    /// Run a git command in `dir` with a pinned, isolated identity (tests build
    /// real repos and worktrees).
    fn tgit(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn head_of(dir: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["branch", "--list", branch])
            .output()
            .expect("git branch");
        !String::from_utf8_lossy(&out.stdout).trim().is_empty()
    }

    /// A committed repo plus a worktree cut from it under an explicit scheme
    /// root (a tempdir subdir), so the disposal guard sees it as "ours" **without
    /// mutating process env**. Returns `(tempdir, scheme_root, repo, worktree,
    /// meta)`.
    fn fixture(
        branch: &str,
        class: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, WorktreeMeta) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("worktrees")).expect("mkdir scheme");
        let scheme_root = tmp
            .path()
            .join("worktrees")
            .canonicalize()
            .expect("canon scheme");

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        tgit(&repo, &["init", "-q"]);
        tgit(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::write(repo.join("f.txt"), "hello").expect("write");
        tgit(&repo, &["add", "f.txt"]);
        tgit(&repo, &["commit", "-qm", "c"]);
        let base = head_of(&repo);

        let sub = if class == WORKTREE_CLASS_FEAT {
            scheme_root.join("feats").join("repo").join(branch)
        } else {
            scheme_root.join("agents").join("sess-1").join(branch)
        };
        std::fs::create_dir_all(sub.parent().expect("parent")).expect("mkdir parents");
        tgit(
            &repo,
            &["worktree", "add", "-b", branch, &sub.to_string_lossy()],
        );

        let worktree = sub.canonicalize().expect("canonicalize wt");
        let meta = WorktreeMeta {
            worktree: worktree.clone(),
            source_repo: repo.canonicalize().expect("canon repo"),
            base_commit: base,
            branch: branch.to_string(),
            name: format!("agent-{branch}"),
            agent_id: Some(branch.to_string()),
            session_id: "sess-1".to_string(),
            created_at: crate::state_snapshot::now_iso(),
            class: class.to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        };
        crate::worktree_create::write_sidecar(&meta).expect("write sidecar");
        (tmp, scheme_root, repo, worktree, meta)
    }

    #[test]
    fn clean_worktree_disposes_end_to_end() {
        let (_tmp, root, repo, worktree, meta) = fixture("agent-clean", WORKTREE_CLASS_AGENT);
        assert_eq!(dispose_in(&meta, &root, false), Disposition::Disposed);
        assert!(!worktree.exists(), "worktree dir removed");
        assert!(!sidecar_path(&worktree).exists(), "sidecar removed");
        assert!(!branch_exists(&repo, "agent-clean"), "branch deleted");
        let list = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("git worktree list");
        assert!(
            !String::from_utf8_lossy(&list.stdout).contains("agent-clean"),
            "registration pruned",
        );
    }

    #[test]
    fn untracked_file_is_kept_dirty() {
        let (_tmp, root, _repo, worktree, meta) = fixture("agent-untracked", WORKTREE_CLASS_AGENT);
        std::fs::write(worktree.join("scratch.txt"), "x").expect("write scratch");
        let d = dispose_in(&meta, &root, false);
        assert!(d.is_kept_dirty(), "untracked file kept dirty: {d:?}");
        assert!(worktree.exists(), "dirty worktree preserved");
        assert!(sidecar_path(&worktree).exists(), "sidecar preserved");
    }

    #[test]
    fn local_commit_is_kept_dirty() {
        let (_tmp, root, _repo, worktree, meta) = fixture("agent-commit", WORKTREE_CLASS_AGENT);
        std::fs::write(worktree.join("g.txt"), "y").expect("write");
        tgit(&worktree, &["add", "g.txt"]);
        tgit(&worktree, &["commit", "-qm", "local"]);
        let d = dispose_in(&meta, &root, false);
        assert!(
            d.is_kept_dirty(),
            "local commit kept dirty (HEAD moved): {d:?}",
        );
        assert!(worktree.exists());
    }

    #[test]
    fn locked_worktree_is_refused_and_kept() {
        let (_tmp, root, repo, worktree, meta) = fixture("agent-locked", WORKTREE_CLASS_AGENT);
        tgit(&repo, &["worktree", "lock", &worktree.to_string_lossy()]);
        let d = dispose_in(&meta, &root, false);
        assert!(
            matches!(d, Disposition::Refused { .. }),
            "git refusal respected: {d:?}",
        );
        assert!(worktree.exists(), "locked worktree kept");
        assert!(sidecar_path(&worktree).exists());
    }

    #[test]
    fn remnant_dir_gone_sweeps_branch_at_base_and_sidecar() {
        let (_tmp, root, repo, worktree, meta) = fixture("agent-remnant", WORKTREE_CLASS_AGENT);
        std::fs::remove_dir_all(&worktree).expect("rm dir");
        assert!(sidecar_path(&worktree).exists(), "sidecar still there");
        assert_eq!(dispose_in(&meta, &root, false), Disposition::Remnant);
        assert!(!sidecar_path(&worktree).exists(), "remnant sidecar swept");
        assert!(
            !branch_exists(&repo, "agent-remnant"),
            "branch at base deleted on remnant convergence",
        );
    }

    #[test]
    fn remnant_keeps_branch_when_tip_diverged_from_base() {
        let (_tmp, root, repo, worktree, meta) = fixture("agent-diverged", WORKTREE_CLASS_AGENT);
        std::fs::write(worktree.join("h.txt"), "z").expect("write");
        tgit(&worktree, &["add", "h.txt"]);
        tgit(&worktree, &["commit", "-qm", "diverge"]);
        std::fs::remove_dir_all(&worktree).expect("rm dir");
        assert_eq!(dispose_in(&meta, &root, false), Disposition::Remnant);
        assert!(
            branch_exists(&repo, "agent-diverged"),
            "branch with divergent tip is kept (holds unmerged commits)",
        );
    }

    #[test]
    fn no_sidecar_is_never_ours() {
        let (_tmp, root, _repo, worktree, meta) = fixture("agent-nosidecar", WORKTREE_CLASS_AGENT);
        std::fs::remove_file(sidecar_path(&worktree)).expect("rm sidecar");
        assert_eq!(dispose_in(&meta, &root, false), Disposition::NotOurs);
        assert!(worktree.exists(), "untouched without a sidecar");
    }

    #[test]
    fn outside_scheme_is_never_ours() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("worktrees");
        std::fs::create_dir_all(&root).expect("mkdir root");
        let root = root.canonicalize().expect("canon");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        crate::worktree_create::write_sidecar(&WorktreeMeta {
            worktree: elsewhere.canonicalize().expect("canon"),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "b".to_string(),
            name: "agent-b".to_string(),
            agent_id: Some("b".to_string()),
            session_id: "s".to_string(),
            created_at: crate::state_snapshot::now_iso(),
            class: WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        })
        .expect("sidecar");
        let meta = WorktreeMeta {
            worktree: elsewhere.canonicalize().expect("canon"),
            source_repo: PathBuf::from("/repo"),
            base_commit: "deadbeef".to_string(),
            branch: "b".to_string(),
            name: "agent-b".to_string(),
            agent_id: Some("b".to_string()),
            session_id: "s".to_string(),
            created_at: crate::state_snapshot::now_iso(),
            class: WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: crate::worktree_create::WORKTREE_VCS_GIT.to_string(),
        };
        assert_eq!(dispose_in(&meta, &root, false), Disposition::NotOurs);
        assert!(elsewhere.exists(), "outside-scheme path untouched");
    }

    #[test]
    fn host_initiated_still_refuses_dirty() {
        let (_tmp, root, _repo, worktree, meta) = fixture("agent-hostdirty", WORKTREE_CLASS_AGENT);
        std::fs::write(worktree.join("scratch.txt"), "x").expect("write");
        let d = dispose_in(&meta, &root, true);
        assert!(
            d.is_kept_dirty(),
            "host-initiated removal still refuses dirty: {d:?}",
        );
        assert!(worktree.exists());
    }

    #[test]
    fn age_sweep_disposes_old_clean_only() {
        let (_tmp, root, _repo, worktree, mut meta) = fixture("agent-aged", WORKTREE_CLASS_AGENT);
        meta.created_at = "2000-01-01T00:00:00.000Z".to_string();
        crate::worktree_create::write_sidecar(&meta).expect("rewrite sidecar");
        let agents = root.join("agents");
        let acted = sweep_aged_agents_in(&agents, &root, SystemTime::now(), AGENT_DISPOSE_MAX_AGE);
        assert!(
            acted
                .iter()
                .any(|(p, d)| p == &worktree && *d == Disposition::Disposed),
            "old clean worktree disposed: {acted:?}",
        );
        assert!(!worktree.exists());
    }

    #[test]
    fn age_sweep_keeps_young_clean() {
        let (_tmp, root, _repo, worktree, _meta) = fixture("agent-young", WORKTREE_CLASS_AGENT);
        let agents = root.join("agents");
        let acted = sweep_aged_agents_in(&agents, &root, SystemTime::now(), AGENT_DISPOSE_MAX_AGE);
        assert!(
            !acted.iter().any(|(p, _)| p == &worktree),
            "young worktree not swept: {acted:?}",
        );
        assert!(worktree.exists());
    }

    #[test]
    fn age_sweep_keeps_old_dirty() {
        let (_tmp, root, _repo, worktree, mut meta) =
            fixture("agent-olddirty", WORKTREE_CLASS_AGENT);
        std::fs::write(worktree.join("scratch.txt"), "x").expect("write");
        meta.created_at = "2000-01-01T00:00:00.000Z".to_string();
        crate::worktree_create::write_sidecar(&meta).expect("rewrite sidecar");
        let agents = root.join("agents");
        let acted = sweep_aged_agents_in(&agents, &root, SystemTime::now(), AGENT_DISPOSE_MAX_AGE);
        assert!(
            acted
                .iter()
                .any(|(p, d)| p == &worktree && d.is_kept_dirty()),
            "old dirty worktree kept: {acted:?}",
        );
        assert!(worktree.exists());
    }

    #[test]
    fn feat_rm_refuses_uncommitted() {
        let (_tmp, root, _repo, worktree, meta) = fixture("feat-dirty", WORKTREE_CLASS_FEAT);
        std::fs::write(worktree.join("wip.txt"), "x").expect("write");
        let d = remove_feat_in(&meta, &root);
        assert!(d.is_kept_dirty(), "feat rm refuses uncommitted: {d:?}");
        assert!(worktree.exists());
    }

    #[test]
    fn feat_rm_refuses_unpushed_commit() {
        let (_tmp, root, _repo, worktree, meta) = fixture("feat-unpushed", WORKTREE_CLASS_FEAT);
        std::fs::write(worktree.join("done.txt"), "x").expect("write");
        tgit(&worktree, &["add", "done.txt"]);
        tgit(&worktree, &["commit", "-qm", "work"]);
        let d = remove_feat_in(&meta, &root);
        assert!(d.is_kept_dirty(), "feat rm refuses unpushed: {d:?}");
        assert!(worktree.exists());
    }

    #[test]
    fn agent_rm_asserted_removes_dirty() {
        let (_tmp, root, _repo, worktree, meta) = fixture("agent-assert", WORKTREE_CLASS_AGENT);
        std::fs::write(worktree.join("scratch.txt"), "x").expect("write");
        let d = remove_agent_asserted_in(&meta, &root);
        assert_eq!(d, Disposition::Disposed, "asserted rm force-removes: {d:?}");
        assert!(!worktree.exists());
        assert!(!sidecar_path(&worktree).exists());
    }

    #[test]
    fn is_older_than_parses_and_compares() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(is_older_than(
            "1970-01-01T00:00:00.000Z",
            now,
            Duration::from_secs(10),
        ));
        let created = chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339();
        assert!(!is_older_than(&created, now, AGENT_DISPOSE_MAX_AGE));
        assert!(!is_older_than("not-a-date", now, Duration::from_secs(0)));
    }

    // ── Non-git disposal (misc 148) ────────────────────────────────────────

    /// Whether `bin` is on PATH (`<bin> --version`) — binary-gated svn/hg tests
    /// skip when their VCS is absent so CI without it stays green.
    fn have_bin(bin: &str) -> bool {
        Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A minimal non-git `WorktreeMeta` (no VCS binary needed) for the synthetic
    /// guards.
    fn bare_meta(worktree: &Path, vcs: &str) -> WorktreeMeta {
        WorktreeMeta {
            worktree: worktree.to_path_buf(),
            source_repo: PathBuf::from("/nonexistent-repo"),
            base_commit: "file:///srv/repo@1".to_string(),
            branch: "x".to_string(),
            name: "agent-x".to_string(),
            agent_id: Some("x".to_string()),
            session_id: "sess-1".to_string(),
            created_at: crate::state_snapshot::now_iso(),
            class: WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: vcs.to_string(),
        }
    }

    #[test]
    fn vcs_of_maps_the_sidecar_tag_defaulting_to_git() {
        let wt = Path::new("/x");
        assert_eq!(vcs_of(&bare_meta(wt, WORKTREE_VCS_SVN)), Vcs::Svn);
        assert_eq!(vcs_of(&bare_meta(wt, WORKTREE_VCS_HG)), Vcs::Hg);
        assert_eq!(
            vcs_of(&bare_meta(wt, crate::worktree_create::WORKTREE_VCS_GIT)),
            Vcs::Git,
        );
        assert_eq!(
            vcs_of(&bare_meta(wt, "unknown-future-vcs")),
            Vcs::Git,
            "an unknown tag defaults to git",
        );
    }

    #[test]
    fn nongit_remnant_dir_gone_sweeps_sidecar_without_touching_git() {
        // A non-git remnant (dir already gone) converges on the sidecar alone —
        // no registration, no branch, and no VCS binary needed. The bogus
        // `source_repo` proves no git leg runs on it.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("worktrees")).expect("mkdir scheme");
        let scheme_root = tmp
            .path()
            .join("worktrees")
            .canonicalize()
            .expect("canon scheme");
        let worktree = scheme_root.join("agents").join("sess-1").join("svncopy");
        std::fs::create_dir_all(worktree.parent().expect("parent")).expect("mkdir parents");
        let meta = bare_meta(&worktree, WORKTREE_VCS_SVN);
        crate::worktree_create::write_sidecar(&meta).expect("write sidecar");
        assert!(sidecar_path(&worktree).exists(), "sidecar present");

        assert_eq!(dispose_in(&meta, &scheme_root, false), Disposition::Remnant);
        assert!(
            !sidecar_path(&worktree).exists(),
            "the non-git remnant sidecar is swept",
        );
        assert!(
            !scheme_root.join("agents").join("sess-1").exists(),
            "the emptied session parent is rmdir'd",
        );
    }

    /// Build an svn working copy directly under a tempdir scheme root, with a
    /// sidecar, so the disposal guard sees it as "ours". `None` when svn is
    /// absent (binary-gated skip).
    fn svn_fixture(branch: &str) -> Option<(tempfile::TempDir, PathBuf, PathBuf, WorktreeMeta)> {
        if !have_bin("svn") || !have_bin("svnadmin") {
            return None;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("worktrees")).expect("mkdir scheme");
        let scheme_root = tmp
            .path()
            .join("worktrees")
            .canonicalize()
            .expect("canon scheme");
        let repo = tmp.path().join("svnrepo");
        let cfg = tmp.path().join("svncfg");
        assert!(
            Command::new("svnadmin")
                .arg("create")
                .arg(&repo)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("run svnadmin")
                .success(),
            "svnadmin create failed",
        );
        let url = format!("file://{}", repo.display());
        let svn = |args: &[&str]| {
            let status = Command::new("svn")
                .arg("--config-dir")
                .arg(&cfg)
                .arg("--non-interactive")
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("run svn");
            assert!(status.success(), "svn {args:?} failed");
        };
        let sub = scheme_root.join("agents").join("sess-1").join(branch);
        std::fs::create_dir_all(sub.parent().expect("parent")).expect("mkdir parents");
        svn(&["checkout", &url, &sub.to_string_lossy()]);
        std::fs::write(sub.join("f.txt"), "hello").expect("write");
        svn(&["add", &sub.join("f.txt").to_string_lossy()]);
        svn(&["commit", "-m", "c", &sub.to_string_lossy()]);

        let worktree = sub.canonicalize().expect("canon wt");
        let meta = WorktreeMeta {
            worktree: worktree.clone(),
            source_repo: repo,
            base_commit: format!("{url}@1"),
            branch: branch.to_string(),
            name: format!("agent-{branch}"),
            agent_id: Some(branch.to_string()),
            session_id: "sess-1".to_string(),
            created_at: crate::state_snapshot::now_iso(),
            class: WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: WORKTREE_VCS_SVN.to_string(),
        };
        crate::worktree_create::write_sidecar(&meta).expect("write sidecar");
        Some((tmp, scheme_root, worktree, meta))
    }

    #[test]
    fn svn_clean_copy_disposes_by_directory_delete() {
        let Some((_tmp, root, worktree, meta)) = svn_fixture("agent-svn-clean") else {
            return;
        };
        assert_eq!(dispose_in(&meta, &root, false), Disposition::Disposed);
        assert!(!worktree.exists(), "clean svn copy dir removed");
        assert!(!sidecar_path(&worktree).exists(), "sidecar removed");
    }

    #[test]
    fn svn_dirty_copy_is_kept() {
        let Some((_tmp, root, worktree, meta)) = svn_fixture("agent-svn-dirty") else {
            return;
        };
        // Modify a tracked file → `svn status` shows `M` → dirty.
        std::fs::write(worktree.join("f.txt"), "changed").expect("modify");
        let d = dispose_in(&meta, &root, false);
        assert!(d.is_kept_dirty(), "dirty svn copy kept: {d:?}");
        assert!(worktree.exists(), "dirty svn copy preserved");
        assert!(sidecar_path(&worktree).exists(), "sidecar preserved");
    }

    /// Run hg in `cwd` with a pinned commit identity (build-time only). Asserts
    /// success.
    fn hg_run(cwd: &Path, args: &[&str]) {
        let status = Command::new("hg")
            .arg("--cwd")
            .arg(cwd)
            .args(["--config", "ui.username=catenary-test"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run hg");
        assert!(status.success(), "hg {args:?} failed");
    }

    /// Build an hg working copy (a clone, so it needs no `share` extension) under
    /// a tempdir scheme root, with a sidecar. `None` when hg is absent.
    fn hg_fixture(branch: &str) -> Option<(tempfile::TempDir, PathBuf, PathBuf, WorktreeMeta)> {
        if !have_bin("hg") {
            return None;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("worktrees")).expect("mkdir scheme");
        let scheme_root = tmp
            .path()
            .join("worktrees")
            .canonicalize()
            .expect("canon scheme");
        let repo = tmp.path().join("hgrepo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        hg_run(&repo, &["init"]);
        std::fs::write(repo.join("f.txt"), "hello").expect("write");
        hg_run(&repo, &["add", "f.txt"]);
        hg_run(&repo, &["commit", "-m", "c"]);

        let sub = scheme_root.join("agents").join("sess-1").join(branch);
        std::fs::create_dir_all(sub.parent().expect("parent")).expect("mkdir parents");
        hg_run(
            &repo,
            &["clone", &repo.to_string_lossy(), &sub.to_string_lossy()],
        );

        // Base marker: the copy's working-dir parent node.
        let node = {
            let out = Command::new("hg")
                .arg("--cwd")
                .arg(&sub)
                .args(["log", "-r", ".", "-T", "{node}"])
                .output()
                .expect("hg log");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let worktree = sub.canonicalize().expect("canon wt");
        let meta = WorktreeMeta {
            worktree: worktree.clone(),
            source_repo: repo,
            base_commit: node,
            branch: branch.to_string(),
            name: format!("agent-{branch}"),
            agent_id: Some(branch.to_string()),
            session_id: "sess-1".to_string(),
            created_at: crate::state_snapshot::now_iso(),
            class: WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: WORKTREE_VCS_HG.to_string(),
        };
        crate::worktree_create::write_sidecar(&meta).expect("write sidecar");
        Some((tmp, scheme_root, worktree, meta))
    }

    #[test]
    fn hg_clean_copy_disposes_by_directory_delete() {
        let Some((_tmp, root, worktree, meta)) = hg_fixture("agent-hg-clean") else {
            return;
        };
        assert_eq!(dispose_in(&meta, &root, false), Disposition::Disposed);
        assert!(!worktree.exists(), "clean hg copy dir removed");
        assert!(!sidecar_path(&worktree).exists(), "sidecar removed");
    }

    #[test]
    fn hg_uncommitted_copy_is_kept() {
        let Some((_tmp, root, worktree, meta)) = hg_fixture("agent-hg-dirty") else {
            return;
        };
        // Modify a tracked file → `hg status` shows `M` → dirty (proof 1).
        std::fs::write(worktree.join("f.txt"), "changed").expect("modify");
        let d = dispose_in(&meta, &root, false);
        assert!(d.is_kept_dirty(), "uncommitted hg copy kept: {d:?}");
        assert!(worktree.exists());
    }

    #[test]
    fn hg_local_commit_is_kept_as_draft_beyond_base() {
        let Some((_tmp, root, worktree, meta)) = hg_fixture("agent-hg-commit") else {
            return;
        };
        // Commit a change in the copy: `hg status` is empty again, but the new
        // DRAFT changeset descends from the recorded base → kept (proof 2).
        std::fs::write(worktree.join("g.txt"), "y").expect("write");
        hg_run(&worktree, &["add", "g.txt"]);
        hg_run(&worktree, &["commit", "-m", "local"]);
        let d = dispose_in(&meta, &root, false);
        assert!(
            d.is_kept_dirty(),
            "a local hg commit (draft beyond base) is kept: {d:?}",
        );
        assert!(worktree.exists());
    }
}
