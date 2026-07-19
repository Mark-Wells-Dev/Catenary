// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![cfg(unix)]
#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration pins for misc 203: a scoped `catenary diagnostics <path>`
//! ALWAYS serves the named file.
//!
//! The sighting: an edited file in an unmounted, **markerless** directory
//! (`~/.config/catenary/config.toml` — no repository marker, so
//! `enclosing_worktree_root` finds nothing) answered `outside every mounted
//! root` from the scoped serve, and the only escape was the manual
//! `catenary pin <dir>` → diagnose → `unpin` dance. The ruling: scoped
//! diagnostics is a diagnostics SERVICE — the explicitly named path is the
//! intent signal the ambient marker gate exists to demand, so the serve now
//! mounts the pin-shaped enclosing directory ephemerally and serves.
//!
//! Three pins, all against a real daemon + mockls over the same IPC the CLI
//! speaks:
//!
//! - **The refusal retires**: the sighting's exact geometry — a scoped
//!   diagnose naming a file in an unmounted markerless directory — serves the
//!   file's diagnostics.
//! - **The mount is ephemeral**: `tool/roots-ls` reports the auto-mounted
//!   directory in the `ephemeral` (expires-when-idle) class, never as a pin.
//! - **Covered in-root serves are untouched**: a scoped diagnose of a file
//!   inside the tracked root behaves exactly as before — served receipt, no
//!   out-of-scope line, and no ephemeral mount appears.

mod common;

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;

use common::{BridgeProcess, ipc_request, mockls_lsp_arg};

// The blessed event-discipline persona (diagnostics-debt 04c): the value
// doubles as the server key, the language, and the file extension.
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

/// The sighting's exact geometry: an edited file in an unmounted, markerless
/// directory. The scoped serve names it, mounts the containing directory
/// ephemerally (no repository marker required — the named path is the intent
/// signal), and serves its diagnostics instead of refusing.
#[test]
fn scoped_diagnose_serves_markerless_out_of_root_file() -> Result<()> {
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
        "the markerless refusal retired (misc 203) — the named path is served. Got: {text}"
    );
    assert!(
        !text.contains("[no LSP coverage]"),
        "the freshly mounted directory's server covers the file. Got: {text}"
    );
    assert!(
        text.contains("mock diagnostic"),
        "the scoped serve delivers real diagnostics for the named file. Got: {text}"
    );
    Ok(())
}

/// The automatic mount is ephemeral — the idle-expiry class the reaper tears
/// down — never a permanent pin. Asserted on the same surface the
/// pin-persistence pins use: the `tool/roots-ls` `ephemeral` flag.
#[test]
fn markerless_explicit_mount_is_ephemeral_not_pinned() -> Result<()> {
    let tracked = common::canonical_tempdir()?;
    let orphan = common::canonical_tempdir()?;
    let file = orphan.path().join(format!("config.{MOCK_LANG}"));
    std::fs::write(&file, "echo hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], tracked.path().to_str().context("root")?)?;
    bridge.initialize()?;

    let text = bridge.call_diagnostics(file.to_str().context("file path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "the serve succeeded (precondition for the mount assertion). Got: {text}"
    );

    let socket = bridge.wait_for_ipc_socket()?;
    let orphan_str = orphan.path().to_str().context("orphan path")?;
    let entry = roots_ls_entry(&socket, orphan_str)?
        .context("the auto-mounted markerless directory appears on the root board")?;
    assert_eq!(
        entry["ephemeral"].as_bool(),
        Some(true),
        "the misc-203 mount is ephemeral (expires when idle), not a pin: {entry}"
    );
    Ok(())
}

/// Regression pin: a scoped diagnose of a covered file INSIDE the tracked root
/// is untouched by misc 203 — the covered check short-circuits before any
/// root-shaping, so the receipt serves as before and no ephemeral mount
/// appears on the root board.
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
