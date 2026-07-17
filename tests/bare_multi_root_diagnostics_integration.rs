// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bug 121: bare `catenary diagnostics` answered `[no edited files]` while
//! another root held fresh booked debt.
//!
//! The sighting: from the main repo as cwd, ten covered edits booked into a
//! SECOND root's ledger (the edit seam books by the FILE's resolved lock root,
//! not the caller's cwd). A bare `catenary diagnostics` seconds later answered
//! `[no edited files]` — the bare enumeration read only the cwd's root ledger
//! (`due_files(resolve_lock_root(cwd))`), so debt one kitchen over was
//! invisible. A scoped run naming the same paths served them normally: the
//! debt was real, booked, and servable — the bare form didn't see it.
//!
//! These tests drive the REAL `catenary hook pre-tool` binary to book files at
//! the edit seam into a root that is NOT the caller's cwd root, then run the
//! bare serve against a LIVE daemon and assert the due set includes the other
//! kitchen's files — and that delivery pays that kitchen's ledger.

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{
    BridgeProcess, diagnostics_output, ipc_request_long, isolate_env, mockls_lsp_arg,
    xdg_config_home, xdg_state_home,
};

/// The blessed mock persona (extension doubles as the language id).
const MOCK_LANG: &str = "mockls-event";

/// A user config binding the mock language to a server so the hook-side
/// [`Booking`](catenary_cli::lock::Booking) books `.mockls-event` edits — the
/// same coverage the daemon gets from `CATENARY_SERVERS`. Written AFTER the
/// daemon spawns, so only the hook subprocesses read it.
const MOCKLS_BOOKING: &str = "\
[lsp.server.mockls]

[lsp.language.mockls-event]
extensions = [\"mockls-event\"]
servers = [\"mockls\"]
";

/// Write the user config the isolated `catenary` hook reads (`XDG_CONFIG_HOME`).
fn write_user_config(state_home: &str, contents: &str) -> Result<()> {
    let dir = xdg_config_home(state_home).join("catenary");
    std::fs::create_dir_all(&dir).context("create config dir")?;
    std::fs::write(dir.join("config.toml"), contents).context("write config")?;
    Ok(())
}

/// Drive `catenary hook pre-tool --format=claude` for an edit-tool `payload`
/// against `bridge`'s isolated env, so the booking lands in the durable ledger.
fn run_hook(bridge: &BridgeProcess, payload: &Value) -> Result<String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, bridge.state_home());
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

/// Book `file` at the edit seam via the `Edit` tool with the caller's `cwd`
/// pointing somewhere ELSE — the bug-121 shape: booking follows the file's
/// resolved lock root, never the cwd.
fn book_via_hook(bridge: &BridgeProcess, cwd: &Path, file: &Path) -> Result<()> {
    let out = run_hook(
        bridge,
        &json!({
            "cwd": cwd.to_str().context("cwd utf-8")?,
            "tool_name": "Edit",
            "tool_input": { "file_path": file.to_str().context("file utf-8")? },
        }),
    )?;
    anyhow::ensure!(
        !out.contains("\"deny\""),
        "the Edit booking hook must be admitted, got: {out}"
    );
    Ok(())
}

/// Runs the BARE `tool/editing-stop` serve with an explicit caller `cwd` —
/// exactly what the `catenary diagnostics` CLI sends for the bare form.
fn call_diagnostics_bare_from(bridge: &BridgeProcess, cwd: &Path) -> Result<String> {
    let socket_path = bridge.wait_for_ipc_socket()?;
    let request = json!({
        "method": "tool/editing-stop",
        "files": [],
        "cwd": cwd.to_str().context("cwd utf-8")?,
    });
    let text = ipc_request_long(&socket_path, bridge.daemon_pid(), &request)?;
    Ok(diagnostics_output(&text))
}

/// The due files in `root`'s ledger, read from the bridge's isolated state dir.
fn due_files(bridge: &BridgeProcess, root: &Path) -> Vec<PathBuf> {
    let locks_base = xdg_state_home(bridge.state_home()).join("locks");
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    catenary_cli::lock::due_files_in(&locks_base, &canonical)
}

/// A pair of sibling repos (each with a `.git` marker) under one canonical
/// tempdir, so two distinct lock roots resolve.
fn two_repos() -> Result<(tempfile::TempDir, PathBuf, PathBuf)> {
    let dir = common::canonical_tempdir()?;
    let root_a = dir.path().join("repo-a");
    let root_b = dir.path().join("repo-b");
    for root in [&root_a, &root_b] {
        std::fs::create_dir_all(root.join(".git"))?;
    }
    Ok((dir, root_a, root_b))
}

/// The bug-121 repro: covered edits book into root B's ledger while the caller
/// stands in root A. The BARE serve from root A must see root B's debt —
/// pre-fix it enumerated only the cwd's root and answered `[no edited files]`
/// (an empty receipt) — and delivery must pay root B's ledger.
#[test]
fn bare_run_serves_sibling_root_debt() -> Result<()> {
    let (_dir, root_a, root_b) = two_repos()?;
    let docs = root_b.join("docs");
    std::fs::create_dir_all(&docs)?;
    let first = docs.join(format!("guide.{MOCK_LANG}"));
    let second = docs.join(format!("notes.{MOCK_LANG}"));
    for file in [&first, &second] {
        std::fs::write(file, "echo content\n")?;
    }

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let roots = [
        root_a.to_str().context("root a utf-8")?,
        root_b.to_str().context("root b utf-8")?,
    ];
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &roots)?;
    bridge.initialize()?;
    bridge.wait_for_ipc_socket()?;
    write_user_config(bridge.state_home(), MOCKLS_BOOKING)?;

    // The sighting's shape: every hook event carries the FIRST root as cwd,
    // while the edited files live under the second root.
    book_via_hook(&bridge, &root_a, &first)?;
    book_via_hook(&bridge, &root_a, &second)?;

    let due_before = due_files(&bridge, &root_b);
    assert_eq!(
        due_before.len(),
        2,
        "both covered edits are booked into root B's ledger, got: {due_before:?}"
    );
    assert!(
        due_files(&bridge, &root_a).is_empty(),
        "root A's ledger stays empty — nothing was edited there"
    );

    // The BARE run from root A. Pre-fix: `[no edited files]` (an empty daemon
    // receipt) — root B's booked debt was invisible to the cwd-keyed
    // enumeration.
    let receipt = call_diagnostics_bare_from(&bridge, &root_a)?;
    assert!(
        receipt.contains("mock diagnostic"),
        "the bare run must serve root B's booked debt, got:\n{receipt}"
    );
    for name in ["guide", "notes"] {
        assert!(
            receipt.contains(name),
            "the bare receipt must name {name}, got:\n{receipt}"
        );
    }

    // Delivery pays the sibling kitchen: root B's ledger empties.
    let due_after = due_files(&bridge, &root_b);
    assert!(
        due_after.is_empty(),
        "the bare delivery must pay root B's ledger; survivors: {due_after:?}\nreceipt:\n{receipt}"
    );

    Ok(())
}

/// The honest answer for a cwd OUTSIDE any root while debt exists elsewhere
/// (bug 121 pin): the ledger, not the cwd, is the truth — the bare run still
/// serves the session's booked debt when its attribution is unambiguous (all
/// debt-holding kitchens share one owner).
#[test]
fn bare_run_outside_any_root_still_serves_the_debt() -> Result<()> {
    let (dir, root_a, root_b) = two_repos()?;
    // A scratch cwd with no repository marker: resolves to NO lock root.
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch)?;
    let file = root_b.join(format!("readme.{MOCK_LANG}"));
    std::fs::write(&file, "echo content\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let roots = [
        root_a.to_str().context("root a utf-8")?,
        root_b.to_str().context("root b utf-8")?,
    ];
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &roots)?;
    bridge.initialize()?;
    bridge.wait_for_ipc_socket()?;
    write_user_config(bridge.state_home(), MOCKLS_BOOKING)?;

    book_via_hook(&bridge, &scratch, &file)?;
    assert_eq!(
        due_files(&bridge, &root_b).len(),
        1,
        "the covered edit books into root B's ledger"
    );

    let receipt = call_diagnostics_bare_from(&bridge, &scratch)?;
    assert!(
        receipt.contains("readme") && receipt.contains("mock diagnostic"),
        "a bare run from outside any root serves the session's booked debt, got:\n{receipt}"
    );
    assert!(
        due_files(&bridge, &root_b).is_empty(),
        "delivery pays root B's ledger"
    );

    Ok(())
}
