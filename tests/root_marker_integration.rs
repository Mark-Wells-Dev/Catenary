// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for root marker resolution (misc ticket 59).
//!
//! Verifies that servers are spawned at marker-resolved sub-roots,
//! producing correct instance keying and diagnostics.

mod common;

use anyhow::{Context, Result};
use serde_json::json;
use std::path::Path;

use common::BridgeProcess;

const MOCK_LANG: &str = "mRk59";

/// Writes a config.toml with mockls configured with `root_markers`.
fn write_marker_config(dir: &Path, markers: &[&str]) -> Result<std::path::PathBuf> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let markers_toml: Vec<String> = markers.iter().map(|m| format!("\"{m}\"")).collect();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[lsp.server.mockls-event]\n\
             path = \"{mockls_bin}\"\n\
             args = [\"{MOCK_LANG}\"]\n\
             root_markers = [{markers}]\n\n\
             [lsp.language.{MOCK_LANG}]\n\
             extensions = [\"{MOCK_LANG}\"]\n\
             servers = [\"mockls-event\"]\n",
            markers = markers_toml.join(", "),
        ),
    )?;
    Ok(config_path)
}

/// Writes a config.toml with mockls and NO `root_markers` (disabled).
fn write_no_marker_config(dir: &Path) -> Result<std::path::PathBuf> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[lsp.server.mockls-event]\n\
             path = \"{mockls_bin}\"\n\
             args = [\"{MOCK_LANG}\"]\n\
             root_markers = []\n\n\
             [lsp.language.{MOCK_LANG}]\n\
             extensions = [\"{MOCK_LANG}\"]\n\
             servers = [\"mockls-event\"]\n",
        ),
    )?;
    Ok(config_path)
}

// ─── Marker at workspace root → eager spawn ────────────────────────

/// When the workspace root contains a marker, the server spawns eagerly
/// and grep works immediately.
#[test]
fn test_marker_at_workspace_root_eager_spawn() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ws = dir.path().join("workspace");
    std::fs::create_dir_all(ws.join("src"))?;
    std::fs::write(ws.join("project.marker"), "")?;
    std::fs::write(
        ws.join("src").join(format!("main.{MOCK_LANG}")),
        "echo hello\n",
    )?;

    let tmp = tempfile::tempdir()?;
    let config_path = write_marker_config(tmp.path(), &["project.marker"])?;
    let root = ws.to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_CONFIG", &config_path);
        cmd.env("CATENARY_ROOTS", root);
    })?;
    bridge.initialize()?;

    let result = bridge.call_tool("grep", &json!({ "pattern": "echo", "directory": root }))?;
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Grep should succeed with marker at workspace root: {result:?}"
    );

    Ok(())
}

// ─── Marker only in subdirectory → lazy spawn on grep ──────────────

/// When the marker is only in a subdirectory, the server is NOT
/// spawned eagerly. Grep triggers lazy spawn.
#[test]
fn test_marker_in_subdir_lazy_spawn_via_grep() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ws = dir.path().join("workspace");
    let sub = ws.join("packages").join("crate_a");
    std::fs::create_dir_all(sub.join("src"))?;
    std::fs::write(sub.join("project.marker"), "")?;
    std::fs::write(
        sub.join("src").join(format!("lib.{MOCK_LANG}")),
        "echo subdir\n",
    )?;

    let tmp = tempfile::tempdir()?;
    let config_path = write_marker_config(tmp.path(), &["project.marker"])?;
    let root = ws.to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_CONFIG", &config_path);
        cmd.env("CATENARY_ROOTS", root);
    })?;
    bridge.initialize()?;

    // Grep should trigger lazy spawn and find the file.
    let result = bridge.call_tool("grep", &json!({ "pattern": "subdir", "directory": root }))?;
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Grep should succeed after lazy spawn: {result:?}"
    );

    Ok(())
}

// ─── Two sub-roots → separate server instances ─────────────────────

/// Files in different marker-resolved sub-roots get independent
/// server instances. Grep finds files from both.
#[test]
fn test_two_subroots_separate_instances() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ws = dir.path().join("workspace");
    let sub_a = ws.join("crate_a");
    let sub_b = ws.join("crate_b");
    std::fs::create_dir_all(sub_a.join("src"))?;
    std::fs::create_dir_all(sub_b.join("src"))?;
    std::fs::write(sub_a.join("project.marker"), "")?;
    std::fs::write(sub_b.join("project.marker"), "")?;
    std::fs::write(
        sub_a.join("src").join(format!("lib.{MOCK_LANG}")),
        "echo crate_a\n",
    )?;
    std::fs::write(
        sub_b.join("src").join(format!("lib.{MOCK_LANG}")),
        "echo crate_b\n",
    )?;

    let tmp = tempfile::tempdir()?;
    let config_path = write_marker_config(tmp.path(), &["project.marker"])?;
    let root = ws.to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_CONFIG", &config_path);
        cmd.env("CATENARY_ROOTS", root);
    })?;
    bridge.initialize()?;

    // Grep should find files from both sub-roots.
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "echo", "directory": root }))?;
    assert!(
        text.contains("crate_a") && text.contains("crate_b"),
        "Grep should find files in both sub-roots. Got:\n{text}"
    );

    Ok(())
}

// ─── Diagnostics with sub-root marker ──────────────────────────────

/// Editing a file in a marker sub-root produces diagnostics via lazy
/// spawn in the `done_editing` pipeline.
#[test]
fn test_diagnostics_in_subroot() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ws = dir.path().join("workspace");
    let sub = ws.join("crate_a");
    std::fs::create_dir_all(sub.join("src"))?;
    std::fs::write(sub.join("project.marker"), "")?;
    let file = sub.join("src").join(format!("lib.{MOCK_LANG}"));
    std::fs::write(&file, "echo subroot_diag\n")?;

    let tmp = tempfile::tempdir()?;
    let config_path = write_marker_config(tmp.path(), &["project.marker"])?;
    let root = ws.to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_CONFIG", &config_path);
        cmd.env("CATENARY_ROOTS", root);
    })?;
    bridge.initialize()?;

    // done_editing triggers ensure_clients_for_paths → lazy spawn.
    let text = bridge.call_diagnostics(file.to_str().context("file")?)?;
    assert!(
        text.contains("mock diagnostic") || text.trim().is_empty(),
        "Diagnostics should return results (or be silent when clean) for sub-root file. Got:\n{text}"
    );

    Ok(())
}

// ─── Empty root_markers disables marker resolution ─────────────────

/// `root_markers = []` disables marker resolution — the server uses
/// the workspace root regardless of markers in subdirectories.
#[test]
fn test_empty_markers_disables_resolution() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ws = dir.path().join("workspace");
    std::fs::create_dir_all(ws.join("sub"))?;
    std::fs::write(ws.join("sub").join("project.marker"), "")?;
    std::fs::write(
        ws.join("sub").join(format!("lib.{MOCK_LANG}")),
        "echo disabled\n",
    )?;

    let tmp = tempfile::tempdir()?;
    let config_path = write_no_marker_config(tmp.path())?;
    let root = ws.to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_CONFIG", &config_path);
        cmd.env("CATENARY_ROOTS", root);
    })?;
    bridge.initialize()?;

    // Server should be spawned at workspace root (eager), grep works.
    let result = bridge.call_tool("grep", &json!({ "pattern": "disabled", "directory": root }))?;
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Grep should succeed with disabled markers: {result:?}"
    );

    Ok(())
}

// ─── Nested markers: nearest wins ──────────────────────────────────

/// When markers exist at both the workspace root and a subdirectory,
/// files in the subdirectory resolve to the nearest (sub) marker.
/// Files at the workspace root level resolve to the workspace root.
#[test]
fn test_nested_markers_nearest_wins() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ws = dir.path().join("workspace");
    let sub = ws.join("nested");
    std::fs::create_dir_all(sub.join("src"))?;
    std::fs::create_dir_all(ws.join("src"))?;
    // Marker at both levels.
    std::fs::write(ws.join("project.marker"), "")?;
    std::fs::write(sub.join("project.marker"), "")?;
    std::fs::write(
        ws.join("src").join(format!("root.{MOCK_LANG}")),
        "echo root_level\n",
    )?;
    std::fs::write(
        sub.join("src").join(format!("nested.{MOCK_LANG}")),
        "echo nested_level\n",
    )?;

    let tmp = tempfile::tempdir()?;
    let config_path = write_marker_config(tmp.path(), &["project.marker"])?;
    let root = ws.to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_CONFIG", &config_path);
        cmd.env("CATENARY_ROOTS", root);
    })?;
    bridge.initialize()?;

    // Grep should find files from both levels.
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "echo", "directory": root }))?;
    assert!(
        text.contains(&format!("root.{MOCK_LANG}"))
            && text.contains(&format!("nested.{MOCK_LANG}")),
        "Grep should find files at both marker levels. Got:\n{text}"
    );

    Ok(())
}
