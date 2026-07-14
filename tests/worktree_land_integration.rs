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

// ── Batch arming through the PreToolUse hook: debt transfer (misc 189) ───────
//
// The ruling: landing a worktree transfers its worker's **unpaid** diagnostics
// debt to the landing agent, and nothing more. A worktree whose worker **paid**
// its gate lands debt-free; the old content-based arm (arm the whole landed
// diff regardless of payment) is retired. These tests drive the full chain: the
// owner subagent edits (and optionally pays) through the real hook IPC, then the
// landing agent runs the real `PreToolUse` hook for `catenary worktree land` and
// a bare diagnostics run — asserting the transfer, not the content.

/// The mock language: files with this extension are covered by mockls, so the
/// owner's edits accumulate and land's transfer set enters the landing batch.
/// The server key is the blessed `mockls-event` persona (diagnostics-debt 04c):
/// manifest membership is what makes the mock a diagnostics source, and its
/// empty behavior bundle is plain default-push mockls — no wire change.
const LAND_LANG: &str = "mockls-event";

/// The owner subagent's identity — the session the worktree records in its
/// sidecar (the parent session) and its agent id (the worktree's leaf dirname).
const OWNER_SESSION: &str = "land-test";
const OWNER_AGENT: &str = "w1";

/// Drives the real `catenary hook pre-tool` binary for a Claude `Bash`
/// `tool_input.command` under `state_home`, carrying the landing agent's
/// `session_id` (so the daemon routes to the same session that holds the owner's
/// batch) with PATH restored (the land resolver shells out to git). Returns the
/// hook's stdout — a deny JSON, or empty on allow.
fn pre_tool_land(state_home: &str, session_id: &str, command: &str) -> String {
    let payload = json!({
        "tool_name": "Bash",
        "session_id": session_id,
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

/// Spawns the daemon with mockls covering `.mockls-event`, the repo as the sole root,
/// and an active `[commands]` allowlist (so the land command's write resolver
/// runs — an absent section short-circuits resolution). Returns the bridge.
fn spawn_land_daemon(repo: &Path) -> BridgeProcess {
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
    let cfg_dir = xdg_config_home(&state_home).join("catenary");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir config dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[commands]\nallow = [\"git\"]\npipeline = [\"grep\"]\n",
    )
    .expect("write commands config");
    bridge
}

/// Pins `path` as a tracked root via `tool/roots-add` (the `catenary pin` path).
///
/// Only the worktree needs pinning: the owning repo is the `CATENARY_ROOTS` seed,
/// registered as the `seed:env` tracker contributor at boot (misc 192), so it
/// survives the `sync_roots` this pin triggers (which rebuilds the primary
/// session's roots from tracker contributors). Before misc 192 both had to be
/// pinned — the seed was not a contributor and the worktree pin evicted it.
fn pin_root(socket: &Path, path: &Path) {
    ipc_request(
        socket,
        &json!({
            "method": "tool/roots-add",
            "path": path.to_str().expect("root path"),
        }),
    )
    .expect("pin root");
}

/// Records one covered edit into the OWNER's batch (`OWNER_SESSION`/`OWNER_AGENT`)
/// via the real editing-state hook IPC — the worker touching a file in its
/// worktree.
fn owner_edits(socket: &Path, file: &Path) {
    ipc_request(
        socket,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": file.to_str().expect("file path"),
            "session_id": OWNER_SESSION,
            "agent_id": OWNER_AGENT,
        }),
    )
    .expect("owner edit");
}

/// Pays the OWNER's gate — a bare `catenary diagnostics` for its batch: prepare
/// the handoff, then consume it, flipping every batch file to delivered.
fn owner_pays(bridge: &BridgeProcess, socket: &Path) {
    ipc_request(
        socket,
        &json!({
            "method": "pre-tool/editing-stop",
            "session_id": OWNER_SESSION,
            "agent_id": OWNER_AGENT,
        }),
    )
    .expect("owner prepare handoff");
    ipc_request_long(
        socket,
        bridge.daemon_pid(),
        &json!({
            "method": "tool/editing-stop",
            "session_id": OWNER_SESSION,
            "agent_id": OWNER_AGENT,
        }),
    )
    .expect("owner consume handoff");
}

/// The landing agent's bare `catenary diagnostics` receipt for its own batch
/// (session `OWNER_SESSION`, main agent `""`) after the land — the transfer's
/// visible effect.
fn landing_receipt(bridge: &BridgeProcess, socket: &Path) -> String {
    ipc_request(
        socket,
        &json!({
            "method": "pre-tool/editing-stop",
            "session_id": OWNER_SESSION,
            "agent_id": "",
        }),
    )
    .expect("landing prepare handoff");
    let text = ipc_request_long(
        socket,
        bridge.daemon_pid(),
        &json!({
            "method": "tool/editing-stop",
            "session_id": OWNER_SESSION,
            "agent_id": "",
        }),
    )
    .expect("landing consume handoff");
    diagnostics_output(&text)
}

#[test]
fn land_of_a_paid_worktree_arms_nothing() {
    // Acceptance: a worktree whose worker PAID its gate before landing arms
    // nothing — re-arming would pay already-paid debt.
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

    let bridge = spawn_land_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let socket = bridge.wait_for_ipc_socket().expect("daemon socket");

    let worktree = create_worktree(&state_home, &repo);
    // Pin the worktree as a tracker root; the owning repo is the seed:env
    // contributor and survives on its own (see `pin_root`).
    pin_root(&socket, &worktree);

    // The worker edits a covered file in the worktree, then PAYS its gate.
    let edited = worktree.join(format!("tracked.{LAND_LANG}"));
    std::fs::write(&edited, "echo changed\n").expect("modify tracked");
    owner_edits(&socket, &edited);
    owner_pays(&bridge, &socket);

    // The landing agent runs the real land hook, then lands.
    let hook_out = pre_tool_land(
        &state_home,
        OWNER_SESSION,
        &format!("catenary worktree land {}", worktree.display()),
    );
    assert!(
        !hook_out.contains("\"deny\"") && !hook_out.to_lowercase().contains("permissiondecision"),
        "the land command must pass the hook: {hook_out}"
    );
    let resp = land(&bridge, &worktree, false);
    assert_eq!(resp["status"], "ok", "land response: {resp}");

    // The landing agent's batch is empty — a paid worktree lands debt-free. The
    // daemon returns an empty receipt for a genuinely empty batch (the CLI is what
    // prints `[no edited files]`), so the diagnostics output is empty and never
    // names the already-paid file.
    let receipt = landing_receipt(&bridge, &socket);
    assert!(
        receipt.trim().is_empty(),
        "a paid worktree must arm nothing on the landing agent:\n{receipt}"
    );
    assert!(
        !receipt.contains(&format!("tracked.{LAND_LANG}")),
        "the already-paid file must not re-arm the landing gate:\n{receipt}"
    );
}

#[test]
fn land_of_an_unpaid_worktree_transfers_exactly_its_unpaid_files() {
    // Acceptance: a worktree with UNPAID entries transfers exactly those files'
    // debt to the landing agent — and only the ones that actually land.
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

    let bridge = spawn_land_daemon(&repo);
    let state_home = bridge.state_home().to_string();
    let socket = bridge.wait_for_ipc_socket().expect("daemon socket");

    let worktree = create_worktree(&state_home, &repo);
    // Pin the worktree as a tracker root; the owning repo is the seed:env
    // contributor and survives on its own (see `pin_root`).
    pin_root(&socket, &worktree);

    // The worker edits two covered files in the worktree and NEVER pays — a
    // worker interrupted before `catenary diagnostics`.
    let tracked = worktree.join(format!("tracked.{LAND_LANG}"));
    let created = worktree.join(format!("new.{LAND_LANG}"));
    std::fs::write(&tracked, "echo changed\n").expect("modify tracked");
    std::fs::write(&created, "echo new\n").expect("add untracked");
    owner_edits(&socket, &tracked);
    owner_edits(&socket, &created);

    // The landing agent runs the real land hook, then lands.
    let hook_out = pre_tool_land(
        &state_home,
        OWNER_SESSION,
        &format!("catenary worktree land {}", worktree.display()),
    );
    assert!(
        !hook_out.contains("\"deny\"") && !hook_out.to_lowercase().contains("permissiondecision"),
        "the land command must pass the hook: {hook_out}"
    );
    let resp = land(&bridge, &worktree, false);
    assert_eq!(resp["status"], "ok", "land response: {resp}");

    // The landing agent's batch armed for exactly the two unpaid files, mapped
    // onto the owning repo — the debt transferred (a non-empty receipt naming
    // both files; an empty receipt would mean nothing armed).
    let receipt = landing_receipt(&bridge, &socket);
    assert!(
        !receipt.trim().is_empty(),
        "unpaid debt must transfer to the landing agent:\n{receipt}"
    );
    assert!(
        receipt.contains(&format!("tracked.{LAND_LANG}")),
        "the unpaid tracked file's debt transferred:\n{receipt}"
    );
    assert!(
        receipt.contains(&format!("new.{LAND_LANG}")),
        "the unpaid new file's debt transferred:\n{receipt}"
    );
}
