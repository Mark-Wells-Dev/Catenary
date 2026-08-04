// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![cfg(unix)]
#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration pins for brackets 03: the markerless explicit-target serve
//! rides the rootless single-file tier, and stray-file debt is per-file.
//!
//! Misc 203 ruled that a scoped `catenary diagnostics <path>` ALWAYS serves
//! the named file; its implementation manufactured an ephemeral root (the
//! pin-shaped enclosing directory — with the `$HOME` wide-root edge). Brackets
//! 03 retires that mount: the serve now rides `with_single_file_bracket` on
//! the `DebtPayment` lane (didOpen → collect → answer → didClose inside the
//! bracket), tags its receipt line `[single-file]`, and books stray-file debt
//! against the FILE, not a root.
//!
//! Pins, all against a real daemon + mockls over the same IPC the CLI speaks
//! (`mockls-event` carries the verified `serves-diagnostics` capability):
//!
//! - **The rootless serve answers**: the misc-203 geometry — a scoped
//!   diagnose naming a file in an unmounted markerless directory — serves the
//!   file's diagnostics with the `[single-file]` tag, and the root board is
//!   UNCHANGED (the retired mount never appears).
//! - **The `$HOME`-shaped edge stops existing**: a file directly under a
//!   wide top-level directory serves without mounting that directory.
//! - **The honest disclosure**: an `enrichment-only` language still answers
//!   for the named file, naming the server that cannot serve.
//! - **The end-to-end debt loop (the ticket's gold)**: a hook-tracked edit to
//!   a stray file books per-file debt, the Stop gate names it, and a scoped
//!   diagnose through the rootless serve pays it.
//! - **Covered in-root serves are untouched.**

mod common;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::json;

use common::{BridgeProcess, ipc_request, mockls_lsp_arg};

// The blessed event-discipline persona (diagnostics-debt 04c) — carries the
// verified `single_file = "serves-diagnostics"` capability (brackets 01). The
// value doubles as the server key, the language, and the file extension.
const MOCK_LANG: &str = "mockls-event";

/// Returns the `tool/roots-ls` entry for `path`, if tracked.
fn roots_ls_entry(socket: &Path, path: &str) -> Result<Option<serde_json::Value>> {
    let resp = ipc_request(socket, &json!({ "method": "tool/roots-ls" }))?;
    let roots: serde_json::Value = serde_json::from_str(resp.trim()).context("roots-ls json")?;
    Ok(roots["roots"]
        .as_array()
        .and_then(|arr| arr.iter().find(|e| e["path"].as_str() == Some(path)))
        .cloned())
}

/// Drives the real `catenary hook pre-tool` binary for a Claude `Edit` of
/// `file`, against this test's daemon state — the edit seam that books the
/// per-file ledger (hook-process-local) and arms the daemon-side gate.
fn run_hook_edit(bridge: &BridgeProcess, file: &Path, session: &str) -> Result<String> {
    let cwd = file.parent().context("edit target parent")?;
    let payload = json!({
        "tool_name": "Edit",
        "tool_input": { "file_path": file.to_string_lossy() },
        "session_id": session,
        "cwd": cwd.to_string_lossy(),
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    common::isolate_env(&mut cmd, bridge.state_home());
    // The hook's static booking gate reads the merged config: the mockls
    // binding must be visible to the hook process exactly as the daemon
    // sees it (isolate_env cleared the inherited value).
    cmd.env("CATENARY_SERVERS", mockls_lsp_arg(MOCK_LANG, ""));
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

/// The isolated on-disk lock base the hook and daemon share:
/// `<CATENARY_STATE_DIR>/locks` (`crate::lock::locks_dir`).
fn locks_base(bridge: &BridgeProcess) -> std::path::PathBuf {
    common::xdg_state_home(bridge.state_home()).join("locks")
}

/// Counts `.lock` ledger leaves under every lock dir at `base`.
fn ledger_leaves(base: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(base) else {
        return out;
    };
    for dir in dirs.flatten() {
        let ledger = dir.path().join("dir");
        let Ok(entries) = std::fs::read_dir(&ledger) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|e| e == "lock") {
                out.push(entry.path());
            }
        }
    }
    out
}

/// The misc-203 geometry, re-served through the rootless tier: a scoped
/// diagnose naming a file in an unmounted, markerless directory answers with
/// the file's real diagnostics, tags the mode, and manufactures NO ephemeral
/// root — the root board is unchanged after the serve.
#[test]
fn markerless_scoped_serve_rides_the_rootless_singleton() -> Result<()> {
    // The daemon's tracked root: an unrelated directory.
    let tracked = common::canonical_tempdir()?;
    // The markerless directory: a bare tempdir — no `.git`/`.svn`/`.hg`/`.jj`
    // anywhere above the file inside the temp base.
    let orphan = common::canonical_tempdir()?;
    let file = orphan.path().join(format!("config.{MOCK_LANG}"));
    std::fs::write(&file, "echo hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], tracked.path().to_str().context("root")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("file path")?)?;

    assert!(
        !text.contains("outside every mounted root"),
        "the markerless refusal stays retired (misc 203) — the named path is served. Got: {text}"
    );
    assert!(
        text.contains("mock diagnostic"),
        "the rootless serve delivers real diagnostics for the named file. Got: {text}"
    );
    assert!(
        text.contains("[single-file]"),
        "the serve names its mode on the receipt line (brackets 03). Got: {text}"
    );

    // The retired mount never happens: the orphan directory is NOT on the
    // root board, and the tracked root is still there — the set is unchanged.
    let socket = bridge.wait_for_ipc_socket()?;
    let orphan_str = orphan.path().to_str().context("orphan path")?;
    assert!(
        roots_ls_entry(&socket, orphan_str)?.is_none(),
        "a markerless explicit target manufactures no ephemeral root (brackets 03)"
    );
    let tracked_str = tracked.path().to_str().context("tracked path")?;
    assert!(
        roots_ls_entry(&socket, tracked_str)?.is_some(),
        "the tracked root survives the serve untouched"
    );
    Ok(())
}

/// The `$HOME` wide-root edge stops existing: a file directly under a wide
/// top-level directory (the home shape — the old fallback mounted the
/// directory ITSELF as a root) serves through the singleton with that
/// directory never appearing on the root board.
#[test]
fn home_shaped_file_serves_without_mounting_the_home_dir() -> Result<()> {
    let tracked = common::canonical_tempdir()?;
    // The "home": a wide top-level dir with unrelated content beside the file.
    let home = common::canonical_tempdir()?;
    std::fs::create_dir_all(home.path().join("Documents"))?;
    std::fs::create_dir_all(home.path().join(".config"))?;
    let file = home.path().join(format!(".profile.{MOCK_LANG}"));
    std::fs::write(&file, "echo hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], tracked.path().to_str().context("root")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("file path")?)?;
    assert!(
        text.contains("mock diagnostic") && text.contains("[single-file]"),
        "the home-shaped stray serves through the singleton. Got: {text}"
    );

    let socket = bridge.wait_for_ipc_socket()?;
    let home_str = home.path().to_str().context("home path")?;
    assert!(
        roots_ls_entry(&socket, home_str)?.is_none(),
        "the wide 'home' directory is never mounted as a root (brackets 03)"
    );
    Ok(())
}

/// An `enrichment-only` capability still ANSWERS for the named file: the
/// receipt names the file with the honest disclosure that no server serves
/// single-file diagnostics for it — never a refusal, never a mount. (lattice
/// is the verified-negative row, brackets 06; the server binary is never
/// spawned, so mockls stands in as the configured command.)
#[test]
fn enrichment_only_language_answers_with_the_disclosure() -> Result<()> {
    let tracked = common::canonical_tempdir()?;
    let orphan = common::canonical_tempdir()?;
    let file = orphan.path().join("note.lattice");
    std::fs::write(&file, "# stray note\n")?;

    // Bind the `lattice` server key to the mock binary for the `lattice`
    // extension: the capability gate consults the manifest row for the KEY
    // (`enrichment-only`), so the serve must refuse the singleton and
    // disclose — the binary behind the key is irrelevant (never spawned).
    let lsp = mockls_lsp_arg("lattice", "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], tracked.path().to_str().context("root")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("file path")?)?;
    assert!(
        text.contains("[single-file]")
            && text.contains("no server serves single-file diagnostics")
            && text.contains("lattice"),
        "an enrichment-only language answers with the named disclosure. Got: {text}"
    );
    assert!(
        !text.contains("outside every mounted root"),
        "the named file is answered, never refused (misc 203 ruling). Got: {text}"
    );

    let socket = bridge.wait_for_ipc_socket()?;
    let orphan_str = orphan.path().to_str().context("orphan path")?;
    assert!(
        roots_ls_entry(&socket, orphan_str)?.is_none(),
        "the disclosure path mounts nothing either"
    );
    Ok(())
}

/// The ticket's gold: the whole per-file debt loop. A hook-tracked edit to a
/// stray TOML-class file (mockls-event stands in, `serves-diagnostics`) arms
/// the gate and books debt against the FILE; the Stop gate names the unpaid
/// stray; a scoped `catenary diagnostics <that file>` serves it through the
/// rootless singleton and pays the ledger; the next Stop passes.
#[test]
fn stray_edit_books_per_file_debt_and_the_rootless_serve_pays_it() -> Result<()> {
    let tracked = common::canonical_tempdir()?;
    let orphan = common::canonical_tempdir()?;
    let stray = orphan.path().join(format!("stray.{MOCK_LANG}"));
    std::fs::write(&stray, "echo hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], tracked.path().to_str().context("root")?)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // 1. The edit: the real hook binary books the per-file ledger
    //    (hook-process-local) and arms the daemon-side gate over IPC.
    let hook_out = run_hook_edit(&bridge, &stray, "sess-loop")?;
    assert!(
        !hook_out.contains("deny"),
        "a lone agent's stray edit is admitted. Got: {hook_out}"
    );
    // The admitted edit then RUNS (misc 230): the booking tracked the stray's
    // pre-write bytes, and debt is asserted at consult only once they moved.
    std::fs::write(&stray, "echo edited\n")?;

    // 2. Per-file booking: the FILE's own ledger leaf exists — and it names
    //    the file, not a root-relative mirror.
    let leaves = ledger_leaves(&locks_base(&bridge));
    assert_eq!(leaves.len(), 1, "exactly one booked leaf: {leaves:?}");
    let leaf_name = leaves[0]
        .file_name()
        .and_then(|n| n.to_str())
        .context("leaf name")?;
    assert_eq!(
        leaf_name,
        format!("stray.{MOCK_LANG}.lock"),
        "the ledger books the stray FILE by name (per-file scope)"
    );

    // 3. The Stop gate names the unpaid stray like any other debt.
    let stop = ipc_request(
        &socket,
        &json!({
            "method": "post-agent/require-release",
            "session_id": "sess-loop",
            "agent_id": "",
            "stop_hook_active": false,
        }),
    )?;
    assert!(
        stop.contains("block"),
        "the Stop gate blocks on the unpaid stray file. Got: {stop}"
    );
    assert!(
        stop.contains(&format!("stray.{MOCK_LANG}")),
        "the block names the owed file. Got: {stop}"
    );

    // 4. Payment: a scoped diagnose naming the file serves it through the
    //    rootless singleton (the daemon-level tier-3 serve, previously
    //    unreachable behind the misc-203 mount) and unlinks the leaf.
    let receipt = bridge.call_diagnostics(stray.to_str().context("stray path")?)?;
    assert!(
        receipt.contains("mock diagnostic") && receipt.contains("[single-file]"),
        "the rootless serve answers with real diagnostics. Got: {receipt}"
    );
    assert!(
        ledger_leaves(&locks_base(&bridge)).is_empty(),
        "delivery pays the per-file debt — the ledger leaf unlinks"
    );

    // No mount rode along: the orphan dir never joined the root board.
    let orphan_str = orphan.path().to_str().context("orphan path")?;
    assert!(
        roots_ls_entry(&socket, orphan_str)?.is_none(),
        "the debt loop runs with zero root collateral"
    );

    // 5. The next Stop passes — the debt is paid.
    let stop_after = ipc_request(
        &socket,
        &json!({
            "method": "post-agent/require-release",
            "session_id": "sess-loop",
            "agent_id": "",
            "stop_hook_active": false,
        }),
    )?;
    assert!(
        !stop_after.contains("block"),
        "a paid stray file no longer blocks the Stop. Got: {stop_after}"
    );
    Ok(())
}

/// Regression pin: a scoped diagnose of a covered file INSIDE the tracked root
/// is untouched by brackets 03 — served exactly as before, no `[single-file]`
/// tag, no out-of-scope line, and the tracked root keeps its class.
#[test]
fn scoped_diagnose_in_root_is_unchanged() -> Result<()> {
    let root = common::canonical_tempdir()?;
    let file = root.path().join(format!("code.{MOCK_LANG}"));
    std::fs::write(&file, "echo hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root.path().to_str().context("root")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("file path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "the in-root scoped serve delivers diagnostics exactly as before. Got: {text}"
    );
    assert!(
        !text.contains("[single-file]"),
        "an in-root serve is root-scoped — the mode tag never appears. Got: {text}"
    );
    assert!(
        !text.contains("outside every mounted root") && !text.contains("path does not exist"),
        "no out-of-scope line for a covered in-root file. Got: {text}"
    );

    // No ephemeral contributor appears for the covered root: the serve reused
    // the tracked mount rather than converting anything.
    let socket = bridge.wait_for_ipc_socket()?;
    let root_str = root.path().to_str().context("root path")?;
    let entry = roots_ls_entry(&socket, root_str)?.context("the tracked root is on the board")?;
    assert_eq!(
        entry["ephemeral"].as_bool(),
        Some(false),
        "the tracked root stays in its pinned-class, no ephemeral conversion: {entry}"
    );
    Ok(())
}
