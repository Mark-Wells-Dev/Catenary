// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration coverage for the failed-daemon-boot storm (bug 111).
//!
//! Three defects, three proofs:
//!
//! 1. A boot that aborts *after* the socket bind (an invalid config today, any
//!    post-bind failure tomorrow) must leave **no stranded socket** — so a
//!    subsequent client gets the quiet "no daemon running" arm (os error 2), not
//!    the "unreachable" storm (os error 111).
//! 2. The refusal earns **one** desktop interrupt carrying the real cause,
//!    emitted point-blank (the desktop sink is not yet registered at the abort),
//!    honoring `CATENARY_NOTIFY` off so this test never reaches the real desktop.
//! 3. A genuinely stranded socket (a dead daemon that leaked its socket) makes
//!    every short-lived hook process see the same failure — the cross-process
//!    onset stamp keeps that to **one** notification across N hook invocations.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(clippy::panic, reason = "tests use panic for diagnostics")]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use common::{isolate_env, xdg_config_home, xdg_runtime_dir, xdg_state_home};

/// The IPC socket path under an isolated state home.
fn ipc_socket(root: &str) -> PathBuf {
    xdg_state_home(root).join("catenary").join("catenary.sock")
}

/// The MCP socket path under an isolated state home.
fn mcp_socket(root: &str) -> PathBuf {
    xdg_state_home(root)
        .join("catenary")
        .join("catenary-mcp.sock")
}

/// The onset-dedup stamp path under an isolated runtime dir.
fn unreachable_stamp(root: &str) -> PathBuf {
    xdg_runtime_dir(root)
        .join("catenary")
        .join("daemon-unreachable.stamp")
}

/// Write a user config the daemon reads. `contents` may be malformed to force a
/// post-bind boot abort.
fn write_user_config(root: &str, contents: &str) -> Result<()> {
    let dir = xdg_config_home(root).join("catenary");
    std::fs::create_dir_all(&dir).context("create config dir")?;
    std::fs::write(dir.join("config.toml"), contents).context("write config")?;
    Ok(())
}

/// Spawn `catenary daemon` under `root`, capturing its stderr to a file, and
/// wait for it to exit (bounded). Returns `(exit_success, stderr)`.
fn run_daemon_to_exit(root: &str, timeout: Duration) -> Result<(bool, String)> {
    let stderr_log = xdg_state_home(root).join("daemon_stderr.log");
    if let Some(parent) = stderr_log.parent() {
        std::fs::create_dir_all(parent).context("create state dir for stderr log")?;
    }
    let stderr_file = std::fs::File::create(&stderr_log).context("create daemon stderr log")?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file));

    let mut child = cmd.spawn().context("spawn daemon")?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().context("poll daemon")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon did not exit within {timeout:?} on the invalid-config boot path");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stderr = std::fs::read_to_string(&stderr_log).unwrap_or_default();
    Ok((status.success(), stderr))
}

/// A config the daemon cannot even parse: a torn TOML line. A parse error refuses
/// boot on *every* code path, so this test stays valid after the section-quarantine
/// change (which will let some section-scoped semantic errors boot on through) —
/// it exercises the generic post-bind-abort cleanup, not a config-specific arm.
const TORN_TOML: &str = "this is = = not valid toml\n";

#[test]
fn failed_boot_leaves_no_stranded_sockets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;
    write_user_config(root, TORN_TOML)?;

    let (success, stderr) = run_daemon_to_exit(root, Duration::from_secs(10))?;

    assert!(
        !success,
        "an invalid config must refuse the daemon boot (non-zero exit), stderr:\n{stderr}"
    );

    // The core of the fix: the boot bound the sockets first, then aborted — and
    // the boot-abort guard must have unlinked BOTH so nothing is stranded.
    let ipc = ipc_socket(root);
    let mcp = mcp_socket(root);
    assert!(
        !ipc.exists(),
        "failed boot must not strand the IPC socket at {}",
        ipc.display(),
    );
    assert!(
        !mcp.exists(),
        "failed boot must not strand the MCP socket at {}",
        mcp.display(),
    );
    Ok(())
}

/// Drive `catenary hook pre-tool` once under `root` with `CATENARY_NOTIFY_LOG`
/// pointed at `notify_log`, feeding a benign Bash payload. Returns the hook's
/// stdout (empty on allow).
fn run_hook(root: &str, notify_log: &Path) -> Result<String> {
    use std::io::Write as _;

    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": "true" },
    });

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    // The notify tally seam: records every notification intent, even with
    // `CATENARY_NOTIFY` off (so no real desktop is ever reached). Set AFTER
    // isolate_env, which clears all CATENARY_* vars.
    cmd.env("CATENARY_NOTIFY_LOG", notify_log);
    cmd.args(["hook", "pre-tool", "--format=claude"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn `hook pre-tool`")?;
    {
        let mut stdin = child.stdin.take().context("hook stdin")?;
        writeln!(stdin, "{payload}").context("write hook payload")?;
    }
    let out = child.wait_with_output().context("wait for hook")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Count the lines in the notify tally (each line is one fired notification), or
/// 0 when the file does not exist.
fn notify_count(notify_log: &Path) -> usize {
    std::fs::read_to_string(notify_log)
        .map_or(0, |s| s.lines().filter(|l| !l.trim().is_empty()).count())
}

/// The number of hook invocations the storm-shape test fires against one
/// unchanged stranded socket — pre-fix, each would fire its own interrupt.
const STORM_HOOKS: usize = 8;

#[test]
fn stranded_socket_notifies_exactly_once_across_hooks() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    // Simulate a stranded socket: a plain file where the IPC socket lives. It
    // `exists()` (so the hook does not take the quiet no-daemon arm) but
    // `UnixStream::connect` fails (it is not a listening socket) — exactly the
    // "socket exists, nobody listening" strand bug 111 describes.
    let ipc = ipc_socket(root);
    std::fs::create_dir_all(ipc.parent().expect("ipc parent")).context("create socket dir")?;
    std::fs::write(&ipc, b"not a socket").context("write stranded socket file")?;

    let notify_log = dir.path().join("notify_tally.log");

    // N hook invocations, each its own short-lived process — the storm shape.
    for _ in 0..STORM_HOOKS {
        run_hook(root, &notify_log)?;
    }

    let count = notify_count(&notify_log);
    assert_eq!(
        count, 1,
        "the storm must collapse to ONE notification across {STORM_HOOKS} hooks against an \
         unchanged stranded socket, got {count}",
    );

    // The onset stamp must be present, keyed to the stranded socket's identity.
    assert!(
        unreachable_stamp(root).exists(),
        "the first unreachable sighting must leave the onset stamp",
    );
    Ok(())
}

#[test]
fn a_new_strand_after_clear_re_notifies() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let ipc = ipc_socket(root);
    std::fs::create_dir_all(ipc.parent().expect("ipc parent")).context("create socket dir")?;
    std::fs::write(&ipc, b"not a socket").context("write stranded socket file")?;

    let notify_log = dir.path().join("notify_tally.log");

    // First strand: one notification.
    run_hook(root, &notify_log)?;
    run_hook(root, &notify_log)?;
    assert_eq!(
        notify_count(&notify_log),
        1,
        "first strand fires exactly once",
    );

    // A NEW strand: a fresh daemon bound and died, minting a new socket inode.
    // Remove and recreate the stranded file so its (inode, mtime) identity
    // differs — the mtime alone changes even if the inode is reused.
    std::fs::remove_file(&ipc).context("remove old strand")?;
    std::thread::sleep(Duration::from_millis(1100)); // ensure a distinct mtime second
    std::fs::write(&ipc, b"a different strand").context("write new strand")?;

    run_hook(root, &notify_log)?;
    run_hook(root, &notify_log)?;
    assert_eq!(
        notify_count(&notify_log),
        2,
        "a new socket identity is a fresh onset that earns its own single interrupt",
    );
    Ok(())
}
