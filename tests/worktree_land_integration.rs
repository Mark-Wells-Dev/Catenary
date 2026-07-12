// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for `catenary worktree diff` / `worktree land` (misc 158).
//!
//! Each test spawns the real daemon in an isolated `XDG_STATE_HOME`, creates a
//! registered agent worktree with the `worktree-create` hook (sharing that state
//! home so the daemon and the sidecar agree on paths), makes changes in the
//! worktree, and drives `worktree diff` (the filesystem-local CLI) and
//! `worktree land` (`tool/worktree-land` IPC) against them.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use common::{
    BridgeProcess, diagnostics_output, ipc_request, ipc_request_long, isolate_env, mockls_lsp_arg,
    xdg_config_home,
};

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

/// Spawns the daemon with `repo` as the sole workspace root, restoring the real
/// `PATH` that `isolate_env` clears so the daemon can shell out to `git` (the
/// `worktree land` apply/diff path). The XDG bases stay isolated per-test.
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

/// The current `HEAD` oid of `dir` (`git rev-parse HEAD`).
fn git_head(dir: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse HEAD");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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
        "session_id": "land-test",
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

/// Runs `catenary worktree diff <worktree> [--name-only]` under `state_home`,
/// returning its stdout.
fn run_diff(state_home: &str, worktree: &Path, name_only: bool) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.args(["worktree", "diff", worktree.to_str().expect("wt path")]);
    if name_only {
        cmd.arg("--name-only");
    }
    let out = cmd.output().expect("run worktree diff");
    assert!(
        out.status.success(),
        "worktree diff failed; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Sends a `tool/worktree-land` request and returns the parsed response.
fn land(bridge: &BridgeProcess, worktree: &Path, keep: bool) -> Value {
    let socket = bridge.wait_for_ipc_socket().expect("socket");
    let response = ipc_request(
        &socket,
        &json!({
            "method": "tool/worktree-land",
            "path": worktree.to_str().expect("wt path"),
            "keep": keep,
            "session_id": "land-test",
        }),
    )
    .expect("land ipc");
    serde_json::from_str(response.trim()).expect("parse land response")
}

#[test]
fn diff_shows_tracked_and_untracked_then_land_applies_and_removes() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();

    let worktree = create_worktree(&state_home, &repo);

    // A tracked modification AND an untracked new file.
    std::fs::write(worktree.join("README.md"), "hello world\n").expect("modify tracked");
    std::fs::write(worktree.join("new.txt"), "brand new\n").expect("add untracked");

    // `worktree diff` shows BOTH (untracked as a new-file hunk).
    let diff = run_diff(&state_home, &worktree, false);
    assert!(
        diff.contains("README.md"),
        "diff must show the tracked change:\n{diff}"
    );
    assert!(
        diff.contains("new.txt") && diff.contains("new file mode"),
        "diff must render the untracked file as a new-file hunk:\n{diff}"
    );

    // `--name-only` lists both.
    let names = run_diff(&state_home, &worktree, true);
    let listed: Vec<&str> = names
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert!(
        listed.contains(&"README.md"),
        "name-only lists tracked: {listed:?}"
    );
    assert!(
        listed.contains(&"new.txt"),
        "name-only lists untracked: {listed:?}"
    );

    // `land` applies BOTH into the owning repo and removes the worktree.
    let resp = land(&bridge, &worktree, false);
    assert_eq!(resp["status"], "ok", "land response: {resp}");
    assert_eq!(resp["removed"], true, "worktree removed on success");
    let landed: Vec<String> = resp["paths"]
        .as_array()
        .expect("paths array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(
        landed.contains(&"README.md".to_string()),
        "landed: {landed:?}"
    );
    assert!(
        landed.contains(&"new.txt".to_string()),
        "landed: {landed:?}"
    );

    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).expect("read"),
        "hello world\n",
        "the tracked modification landed in the owning repo",
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).expect("read"),
        "brand new\n",
        "the untracked file landed as a new file",
    );
    assert!(!worktree.exists(), "the worktree dir is removed");
}

#[test]
fn land_conflict_refuses_naming_the_file_and_leaves_everything_untouched() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let worktree = create_worktree(&state_home, &repo);

    // Both sides edit the SAME line differently → the 3way apply conflicts.
    std::fs::write(worktree.join("README.md"), "worktree version\n").expect("wt edit");
    std::fs::write(repo.join("README.md"), "owner version\n").expect("owner edit");
    run_git(&repo, &["commit", "-aqm", "owner change"]);

    let resp = land(&bridge, &worktree, false);
    assert_eq!(resp["status"], "refused", "land response: {resp}");
    let msg = resp["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("README.md"),
        "the refusal names the conflicting file: {msg}"
    );

    // The owning repo is untouched (still the owner's committed content), and the
    // worktree is kept.
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).expect("read"),
        "owner version\n",
        "the owning repo is untouched on a conflict refusal",
    );
    assert!(worktree.exists(), "the worktree is kept on refusal");
}

#[test]
fn land_keep_lands_without_removing() {
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let worktree = create_worktree(&state_home, &repo);

    std::fs::write(worktree.join("new.txt"), "kept\n").expect("add untracked");

    let resp = land(&bridge, &worktree, true);
    assert_eq!(resp["status"], "ok", "land response: {resp}");
    assert_eq!(resp["removed"], false, "--keep leaves the worktree");
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).expect("read"),
        "kept\n",
        "the change landed despite --keep",
    );
    assert!(worktree.exists(), "--keep leaves the worktree dir in place");
}

#[test]
fn committed_worktree_diffs_and_lands_without_committing_in_the_parent() {
    // Commit-aware diff/land (misc 166): a worktree that COMMITTED its work must
    // still diff to its real delta (not an empty vs-HEAD patch) and land that work
    // into the owning repo — as an uncommitted change; land never commits in the
    // parent.
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let worktree = create_worktree(&state_home, &repo);

    let owner_head_before = git_head(&repo);

    // The worker commits its work in the worktree (against the intended flow, but
    // exactly the case misc 166 makes recoverable).
    std::fs::write(worktree.join("committed.txt"), "c\n").expect("write");
    std::fs::write(worktree.join("README.md"), "hello committed\n").expect("modify tracked");
    run_git(&worktree, &["config", "user.email", "t@example.com"]);
    run_git(&worktree, &["config", "user.name", "Test"]);
    run_git(&worktree, &["add", "-A"]);
    run_git(&worktree, &["commit", "-q", "-m", "worker local commit"]);

    // The commit-aware diff shows the committed work (a vs-HEAD diff would be
    // empty — the pre-166 blindness).
    let diff = run_diff(&state_home, &worktree, false);
    assert!(
        diff.contains("committed.txt") && diff.contains("new file mode"),
        "the committed new file is visible in the diff:\n{diff}"
    );
    assert!(
        diff.contains("README.md"),
        "the committed tracked change is visible in the diff:\n{diff}"
    );

    // Land applies the committed work into the owning repo.
    let resp = land(&bridge, &worktree, false);
    assert_eq!(resp["status"], "ok", "land response: {resp}");
    assert_eq!(resp["removed"], true, "worktree removed on success");
    assert_eq!(
        std::fs::read_to_string(repo.join("committed.txt")).expect("read"),
        "c\n",
        "the committed new file landed in the owning repo",
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("README.md")).expect("read"),
        "hello committed\n",
        "the committed tracked change landed in the owning repo",
    );
    // Land never commits in the parent — the owning repo's HEAD is unchanged and
    // the landed work is an uncommitted working-tree change.
    assert_eq!(
        git_head(&repo),
        owner_head_before,
        "land must not create a commit in the owning repo",
    );
    assert!(!worktree.exists(), "the worktree is removed on success");
}

#[test]
fn land_nongit_refuses_naming_the_vcs() {
    // Forge a non-git worktree by rewriting the sidecar's `vcs` to `svn` (no svn
    // binary needed — the vcs guard refuses before any git call).
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    init_repo(&repo);

    let bridge = spawn_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let worktree = create_worktree(&state_home, &repo);

    // Rewrite the sidecar's vcs tag.
    let leaf = worktree.file_name().and_then(|n| n.to_str()).expect("leaf");
    let sidecar = worktree.with_file_name(format!("{leaf}.meta.json"));
    let mut meta: Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).expect("read sidecar"))
            .expect("parse sidecar");
    meta["vcs"] = json!("svn");
    std::fs::write(&sidecar, meta.to_string()).expect("rewrite sidecar");

    let resp = land(&bridge, &worktree, false);
    assert_eq!(resp["status"], "refused", "land response: {resp}");
    let msg = resp["message"].as_str().unwrap_or("");
    assert!(msg.contains("svn"), "the refusal names the vcs: {msg}");
    assert!(worktree.exists(), "the worktree is kept");
}

// ── Batch arming through the PreToolUse hook (misc 158) ─────────────────────

/// The mock language for the batch-arming test: files with this extension are
/// covered by mockls, so land's resolved write-set enters the batch.
const LAND_LANG: &str = "wLnd1";

/// Drives the real `catenary hook pre-tool` binary for a Claude `Bash`
/// `tool_input.command` under `state_home`, with PATH restored (the land
/// resolver arm shells out to git). Returns the hook's stdout — a deny JSON,
/// or empty on allow.
fn pre_tool_bash(state_home: &str, command: &str) -> String {
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.args(["hook", "pre-tool", "--format=claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn pre-tool hook");
    {
        let mut stdin = child.stdin.take().expect("hook stdin");
        writeln!(stdin, "{payload}").expect("write hook payload");
    }
    let out = child.wait_with_output().expect("wait for hook");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn land_write_set_arms_the_diagnostics_batch_via_the_hook() {
    // The full first-class arming chain: the PreToolUse hook resolves land's
    // write set (the worktree's changed paths mapped onto the owning repo) and
    // records it into the caller's diagnostics batch — exactly like `git apply`
    // — then the daemon applies and removes, and a bare diagnostics run walks
    // exactly the landed paths.
    let repo_dir = tempfile::tempdir().expect("repo dir");
    let repo = repo_dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mkdir repo");
    run_git(&repo, &["init", "-q"]);
    run_git(&repo, &["config", "user.email", "t@example.com"]);
    run_git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join(format!("tracked.{LAND_LANG}")), "echo hello\n")
        .expect("write tracked");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "init"]);

    // Daemon with mockls covering `.wLnd1` files and the repo as the sole root.
    let lsp = mockls_lsp_arg(LAND_LANG, "");
    let root = repo.to_str().expect("repo path").to_string();
    let bridge = BridgeProcess::spawn_with(move |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", &root);
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
    })
    .expect("spawn daemon");
    let state_home = bridge.state_home().to_string();

    // An active command allowlist so the hook's write resolver runs (an absent
    // `[commands]` section short-circuits resolution entirely).
    let cfg_dir = xdg_config_home(&state_home).join("catenary");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir config dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[commands]\nallow = [\"git\"]\npipeline = [\"grep\"]\n",
    )
    .expect("write commands config");

    let worktree = create_worktree(&state_home, &repo);
    std::fs::write(
        worktree.join(format!("tracked.{LAND_LANG}")),
        "echo changed\n",
    )
    .expect("modify tracked");
    std::fs::write(worktree.join(format!("new.{LAND_LANG}")), "echo new\n").expect("add untracked");

    // The daemon must be up before the hook runs (the hook silently no-ops
    // without a socket, which would drop the write-set on the floor).
    bridge.wait_for_ipc_socket().expect("daemon socket");

    // Drive the REAL PreToolUse hook with the land command. On allow it emits
    // nothing (or a non-deny context payload); the resolved write set rides the
    // `pre-tool/editing-state` IPC into the batch.
    let hook_out = pre_tool_bash(
        &state_home,
        &format!("catenary worktree land {}", worktree.display()),
    );
    assert!(
        !hook_out.contains("\"deny\"") && !hook_out.to_lowercase().contains("permissiondecision"),
        "the land command must pass the hook: {hook_out}"
    );

    // Land for real so the files exist in the owning repo.
    let resp = land(&bridge, &worktree, false);
    assert_eq!(resp["status"], "ok", "land response: {resp}");

    // Bare diagnostics for the same (session, agent) key the hook recorded
    // under: prepare the handoff, then consume it. The receipt must walk
    // exactly the landed paths.
    let socket = bridge.wait_for_ipc_socket().expect("socket");
    ipc_request(
        &socket,
        &json!({ "method": "pre-tool/editing-stop", "agent_id": "" }),
    )
    .expect("prepare handoff");
    let text = ipc_request_long(
        &socket,
        bridge.daemon_pid(),
        &json!({ "method": "tool/editing-stop" }),
    )
    .expect("consume handoff");
    let receipt = diagnostics_output(&text);

    assert!(
        receipt.contains(&format!("tracked.{LAND_LANG}")),
        "the batch armed for the landed tracked file:\n{receipt}"
    );
    assert!(
        receipt.contains(&format!("new.{LAND_LANG}")),
        "the batch armed for the landed untracked file:\n{receipt}"
    );
    assert!(
        !receipt.contains("no edited files"),
        "the batch must not be empty after the hook recorded land's write set:\n{receipt}"
    );
}
