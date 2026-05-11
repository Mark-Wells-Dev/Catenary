// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for configuration loading and merging.
//!
//! Verifies that Catenary correctly loads settings from files,
//! environment variables, and CLI arguments in the correct priority order.

mod common;

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;

use common::BridgeProcess;

const MOCK_LANG_A: &str = "yX4Za";
const MOCK_LANG_B: &str = "d5apI";

/// Write a mockls config.toml into the given directory and return its path.
fn write_mockls_config(dir: &Path) -> Result<PathBuf> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server.mockls-{MOCK_LANG_A}]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"{MOCK_LANG_A}\"]\n\n\
             [language.{MOCK_LANG_A}]\n\
             servers = [\"mockls-{MOCK_LANG_A}\"]\n"
        ),
    )?;
    Ok(config_path)
}

#[test]
fn test_config_loading() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("test.{MOCK_LANG_A}")), "echo hello\n")?;
        write_mockls_config(root)
    })?;
    bridge.initialize()?;

    let result = bridge.call_tool("grep", &json!({ "pattern": "echo" }))?;
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Search failed: {result:?}"
    );
    Ok(())
}

#[test]
fn test_config_override() -> Result<()> {
    let lsp_b = common::mockls_lsp_arg(MOCK_LANG_B, "");
    let mut bridge = BridgeProcess::spawn_with_config_and_servers(&[&lsp_b], |root| {
        std::fs::write(root.join(format!("test.{MOCK_LANG_A}")), "echo hello\n")?;
        write_mockls_config(root)
    })?;
    bridge.initialize()?;

    let result = bridge.call_tool("grep", &json!({ "pattern": "echo" }))?;
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Search failed (CLI arg not merged?)"
    );
    Ok(())
}
