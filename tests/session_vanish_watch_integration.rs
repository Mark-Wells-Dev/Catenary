// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration test for the session vanish watch (ws49-01).
//!
//! MCP's connection lifecycle used to be the daemon's session handle: disconnect
//! drove roots teardown and LSP release. Dropping MCP replaces that handle with a
//! hook-declared one — the hook CLI walks its ancestry to the host process
//! (`claude` / `agy`) and declares that process's `(pid, start-time)` alongside
//! the hook's `session_id`. A cheap liveness watch on the reaper cadence detects a
//! host that dies WITHOUT a `SessionEnd` (crash, kill, OOM) and tears the session
//! down through the normal per-session release path.
//!
//! This test spawns the real daemon in an isolated `XDG_STATE_HOME` with a tiny
//! sweep interval (`CATENARY_SWEEP_INTERVAL_MS`) so the watch ticks in
//! milliseconds. It establishes a hook session that owns a worktree-class root
//! (the only session-keyed root contributor), spawns a real long-lived "host"
//! child process, declares that child as the session's host via a `pre-tool`
//! hook, then KILLS the host — and asserts the session's worktree root is released
//! on the next watch tick while the project root survives.

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
// The vanish watch's liveness probe reads `/proc/<pid>`, which exists only on
// Linux (the macOS `kill(0)` + sysctl leg is a flagged follow-up — ticket 01
// forbids `unsafe`/`libc` here). This whole test binary therefore compiles to
// nothing off Linux, where it could prove nothing — but the crate doc above
// stays ATTACHED even when this cfg elides the contents (doc-before-cfg,
// probe-verified), so the macOS clippy leg's `missing_docs` stays satisfied.
#![cfg(target_os = "linux")]

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// Spawns the daemon with `repo` as the sole workspace root and a tiny sweep
/// interval so the vanish watch ticks in ~50 ms. Restores the real `PATH`
/// `isolate_env` clears so the daemon can shell out to `git`; XDG bases stay
/// isolated per-test.
fn spawn_daemon(repo: &Path) -> BridgeProcess {
    let root = repo.to_str().expect("repo path").to_string();
    BridgeProcess::spawn_with(move |cmd| {
        cmd.env("CATENARY_ROOTS", &root);
        // Shrink the reaper cadence across the process boundary (the same shrink
        // `CATENARY_BIRTH_GRACE_SECS` uses) so the vanish watch fires promptly.
        cmd.env("CATENARY_SWEEP_INTERVAL_MS", "50");
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
    })
    .expect("spawn daemon")
}

/// Runs the `worktree-create` hook under `state_home` with `cwd = repo`, returning
/// the created worktree's absolute path. Shares the daemon's state home so both
/// agree on the worktree layout.
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

/// Sends the `subagent-start/mount-worktree` hook dispatch so the daemon mounts
/// `worktree` under `worktree:{session}:{agent}`.
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

/// Declares a host handle for `session_id` by sending a `pre-tool/editing-state`
/// hook carrying the host process's `(pid, start-time)` — exactly what the real
/// hook CLI writes after its ancestry walk. A `Read` tool call is neutral (never
/// gated) so this records the handle without any editing side effects.
fn declare_host(socket: &Path, session_id: &str, host_pid: u32, host_start_time: u64, cwd: &Path) {
    ipc_request(
        socket,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Read",
            "session_id": session_id,
            "agent_id": "",
            "format": "claude",
            "cwd": cwd.to_str().expect("cwd path"),
            "host_pid": host_pid,
            "host_start_time": host_start_time,
        }),
    )
    .expect("pre-tool handle declaration ipc");
}

/// Reads a live process's start-time (field 22 of `/proc/<pid>/stat`) — the
/// pid-reuse guard the daemon compares against.
fn proc_start_time(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Spawns a long-lived, killable "host" child process. `sleep 300` stands in for
/// the host session process the hook would descend from — the test declares its
/// pid as the session's host, then kills it to simulate a crash/kill/OOM.
fn spawn_fake_host() -> Child {
    Command::new("sleep")
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fake host process")
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
fn killed_host_releases_the_session_roots_on_the_next_watch_tick() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let mut bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();

    // Track the repo as a root so the worktree auto-mount predicate is satisfied
    // (the census stays MCP-keyed for ws49-01; only the SESSION handle is
    // hook-established here).
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

    // Establish a hook session that owns a worktree-class root.
    let session_id = "vanish-watch-test";
    let agent_id = "agent-vanish-1";
    let worktree = create_worktree(&state_home, &repo, session_id, "agent-vanish");
    let canonical_worktree = worktree.canonicalize().expect("canonicalize worktree");
    mount_worktree(&socket, session_id, agent_id, &worktree);
    assert!(
        wait_for_root_state(&socket, &canonical_worktree, true, Duration::from_secs(10)),
        "the worktree root is mounted after subagent-start",
    );

    // Spawn a real host process and declare it as the session's host — the hook's
    // ancestry-walk result, delivered on a neutral Read tool call.
    let mut host = spawn_fake_host();
    let host_pid = host.id();
    let host_start_time = proc_start_time(host_pid);
    declare_host(&socket, session_id, host_pid, host_start_time, &repo);

    // The session is live: its worktree root must NOT be torn down while the host
    // is alive, even across several watch ticks. Sleep past a few sweeps and
    // re-check — a false teardown of a live session would show here.
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        root_tracked(&socket, &canonical_worktree),
        "a LIVE host's session keeps its roots across watch ticks",
    );

    // Kill the host without any SessionEnd — the crash/kill/OOM case. The vanish
    // watch must detect the gone pid on the next tick and release the session's
    // roots through the normal path.
    host.kill().expect("kill fake host");
    host.wait().expect("reap fake host");

    assert!(
        wait_for_root_state(&socket, &canonical_worktree, false, Duration::from_secs(10)),
        "the vanished host's worktree root is released by the vanish watch",
    );

    // Scoped release: the project root the worktree branched from survives — the
    // normal release path removes only this session's contributions.
    assert!(
        root_tracked(&socket, &canonical_repo),
        "the project root survives the vanished session's release",
    );
}
