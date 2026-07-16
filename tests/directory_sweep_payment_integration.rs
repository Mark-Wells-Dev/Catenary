// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Bug 120: a directory-form sweep served a file clean, but its ledger entry
//! survived delivery.
//!
//! The sighting: `catenary diagnostics <dir> <file> …` answered "N files clean"
//! (the directory expanded inside the pipeline), yet the next Stop blocked,
//! naming a file the sweep had verifiably served. The delivery unlink pays the
//! **named** paths only — a directory argument has no ledger leaf of its own,
//! and the files its expansion served never reach the unlink — so every due
//! file served via a directory argument survived delivery as phantom debt.
//!
//! This drives the REAL `catenary hook pre-tool` binary to book files at the
//! edit seam (a `Write`, then an `Edit` — the sighting's twice-booked shape —
//! plus once-booked siblings), runs the directory-form sweep against a LIVE
//! daemon with a real (mock) language server, and then inspects the on-disk
//! ledger directly: every served file's touch leaf must be unlinked.

mod common;

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, isolate_env, mockls_lsp_arg, xdg_config_home, xdg_state_home};

/// The blessed mock persona (extension doubles as the language id).
const MOCK_LANG: &str = "mockls-event";

/// A user config binding the mock language to a server so the hook-side
/// [`Booking`](catenary_cli::lock::Booking) books `.mockls-event` edits — the
/// same coverage the daemon gets from `CATENARY_SERVERS`. Written AFTER the
/// daemon spawns, so only the hook subprocesses read it (the daemon's server
/// set comes from the env override).
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
/// against `bridge`'s isolated env, so the booking lands in the daemon's ledger.
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

/// Book `file` at the edit seam via the named edit `tool` (`Write` / `Edit`).
fn book_via_hook(bridge: &BridgeProcess, tool: &str, file: &str) -> Result<()> {
    let out = run_hook(
        bridge,
        &json!({
            "cwd": null,
            "tool_name": tool,
            "tool_input": { "file_path": file },
        }),
    )?;
    anyhow::ensure!(
        !out.contains("\"deny\""),
        "the {tool} booking hook must be admitted, got: {out}"
    );
    Ok(())
}

/// The due files in `root`'s ledger, read from the bridge's isolated state dir.
fn due_files(bridge: &BridgeProcess, root: &std::path::Path) -> Vec<PathBuf> {
    let locks_base = xdg_state_home(bridge.state_home()).join("locks");
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    catenary_cli::lock::due_files_in(&locks_base, &canonical)
}

/// The bug-120 repro: book covered files (one of them TWICE — Write then Edit,
/// the sighting's distinguishing shape), sweep them with the directory form
/// (`catenary diagnostics <dir> <dir> <file>`), and assert every served file's
/// ledger entry is unlinked on delivery.
#[test]
fn directory_sweep_unlinks_every_served_ledger_entry() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let root = dir.path();
    // A repo marker so the lock root resolves for booking and delivery alike.
    std::fs::create_dir_all(root.join(".git"))?;
    let flow_dir = root.join("tickets").join("worktree-flow");
    let bugs_dir = root.join("bugs");
    std::fs::create_dir_all(&flow_dir)?;
    std::fs::create_dir_all(&bugs_dir)?;

    let twice_booked = flow_dir.join(format!("01_merge.{MOCK_LANG}"));
    let sibling = flow_dir.join(format!("02_sibling.{MOCK_LANG}"));
    let bug_note = bugs_dir.join(format!("115.{MOCK_LANG}"));
    let readme = root.join(format!("readme.{MOCK_LANG}"));
    for file in [&twice_booked, &sibling, &bug_note, &readme] {
        std::fs::write(file, "echo content\n")?;
    }

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let root_str = root.to_str().context("root path utf-8")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_str)?;
    bridge.initialize()?;
    bridge.wait_for_ipc_socket()?;
    write_user_config(bridge.state_home(), MOCKLS_BOOKING)?;

    // Book the target TWICE within the daemon's lifetime — the Write creating
    // it, then a frontmatter Edit (the sighting's shape) — and each companion
    // once.
    let path_str =
        |p: &PathBuf| -> Result<String> { Ok(p.to_str().context("file path utf-8")?.to_string()) };
    book_via_hook(&bridge, "Write", &path_str(&twice_booked)?)?;
    book_via_hook(&bridge, "Edit", &path_str(&twice_booked)?)?;
    book_via_hook(&bridge, "Edit", &path_str(&sibling)?)?;
    book_via_hook(&bridge, "Edit", &path_str(&bug_note)?)?;
    book_via_hook(&bridge, "Edit", &path_str(&readme)?)?;

    let due_before = due_files(&bridge, root);
    assert_eq!(
        due_before.len(),
        4,
        "all four covered files are booked before the sweep, got: {due_before:?}"
    );

    // The directory-form sweep from the sighting: two directories plus a
    // directly-named file, covering all four booked files.
    let receipt = bridge.call_diagnostics_scoped(&[
        flow_dir.to_str().context("flow dir utf-8")?,
        bugs_dir.to_str().context("bugs dir utf-8")?,
        readme.to_str().context("readme utf-8")?,
    ])?;
    assert!(
        receipt.contains("mock diagnostic"),
        "the sweep must actually serve the expanded files, got:\n{receipt}"
    );
    for name in ["01_merge", "02_sibling", "115", "readme"] {
        assert!(
            receipt.contains(name),
            "the sweep receipt must name {name}, got:\n{receipt}"
        );
    }

    // The bug-120 invariant: delivery pays every served file — the ledger holds
    // no entry for any file the sweep served, whether it was named directly or
    // reached through a directory argument, booked once or twice.
    let due_after = due_files(&bridge, root);
    assert!(
        due_after.is_empty(),
        "every served file's ledger entry must be unlinked on delivery; \
         survivors: {due_after:?}\nreceipt:\n{receipt}"
    );

    Ok(())
}
