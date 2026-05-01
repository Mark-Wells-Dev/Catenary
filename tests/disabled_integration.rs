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
fn tools_list_empty_when_disabled() -> Result<()> {
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
    let tools = response["result"]["tools"]
        .as_array()
        .ok_or_else(|| anyhow!("tools should be an array: {response:?}"))?;
    assert!(tools.is_empty(), "disabled session should list no tools");

    Ok(())
}

#[test]
fn enabled_true_is_default() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().ok_or_else(|| anyhow!("dir path"))?;

    // No .catenary.toml at all — should behave normally.
    let mut bridge = BridgeProcess::spawn(&[], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    let tools = response["result"]["tools"]
        .as_array()
        .ok_or_else(|| anyhow!("tools should be an array: {response:?}"))?;
    assert!(
        !tools.is_empty(),
        "default session should list tools, got empty"
    );

    Ok(())
}
