// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Ensures that the Cargo.toml version matches the Claude plugin version.
//! This runs as part of `cargo test`, which is triggered by cargo-husky on push.

use anyhow::{Context, Result};

#[test]
fn test_version_sync() -> Result<()> {
    // 1. Get Cargo version
    let cargo_toml = std::fs::read_to_string("Cargo.toml").context("Failed to read Cargo.toml")?;
    let cargo_table: toml::Table = cargo_toml.parse().context("Failed to parse Cargo.toml")?;
    let cargo_version = cargo_table["package"]["version"]
        .as_str()
        .context("Failed to get version from Cargo.toml")?;

    // 2. Get Plugin version
    let plugin_json = std::fs::read_to_string(".claude-plugin/marketplace.json")
        .context("Failed to read marketplace.json")?;
    let plugin_data: serde_json::Value =
        serde_json::from_str(&plugin_json).context("Failed to parse marketplace.json")?;
    let plugin_version = plugin_data["plugins"][0]["version"]
        .as_str()
        .context("Failed to get version from marketplace.json")?;

    // 3. Compare
    assert_eq!(
        cargo_version, plugin_version,
        "Version mismatch! Cargo.toml: {cargo_version}, marketplace.json: {plugin_version}"
    );
    Ok(())
}

#[test]
fn test_cargo_lock_freshness() -> Result<()> {
    // Run 'cargo check --locked' to ensure Cargo.lock is in sync with Cargo.toml
    // --locked will fail if Cargo.lock needs updating.
    let status = std::process::Command::new("cargo")
        .args(["check", "--locked"])
        .status()
        .context("Failed to execute cargo check")?;

    assert!(
        status.success(),
        "Cargo.lock is out of sync with Cargo.toml! Please run 'cargo check' and stage the changes."
    );
    Ok(())
}
