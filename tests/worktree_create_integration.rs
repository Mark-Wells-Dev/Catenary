// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the `catenary hook worktree-create` subcommand
//! (misc 144): out-of-tree agent worktree creation under the cache dir.
//!
//! Each test spawns the real binary with a synthetic `WorktreeCreate` payload on
//! stdin. `isolate_env` points `XDG_CACHE_HOME` at the test tempdir, so the hook
//! writes its worktrees under `<tempdir>/cache/catenary/worktrees/` instead of
//! the user's real cache. `isolate_env` clears `PATH` (so stray server defaults
//! fail fast); these tests restore it afterwards because the hook shells out to
//! `git`.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use common::{isolate_env, xdg_cache_home};

/// Runs a git command in `cwd`, asserting success (test setup helper — uses the
/// test process's real environment, so `git` is on `PATH`).
fn run_git(cwd: &Path, args: &[&str]) {
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

/// Initializes a git repo with one commit at `dir` (needed for `worktree add -b`,
/// which branches from `HEAD`).
fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir repo");
    run_git(dir, &["init", "-q"]);
    std::fs::write(dir.join("README.md"), "hello\n").expect("write file");
    run_git(dir, &["add", "."]);
    run_git(
        dir,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=Test",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
}

/// Spawns `catenary hook worktree-create --format=claude` with `payload` on
/// stdin, isolated under `home`, and returns its captured output.
fn run_hook(home: &Path, payload: &Value) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, home.to_str().expect("home path"));
    // The hook shells out to `git`; restore the real PATH `isolate_env` cleared.
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.args(["hook", "worktree-create", "--format=claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn worktree-create hook");
    {
        let mut stdin = child.stdin.take().expect("hook stdin");
        stdin
            .write_all(payload.to_string().as_bytes())
            .expect("write hook stdin");
    }
    child.wait_with_output().expect("wait for hook")
}

#[test]
fn worktree_create_makes_out_of_tree_worktree_and_prints_path() {
    let home = tempfile::tempdir().expect("home tempdir");
    let repo = home.path().join("repo");
    init_repo(&repo);

    let output = run_hook(
        home.path(),
        &json!({ "cwd": repo.to_str().expect("repo path"), "hook_event_name": "WorktreeCreate" }),
    );
    assert!(
        output.status.success(),
        "hook must exit 0 on success; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );

    // stdout is EXACTLY the absolute worktree path — no trailing newline, no
    // other output.
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(!stdout.is_empty(), "stdout must carry the worktree path");
    assert_eq!(
        stdout,
        stdout.trim(),
        "stdout must be exactly the path (no surrounding whitespace/newline)",
    );

    let worktree = PathBuf::from(&stdout);
    assert!(worktree.is_absolute(), "printed path must be absolute");
    assert!(worktree.is_dir(), "the worktree dir must exist: {stdout}");

    // It lives OUTSIDE the repo, under the isolated cache dir's worktrees root.
    let cache_worktrees = xdg_cache_home(home.path())
        .join("catenary")
        .join("worktrees");
    assert!(
        worktree.starts_with(&cache_worktrees),
        "worktree {} must live under {}",
        worktree.display(),
        cache_worktrees.display(),
    );
    assert!(
        !worktree.starts_with(&repo),
        "worktree must NOT be nested inside the repo",
    );

    // A linked git worktree carries a `.git` *file* pointing back at the repo.
    let dot_git = worktree.join(".git");
    assert!(dot_git.is_file(), "worktree must have a `.git` file");

    // git registered it against the source repo.
    let listed = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .expect("git worktree list");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains(&stdout),
        "git worktree list must include the new worktree:\n{listed}",
    );
}

#[test]
fn worktree_create_tolerates_extra_payload_fields_and_names_branch() {
    let home = tempfile::tempdir().expect("home tempdir");
    let repo = home.path().join("repo");
    init_repo(&repo);

    // A payload with unknown/extra fields (schema drift) plus a supplied name.
    let output = run_hook(
        home.path(),
        &json!({
            "cwd": repo.to_str().expect("repo path"),
            "session_id": "sess-123",
            "transcript_path": "/tmp/transcript.json",
            "hook_event_name": "WorktreeCreate",
            "worktree_name": "worktree-agent-xyz",
            "permissions": { "mode": "acceptEdits", "nested": { "deep": true } },
            "unknown_future_field": [1, 2, 3]
        }),
    );
    assert!(
        output.status.success(),
        "lenient parse must tolerate extra fields; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        PathBuf::from(&stdout).is_dir(),
        "worktree created: {stdout}"
    );

    // The supplied name drives the branch name.
    let branches = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["branch", "--list", "worktree-agent-xyz"])
        .output()
        .expect("git branch list");
    let branches = String::from_utf8_lossy(&branches.stdout);
    assert!(
        branches.contains("worktree-agent-xyz"),
        "the payload-supplied name should be the branch, got:\n{branches}",
    );
}

#[test]
fn worktree_create_missing_repo_fails_loud_and_nonzero() {
    let home = tempfile::tempdir().expect("home tempdir");
    // A cwd that is NOT inside any git repo (a bare tempdir under /tmp).
    let not_git = home.path().join("not-a-repo");
    std::fs::create_dir_all(&not_git).expect("mkdir not-a-repo");

    let output = run_hook(
        home.path(),
        &json!({ "cwd": not_git.to_str().expect("path") }),
    );
    assert!(
        !output.status.success(),
        "a cwd outside any git repo must fail worktree creation (nonzero exit)",
    );
    assert!(
        output.stdout.is_empty(),
        "no path may be printed on failure; stdout:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        !output.stderr.is_empty(),
        "the failure must be loud on stderr",
    );
}

#[test]
fn worktree_create_prunes_orphans_before_creating() {
    let home = tempfile::tempdir().expect("home tempdir");
    let repo = home.path().join("repo");
    init_repo(&repo);

    // Seed a dead orphan under the cache worktrees root: a dir whose `.git`
    // pointer names a metadata dir that does not exist.
    let cache_worktrees = xdg_cache_home(home.path())
        .join("catenary")
        .join("worktrees");
    let orphan = cache_worktrees.join("-gone-repo-deadbeef");
    std::fs::create_dir_all(&orphan).expect("mkdir orphan");
    std::fs::write(
        orphan.join(".git"),
        format!(
            "gitdir: {}\n",
            home.path().join("gone/.git/worktrees/x").display()
        ),
    )
    .expect("write orphan .git");

    let output = run_hook(
        home.path(),
        &json!({ "cwd": repo.to_str().expect("repo path") }),
    );
    assert!(
        output.status.success(),
        "hook must succeed; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        !orphan.exists(),
        "the dead orphan must be pruned before the create",
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        PathBuf::from(&stdout).is_dir(),
        "the new worktree must exist: {stdout}",
    );
}
