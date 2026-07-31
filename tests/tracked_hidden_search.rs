// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! End-to-end guards for "tracked beats hidden" (misc 227) and its glob-side
//! sibling (bug 66).
//!
//! The specimen is the one the ticket was filed on, reduced: a repository whose
//! CI config lives in a tracked `.github/`, next to an untracked `.secrets/`.
//! Before the rule, a default `catenary grep` answered as if the tracked CI
//! config did not exist — "no matches" indistinguishable from absence.
//!
//! Every run is daemon-less: since the ws43 cutovers the walk is CLI-side, so
//! the binary alone exercises the posture and no LSP server or daemon is needed.
//!
//! `git` must be reachable, so these tests re-add `PATH` *after* `isolate_env`
//! (which clears it): trackedness is a git property and there is no tracked set
//! without the binary that reads the index. Everything else stays isolated.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use common::isolate_env;

/// The token the specimen searches for — the ticket's own.
const NEEDLE: &str = "workflow_dispatch";

/// Runs a git command in `root`, asserting it succeeded.
fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} should succeed");
}

/// Lays out the specimen tree. `.alpha` and `.beta` are structurally identical
/// hidden siblings — bug 66's finding 1 was two such siblings answering
/// differently for the same `**` pattern.
fn populate(root: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join(".github/workflows"))?;
    std::fs::create_dir_all(root.join(".secrets"))?;
    std::fs::create_dir_all(root.join(".alpha"))?;
    std::fs::create_dir_all(root.join(".beta"))?;
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join(".github/workflows/ci.yml"),
        "on:\n  workflow_dispatch:\n",
    )?;
    std::fs::write(
        root.join(".secrets/token.txt"),
        "workflow_dispatch secret\n",
    )?;
    std::fs::write(root.join(".alpha/a.txt"), "workflow_dispatch alpha\n")?;
    std::fs::write(root.join(".beta/b.txt"), "workflow_dispatch beta\n")?;
    std::fs::write(root.join("src/main.rs"), "// workflow_dispatch visible\n")?;
    Ok(())
}

/// The specimen as a **git repository**: `.github`, `.alpha` and `.beta` are
/// tracked; `.secrets` is not.
fn tracked_repo() -> Result<tempfile::TempDir> {
    let dir = common::canonical_tempdir()?;
    populate(dir.path())?;
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", ".github", ".alpha", ".beta", "src"]);
    Ok(dir)
}

/// The same tree with **no repository** — the ruling's non-git carve-out.
fn plain_tree() -> Result<tempfile::TempDir> {
    let dir = common::canonical_tempdir()?;
    populate(dir.path())?;
    Ok(dir)
}

/// Runs the `catenary` binary daemon-less with cwd = `root`, returning
/// `(stdout, stderr)`.
fn run_cli(root: &Path, subargs: &[&str]) -> Result<(String, String)> {
    let state = tempfile::tempdir()?;
    let state_home = state.path().to_str().context("state dir")?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    // `isolate_env` blanks `PATH`; the tracked-set consultation needs `git` on
    // it. Re-adding only `PATH` keeps every Catenary base dir isolated.
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.current_dir(root)
        .args(subargs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().context("run catenary binary")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

// ── grep: the ticket's specimen ──────────────────────────────────────────────

#[test]
fn grep_default_searches_tracked_hidden_and_still_skips_untracked_hidden() -> Result<()> {
    let repo = tracked_repo()?;
    let (stdout, _stderr) = run_cli(repo.path(), &["grep", NEEDLE])?;

    assert!(
        stdout.contains("ci.yml"),
        "tracked CI config under a hidden dir joins the default walk:\n{stdout}"
    );
    assert!(
        stdout.contains("main.rs"),
        "the visible tree is unaffected:\n{stdout}"
    );
    assert!(
        !stdout.contains("token.txt"),
        "an untracked hidden path stays skipped by default:\n{stdout}"
    );
    Ok(())
}

#[test]
fn grep_include_hidden_still_reaches_untracked_hidden() -> Result<()> {
    let repo = tracked_repo()?;
    let (stdout, _stderr) = run_cli(repo.path(), &["grep", NEEDLE, "--include-hidden"])?;

    assert!(
        stdout.contains("token.txt"),
        "`--include-hidden` is unchanged — it still reaches untracked hidden:\n{stdout}"
    );
    assert!(stdout.contains("ci.yml"), "and everything else:\n{stdout}");
    Ok(())
}

#[test]
fn grep_outside_a_repository_keeps_the_plain_hidden_rule() -> Result<()> {
    let plain = plain_tree()?;
    let (stdout, _stderr) = run_cli(plain.path(), &["grep", NEEDLE])?;

    assert!(
        stdout.contains("main.rs"),
        "the visible tree still answers:\n{stdout}"
    );
    assert!(
        !stdout.contains("ci.yml") && !stdout.contains("token.txt"),
        "a non-git root has no tracked set, so every hidden path stays skipped:\n{stdout}"
    );
    Ok(())
}

#[test]
fn grep_default_and_include_hidden_find_the_same_matches_when_nothing_hidden_is_untracked()
-> Result<()> {
    // The delta between the two postures is exactly the *untracked* hidden tail:
    // remove it and the two must find the same matches. Only the tally is
    // compared — `--include-hidden` also descends `.git`, whose binary blobs add
    // a skip note the default posture never earns (the gate refuses `.git` by
    // name), and that tail is a true difference, not a missed match.
    let repo = tracked_repo()?;
    std::fs::remove_dir_all(repo.path().join(".secrets"))?;

    let (plain_out, _) = run_cli(repo.path(), &["grep", NEEDLE, "--count"])?;
    let (hidden_out, _) = run_cli(
        repo.path(),
        &["grep", NEEDLE, "--include-hidden", "--count"],
    )?;
    let tally = |out: &str| {
        out.trim()
            .split_once(" (")
            .map_or_else(|| out.trim().to_string(), |(head, _)| head.to_string())
    };
    assert_eq!(
        tally(&plain_out),
        tally(&hidden_out),
        "with no untracked hidden content left, the two postures find the same set\n\
         default: {plain_out}\n--include-hidden: {hidden_out}"
    );
    Ok(())
}

// ── glob: bug 66, finding 1 (sibling hidden traversal) ───────────────────────

#[test]
fn glob_traverses_sibling_hidden_dirs_consistently() -> Result<()> {
    let repo = tracked_repo()?;
    let pattern = format!("{}/**", repo.path().display());
    let (stdout, _stderr) = run_cli(repo.path(), &["glob", &pattern])?;

    // Two structurally identical, identically-tracked hidden siblings must get
    // the same answer — bug 66's finding 1 was exactly this pair disagreeing.
    assert!(
        stdout.contains("a.txt"),
        "tracked hidden sibling `.alpha` is traversed:\n{stdout}"
    );
    assert!(
        stdout.contains("b.txt"),
        "tracked hidden sibling `.beta` is traversed identically:\n{stdout}"
    );
    assert!(
        !stdout.contains("token.txt"),
        "the untracked hidden sibling stays skipped:\n{stdout}"
    );
    Ok(())
}

#[test]
fn glob_anchored_and_traversed_forms_agree_for_a_tracked_hidden_dir() -> Result<()> {
    // The surviving discriminator before this change: naming a hidden directory
    // as the pattern's anchor traversed it (the walker never filters its own
    // root), while reaching the same directory through a parent's `**` pruned
    // it — so the answer depended on where the pattern was rooted.
    let repo = tracked_repo()?;
    let anchored = format!("{}/.github/**", repo.path().display());
    let traversed = format!("{}/**", repo.path().display());

    let (anchored_out, _) = run_cli(repo.path(), &["glob", &anchored])?;
    let (traversed_out, _) = run_cli(repo.path(), &["glob", &traversed])?;

    assert!(
        anchored_out.contains("ci.yml"),
        "the anchored form finds it:\n{anchored_out}"
    );
    assert!(
        traversed_out.contains("ci.yml"),
        "and so does the traversed form:\n{traversed_out}"
    );
    Ok(())
}

#[test]
fn glob_outside_a_repository_keeps_the_plain_hidden_rule() -> Result<()> {
    let plain = plain_tree()?;
    let pattern = format!("{}/**", plain.path().display());
    let (stdout, _stderr) = run_cli(plain.path(), &["glob", &pattern])?;

    assert!(
        stdout.contains("main.rs"),
        "the visible tree still lists:\n{stdout}"
    );
    assert!(
        !stdout.contains("a.txt") && !stdout.contains("token.txt"),
        "a non-git root has no tracked set, so hidden stays hidden:\n{stdout}"
    );
    Ok(())
}

#[test]
fn glob_listing_of_a_repo_root_shows_tracked_hidden_entries() -> Result<()> {
    // The listing walk shares the posture with the pattern walk, so `dir/*` and
    // the match set cannot disagree about what a repository root contains.
    let repo = tracked_repo()?;
    let pattern = format!("{}/*", repo.path().display());
    let (stdout, _stderr) = run_cli(repo.path(), &["glob", &pattern])?;

    assert!(
        stdout.contains(".github"),
        "a tracked hidden directory is a first-class listing entry:\n{stdout}"
    );
    assert!(
        !stdout.contains(".secrets"),
        "an untracked hidden directory is not:\n{stdout}"
    );
    Ok(())
}

// ── glob: bug 66, finding 2 (the misleading no-match hint) ───────────────────

#[test]
fn glob_absolute_no_match_omits_the_cwd_anchoring_hint() -> Result<()> {
    let plain = plain_tree()?;
    let pattern = format!("{}/nowhere/**", plain.path().display());
    let (_stdout, stderr) = run_cli(plain.path(), &["glob", &pattern])?;

    assert!(
        stderr.contains("no matches for pattern:"),
        "the zero-match report stays loud:\n{stderr}"
    );
    assert!(
        !stderr.contains("relative patterns anchor at cwd"),
        "an absolute pattern must not be told about relative anchoring:\n{stderr}"
    );
    Ok(())
}

#[test]
fn glob_relative_no_match_keeps_the_cwd_anchoring_hint() -> Result<()> {
    let plain = plain_tree()?;
    let (_stdout, stderr) = run_cli(plain.path(), &["glob", "nowhere/**"])?;

    assert!(
        stderr.contains("relative patterns anchor at cwd"),
        "a relative pattern still gets the explanation that describes it:\n{stderr}"
    );
    Ok(())
}
