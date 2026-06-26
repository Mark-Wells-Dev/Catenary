// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the per-root feeder toggles in `.catenary.toml`
//! (workstream 34 ticket 00).
//!
//! The old coarse `lsp = false` kill switch — which disabled the whole
//! session — is gone. A root with `[lsp] disable = true` is still tracked and
//! the daemon serves normally; the toggle only drops the LSP feeder for that
//! root. These tests assert the daemon is **not** wedged or whole-disabled by
//! either the new toggle or a leftover (now-removed) bare `lsp` key.

mod common;

use anyhow::{Result, anyhow};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

use common::BridgeProcess;

/// Drive the MCP handshake, then assert `tools/list` returns method-not-found.
///
/// MCP is a pure heartbeat — it advertises no application tools — so a healthy
/// daemon returns an error here regardless of feeder toggles. A wedged or
/// whole-session-disabled daemon would instead fail the handshake or hang.
fn assert_serves_but_no_tools(bridge: &mut BridgeProcess) -> Result<()> {
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("error").is_some(),
        "tools/list should return method-not-found: {response:?}"
    );
    Ok(())
}

#[test]
fn disable_lsp_root_still_serves() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().ok_or_else(|| anyhow!("dir path"))?;
    fs::write(
        PathBuf::from(root).join(".catenary.toml"),
        "[lsp]\ndisable = true\n",
    )?;

    // [lsp] disable drops the LSP feeder for the root but does not disable the
    // session — the daemon completes the handshake and answers normally.
    let mut bridge = BridgeProcess::spawn(&[], root)?;
    assert_serves_but_no_tools(&mut bridge)
}

#[test]
fn removed_lsp_key_does_not_wedge_daemon() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().ok_or_else(|| anyhow!("dir path"))?;
    // The `lsp` key was removed in 2.0: loading this config errors, the root is
    // logged-and-skipped, and the daemon keeps serving rather than wedging.
    fs::write(PathBuf::from(root).join(".catenary.toml"), "lsp = false\n")?;

    let mut bridge = BridgeProcess::spawn(&[], root)?;
    assert_serves_but_no_tools(&mut bridge)
}

#[test]
fn no_project_config_serves_normally() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().ok_or_else(|| anyhow!("dir path"))?;

    // No .catenary.toml at all — baseline healthy daemon.
    let mut bridge = BridgeProcess::spawn(&[], root)?;
    assert_serves_but_no_tools(&mut bridge)
}
