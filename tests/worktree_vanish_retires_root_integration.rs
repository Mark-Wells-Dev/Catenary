// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration test for the worktree-root release edge (bug 106).
//!
//! A worktree-class root is pinned-class — it does not expire on an idle clock;
//! its lifetime authority is the worktree directory. When the dir vanishes
//! (dispose, `git worktree remove`, or manual removal) the daemon's central
//! worktrees-dir watch retires the root through the full retire discipline
//! (`retire_root` — every contributor declaring the path, never just the
//! `worktree:*` mount, so the per-root server set is never orphaned).
//!
//! Each test spawns the real daemon in an isolated `XDG_STATE_HOME`, tracks a
//! repo as its workspace root, creates a registered agent worktree with the
//! `worktree-create` hook (sharing that state home so the daemon and the sidecar
//! agree on paths), mounts it via the `subagent-start/mount-worktree` hook
//! dispatch, then deletes the worktree dir and asserts the root is retired while
//! the project root survives.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use common::{BridgeProcess, ipc_request, isolate_env};

/// Runs a git command in `cwd`, asserting success (uses the test's real env, so
/// git is on PATH).
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

/// Spawns the daemon with `repo` as the sole workspace root, restoring the real
/// `PATH` that `isolate_env` clears so the daemon can shell out to `git`. The XDG
/// bases stay isolated per-test.
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

/// Runs the `worktree-create` hook under `state_home` with `cwd = repo`, returning
/// the created worktree's absolute path (the hook prints exactly the path). Shares
/// the daemon's state home so both agree on the worktree layout.
fn create_worktree(state_home: &str, repo: &Path, session_id: &str, name: &str) -> PathBuf {
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
        "session_id": session_id,
        "name": name,
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

/// Sends the `subagent-start/mount-worktree` hook dispatch for a subagent whose
/// `cwd` is `worktree` — the daemon mounts it under `worktree:{session}:{agent}`
/// iff its canonical project root (the tracked repo) is distinct and tracked.
fn mount_worktree(socket: &Path, session_id: &str, agent_id: &str, worktree: &Path) {
    ipc_request(
        socket,
        &json!({
            "method": "subagent-start/mount-worktree",
            "session_id": session_id,
            "agent_id": agent_id,
            "cwd": worktree.to_str().expect("worktree path"),
        }),
    )
    .expect("subagent-start mount ipc");
}

/// Whether the daemon currently tracks `path` as a root (`tool/roots-ls`).
fn root_tracked(socket: &Path, path: &Path) -> bool {
    let response =
        ipc_request(socket, &json!({ "method": "tool/roots-ls" })).expect("roots-ls ipc");
    response.contains(&path.display().to_string())
}

/// Polls `tool/roots-ls` until `path`'s tracked-ness matches `want`, or the
/// deadline elapses.
fn wait_for_root_state(socket: &Path, path: &Path, want: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if root_tracked(socket, path) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn vanished_worktree_dir_retires_the_root_and_leaves_the_project_root() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let mut bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();

    // Declare the repo as an MCP root so it enters the daemon's RootTracker (an
    // `mcp:*` contributor) — the auto-mount predicate authorizes a worktree mount
    // only when the worktree's canonical project root is already tracked.
    let canonical_repo = repo.canonicalize().expect("canonicalize repo");
    bridge
        .initialize_with_roots(&[canonical_repo.to_str().expect("repo str")])
        .expect("initialize with repo root");

    let socket = bridge.wait_for_ipc_socket().expect("socket");
    bridge
        .wait_for_root(
            canonical_repo.to_str().expect("repo str"),
            Duration::from_secs(10),
        )
        .expect("project root tracked");

    // Create a registered agent worktree of that repo (real `git worktree add`).
    let session_id = "vanish-test";
    let agent_id = "agent-vanish-1";
    let worktree = create_worktree(&state_home, &repo, session_id, "agent-vanish");
    let canonical_worktree = worktree.canonicalize().expect("canonicalize worktree");

    // Mount it: its canonical project root (the tracked repo) is distinct, so the
    // auto-mount predicate authorizes a `worktree:*` root. The vanish-watch
    // registers on the worktree's parent dir at mount time.
    mount_worktree(&socket, session_id, agent_id, &worktree);
    assert!(
        wait_for_root_state(&socket, &canonical_worktree, true, Duration::from_secs(10)),
        "the worktree root is mounted after subagent-start",
    );

    // The directory is the lifetime authority: delete it (dispose / `git worktree
    // remove` / manual removal all reduce to the dir vanishing). The central
    // worktrees-dir watch fires and retires the root through `retire_root`.
    std::fs::remove_dir_all(&worktree).expect("remove worktree dir");

    assert!(
        wait_for_root_state(&socket, &canonical_worktree, false, Duration::from_secs(15)),
        "the vanished worktree root is retired by the dir-deletion watch",
    );

    // The retire is scoped: the project root the worktree branched from survives
    // (retire_root removes only the vanished path from every contributor).
    assert!(
        root_tracked(&socket, &canonical_repo),
        "the project root survives the worktree retirement",
    );
}
