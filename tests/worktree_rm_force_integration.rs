// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for `catenary worktree rm --force` (misc 166).
//!
//! `--force` is the explicit, user-typed exception to the never-auto-clean rule:
//! it discards a *dirty* worktree through the proper disposal path (retiring the
//! root, sweeping the sidecar) instead of the raw-git dance that bypassed root
//! retirement. Without `--force`, a dirty feats worktree is refused exactly as
//! before — dirty worktrees are never auto-cleaned.
//!
//! Each test spawns the real daemon in an isolated `XDG_STATE_HOME`, creates a
//! registered agent worktree via the `worktree-create` hook, then rewrites its
//! sidecar `class` to `feat` (so the plain-`rm` refusal path applies — agent
//! worktrees already force-remove), dirties it, and drives `tool/worktree-rm`.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use common::{BridgeProcess, ipc_request, isolate_env};

/// Runs a git command in `cwd`, asserting success.
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

/// Spawns the daemon with `repo` as the sole workspace root, restoring the real
/// `PATH` that `isolate_env` clears so the daemon can shell out to `git`.
fn spawn_daemon(repo: &Path) -> BridgeProcess {
    let root = repo.to_str().expect("repo path").to_string();
    BridgeProcess::spawn_with(move |cmd| {
        cmd.env("CATENARY_ROOTS", &root);
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
    })
    .expect("spawn daemon")
}

/// Initializes a git repo with one commit at `dir`.
fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("mkdir repo");
    run_git(dir, &["init", "-q"]);
    run_git(dir, &["config", "user.email", "t@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    std::fs::write(dir.join("README.md"), "hello\n").expect("write file");
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", "init"]);
}

/// Runs the `worktree-create` hook under `state_home` with `cwd = repo`, returns
/// the created worktree's absolute path (the hook prints exactly the path).
fn create_worktree(state_home: &str, repo: &Path) -> PathBuf {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path); // the hook shells out to git
    }
    cmd.args(["hook", "worktree-create", "--format=claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let payload = json!({
        "cwd": repo.to_str().expect("repo path"),
        "hook_event_name": "WorktreeCreate",
        "session_id": "rm-test",
        "name": "agent-w1",
    });
    let mut child = cmd.spawn().expect("spawn worktree-create hook");
    {
        let mut stdin = child.stdin.take().expect("hook stdin");
        stdin
            .write_all(payload.to_string().as_bytes())
            .expect("write hook stdin");
    }
    let out = child.wait_with_output().expect("wait for hook");
    assert!(
        out.status.success(),
        "worktree-create failed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!path.is_empty(), "hook must print the worktree path");
    PathBuf::from(path)
}

/// Rewrites the worktree's sidecar `class` to `feat` — so the plain-`rm` refusal
/// path (feats refuses dirty) applies. Agent worktrees force-remove without
/// `--force`, so they cannot exercise the refusal.
fn make_feat(worktree: &Path) {
    let leaf = worktree.file_name().and_then(|n| n.to_str()).expect("leaf");
    let sidecar = worktree.with_file_name(format!("{leaf}.meta.json"));
    let mut meta: Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).expect("read sidecar"))
            .expect("parse sidecar");
    meta["class"] = json!("feat");
    std::fs::write(&sidecar, meta.to_string()).expect("rewrite sidecar");
}

/// Sends a `tool/worktree-rm` request (optionally forcing) and returns the parsed
/// response.
fn rm(bridge: &BridgeProcess, worktree: &Path, force: bool) -> Value {
    let socket = bridge.wait_for_ipc_socket().expect("socket");
    let response = ipc_request(
        &socket,
        &json!({
            "method": "tool/worktree-rm",
            "path": worktree.to_str().expect("wt path"),
            "force": force,
            "session_id": "rm-test",
        }),
    )
    .expect("rm ipc");
    serde_json::from_str(response.trim()).expect("parse rm response")
}

/// The sidecar path for a worktree (`<worktree>.meta.json`, a sibling).
fn sidecar_of(worktree: &Path) -> PathBuf {
    let leaf = worktree.file_name().and_then(|n| n.to_str()).expect("leaf");
    worktree.with_file_name(format!("{leaf}.meta.json"))
}

#[test]
fn plain_rm_refuses_a_dirty_feat_and_keeps_it() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let worktree = create_worktree(&state_home, &repo);
    make_feat(&worktree);

    // Dirty it with an uncommitted change.
    std::fs::write(worktree.join("scratch.txt"), "wip\n").expect("dirty the worktree");

    // Plain rm (no force) refuses and keeps everything — the never-auto-clean rule.
    let resp = rm(&bridge, &worktree, false);
    assert_eq!(
        resp["status"], "kept",
        "plain rm refuses a dirty feat: {resp}"
    );
    let msg = resp["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("uncommitted"),
        "the refusal names the dirty state: {msg}"
    );
    assert!(worktree.exists(), "the worktree is kept on refusal");
    assert!(
        sidecar_of(&worktree).exists(),
        "the sidecar is kept on refusal"
    );
}

#[test]
fn rm_force_discards_a_dirty_worktree_and_names_the_discard() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let worktree = create_worktree(&state_home, &repo);
    make_feat(&worktree);

    // Dirty it: one uncommitted tracked change and one untracked file (two entries).
    std::fs::write(worktree.join("README.md"), "changed\n").expect("modify tracked");
    std::fs::write(worktree.join("scratch.txt"), "wip\n").expect("add untracked");

    // `--force` discards through the proper disposal path: the dir is gone, the
    // sidecar is swept (root retirement rides `is_disposed()` in the handler),
    // and the response names what was discarded (the dirty file count).
    let resp = rm(&bridge, &worktree, true);
    assert_eq!(resp["status"], "ok", "forced discard succeeds: {resp}");
    assert_eq!(resp["removed"], true, "the worktree is removed: {resp}");
    let discarded = resp["discarded"].as_str().unwrap_or("");
    assert!(
        discarded.contains("2 uncommitted files"),
        "the discard names the dropped dirty files: {resp}"
    );

    assert!(!worktree.exists(), "the worktree dir is removed");
    assert!(
        !sidecar_of(&worktree).exists(),
        "the sidecar is swept on a forced discard (registry consistent)"
    );
}

#[test]
fn rm_force_on_a_clean_worktree_removes_without_a_discard_summary() {
    // `--force` on a clean worktree still removes, but names no discard — nothing
    // dirty was dropped.
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let worktree = create_worktree(&state_home, &repo);
    make_feat(&worktree);

    let resp = rm(&bridge, &worktree, true);
    assert_eq!(
        resp["status"], "ok",
        "forced clean removal succeeds: {resp}"
    );
    assert_eq!(resp["removed"], true, "the worktree is removed: {resp}");
    assert!(
        resp["discarded"].is_null(),
        "a clean forced removal names no discard: {resp}"
    );
    assert!(!worktree.exists(), "the worktree dir is removed");
}
