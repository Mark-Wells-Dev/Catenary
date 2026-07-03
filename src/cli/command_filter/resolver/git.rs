// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Git authorship split (ws38 ticket 03, decision 026 §2).
//!
//! Debt attaches to the introduction of **uncommitted content**, never to
//! navigation between **committed states** — a commit is content someone
//! already accepted, and `catenary diagnostics` is the pre-acceptance review a
//! new write still awaits. Git forms therefore split three ways:
//!
//! - **Sync** (`pull`, `merge`, `checkout <branch>`, `switch`, `reset --hard`,
//!   `restore` to HEAD, `rebase`) — allowed, **no debt**, and marked a
//!   **barrier**: they move the working tree between committed states without
//!   recording a set, so a downstream *state-query* resolution (glob
//!   expansion, git query) is Opaque, the same poisoning shape as an opaque
//!   `cd`. Argument opacity is irrelevant for pure navigation
//!   (`git checkout $BRANCH` allows — no write-set is needed); `fetch`
//!   (`.git/`-only) and `clean` (a pure delete) are no-debt but not barriers
//!   (neither moves the working tree between committed states, so a downstream
//!   query stays sound).
//! - **Content introduction** (`git apply`, `stash pop`/`apply`,
//!   `checkout <ref> -- <paths>`, `restore --source=<ref>`) — a Write in Bash
//!   clothing: the exact set is resolved by **querying git** (git is literally
//!   a tracked filesystem). Literal forms only; a computed ref/patch/stash, an
//!   opaque cwd, or a failed query is Opaque.
//! - **Everything else git** (`status`, `add`, `commit`, `clone`, `mv`, …) —
//!   keeps its prior allowlist treatment (`NoWrite`): it introduces no
//!   *attributable* uncommitted content (it rearranges committed/tracked
//!   content), and coherence backstops any stray write. The table grows
//!   fail-closed.
//!
//! **Queries run at hook time** in the command's cwd (the resolver runs
//! client-side, so cwd is accurate; the hook is blocking, so hook-time state
//! *is* execution-time state). They are non-interactive
//! (`GIT_TERMINAL_PROMPT=0`, stdin closed) and read-only. The paths git returns
//! are **repo-relative**, so they resolve against the repository root
//! (discovered with `git rev-parse --show-toplevel` in the cwd), never against
//! the cwd itself.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Cwd, SegmentClass, State, Unresolved, run, u};
use crate::cli::command_filter::parse::{SimpleCommand, WordMeta};

/// Resolve a `git` invocation into its authorship class. Sync forms set the
/// barrier and record nothing; content forms query git; everything else keeps
/// the inherited `NoWrite` boundary.
pub(super) fn resolve_git(cmd: &SimpleCommand, state: &mut State, seg_name: &str) -> SegmentClass {
    let Some((relocated, start)) = split_git(&cmd.argv) else {
        // Bare `git` (or only global options): prints usage, writes nothing.
        return SegmentClass::NoWrite;
    };
    let sub = cmd.argv[start].as_str();
    let args = cmd.argv.get(start + 1..).unwrap_or(&[]);
    let metas = cmd.argv_meta.get(start + 1..).unwrap_or(&[]);

    match sub {
        // Sync movers: no debt, barrier the working tree.
        "pull" | "merge" | "rebase" | "switch" => {
            state.barrier = true;
            SegmentClass::NoWrite
        }
        // reset carries no debt; it barriers only when it moves the working
        // tree (`--hard`/`--merge`/`--keep`). A soft/mixed reset touches only
        // HEAD/index, so a downstream query stays sound.
        "reset" => {
            if args
                .iter()
                .any(|a| matches!(a.as_str(), "--hard" | "--merge" | "--keep"))
            {
                state.barrier = true;
            }
            SegmentClass::NoWrite
        }
        // clean deletes untracked files — a pure delete only shrinks later
        // expansions, so it carries no debt and is not a barrier.
        "clean" => SegmentClass::PureDelete,
        // checkout / restore split within themselves (navigation vs pathspec /
        // --source content).
        "checkout" => resolve_checkout(args, metas, state, relocated, seg_name),
        "restore" => resolve_restore(args, metas, state, relocated, seg_name),
        // Pure content introduction, resolved by querying git.
        "apply" => run(resolve_apply(args, metas, state, relocated), seg_name),
        "stash" => resolve_stash(args, metas, state, relocated, seg_name),
        // `fetch` (`.git/`-only), `clone` (a root-lifecycle event realizing
        // committed upstream content), and every other subcommand introduce no
        // *attributable* uncommitted content, so they keep the inherited
        // NoWrite boundary — and, not moving the working tree between committed
        // states, are not barriers either.
        _ => SegmentClass::NoWrite,
    }
}

/// Resolve the standalone `patch` utility. Its target is the file being
/// patched: named as the first positional operand
/// (`patch file.rs < changes.diff`), it is recorded directly. A patch whose
/// targets live only in a diff on stdin can't be resolved (the hook can't read
/// stdin) — Opaque, teaching the resolvable form.
pub(super) fn resolve_patch(cmd: &SimpleCommand, state: &State, seg_name: &str) -> SegmentClass {
    run(patch_query(cmd, state), seg_name)
}

// ── git argv shape ───────────────────────────────────────────────────────────

/// Skip git's global options to find the subcommand index. Returns
/// `(relocated, index)` where `relocated` is set when a repo-relocating global
/// option (`-C`, `--git-dir`, `--work-tree`) is present — such forms move the
/// repo out from under a hook-time query, so content introduction under them
/// is Opaque. `None` when there is no subcommand word.
fn split_git(argv: &[String]) -> Option<(bool, usize)> {
    let mut relocated = false;
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if !a.starts_with('-') {
            return Some((relocated, i));
        }
        match a {
            "-C" | "--git-dir" | "--work-tree" => {
                relocated = true;
                i += 2;
            }
            "-c" | "--namespace" | "--exec-path" | "--super-prefix" => i += 2,
            _ if a.starts_with("--git-dir=") || a.starts_with("--work-tree=") => {
                relocated = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

// ── checkout ─────────────────────────────────────────────────────────────────

/// `git checkout` splits by shape: a branch switch (or any form with no
/// pathspec) is pure navigation — allow, no debt, barrier; a pathspec form
/// (`checkout <ref> -- <paths>` or `checkout <ref> <paths>`) is content
/// introduction resolved by `git diff --name-only <ref> -- <paths>`.
/// `checkout -- <paths>` (no ref) restores from the index — an undo, no debt,
/// barrier.
fn resolve_checkout(
    args: &[String],
    metas: &[WordMeta],
    state: &mut State,
    relocated: bool,
    seg_name: &str,
) -> SegmentClass {
    let mut operands: Vec<usize> = Vec::new();
    let mut dd_before: Option<usize> = None;
    let mut branch_creating = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if dd_before.is_some() {
            operands.push(i);
            i += 1;
            continue;
        }
        if a == "--" {
            dd_before = Some(operands.len());
            i += 1;
            continue;
        }
        if is_flag(a) {
            match a {
                // Branch-creating flags consume the new branch name and force
                // the navigation reading.
                "-b" | "-B" | "--orphan" => {
                    branch_creating = true;
                    i += 2;
                }
                "-p" | "--patch" => return run(Err(interactive_select()), seg_name),
                _ if is_pathspec_from_file(a) => return run(Err(pathspec_from_file()), seg_name),
                _ => i += 1,
            }
            continue;
        }
        operands.push(i);
        i += 1;
    }

    if branch_creating {
        state.barrier = true;
        return SegmentClass::NoWrite;
    }
    match dd_before {
        // `checkout <ref> -- <paths>`: content from an explicit ref.
        Some(k) if k >= 1 => run(
            content_diff(args, metas, operands[0], &operands[k..], state, relocated),
            seg_name,
        ),
        // `checkout -- <paths>`: restore working tree from the index — an undo.
        Some(_) => {
            state.barrier = true;
            SegmentClass::NoWrite
        }
        None => {
            if operands.len() <= 1 {
                // Branch switch (or bare `checkout`) — pure navigation;
                // argument opacity is irrelevant.
                state.barrier = true;
                SegmentClass::NoWrite
            } else {
                // `checkout <ref> <paths>` (no `--`) — content from a ref.
                run(
                    content_diff(args, metas, operands[0], &operands[1..], state, relocated),
                    seg_name,
                )
            }
        }
    }
}

// ── restore ──────────────────────────────────────────────────────────────────

/// `git restore` is an undo (restore from index/HEAD) — no debt, barrier —
/// **unless** `--source=<ref>` names an arbitrary ref, which introduces
/// content resolved by `git diff --name-only <ref> -- <paths>`. A `--staged`
/// restore without `--worktree` touches only the index (no working-tree move,
/// so not a barrier).
fn resolve_restore(
    args: &[String],
    metas: &[WordMeta],
    state: &mut State,
    relocated: bool,
    seg_name: &str,
) -> SegmentClass {
    let mut source: Option<(&str, WordMeta)> = None;
    let mut staged = false;
    let mut worktree = false;
    let mut paths: Vec<usize> = Vec::new();
    let mut after_dd = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let meta = metas.get(i).copied().unwrap_or_default();
        if after_dd {
            paths.push(i);
            i += 1;
            continue;
        }
        if a == "--" {
            after_dd = true;
            i += 1;
            continue;
        }
        if is_flag(a) {
            if let Some(v) = a.strip_prefix("--source=") {
                source = Some((v, meta));
                i += 1;
                continue;
            }
            match a {
                "--source" | "-s" => {
                    // The ref is the following token — its literalness rides
                    // that token's meta.
                    if let Some(next) = args.get(i + 1) {
                        source =
                            Some((next.as_str(), metas.get(i + 1).copied().unwrap_or_default()));
                    }
                    i += 2;
                }
                "--staged" | "-S" => {
                    staged = true;
                    i += 1;
                }
                "--worktree" | "-W" => {
                    worktree = true;
                    i += 1;
                }
                "-p" | "--patch" => return run(Err(interactive_select()), seg_name),
                _ if is_pathspec_from_file(a) => return run(Err(pathspec_from_file()), seg_name),
                _ => i += 1,
            }
            continue;
        }
        paths.push(i);
        i += 1;
    }

    if let Some((ref_word, ref_meta)) = source {
        return run(
            content_diff_words(
                ref_word,
                ref_meta,
                &path_words(args, metas, &paths),
                state,
                relocated,
            ),
            seg_name,
        );
    }
    // Undo (restore from index/HEAD).
    if staged && !worktree {
        // Index-only: no working-tree move, so not a barrier.
        return SegmentClass::NoWrite;
    }
    state.barrier = true;
    SegmentClass::NoWrite
}

// ── content diff query (checkout pathspec / restore --source) ────────────────

/// Resolve `git diff --name-only <ref> -- <paths>` from operand indices.
fn content_diff(
    args: &[String],
    metas: &[WordMeta],
    ref_idx: usize,
    path_idxs: &[usize],
    state: &State,
    relocated: bool,
) -> Result<SegmentClass, Unresolved> {
    let ref_meta = metas.get(ref_idx).copied().unwrap_or_default();
    content_diff_words(
        args[ref_idx].as_str(),
        ref_meta,
        &path_words(args, metas, path_idxs),
        state,
        relocated,
    )
}

/// Resolve `git diff --name-only <ref> -- <paths>` from an already-extracted
/// ref and path list. The files that differ between `<ref>` and the working
/// tree are exactly those the checkout/restore will overwrite.
fn content_diff_words(
    ref_word: &str,
    ref_meta: WordMeta,
    paths: &[(String, WordMeta)],
    state: &State,
    relocated: bool,
) -> Result<SegmentClass, Unresolved> {
    barrier_check(state)?;
    if relocated {
        return Err(relocated_repo());
    }
    if !is_literal(ref_meta) {
        return Err(opaque_ref());
    }
    for (_, m) in paths {
        if !is_literal(*m) {
            return Err(opaque_paths());
        }
    }
    let cwd = query_cwd(state).ok_or_else(no_cwd)?;
    let root = repo_root(cwd).ok_or_else(query_failed)?;

    let mut q: Vec<&str> = vec!["diff", "--name-only", ref_word, "--"];
    for (p, _) in paths {
        q.push(p.as_str());
    }
    let lines = git_query(cwd, &q).ok_or_else(query_failed)?;
    Ok(SegmentClass::Recorded(join_repo_relative(&root, &lines)))
}

// ── apply ────────────────────────────────────────────────────────────────────

/// `git apply <patch>` → the exact file set from a `git apply --numstat`
/// dry-run (a read-only state query on the patch). The patch must be a literal
/// file argument; a patch on stdin, a computed argument, or a rename the
/// resolver doesn't model is Opaque.
fn resolve_apply(
    args: &[String],
    metas: &[WordMeta],
    state: &State,
    relocated: bool,
) -> Result<SegmentClass, Unresolved> {
    barrier_check(state)?;
    if relocated {
        return Err(relocated_repo());
    }
    let mut patch_files: Vec<usize> = Vec::new();
    // Flags that must ride along to `--numstat` so its reported paths match
    // what apply would write (the strip level; `-R` is harmless but kept).
    let mut passthru: Vec<&str> = Vec::new();
    let mut after_dd = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if after_dd {
            patch_files.push(i);
            i += 1;
            continue;
        }
        if a == "--" {
            after_dd = true;
            i += 1;
            continue;
        }
        if is_flag(a) {
            if a == "--directory" || a.starts_with("--directory=") {
                // Relocates the patch targets — unmodeled.
                return Err(relocated_repo());
            }
            // Strip level, space form: `-p N` / `--strip N`.
            if a == "-p" || a == "--strip" {
                passthru.push(a);
                if let Some(n) = args.get(i + 1) {
                    passthru.push(n.as_str());
                }
                i += 2;
                continue;
            }
            // Strip level, glued form: `-p1` / `--strip=1`.
            if is_numeric_strip(a) {
                passthru.push(a);
                i += 1;
                continue;
            }
            if a == "-R" || a == "--reverse" {
                passthru.push(a);
                i += 1;
                continue;
            }
            // Value flags whose argument is a separate token.
            if matches!(a, "--whitespace" | "--exclude" | "--include") {
                i += 2;
                continue;
            }
            // Boolean / `=`-glued / unknown flags: skip the flag only. An
            // unknown value-flag's value falls through as a "patch file" and
            // fails the numstat query — Opaque, the safe direction.
            i += 1;
            continue;
        }
        patch_files.push(i);
        i += 1;
    }

    if patch_files.is_empty() {
        return Err(stdin_patch());
    }
    for &pi in &patch_files {
        if !is_literal(metas.get(pi).copied().unwrap_or_default()) {
            return Err(opaque_patch());
        }
    }
    let cwd = query_cwd(state).ok_or_else(no_cwd)?;

    let mut q: Vec<&str> = vec!["apply", "--numstat"];
    q.extend(passthru);
    for &pi in &patch_files {
        q.push(args[pi].as_str());
    }
    let lines = git_query(cwd, &q).ok_or_else(query_failed)?;
    let rels = numstat_paths(&lines)?;

    // `git apply` reports patch paths after `-p` stripping; whether it writes
    // them relative to the repo root (index mode) or the cwd (working-tree mode
    // from a subdirectory) is mode-dependent, so record both. They coincide
    // when cwd is the root (the common case); otherwise this over-records, the
    // safe direction (decision 026). The repo root is best-effort — a
    // working-tree apply resolves against the cwd even outside a repo.
    let root = repo_root(cwd);
    let mut writes = BTreeSet::new();
    for rel in &rels {
        writes.insert(super::normalize(&cwd.join(rel)));
        if let Some(r) = &root {
            writes.insert(super::normalize(&r.join(rel)));
        }
    }
    Ok(SegmentClass::Recorded(writes))
}

/// Extract the path column from `git apply --numstat` output
/// (`added<TAB>deleted<TAB>path`). A rename (`a => b`) is not modeled — Opaque.
fn numstat_paths(lines: &[String]) -> Result<Vec<String>, Unresolved> {
    let mut out = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(3, '\t');
        let (_add, _del) = (fields.next(), fields.next());
        let Some(path) = fields.next() else {
            return Err(query_failed());
        };
        if path.contains(" => ") {
            return Err(rename_patch());
        }
        out.push(path.to_string());
    }
    Ok(out)
}

// ── stash ────────────────────────────────────────────────────────────────────

/// `git stash pop`/`apply` restores the agent's own stashed work → the file
/// set from `git stash show --name-only [<stash>]`. Other `stash`
/// subcommands (`push`, `list`, `drop`, …) introduce no working-tree content.
fn resolve_stash(
    args: &[String],
    metas: &[WordMeta],
    state: &State,
    relocated: bool,
    seg_name: &str,
) -> SegmentClass {
    match args.first().map(String::as_str) {
        Some("pop" | "apply") => run(
            stash_show(
                args.get(1..).unwrap_or(&[]),
                metas.get(1..).unwrap_or(&[]),
                state,
                relocated,
            ),
            seg_name,
        ),
        _ => SegmentClass::NoWrite,
    }
}

/// Query `git stash show --name-only [<stash>]` for the restored file set.
fn stash_show(
    rest: &[String],
    metas: &[WordMeta],
    state: &State,
    relocated: bool,
) -> Result<SegmentClass, Unresolved> {
    barrier_check(state)?;
    if relocated {
        return Err(relocated_repo());
    }
    // The stash ref is the first non-flag operand (default: the latest stash).
    let mut stash_ref: Option<&str> = None;
    for (k, a) in rest.iter().enumerate() {
        if is_flag(a) {
            continue;
        }
        if !is_literal(metas.get(k).copied().unwrap_or_default()) {
            return Err(opaque_ref());
        }
        stash_ref = Some(a.as_str());
        break;
    }
    let cwd = query_cwd(state).ok_or_else(no_cwd)?;
    let root = repo_root(cwd).ok_or_else(query_failed)?;

    let mut q: Vec<&str> = vec!["stash", "show", "--name-only"];
    if let Some(r) = stash_ref {
        q.push(r);
    }
    let lines = git_query(cwd, &q).ok_or_else(query_failed)?;
    Ok(SegmentClass::Recorded(join_repo_relative(&root, &lines)))
}

// ── patch (standalone utility) ───────────────────────────────────────────────

/// Resolve the `patch` utility's write-set: the file being patched, named as
/// the first positional operand (`patch file.rs < changes.diff`). Value flags
/// are skipped so their arguments aren't mistaken for the target. A patch
/// whose targets come only from a diff on stdin (no positional) is Opaque, as
/// is any output-relocating form (`-o`, `-d`, `-D`, `--prefix`, …) the
/// resolver doesn't model.
fn patch_query(cmd: &SimpleCommand, state: &State) -> Result<SegmentClass, Unresolved> {
    barrier_check(state)?;
    let args = &cmd.argv;
    let metas = &cmd.argv_meta;
    let mut positionals: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if is_flag(a) {
            // A form that redirects/relocates the write target — unmodeled.
            if is_patch_relocating(a) {
                return Err(patch_relocated());
            }
            // Value flags whose argument is a separate token — skip both so a
            // path argument (the patch file, a fuzz count, …) isn't mistaken
            // for the patched file.
            if patch_takes_value(a) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        positionals.push(i);
        i += 1;
    }
    let Some(&first) = positionals.first() else {
        return Err(stdin_patch());
    };
    if !is_literal(metas.get(first).copied().unwrap_or_default()) {
        return Err(opaque_paths());
    }
    // The patched file resolves against the cwd (patch's convention).
    let path = super::resolve_path(args[first].as_str(), state)?;
    Ok(SegmentClass::Recorded(BTreeSet::from([path])))
}

/// Whether a `patch` flag redirects or relocates the write target in a way the
/// resolver doesn't model (`-o`/`--output`, `-d`/`--directory`, `-D`/`--ifdef`,
/// `-B`/`--prefix`, `-Y`/`--basename-prefix`), in space or `=`-glued form.
fn is_patch_relocating(a: &str) -> bool {
    const RELOCATING: &[&str] = &[
        "-o",
        "--output",
        "-d",
        "--directory",
        "-D",
        "--ifdef",
        "-B",
        "--prefix",
        "-Y",
        "--basename-prefix",
    ];
    RELOCATING.contains(&a)
        || [
            "--output=",
            "--directory=",
            "--ifdef=",
            "--prefix=",
            "--basename-prefix=",
        ]
        .iter()
        .any(|p| a.starts_with(p))
}

/// Whether a `patch` flag takes its value as the following token (so both must
/// be skipped when scanning for the positional target).
fn patch_takes_value(a: &str) -> bool {
    matches!(
        a,
        "-i" | "--input"
            | "-p"
            | "--strip"
            | "-r"
            | "--reject-file"
            | "-F"
            | "--fuzz"
            | "-g"
            | "--get"
            | "-z"
            | "--suffix"
            | "-V"
            | "--version-control"
    )
}

// ── shared helpers ───────────────────────────────────────────────────────────

/// Whether an argv word is an option (`-x`, `--long`) rather than an operand.
/// A bare `-` is an operand (stdin conventionally).
fn is_flag(a: &str) -> bool {
    a.len() > 1 && a.starts_with('-')
}

/// Whether `a` is a glued strip-level flag (`-p1`, `--strip=1`) whose value
/// must ride along to `git apply --numstat`.
fn is_numeric_strip(a: &str) -> bool {
    let digits = a.strip_prefix("--strip=").or_else(|| a.strip_prefix("-p"));
    digits.is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether a flag is `--pathspec-from-file` (space or `=`-glued form).
fn is_pathspec_from_file(a: &str) -> bool {
    a == "--pathspec-from-file" || a.starts_with("--pathspec-from-file=")
}

/// Whether a word is a plain literal usable in a git query: no command
/// substitution, process substitution, or live shell expansion. A
/// quoted-metacharacter word (`'*.rs'`) is literal — git receives it verbatim
/// and does its own pathspec matching.
const fn is_literal(meta: WordMeta) -> bool {
    !meta.value_subs && !meta.process_subs && !meta.any_live()
}

/// The threaded cwd as an absolute directory, when known. Content queries need
/// one to run git and to resolve the repo root.
fn query_cwd(state: &State) -> Option<&Path> {
    match &state.cwd {
        Cwd::Abs(p) => Some(p.as_path()),
        Cwd::Rel(_) | Cwd::Poisoned => None,
    }
}

/// Deny a content-query resolution downstream of a git sync barrier.
fn barrier_check(state: &State) -> Result<(), Unresolved> {
    if state.barrier {
        Err(super::git_barrier())
    } else {
        Ok(())
    }
}

/// Build `(word, meta)` pairs for a set of operand indices.
fn path_words(args: &[String], metas: &[WordMeta], idxs: &[usize]) -> Vec<(String, WordMeta)> {
    idxs.iter()
        .map(|&i| (args[i].clone(), metas.get(i).copied().unwrap_or_default()))
        .collect()
}

/// Join repo-relative query results against the repository root, normalized.
fn join_repo_relative(root: &Path, rels: &[String]) -> BTreeSet<PathBuf> {
    rels.iter()
        .filter(|r| !r.is_empty())
        .map(|r| super::normalize(&root.join(r)))
        .collect()
}

/// The repository root (`git rev-parse --show-toplevel`) run in `cwd`.
fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let lines = git_query(cwd, &["rev-parse", "--show-toplevel"])?;
    lines.into_iter().next().map(PathBuf::from)
}

/// Run a non-interactive, read-only git query in `cwd`, returning its stdout
/// lines on a clean (exit 0) run. Any failure — git missing, a non-zero exit
/// (not a repo, unknown ref/patch/stash), or non-UTF-8 output — is `None`, and
/// the caller classifies the write Opaque (fail-closed).
fn git_query(cwd: &Path, args: &[&str]) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.lines().map(str::to_string).collect())
}

// ── teaching denials ─────────────────────────────────────────────────────────

fn opaque_ref() -> Unresolved {
    u(
        "git-opaque-ref",
        "This git ref is assembled at runtime (`$VAR` / `$(…)` / an unquoted glob), so \
         the hook can't ask git which files it would change. Name the ref literally.",
    )
}

fn opaque_paths() -> Unresolved {
    u(
        "git-opaque-paths",
        "A pathspec here is assembled at runtime (`$VAR` / `$(…)` / an unquoted glob), \
         so the hook can't ask git which files it would change. Name the paths \
         literally (a quoted git pathspec like `'*.rs'` is fine).",
    )
}

fn opaque_patch() -> Unresolved {
    u(
        "git-opaque-patch",
        "The patch argument is assembled at runtime, so the hook can't read which files \
         the patch touches. Pass the patch as a literal file path.",
    )
}

fn relocated_repo() -> Unresolved {
    u(
        "git-relocated-repo",
        "`git -C` / `--git-dir` / `--work-tree` (or `git apply --directory`) runs git \
         against another repository, so the hook can't attribute the files it would \
         write there. Run it from inside that repository.",
    )
}

fn no_cwd() -> Unresolved {
    u(
        "git-no-cwd",
        "This git command needs a working directory so the hook can ask git which files \
         it would change, but it was given none for the call. Run it where the \
         repository is.",
    )
}

fn query_failed() -> Unresolved {
    u(
        "git-query-failed",
        "git couldn't tell the hook which files this command would change — this isn't a \
         repository here, or the ref / patch / stash is unknown. Check the arguments, or \
         apply the change through the host's edit tools.",
    )
}

fn patch_relocated() -> Unresolved {
    u(
        "git-patch-relocated",
        "This `patch` form sends its output somewhere the hook can't predict (`-o` / \
         `-d` / `-D` / `--prefix`). Patch the file in place, or write the output with a \
         redirect.",
    )
}

fn stdin_patch() -> Unresolved {
    u(
        "git-stdin-patch",
        "The files this patch would change come from a diff the hook can't read (stdin, \
         or no file argument). Name the patch file as an argument — and for `patch`, \
         name the file being patched.",
    )
}

fn rename_patch() -> Unresolved {
    u(
        "git-rename-patch",
        "This patch renames files, and the hook can't yet tell which paths the rename \
         produces. Apply the rename through the host's edit tools, or split it out.",
    )
}

fn interactive_select() -> Unresolved {
    u(
        "git-interactive-select",
        "`-p` / `--patch` picks hunks interactively, so which files change is decided at \
         runtime and the hook can't see it in advance. Apply the whole set (drop `-p`), \
         or use the host's edit tools.",
    )
}

fn pathspec_from_file() -> Unresolved {
    u(
        "git-pathspec-from-file",
        "`--pathspec-from-file` reads the path list from a file at runtime, so the hook \
         can't tell which files change. Pass the paths on the command line.",
    )
}
