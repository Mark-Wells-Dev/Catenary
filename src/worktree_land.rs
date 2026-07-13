// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The `catenary worktree diff` / `worktree land` lifecycle verbs (misc 158).
//!
//! Two verbs that close the landing loop the disposal machinery (misc 151) left
//! open — the stop-hook nag says "land their work" but there was no land verb, so
//! landing was a six-step hand dance with two trap doors (untracked files
//! invisible to `git diff` until `git add -N`; write-capable git denied through
//! `-C` but allowed after `cd`).
//!
//! ## `worktree diff` — complete by construction, commit-aware
//!
//! [`worktree_diff`] emits a COMPLETE unified diff of the worktree's state vs its
//! **branch point** — the merge-base of the recorded creation base and the
//! worktree's current `HEAD` ([`diff_base`], misc 166): tracked changes **plus**
//! untracked files rendered as new-file hunks (the `git add -N` trap absorbed
//! into the verb), and — the misc-166 leg — **committed** work too. The synthesis
//! uses a **temporary index** (`GIT_INDEX_FILE` pointed at a scratch file):
//! `read-tree <base>` loads the branch-point tree, `add -A` stages every change
//! against it — tracked modification, untracked non-ignored file, and anything a
//! local commit already wrote into the working tree alike, gitignore-honest by
//! construction — and `diff --cached <base>` renders the lot. Anchoring on the
//! merge-base rather than `HEAD` is what makes a worktree that committed its work
//! diff to its real delta instead of an empty patch (the pre-166 vs-`HEAD` diff
//! read as "no changes"). The worktree's real index is never touched. The output
//! is a valid unified diff a `git apply` consumes. [`worktree_changed_paths`] is
//! the same set as a path list — the write-set resolution primitive `land` and
//! the hook need.
//!
//! ## `worktree land` — apply into the owning repo, then remove
//!
//! [`land`] applies that complete diff into the OWNING repo (from the registered
//! [`WorktreeMeta`]) via `git apply --3way`, from the repo root. It **never
//! commits**. The atomicity guard is a plain `git apply --check` first (see
//! `apply_check` for why the check must not carry `--3way`): a refusal leaves
//! the owning repo untouched, and [`land`] returns a [`LandOutcome`] naming the
//! actual cause — the conflicting file paths on an apply refusal, the vcs on a
//! non-git worktree. Because the diff is now anchored on the branch point
//! ([`diff_base`], misc 166), a worktree that **committed** its work lands that
//! work as an ordinary patch — no separate committed-work refusal, and no commit
//! ever made in the parent (`git apply` writes the working tree only).
//!
//! On full success the worktree is removed through the existing disposal
//! machinery ([`crate::worktree_dispose::remove_agent_asserted`]); the caller's
//! `--keep` flag skips that tail. Batch arming rides the `PreToolUse` hook, but
//! it is **debt-transfer**, not content (misc 189): the resolver maps land's
//! changed paths (the candidate/landed set, opaque-gated), and the daemon arms
//! only the subset the worktree's owner left **unpaid** — read from the owner's
//! diagnostics ledger, not the git diff. A worktree whose worker paid its gate
//! lands debt-free. [`owner_unpaid_landed`] is the pure intersection primitive;
//! [`worktree_owner_label`] names the owner for the ledger lookup.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::worktree_create::{WORKTREE_VCS_GIT, WorktreeMeta};

/// The outcome of a [`land`] attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LandOutcome {
    /// The diff applied cleanly into the owning repo. `paths` are the applied
    /// file paths (owning-repo-relative), for the diagnostics-batch arming leg.
    /// `removed` is whether the worktree was then removed (`--keep` leaves it).
    Landed {
        /// The applied file paths, relative to the owning repo root.
        paths: Vec<String>,
        /// Whether the worktree was removed after the successful apply.
        removed: bool,
    },
    /// Nothing to land — the worktree carries no changes vs `HEAD`. A no-op:
    /// nothing applied, worktree kept.
    Empty,
    /// Refused before mutating anything. The owning repo is untouched, the
    /// worktree is kept, and `reason` names the actual cause (conflicting files,
    /// local commits, a non-git vcs, a missing/unmounted path).
    Refused {
        /// The teaching message naming why the land was refused.
        reason: String,
    },
}

/// Run git in `dir` with an optional temporary index, returning `(success,
/// stdout, stderr)` — `None` when git could not be spawned.
///
/// When `index` is `Some`, `GIT_INDEX_FILE` points git at that scratch index so
/// staging operations (`read-tree`, `add`) never touch the worktree's real
/// index. Uses the user's real git configuration (no isolation) so hooks/attrs
/// behave as expected; every operation here is read-only against the working
/// tree except the temp-index staging, which is discarded.
fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Option<(bool, String, String)> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    if let Some(idx) = index {
        cmd.env("GIT_INDEX_FILE", idx);
    }
    let output = cmd.output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Some((output.status.success(), stdout, stderr))
}

/// The commit the diff/land is anchored on — the **branch point** of the
/// worktree, so committed work is visible and landable (misc 166).
///
/// This is `git merge-base <recorded-base> HEAD`: the common ancestor of the
/// worktree's current `HEAD` and the commit the worktree branch was cut from
/// (recorded in the sidecar's `base_commit` at creation). When the worktree
/// never committed, `HEAD` is still at the recorded base and the merge-base *is*
/// that commit, so the diff is identical to the pre-166 vs-`HEAD` diff. When the
/// worktree committed, the merge-base stays at the branch point and the diff
/// spans the committed work too — the whole point of the leg.
///
/// Falls back to the literal ref `HEAD` when no branch point can be resolved: the
/// worktree carries no sidecar (a bare path, not a registered worktree), the
/// sidecar records no base (`git rev-parse HEAD` failed at creation), or the
/// merge-base query itself fails. That fallback is exactly the pre-166 behavior —
/// a worktree with no recorded branch point can only be diffed against its own
/// `HEAD`, and a committed one still trips the disposal moved-HEAD guard rather
/// than silently dropping work.
fn diff_base(worktree: &Path) -> String {
    let recorded = read_sidecar(worktree)
        .map(|m| m.base_commit)
        .filter(|b| !b.trim().is_empty());
    let Some(base) = recorded else {
        return "HEAD".to_string();
    };
    match git(worktree, None, &["merge-base", &base, "HEAD"]) {
        Some((true, out, _)) if !out.trim().is_empty() => out.trim().to_string(),
        // The recorded base is unreachable from HEAD (history rewritten, or HEAD
        // detached below it) — fall back to the recorded base itself, still a
        // committed-work-visible anchor.
        _ => base,
    }
}

/// Read the worktree's sidecar [`WorktreeMeta`] — `Some` when it exists and
/// parses, `None` for a bare (unregistered) path or a malformed sidecar.
fn read_sidecar(worktree: &Path) -> Option<WorktreeMeta> {
    let sidecar = crate::worktree_create::sidecar_path(worktree);
    std::fs::read_to_string(sidecar)
        .ok()
        .and_then(|c| serde_json::from_str::<WorktreeMeta>(&c).ok())
}

/// A scratch temporary-index path unique to this call, deleted on drop.
///
/// The temp index lives beside the worktree's sidecar area under the system temp
/// dir; `read-tree` + `add` stage into it so `git diff --cached` sees the whole
/// working-tree delta (tracked + untracked) without the worktree's real index
/// ever changing.
struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    /// Create a unique scratch index path under the system temp dir. The file
    /// itself is created by git's first `read-tree`; we only reserve the path.
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "catenary-land-index-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        path.push(unique);
        Self { path }
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Stage the worktree's full working-tree state (tracked changes + untracked
/// non-ignored files) into a fresh temporary index seeded from `base` (the
/// branch point; misc 166).
///
/// Returns the [`TempIndex`] on success so the caller can run
/// `diff --cached <base>` against it, or `None` when any staging step fails (not a
/// git worktree, git missing, an unresolvable base). Seeding from the branch point
/// rather than `HEAD` is what carries committed work into the diff: `add -A`
/// stages the whole working tree — which reflects committed and uncommitted
/// changes alike — over the branch-point tree. `add -A` respects gitignore, so
/// ignored files never enter the index and never appear in the diff — they are not
/// part of the work product.
fn stage_into_temp_index(worktree: &Path, base: &str) -> Option<TempIndex> {
    let index = TempIndex::new();
    // Seed the temp index from the branch point's tree.
    let (ok, _, _) = git(worktree, Some(&index.path), &["read-tree", base])?;
    if !ok {
        return None;
    }
    // Stage every change into the temp index: committed AND uncommitted tracked
    // modifications AND untracked (non-ignored) files. `-A` includes deletions;
    // gitignore is honored.
    let (ok, _, _) = git(worktree, Some(&index.path), &["add", "-A"])?;
    if !ok {
        return None;
    }
    Some(index)
}

/// The COMPLETE unified diff of the worktree vs its **branch point** (misc
/// 158/166).
///
/// Tracked changes plus untracked non-ignored files rendered as new-file hunks,
/// plus committed work — everything the worktree did since its branch point (the
/// merge-base of the recorded creation base and `HEAD`; [`diff_base`]) — via the
/// temporary-index synthesis (the worktree's real index is untouched). The output
/// is a valid unified diff a `git apply` consumes; an empty string means the
/// worktree matches its branch point (nothing to land).
///
/// # Errors
///
/// Returns an error when the path is not a git worktree, git is unavailable, or
/// the diff synthesis fails — the caller surfaces the state (never a bare
/// not-found).
pub fn worktree_diff(worktree: &Path) -> anyhow::Result<String> {
    if !worktree.exists() {
        anyhow::bail!(
            "worktree path does not exist — it may have been removed or unmounted: {}",
            worktree.display()
        );
    }
    let base = diff_base(worktree);
    let index = stage_into_temp_index(worktree, &base)
        .ok_or_else(|| anyhow::anyhow!("{} is not a git worktree", worktree.display()))?;
    // `--cached <base>` renders every staged delta against the branch point; the
    // temp index carries the full working-tree state, so untracked files appear as
    // new-file hunks and committed work appears as ordinary hunks.
    let (ok, stdout, stderr) = git(
        worktree,
        Some(&index.path),
        &["diff", "--cached", "--no-color", &base],
    )
    .ok_or_else(|| anyhow::anyhow!("could not run git in {}", worktree.display()))?;
    if !ok {
        anyhow::bail!(
            "`git diff` failed in {}: {}",
            worktree.display(),
            stderr.trim()
        );
    }
    Ok(stdout)
}

/// The changed-path list of the worktree vs its **branch point** — the
/// `--name-only` view (misc 158/166).
///
/// Tracked-modified plus untracked non-ignored paths plus committed-work paths,
/// one per entry, relative to the worktree root (git's native diff-path
/// convention), against the branch point ([`diff_base`]). This is the write-set
/// resolution primitive `land` and the hook use — anchored on the same base as
/// [`worktree_diff`], so the hook's resolved write-set and land's applied set stay
/// identical whether or not the worktree committed.
///
/// # Errors
///
/// Returns an error when the path is not a git worktree or git is unavailable.
pub fn worktree_changed_paths(worktree: &Path) -> anyhow::Result<Vec<String>> {
    if !worktree.exists() {
        anyhow::bail!(
            "worktree path does not exist — it may have been removed or unmounted: {}",
            worktree.display()
        );
    }
    let base = diff_base(worktree);
    let index = stage_into_temp_index(worktree, &base)
        .ok_or_else(|| anyhow::anyhow!("{} is not a git worktree", worktree.display()))?;
    let (ok, stdout, stderr) = git(
        worktree,
        Some(&index.path),
        &["diff", "--cached", "--name-only", &base],
    )
    .ok_or_else(|| anyhow::anyhow!("could not run git in {}", worktree.display()))?;
    if !ok {
        anyhow::bail!(
            "`git diff --name-only` failed in {}: {}",
            worktree.display(),
            stderr.trim()
        );
    }
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// The owning agent id of a worktree — its on-disk leaf directory name (bug 91's
/// `worktree_owner_label` primitive; misc 150).
///
/// A subagent worktree's leaf segment **is** its agent id (`worktree_segment`
/// stores the bare `<id>` — [`crate::worktree_create::parse_agent_id`]'s output —
/// as the path segment), so the dir name is the self-describing owner. This is
/// the key half of the owner's diagnostics-batch lookup: the batch is keyed
/// `(session_id, agent_id)`, and this yields the `agent_id` the worker edited
/// under. Falls back to the full path display when the leaf is unreadable.
#[must_use]
pub fn worktree_owner_label(worktree: &Path) -> String {
    worktree.file_name().map_or_else(
        || worktree.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The subset of the owner's **unpaid** batch that actually lands, mapped onto
/// the owning repo — the debt-transfer set (misc 189).
///
/// The ruling: landing a worktree whose worker **paid** its diagnostics gate arms
/// nothing; landing one with **unpaid** entries transfers exactly those files'
/// debt to the landing session. Debt follows content, and it transfers — never
/// duplicates. This function computes that transfer set, purely, from the inputs
/// the daemon has at land time:
///
/// - `owner_unpaid` — the owner's still-undelivered batch paths, absolute under
///   the worktree (the daemon's ledger lookup, keyed
///   `(sidecar.session_id, worktree_owner_label)`). An **empty** slice means the
///   owner paid, never edited, or its batch died with a bounced daemon (bug 79) —
///   all three are debt-free, and the result is empty (the never-lock-out
///   doctrine: an honest nothing beats phantom debt, decision 026's annotation).
/// - `worktree` — the owner's worktree root, to strip from each unpaid path.
/// - `source_repo` — the owning repo root, to re-anchor each stripped path onto.
/// - `landed` — the paths the land actually applied into the owning repo
///   (absolute, owning-repo-relative-joined). An owner-unpaid file that did **not**
///   land (a conflict-refused or dropped hunk) transfers no debt: only content
///   that arrived carries its gate.
///
/// The result is the intersection: for each owner-unpaid path under the worktree,
/// map it onto the owning repo and keep it only if it is in `landed`. Paths
/// outside the worktree (a defensive guard — the ledger should never hold one)
/// are skipped.
#[must_use]
pub fn owner_unpaid_landed(
    owner_unpaid: &[PathBuf],
    worktree: &Path,
    source_repo: &Path,
    landed: &std::collections::BTreeSet<PathBuf>,
) -> std::collections::BTreeSet<PathBuf> {
    let mut transfer = std::collections::BTreeSet::new();
    for unpaid in owner_unpaid {
        let Ok(rel) = unpaid.strip_prefix(worktree) else {
            continue; // not under the worktree — never part of this land's debt
        };
        let mapped = source_repo.join(rel);
        if landed.contains(&mapped) {
            transfer.insert(mapped);
        }
    }
    transfer
}

/// Whether the diff applies cleanly into the owning repo, or the conflicting
/// paths (misc 158).
///
/// Runs a **plain** `git apply --check` from the owning repo root with the diff
/// on stdin — a pure validation that mutates nothing. Deliberately NOT
/// `--3way --check`: on a content conflict the 3way fallback *mutates* the
/// working tree (it writes conflict markers before exiting nonzero), so gating
/// on the plain check is what makes a refusal leave the owning repo untouched.
/// A patch the plain check passes is one the real `--3way` apply lands without
/// any merge fallback. The conservative cost: a context drift a 3way blob merge
/// could have resolved cleanly is refused instead — atomicity outranks
/// cleverness here (the ticket's atomicity note).
///
/// `Ok(())` means the apply will succeed; `Err(paths)` carries the files
/// `git apply` reported it could not apply cleanly (parsed from stderr,
/// best-effort — falls back to the raw stderr when no path lines are
/// recognized).
fn apply_check(owning_repo: &Path, diff: &str) -> Result<(), String> {
    let out = run_git_stdin(
        owning_repo,
        &["apply", "--check", "--whitespace=nowarn"],
        diff,
    );
    match out {
        Some((true, _, _)) => Ok(()),
        Some((false, _, stderr)) => Err(conflict_paths(&stderr)),
        None => Err("git could not be run to validate the patch".to_string()),
    }
}

/// Apply the diff into the owning repo with `git apply --3way` (the real
/// mutation, after [`apply_check`] proved it clean).
///
/// Returns `Ok(())` on success or `Err(reason)` with git's stderr on a
/// (TOCTOU-race) failure — the caller keeps the worktree and surfaces the
/// reason.
fn apply_3way(owning_repo: &Path, diff: &str) -> Result<(), String> {
    match run_git_stdin(
        owning_repo,
        &["apply", "--3way", "--whitespace=nowarn"],
        diff,
    ) {
        Some((true, _, _)) => Ok(()),
        Some((false, _, stderr)) => Err(conflict_paths(&stderr)),
        None => Err("git could not be run to apply the patch".to_string()),
    }
}

/// Run git in `dir` with `diff` on stdin, returning `(success, stdout, stderr)`.
fn run_git_stdin(dir: &Path, args: &[&str], stdin_data: &str) -> Option<(bool, String, String)> {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(stdin_data.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Some((output.status.success(), stdout, stderr))
}

/// Extract the conflicting file paths from `git apply`'s stderr, formatted into
/// a teaching message; falls back to the raw stderr when no path lines parse.
///
/// `git apply` reports refusals as `error: patch failed: <path>:<line>` and
/// `error: <path>: does not exist in index` / `... patch does not apply`. The
/// path is the token after `patch failed: ` (up to the last `:`), or the token
/// between `error: ` and `:` for the other forms.
fn conflict_paths(stderr: &str) -> String {
    let mut paths: Vec<String> = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("error: patch failed: ") {
            // `<path>:<line>` — the path is everything up to the last `:`.
            let path = rest.rsplit_once(':').map_or(rest, |(p, _)| p);
            push_unique(&mut paths, path);
        } else if let Some(rest) = line.strip_prefix("error: ") {
            // `<path>: does not exist in index` / `<path>: patch does not apply`.
            if let Some((path, _)) = rest.split_once(": ") {
                push_unique(&mut paths, path);
            }
        }
    }
    if paths.is_empty() {
        let trimmed = stderr.trim();
        if trimmed.is_empty() {
            "the patch does not apply cleanly to the owning repo".to_string()
        } else {
            format!("the patch does not apply cleanly: {trimmed}")
        }
    } else {
        format!(
            "the patch conflicts in the owning repo — these files already diverge: {}. \
             Reconcile them (the owning repo is untouched, the worktree is kept), then land again.",
            paths.join(", ")
        )
    }
}

/// Push `path` into `paths` if not already present (dedupe conflict entries).
fn push_unique(paths: &mut Vec<String>, path: &str) {
    let path = path.trim();
    if !path.is_empty() && !paths.iter().any(|p| p == path) {
        paths.push(path.to_string());
    }
}

/// Land the worktree's complete diff into its owning repo (misc 158/166).
///
/// The full guarded sequence:
///
/// 1. **Non-git refusal** — a worktree whose sidecar `vcs` is not git
///    (svn/hg; misc 148) refuses, naming the vcs (the non-git leg is post-v2).
/// 2. **Missing-path refusal** — a worktree dir that no longer exists refuses by
///    naming the state, never a bare ENOENT.
/// 3. **Compute the diff** — the complete unified diff of the worktree vs its
///    branch point ([`diff_base`], misc 166): untracked files as new-file hunks
///    AND committed work as ordinary hunks. An empty diff is
///    [`LandOutcome::Empty`] (nothing to land). There is no longer a local-commit
///    refusal — committed work lands as an ordinary patch; the apply guards below
///    backstop it, and `git apply` never commits in the parent.
/// 4. **Plain `--check`** — validate against the owning repo (`apply_check`);
///    a refusal leaves the owning repo untouched and names the conflicting
///    files.
/// 5. **`--3way` apply** — the real mutation into the owning repo. Never commits.
/// 6. **Remove** — on full success, remove the worktree through the disposal
///    machinery ([`crate::worktree_dispose::remove_agent_asserted`]) unless
///    `keep` is set.
///
/// The applied paths (owning-repo-relative) ride back in
/// [`LandOutcome::Landed`] so the caller can arm the diagnostics batch for
/// exactly the landed set.
#[must_use]
pub fn land(meta: &WorktreeMeta, keep: bool) -> LandOutcome {
    // 1. Non-git worktrees refuse, naming the vcs (post-v2 leg).
    if meta.vcs != WORKTREE_VCS_GIT {
        return LandOutcome::Refused {
            reason: format!(
                "`catenary worktree land` supports git worktrees only — this one is `{}`. \
                 Landing a {} working copy is a post-v2 leg; capture the work manually and \
                 `catenary worktree rm` the copy.",
                meta.vcs, meta.vcs,
            ),
        };
    }

    // 2. A missing/unmounted worktree refuses by naming the state.
    if !meta.worktree.exists() {
        return LandOutcome::Refused {
            reason: format!(
                "worktree path does not exist — it may have been removed or unmounted: {}",
                meta.worktree.display(),
            ),
        };
    }

    // 3. Compute the complete diff vs the branch point (untracked + committed
    //    work included; misc 166). An empty diff is a kept no-op.
    let diff = match worktree_diff(&meta.worktree) {
        Ok(d) => d,
        Err(e) => {
            return LandOutcome::Refused {
                reason: e.to_string(),
            };
        }
    };
    if diff.trim().is_empty() {
        return LandOutcome::Empty;
    }
    let paths = worktree_changed_paths(&meta.worktree).unwrap_or_default();

    let owning_repo = &meta.source_repo;
    if !owning_repo.exists() {
        return LandOutcome::Refused {
            reason: format!(
                "the owning repo no longer exists at {} — cannot land into it",
                owning_repo.display(),
            ),
        };
    }

    // 4. Validate the apply first (mutates nothing) — a refusal here leaves the
    //    owning repo untouched, naming the conflicting files.
    if let Err(reason) = apply_check(owning_repo, &diff) {
        return LandOutcome::Refused { reason };
    }

    // 5. Apply for real. A TOCTOU-race failure keeps the worktree and surfaces
    //    the reason (the check passed, so this is rare).
    if let Err(reason) = apply_3way(owning_repo, &diff) {
        return LandOutcome::Refused { reason };
    }

    // 6. Remove the worktree on full success (unless `--keep`). A removal refusal
    //    does not un-land the applied work — report the applied paths and that
    //    the worktree was kept; the caller surfaces the removal reason.
    let removed = if keep {
        false
    } else {
        crate::worktree_dispose::remove_agent_asserted(meta).is_disposed()
    };

    LandOutcome::Landed { paths, removed }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests use expect/unwrap/panic for readable assertions"
)]
mod tests {
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::{
        LandOutcome, diff_base, land, owner_unpaid_landed, worktree_changed_paths, worktree_diff,
        worktree_owner_label,
    };
    use crate::worktree_create::{
        WORKTREE_CLASS_AGENT, WORKTREE_VCS_GIT, WorktreeMeta, sidecar_path,
    };

    /// Whether a binary is on PATH (skip git-dependent tests where absent).
    fn have_bin(bin: &str) -> bool {
        Command::new(bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Run a git command in `cwd`, asserting success.
    fn tgit(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", cwd.display());
    }

    /// The current `HEAD` oid of `dir`.
    fn head_of(dir: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Init a git repo with one commit at `dir` and a `README.md`.
    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).expect("mkdir");
        tgit(dir, &["init", "-q"]);
        tgit(dir, &["config", "user.email", "t@example.com"]);
        tgit(dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "hello\n").expect("write");
        tgit(dir, &["add", "."]);
        tgit(dir, &["commit", "-q", "-m", "init"]);
    }

    /// A minimal agent `WorktreeMeta` for a git worktree at `worktree`, cut from
    /// `repo` at its current HEAD.
    fn meta_for(repo: &Path, worktree: &Path) -> WorktreeMeta {
        WorktreeMeta {
            worktree: worktree.to_path_buf(),
            source_repo: repo.to_path_buf(),
            base_commit: head_of(worktree),
            branch: "topic".to_string(),
            name: "agent-x".to_string(),
            agent_id: Some("x".to_string()),
            session_id: "s".to_string(),
            created_at: "2026-07-08T00:00:00Z".to_string(),
            class: WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: WORKTREE_VCS_GIT.to_string(),
        }
    }

    /// Write `meta` to the worktree's sidecar (`<worktree>.meta.json`) so
    /// [`diff_base`] can read the recorded `base_commit` — the branch-point anchor
    /// the commit-aware diff/land is computed against (misc 166).
    fn write_meta_sidecar(meta: &WorktreeMeta) {
        let path = sidecar_path(&meta.worktree);
        std::fs::write(
            &path,
            serde_json::to_string(meta).expect("serialize sidecar"),
        )
        .expect("write sidecar");
    }

    /// Create a real linked worktree `<repo>-wt` off `repo`'s HEAD.
    fn add_worktree(repo: &Path) -> std::path::PathBuf {
        let wt = repo.with_file_name(format!(
            "{}-wt",
            repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo")
        ));
        tgit(
            repo,
            &["worktree", "add", "-q", "-b", "topic", wt.to_str().unwrap()],
        );
        wt
    }

    #[test]
    fn diff_includes_tracked_and_untracked() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);

        // A tracked modification and an untracked new file.
        std::fs::write(wt.join("README.md"), "hello world\n").expect("modify tracked");
        std::fs::write(wt.join("new.txt"), "brand new\n").expect("add untracked");

        let diff = worktree_diff(&wt).expect("diff");
        assert!(
            diff.contains("README.md"),
            "diff must include the tracked modification:\n{diff}"
        );
        assert!(
            diff.contains("new.txt") && diff.contains("new file mode"),
            "diff must render the untracked file as a new-file hunk:\n{diff}"
        );

        let names = worktree_changed_paths(&wt).expect("names");
        assert!(
            names.iter().any(|p| p == "README.md"),
            "name-only lists the tracked mod: {names:?}"
        );
        assert!(
            names.iter().any(|p| p == "new.txt"),
            "name-only lists the untracked file: {names:?}"
        );
    }

    #[test]
    fn diff_respects_gitignore() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        // Commit a .gitignore so a fresh worktree inherits it.
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").expect("write gitignore");
        tgit(&repo, &["add", ".gitignore"]);
        tgit(&repo, &["commit", "-q", "-m", "ignore"]);
        let wt = add_worktree(&repo);

        std::fs::write(wt.join("ignored.txt"), "secret\n").expect("write ignored");
        std::fs::write(wt.join("kept.txt"), "kept\n").expect("write kept");

        let names = worktree_changed_paths(&wt).expect("names");
        assert!(
            !names.iter().any(|p| p == "ignored.txt"),
            "gitignored files are not part of the work product: {names:?}"
        );
        assert!(
            names.iter().any(|p| p == "kept.txt"),
            "non-ignored untracked files are included: {names:?}"
        );
    }

    #[test]
    fn diff_does_not_mutate_the_real_index() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);
        std::fs::write(wt.join("new.txt"), "x\n").expect("write");

        let _ = worktree_diff(&wt).expect("diff");

        // The real index must still show new.txt as untracked (`??`), not staged.
        let out = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["status", "--porcelain"])
            .output()
            .expect("status");
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(
            status.contains("?? new.txt"),
            "the real index must be untouched (new.txt stays untracked):\n{status}"
        );
    }

    #[test]
    fn land_applies_tracked_and_untracked_into_owning_repo() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);
        std::fs::write(wt.join("README.md"), "hello world\n").expect("modify");
        std::fs::write(wt.join("new.txt"), "brand new\n").expect("add");

        let meta = meta_for(&repo, &wt);
        // `--keep` so we assert the apply independently of removal (removal needs
        // the disposal scheme root, exercised in the integration tests).
        let outcome = land(&meta, true);
        match outcome {
            LandOutcome::Landed { paths, removed } => {
                assert!(!removed, "--keep leaves the worktree");
                assert!(paths.iter().any(|p| p == "README.md"), "paths: {paths:?}");
                assert!(paths.iter().any(|p| p == "new.txt"), "paths: {paths:?}");
            }
            other => panic!("expected Landed, got {other:?}"),
        }

        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).expect("read"),
            "hello world\n",
            "the tracked modification landed in the owning repo",
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("new.txt")).expect("read"),
            "brand new\n",
            "the untracked file landed in the owning repo as a new file",
        );
    }

    #[test]
    fn land_refuses_conflict_and_leaves_owning_repo_untouched() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);

        // Worktree edits README; owning repo edits the SAME line differently → the
        // 3way apply conflicts.
        std::fs::write(wt.join("README.md"), "worktree version\n").expect("wt edit");
        std::fs::write(repo.join("README.md"), "owner version\n").expect("owner edit");

        let meta = meta_for(&repo, &wt);
        let outcome = land(&meta, true);
        match outcome {
            LandOutcome::Refused { reason } => {
                assert!(
                    reason.contains("README.md"),
                    "the refusal must name the conflicting file: {reason}"
                );
            }
            other => panic!("expected Refused, got {other:?}"),
        }

        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).expect("read"),
            "owner version\n",
            "the owning repo is untouched on a conflict refusal",
        );
        assert!(wt.exists(), "the worktree is kept on refusal");
    }

    #[test]
    fn diff_base_falls_back_to_head_without_a_sidecar() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);
        // No sidecar: the branch-point anchor is unresolvable, so the diff base is
        // the literal `HEAD` ref (the pre-166 behavior).
        assert_eq!(diff_base(&wt), "HEAD");
    }

    #[test]
    fn diff_shows_committed_work_against_the_branch_point() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);

        // Record the base BEFORE committing (the sidecar's branch point), then make
        // a local commit in the worktree.
        let meta = meta_for(&repo, &wt);
        write_meta_sidecar(&meta);
        std::fs::write(wt.join("committed.txt"), "c\n").expect("write");
        tgit(&wt, &["add", "committed.txt"]);
        tgit(&wt, &["commit", "-q", "-m", "local work"]);

        // The diff base is the branch point (the recorded base), NOT HEAD (which
        // moved off it). A vs-HEAD diff would be empty — the pre-166 blindness.
        assert_eq!(
            diff_base(&wt),
            meta.base_commit,
            "the diff anchors on the branch point, not the moved HEAD",
        );
        let diff = worktree_diff(&wt).expect("diff");
        assert!(
            diff.contains("committed.txt") && diff.contains("new file mode"),
            "the committed file is visible in the diff:\n{diff}"
        );
        let names = worktree_changed_paths(&wt).expect("names");
        assert!(
            names.iter().any(|p| p == "committed.txt"),
            "name-only lists the committed file: {names:?}"
        );
    }

    #[test]
    fn land_applies_committed_work_without_committing_in_the_parent() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);

        let owner_head_before = head_of(&repo);

        // Record the base, then commit work in the worktree.
        let meta = meta_for(&repo, &wt);
        write_meta_sidecar(&meta);
        std::fs::write(wt.join("committed.txt"), "c\n").expect("write");
        tgit(&wt, &["add", "committed.txt"]);
        tgit(&wt, &["commit", "-q", "-m", "local work"]);

        // `--keep` so we assert the apply independently of removal.
        match land(&meta, true) {
            LandOutcome::Landed { paths, .. } => {
                assert!(
                    paths.iter().any(|p| p == "committed.txt"),
                    "the committed file lands: {paths:?}"
                );
            }
            other => panic!("expected Landed, got {other:?}"),
        }
        // The committed content is now in the owning repo's working tree...
        assert_eq!(
            std::fs::read_to_string(repo.join("committed.txt")).expect("read"),
            "c\n",
            "the committed work landed in the owning repo",
        );
        // ...but as an uncommitted change — land never commits in the parent.
        assert_eq!(
            head_of(&repo),
            owner_head_before,
            "land must not create a commit in the owning repo",
        );
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["status", "--porcelain"])
            .output()
            .expect("status");
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(
            status.contains("committed.txt"),
            "the landed file is an uncommitted working-tree change:\n{status}"
        );
    }

    #[test]
    fn land_refuses_nongit_naming_the_vcs() {
        let mut meta = WorktreeMeta {
            worktree: std::path::PathBuf::from("/nonexistent/wt"),
            source_repo: std::path::PathBuf::from("/nonexistent/repo"),
            base_commit: String::new(),
            branch: "b".to_string(),
            name: "agent-x".to_string(),
            agent_id: Some("x".to_string()),
            session_id: "s".to_string(),
            created_at: "2026-07-08T00:00:00Z".to_string(),
            class: WORKTREE_CLASS_AGENT.to_string(),
            link: None,
            vcs: "svn".to_string(),
        };
        match land(&meta, true) {
            LandOutcome::Refused { reason } => {
                assert!(reason.contains("svn"), "refusal names the vcs: {reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        meta.vcs = "hg".to_string();
        match land(&meta, true) {
            LandOutcome::Refused { reason } => {
                assert!(reason.contains("hg"), "refusal names the vcs: {reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[test]
    fn land_empty_worktree_is_a_noop() {
        if !have_bin("git") {
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let wt = add_worktree(&repo);
        let meta = meta_for(&repo, &wt);
        assert_eq!(
            land(&meta, true),
            LandOutcome::Empty,
            "clean worktree lands nothing"
        );
    }

    // ── misc 189: debt-transfer primitives ──────────────────────────────────

    #[test]
    fn owner_label_is_the_worktree_leaf() {
        // The dirname IS the owner's agent id (bug 91): a subagent worktree's leaf
        // segment is the bare `<id>` the worker edited under.
        assert_eq!(
            worktree_owner_label(Path::new("/wt/agents/s/w1")),
            "w1",
            "the leaf is the owner's agent id",
        );
    }

    #[test]
    fn transfer_of_a_paid_owner_is_empty() {
        // No unpaid entries (the owner paid its gate, never edited, or its batch
        // died with a bounced daemon) → nothing transfers, regardless of what
        // landed. The never-lock-out doctrine, in a pure function.
        let wt = Path::new("/wt");
        let repo = Path::new("/repo");
        let landed: std::collections::BTreeSet<_> =
            [repo.join("a.rs"), repo.join("b.rs")].into_iter().collect();
        assert!(
            owner_unpaid_landed(&[], wt, repo, &landed).is_empty(),
            "a paid (empty-ledger) owner transfers no debt",
        );
    }

    #[test]
    fn transfer_is_unpaid_intersect_landed_mapped_onto_the_repo() {
        // The owner left three unpaid files; only two of them landed. Exactly those
        // two transfer, re-anchored from the worktree onto the owning repo — a file
        // that did not land carries no debt.
        let wt = Path::new("/wt");
        let repo = Path::new("/repo");
        let unpaid = vec![wt.join("a.rs"), wt.join("sub/b.rs"), wt.join("dropped.rs")];
        let landed: std::collections::BTreeSet<_> = [repo.join("a.rs"), repo.join("sub/b.rs")]
            .into_iter()
            .collect();
        let transfer = owner_unpaid_landed(&unpaid, wt, repo, &landed);
        let expected: std::collections::BTreeSet<_> = [repo.join("a.rs"), repo.join("sub/b.rs")]
            .into_iter()
            .collect();
        assert_eq!(
            transfer, expected,
            "only unpaid files that actually landed transfer, mapped onto the repo",
        );
    }

    #[test]
    fn transfer_skips_paths_outside_the_worktree() {
        // A defensive guard: an unpaid path that is not under the worktree (the
        // ledger should never hold one) is skipped rather than mis-mapped.
        let wt = Path::new("/wt");
        let repo = Path::new("/repo");
        let unpaid = vec![Path::new("/elsewhere/x.rs").to_path_buf(), wt.join("in.rs")];
        let mut landed = std::collections::BTreeSet::new();
        landed.insert(repo.join("in.rs"));
        let transfer = owner_unpaid_landed(&unpaid, wt, repo, &landed);
        let mut expected = std::collections::BTreeSet::new();
        expected.insert(repo.join("in.rs"));
        assert_eq!(
            transfer, expected,
            "an out-of-worktree unpaid path is skipped; the in-worktree one transfers",
        );
    }
}
