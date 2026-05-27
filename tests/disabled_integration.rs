// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for `lsp = false` in `.catenary.toml`.
//!
//! When the primary workspace root has `lsp = false`, the session
//! is disabled: no tools, no servers, no hooks, no database writes.

mod common;

use anyhow::{Result, anyhow};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

use common::BridgeProcess;

/// Spawn a bridge whose primary workspace root has `lsp = false`.
fn spawn_disabled_bridge(root: &str) -> Result<BridgeProcess> {
    fs::write(PathBuf::from(root).join(".catenary.toml"), "lsp = false\n")?;
    BridgeProcess::spawn(&[], root)
}

#[test]
fn tools_list_returns_method_not_found_when_disabled() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().ok_or_else(|| anyhow!("dir path"))?;
    let mut bridge = spawn_disabled_bridge(root)?;

    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("error").is_some(),
        "tools/list should return error (method not found): {response:?}"
    );

    Ok(())
}

#[test]
fn tools_list_returns_method_not_found_when_enabled() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().ok_or_else(|| anyhow!("dir path"))?;

    // No .catenary.toml at all — should behave normally.
    // MCP no longer serves application tools, so tools/list returns
    // method-not-found regardless of enabled/disabled state.
    let mut bridge = BridgeProcess::spawn(&[], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("error").is_some(),
        "tools/list should return error (method not found): {response:?}"
    );

    Ok(())
}
