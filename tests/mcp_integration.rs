// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end integration tests for the MCP-LSP bridge.
//!
//! These tests spawn the actual bridge binary and communicate with it
//! via stdin/stdout using the MCP protocol.

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;

use common::{BridgeProcess, ipc_request, mockls_lsp_arg};

const MOCK_LANG_A: &str = "yX4Za";
const MOCK_LANG_B: &str = "d5apI";

#[test]
fn test_mcp_initialize() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }
    }))?;

    let response = bridge.recv()?;

    assert_eq!(response["jsonrpc"], "2.0");
    assert_eq!(response["id"], 1);
    assert!(response.get("result").is_some());

    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "catenary");
    // MCP no longer advertises tool capabilities.
    assert!(
        result["capabilities"]["tools"].is_null(),
        "capabilities should not include tools"
    );
    Ok(())
}

#[test]
fn test_mcp_tools_list_returns_method_not_found() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
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
fn test_mcp_tools_call_returns_method_not_found() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "unknown_tool",
            "arguments": {}
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("error").is_some(),
        "tools/call should return method-not-found: {response:?}"
    );
    Ok(())
}

#[test]
fn test_mcp_ping() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;

    assert!(response.get("result").is_some());
    assert!(response.get("error").is_none());
    Ok(())
}

#[test]
fn test_client_info_stored_in_session() -> Result<()> {
    let dir = tempfile::tempdir().context("Failed to create temp dir")?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");

    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;

    // Send initialize with specific client info (not bridge.initialize())
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "TestClient",
                "version": "42.0.0"
            }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(response.get("result").is_some(), "Initialize failed");

    // Small delay to allow session update
    std::thread::sleep(Duration::from_millis(200));

    // Run catenary list with the bridge's isolated state dir
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .arg("list")
        .env("XDG_STATE_HOME", bridge.state_home())
        .output()
        .context("Failed to run catenary list")?;

    let stdout_str = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout_str.contains("TestClient"),
        "Expected client info 'TestClient' in catenary list output, got:\n{stdout_str}"
    );
    Ok(())
}

#[test]
fn test_multi_root_find_symbol() -> Result<()> {
    // Create two roots with unique function names
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("alpha.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "function alpha_func()\nalpha_func\n")?;

    let script_b = dir_b.path().join(format!("beta.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "function beta_func()\nbeta_func\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    // Search should locate alpha_func from root A (via symbols or heatmap)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 700,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_func" }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "search for alpha_func failed: {response_a:?}"
    );
    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing text for alpha_func")?;
    assert!(
        text_a.contains(&format!("alpha.{MOCK_LANG_A}")),
        "Expected search to find alpha.mock, got: {text_a}"
    );
    assert!(
        text_a.contains("alpha_func"),
        "Expected alpha_func in output, got: {text_a}"
    );

    // Search should locate beta_func from root B (via symbols or heatmap)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 701,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "beta_func" }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "search for beta_func failed: {response_b:?}"
    );
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing text for beta_func")?;
    assert!(
        text_b.contains(&format!("beta.{MOCK_LANG_A}")),
        "Expected search to find beta.mock, got: {text_b}"
    );

    Ok(())
}

#[test]
fn test_multi_root_glob_file() -> Result<()> {
    // Create two roots with different outline symbols
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("syms_a.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "struct AlphaType\nenum BetaMode\n")?;

    let script_b = dir_b.path().join(format!("syms_b.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "struct GammaType\nenum DeltaMode\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    // Get outline from root A file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 720,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": script_a.to_str().context("Invalid script A path")?
            }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "Glob file from root A failed: {response_a:?}"
    );
    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing text for symbols A")?;
    // Glob file mode: line count header (no symbols until 08b).
    assert!(
        text_a.contains("(2 lines)"),
        "Should show line count for root A file, got: {text_a}"
    );

    // Get header from root B file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 721,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": {
                "pattern": script_b.to_str().context("Invalid script B path")?
            }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "Glob file from root B failed: {response_b:?}"
    );
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing text for symbols B")?;
    assert!(
        text_b.contains("(2 lines)"),
        "Should show line count for root B file, got: {text_b}"
    );

    Ok(())
}

// ─── sync_roots capability tests ────────────────────────────────────────

/// mockls without `--workspace-folders` does NOT support
/// `workspace/didChangeWorkspaceFolders`. When roots change, the server should
/// be shut down and lazily respawned with the updated root set on the next
/// query.
#[test]
fn test_sync_roots_restart_no_workspace_folders() -> Result<()> {
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("funcs_a.{MOCK_LANG_A}"));
    std::fs::write(
        &script_a,
        "function unique_root_a_func()\nunique_root_a_func\n",
    )?;

    let script_b = dir_b.path().join(format!("funcs_b.{MOCK_LANG_A}"));
    std::fs::write(
        &script_b,
        "function unique_root_b_func()\nunique_root_b_func\n",
    )?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    // Spawn bridge with only root_a
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
    bridge.initialize_with_roots(&[root_a])?;

    // Search in root_a — server should be working
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "unique_root_a_func" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "Search in root A failed: {response:?}"
    );

    // Send roots/list_changed, respond with both roots
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    let roots_request = bridge.recv()?;
    let method = roots_request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
    assert_eq!(method, "roots/list");

    let request_id = roots_request
        .get("id")
        .ok_or_else(|| anyhow!("roots/list request missing id"))?
        .clone();

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "roots": [
                {"uri": format!("file://{root_a}")},
                {"uri": format!("file://{root_b}")}
            ]
        }
    }))?;

    // Search in root_b — server should have been restarted with new roots.
    // search waits for all servers to be ready, but retry to accommodate restart.
    let mut success = false;
    let mut last_text = String::new();
    for i in 0..10 {
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 20 + i,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "unique_root_b_func" }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        if result["isError"] == true {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let text = result["content"][0]["text"]
            .as_str()
            .context("Missing text")?;
        last_text = text.to_string();
        if text.contains("unique_root_b_func") && text.contains(&format!("funcs_b.{MOCK_LANG_A}")) {
            success = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    assert!(
        success,
        "Search in root B should find ## [ with funcs_b.mock after server restart. Last output: {last_text}"
    );

    Ok(())
}

// ─── roots/list tests ───────────────────────────────────────────────────

#[test]
fn test_roots_list_after_initialize() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("dir")?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;

    // Initialize with roots capability — this validates the full round-trip:
    // initialize → notifications/initialized → server sends roots/list →
    // client responds → server applies roots
    bridge.initialize_with_roots(&[root])?;

    // Verify the server is still functional after roots exchange
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("result").is_some(),
        "Ping should succeed after roots exchange"
    );

    Ok(())
}

#[test]
fn test_roots_list_changed_notification() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("dir")?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;

    // Initialize with roots capability
    bridge.initialize_with_roots(&[root])?;

    // Send roots/list_changed notification — server should send another roots/list request
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    // Read the roots/list request
    let roots_request = bridge.recv()?;
    let method = roots_request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
    assert_eq!(method, "roots/list", "Server should re-fetch roots");

    let request_id = roots_request
        .get("id")
        .ok_or_else(|| anyhow!("roots/list request missing id"))?
        .clone();

    // Respond with updated roots
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "roots": [
                {"uri": "file:///tmp", "name": "tmp"},
                {"uri": "file:///var", "name": "var"}
            ]
        }
    }))?;

    std::thread::sleep(Duration::from_millis(100));

    // Verify still functional
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 200,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("result").is_some(),
        "Ping should succeed after roots update"
    );

    Ok(())
}

#[test]
fn test_no_roots_request_without_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("dir")?)?;

    // Initialize WITHOUT roots capability
    bridge.initialize()?;

    // Send a ping immediately — if the server had sent a roots/list request,
    // we'd read that instead of the ping response
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 300,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;

    // This should be the ping response, not a roots/list request
    let id = response
        .get("id")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow!("Expected ping response, got: {response:?}"))?;
    assert_eq!(id, 300, "Should receive ping response, not roots/list");
    assert!(response.get("result").is_some());

    Ok(())
}

// ─── mockls-based tests ─────────────────────────────────────────────────
// These tests use mockls instead of real language servers, so they always
// run regardless of installed toolchains.

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "Parameterized test iterates over profiles"
)]
fn test_mockls_sync_roots_across_profiles() -> Result<()> {
    let profiles: &[(&str, &str)] = &[
        ("no-workspace-folders", "--scan-roots"),
        ("workspace-folders", "--workspace-folders --scan-roots"),
    ];

    for (name, flags) in profiles {
        let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
        let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

        let script_a = dir_a.path().join(format!("funcs_a.{MOCK_LANG_A}"));
        std::fs::write(&script_a, "fn unique_root_a_func()\nunique_root_a_func\n")?;

        let script_b = dir_b.path().join(format!("funcs_b.{MOCK_LANG_A}"));
        std::fs::write(&script_b, "fn unique_root_b_func()\nunique_root_b_func\n")?;

        let root_a = dir_a.path().to_str().context("Invalid path A")?;
        let root_b = dir_b.path().to_str().context("Invalid path B")?;

        let lsp = mockls_lsp_arg(MOCK_LANG_A, flags);
        let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
        bridge.initialize_with_roots(&[root_a])?;

        // Search in root_a — server should be working
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "unique_root_a_func" }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        assert!(
            result["isError"].is_null() || result["isError"] == false,
            "Profile {name}: search in root A failed: {response:?}"
        );

        // Send roots/list_changed with both roots
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/roots/list_changed"
        }))?;

        let roots_request = bridge.recv()?;
        let method = roots_request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or_else(|| {
                anyhow!("Profile {name}: Expected roots/list, got: {roots_request:?}")
            })?;
        assert_eq!(method, "roots/list");

        let request_id = roots_request
            .get("id")
            .ok_or_else(|| anyhow!("Profile {name}: roots/list missing id"))?
            .clone();

        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "roots": [
                    {"uri": format!("file://{root_a}")},
                    {"uri": format!("file://{root_b}")}
                ]
            }
        }))?;

        // Wait for root_b to appear in the daemon's root tracker.
        bridge.wait_for_root(root_b, std::time::Duration::from_secs(5))?;

        // Search in root_b
        bridge.send(&json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": { "pattern": "unique_root_b_func" }
            }
        }))?;

        let response = bridge.recv()?;
        let result = &response["result"];
        assert!(
            result["isError"] != true,
            "Profile {name}: search in root B returned error: {response:?}"
        );
        let text = result["content"][0]["text"]
            .as_str()
            .context("Missing text")?;
        assert!(
            text.contains(&format!("funcs_b.{MOCK_LANG_A}")),
            "Profile {name}: search in root B should reference funcs_b.mock, got: {text}"
        );
        assert!(
            text.contains("unique_root_b_func"),
            "Profile {name}: search in root B should find unique_root_b_func, got: {text}"
        );
    }
    Ok(())
}

/// Verifies that a server supporting workspace folders but not `$/progress`
/// doesn't hang after a root is added. The `wait_ready()` activity settle
/// fallback must transition the server back to `Ready`.
#[test]
fn test_mockls_sync_roots_no_progress_no_hang() -> Result<()> {
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let file_a = dir_a.path().join(format!("funcs_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn hello()\nhello\n")?;
    let file_b = dir_b.path().join(format!("funcs_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn world()\nworld\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    // mockls with --workspace-folders and --scan-roots but NO --indexing-delay:
    // supports didChangeWorkspaceFolders, never sends $/progress.
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--workspace-folders --scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_a)?;
    bridge.initialize_with_roots(&[root_a])?;

    // Search in root_a — establishes server is working
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "hello" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Root A search failed: {response:?}"
    );

    // Add root_b via roots/list_changed
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    let roots_request = bridge.recv()?;
    assert_eq!(
        roots_request["method"], "roots/list",
        "Expected roots/list request, got: {roots_request:?}"
    );

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": roots_request["id"],
        "result": {
            "roots": [
                {"uri": format!("file://{root_a}")},
                {"uri": format!("file://{root_b}")}
            ]
        }
    }))?;

    // Wait for root_b to appear in the daemon's root tracker.
    // MCP roots/list is processed asynchronously on the MCP connection
    // while IPC grep runs on a separate connection.
    bridge.wait_for_root(root_b, std::time::Duration::from_secs(5))?;

    // Search in root_b — must not hang.
    // did_change_workspace_folders sets state to Busy.
    // Since mockls never sends $/progress, wait_ready() uses
    // the activity settle fallback to transition back to Ready.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 20,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "world" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Root B search should not hang or error: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing search text for root B")?;
    assert!(
        text.contains(&format!("funcs_b.{MOCK_LANG_A}")),
        "Expected 'funcs_b.mock' in search results, got: {text}"
    );
    assert!(
        text.contains("world"),
        "Root B search should find world, got: {text}"
    );

    Ok(())
}

#[test]
fn test_mockls_multiplexing() -> Result<()> {
    // Spawn two mockls instances as different languages
    let dir = tempfile::tempdir()?;

    let shell_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&shell_file, "fn greet()\ngreet\n")?;

    let second_file = dir.path().join(format!("test.{MOCK_LANG_B}"));
    std::fs::write(&second_file, "[package]\nname = \"test\"\n")?;

    let lsp_shell = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let lsp_second = mockls_lsp_arg(MOCK_LANG_B, "");
    let root = dir.path().to_str().context("Invalid root path")?;

    let mut bridge = BridgeProcess::spawn(&[&lsp_shell, &lsp_second], root)?;
    bridge.initialize()?;

    // Search for "greet" — should find in MOCK_LANG_A file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 100,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "greet" }
        }
    }))?;

    let response_a = bridge.recv()?;
    let result_a = &response_a["result"];
    assert!(
        result_a["isError"].is_null() || result_a["isError"] == false,
        "Lang A search failed: {response_a:?}"
    );

    // Search for "package" — should find in MOCK_LANG_B file
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 101,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "package" }
        }
    }))?;

    let response_b = bridge.recv()?;
    let result_b = &response_b["result"];
    assert!(
        result_b["isError"].is_null() || result_b["isError"] == false,
        "Lang B search failed: {response_b:?}"
    );

    let text_a = result_a["content"][0]["text"]
        .as_str()
        .context("Missing lang A search text")?;
    let text_b = result_b["content"][0]["text"]
        .as_str()
        .context("Missing lang B search text")?;

    assert!(
        text_a.contains(&format!("test.{MOCK_LANG_A}")),
        "Lang A search should reference test file, got: {text_a}"
    );
    assert!(
        text_a.contains("greet"),
        "Lang A search should find symbol, got: {text_a}"
    );
    assert!(
        text_b.contains(&format!("test.{MOCK_LANG_B}")),
        "Lang B search should reference test file, got: {text_b}"
    );

    Ok(())
}

/// Verifies that Catenary does NOT send `didSave` when the server does not
/// advertise `textDocumentSync.save` (Gap 2 negative case).
#[test]
fn test_mockls_did_save_not_sent_without_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "echo hello\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--publish-version --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Notify via socket — this triggers didOpen + (possibly) didSave
    let _ = bridge.call_diagnostics(test_file.to_str().context("file path")?)?;

    // Shut down to flush the log
    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("textDocument/didSave"),
        "didSave should NOT be sent without save capability. Log:\n{log}"
    );

    Ok(())
}

/// Verifies that Catenary DOES send `didSave` when the server advertises
/// `textDocumentSync.save` (Gap 2 positive case).
#[test]
fn test_mockls_did_save_sent_with_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "echo hello\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        &format!("--publish-version --advertise-save --notification-log {log_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let _ = bridge.call_diagnostics(test_file.to_str().context("file path")?)?;

    drop(bridge);
    std::thread::sleep(Duration::from_millis(200));

    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("textDocument/didSave"),
        "didSave SHOULD be sent with save capability. Log:\n{log}"
    );

    Ok(())
}

/// Symbol source present: `documentSymbol` provides kind labels and
/// structure context even without a grammar installation step.
#[test]
fn test_search_graceful_degradation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn greet()\ngreet\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Search for "fn" — the line is a symbol definition (from documentSymbol),
    // so the hit is classified as a symbol.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "fn" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "grep should succeed: {response:?}"
    );
    let text_fn = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;
    // With documentSymbol, kind labels are present
    assert!(
        text_fn.contains("<Function>"),
        "documentSymbol path should have kind labels, got: {text_fn}"
    );

    // Search for "greet" — a symbol, not a keyword.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "greet" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;
    // rg-only heatmap: symbol found via ripgrep, file reference present
    assert!(
        text.contains("greet"),
        "Should find greet via ripgrep, got: {text}"
    );
    assert!(
        text.contains(&format!("test.{MOCK_LANG_A}")),
        "Should show file reference, got: {text}"
    );
    // With documentSymbol, kind labels are present
    assert!(
        text.contains("<Function>"),
        "documentSymbol path should have kind labels, got: {text}"
    );

    Ok(())
}

/// Verifies that a server burning CPU after a workspace folder change
/// does not block `wait_ready` — lifecycle-based readiness returns
/// immediately since the server is already `Healthy`.
///
/// mockls `--cpu-on-workspace-change 15000` burns 15s of CPU on
/// `workspace/didChangeWorkspaceFolders`. The server is already `Healthy`
/// (init completed), so `wait_ready` returns `true` immediately.
/// Individual LSP requests may time out via `Connection::request`'s
/// failure detection, but grep degrades gracefully via ripgrep.
#[test]
fn test_wait_ready_failure_detection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "echo hello\n")?;

    let dir2 = tempfile::tempdir()?;

    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        "--workspace-folders --cpu-on-workspace-change 15000",
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize_with_roots(&[root])?;

    // Send roots/list_changed notification to trigger workspace folder change
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/roots/list_changed"
    }))?;

    // Server sends roots/list request — respond with both roots
    let roots_request = bridge.recv()?;
    let method = roots_request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
    if method != "roots/list" {
        bail!("Expected roots/list, got {method}");
    }
    let request_id = roots_request
        .get("id")
        .ok_or_else(|| anyhow!("roots/list request missing id"))?
        .clone();

    let root2 = dir2.path().to_str().context("root2 path")?;
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "roots": [
                {"uri": format!("file://{root}")},
                {"uri": format!("file://{root2}")}
            ]
        }
    }))?;

    // Small delay for the workspace folder change to be sent to mockls
    std::thread::sleep(Duration::from_millis(200));

    // Send a search request — wait_ready returns true (server is Healthy),
    // but individual LSP requests may time out during the CPU burn.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "hello" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];

    // Search degrades gracefully — ripgrep results still present
    assert!(
        result.get("isError").is_none() || result["isError"] == false,
        "Search should degrade gracefully, not error. Got: {response:?}"
    );

    let content = result["content"]
        .as_array()
        .context("Missing content array")?;
    let text = content[0]["text"].as_str().context("Missing result text")?;
    assert!(
        text.contains("hello"),
        "Ripgrep results should still contain the match. Got: {text}"
    );

    Ok(())
}

/// Verifies that a server burning CPU on `initialized` does not prevent
/// search from succeeding.
///
/// mockls `--cpu-on-initialized 3000` burns 3s of CPU on `initialized`.
/// The server is set to `Healthy` after init completes, so `wait_ready`
/// returns `true` immediately. The search request succeeds because the
/// server responds to requests after the CPU burn finishes.
#[test]
fn test_warmup_observation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn my_function()\nmy_function\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --cpu-on-initialized 3000");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Send search immediately — server is still burning CPU
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "my_function" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    // Should succeed — wait_ready waits for CPU burn to finish
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains(&format!("test.{MOCK_LANG_A}")),
        "Search should succeed after warmup observation. Got: {text}"
    );
    assert!(
        text.contains("my_function"),
        "Search after warmup should find symbol, got: {text}"
    );

    Ok(())
}

// ─── scan-roots and enrichment tests ─────────────────────────────────────

/// Verifies that `--scan-roots` makes workspace symbols available without
/// a prior `didOpen`. Without `--scan-roots`, search only finds text via
/// ripgrep; with it, LSP workspace symbols appear in the `## Symbols` section.
#[test]
fn test_search_symbols_with_scan_roots() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("greeter.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn greet()\ngreet\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "greet" }
        }
    }))?;

    let response = bridge.recv()?;
    assert!(
        response["result"]["isError"] != true,
        "Search should succeed: {response:?}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing search text")?;

    assert!(
        text.contains("greet"),
        "Search should find 'greet', got: {text}"
    );
    assert!(
        text.contains(&format!("greeter.{MOCK_LANG_A}")),
        "Search should find greeter file, got: {text}"
    );

    Ok(())
}

/// Verifies classified output for two symbols found via alternation.
#[test]
fn test_grep_per_symbol_output() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file_a = dir.path().join(format!("mod_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn load_config()\nload_config\n")?;

    let file_b = dir.path().join(format!("mod_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn save_config()\nsave_config\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "load_config|save_config" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Both symbols should appear in output
    assert!(
        text.contains("load_config"),
        "Expected load_config in output, got:\n{text}"
    );
    assert!(
        text.contains("save_config"),
        "Expected save_config in output, got:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("mod_a.{MOCK_LANG_A}")),
        "Expected mod_a file, got:\n{text}"
    );
    assert!(
        text.contains(&format!("mod_b.{MOCK_LANG_A}")),
        "Expected mod_b file, got:\n{text}"
    );

    Ok(())
}

/// Verifies that grep finds symbols via ripgrep + prepareRename (no grammar).
#[test]
fn test_grep_resolve_provider() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("resolve.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn resolve_me()\nresolve_me\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --resolve-provider");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "resolve_me" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("resolve_me"),
        "Expected resolve_me in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("resolve.{MOCK_LANG_A}")),
        "Expected file name in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that pipe-separated alternation finds symbols from both patterns.
/// `pattern: "alpha_func|beta_func"` should find both across two roots.
#[test]
fn test_grep_alternation() -> Result<()> {
    let dir_a = tempfile::tempdir().context("Failed to create temp dir A")?;
    let dir_b = tempfile::tempdir().context("Failed to create temp dir B")?;

    let script_a = dir_a.path().join(format!("alpha.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "function alpha_func()\nalpha_func\n")?;

    let script_b = dir_b.path().join(format!("beta.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "function beta_func()\nbeta_func\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 800,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_func|beta_func" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep alternation failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for alternation")?;

    // Both files should appear
    assert!(
        text.contains(&format!("alpha.{MOCK_LANG_A}")),
        "Expected alpha.mock in alternation results, got: {text}"
    );
    assert!(
        text.contains(&format!("beta.{MOCK_LANG_A}")),
        "Expected beta.mock in alternation results, got: {text}"
    );

    // Both symbols should appear
    assert!(
        text.contains("alpha_func"),
        "Expected alpha_func symbol, got: {text}"
    );
    assert!(
        text.contains("beta_func"),
        "Expected beta_func symbol, got: {text}"
    );

    Ok(())
}

/// Budget-driven paging: broad pattern exceeding `budget` produces
/// multiple pages with `[page N/M]` header instead of tier demotion.
#[test]
fn test_grep_enrichment_threshold_broad() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        let mut content = String::new();
        for i in 0..30 {
            use std::fmt::Write;
            let _ = writeln!(content, "fn zz_broad_{i}");
        }
        for i in 0..30 {
            use std::fmt::Write;
            let _ = writeln!(content, "zz_broad_{i}");
        }
        std::fs::write(root.join(format!("many.{MOCK_LANG_A}")), &content)?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[tools.grep]\nbudget = 200\n\n\
                 [server.mockls]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--scan-roots\"]\n\n\
                 [language.{MOCK_LANG_A}]\nservers = [\"mockls\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;

    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 810,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "zz_broad" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep broad should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for broad search")?;

    // Should have page header indicating multiple pages
    assert!(
        text.starts_with("[page 1/"),
        "Expected [page 1/N] header, got: {text}"
    );
    // Should still contain results
    assert!(
        text.contains("zz_broad"),
        "Expected results present, got: {text}"
    );
    // Should not contain all 30 symbols (truncated to page)
    let sym_count = (0..30)
        .filter(|i| text.contains(&format!("zz_broad_{i}")))
        .count();
    assert!(
        sym_count < 30,
        "Expected paged output (not all 30 symbols), got {sym_count}"
    );

    Ok(())
}

/// Test A — rg-only groups by matched string.
///
/// Files with no symbol definitions (plain text without `fn`/`function`/etc.)
/// should be grouped under `# matched_text` headings when queried with alternation.
#[test]
fn test_grep_rg_only_groups_by_matched_string() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create files with no symbol definitions — just plain text
    let file_a = dir.path().join(format!("notes.{MOCK_LANG_A}"));
    std::fs::write(
        &file_a,
        "the alpha_token is important\nalpha_token appears again\n",
    )?;

    let file_b = dir.path().join(format!("readme.{MOCK_LANG_A}"));
    std::fs::write(
        &file_b,
        "beta_token is used here\nbeta_token is also here\n",
    )?;

    // Use --scan-roots so mockls indexes files, but no fn/struct/etc. definitions exist
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 900,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_token|beta_token" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep rg-only failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Both tokens should appear in output
    assert!(
        text.contains("alpha_token"),
        "Expected alpha_token in output, got:\n{text}"
    );
    assert!(
        text.contains("beta_token"),
        "Expected beta_token in output, got:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("notes.{MOCK_LANG_A}")),
        "Expected notes file, got:\n{text}"
    );
    assert!(
        text.contains(&format!("readme.{MOCK_LANG_A}")),
        "Expected readme file, got:\n{text}"
    );

    Ok(())
}

/// Test B — alternation routes non-code hits correctly.
///
/// Files with symbol definitions AND non-code mentions should have each `#`
/// heading receive the correct rg hits, not all dumped under the first one.
#[test]
fn test_grep_alternation_routes_non_code_hits() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // File with a symbol definition for "compute" and a plain mention of "render"
    let file_a = dir.path().join(format!("engine.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn compute()\ncompute\nrender is mentioned here\n")?;

    // File with a symbol definition for "render" and a plain mention of "compute"
    let file_b = dir.path().join(format!("display.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn render()\nrender\ncompute is mentioned here\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 910,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "compute|render" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep alternation routing failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Both symbols should appear in output
    assert!(
        text.contains("compute"),
        "Expected compute in output, got:\n{text}"
    );
    assert!(
        text.contains("render"),
        "Expected render in output, got:\n{text}"
    );

    // Both files should appear
    assert!(
        text.contains(&format!("engine.{MOCK_LANG_A}")),
        "Expected engine file, got:\n{text}"
    );
    assert!(
        text.contains(&format!("display.{MOCK_LANG_A}")),
        "Expected display file, got:\n{text}"
    );

    Ok(())
}

/// Test C — two definitions under one `#` heading with per-`##` references.
///
/// Two files each defining the same function name should produce one `#`
/// heading with two `##` sub-headings, each showing their own references.
#[test]
fn test_grep_two_defs_same_name_per_heading_refs() -> Result<()> {
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;

    let file_a = dir_a.path().join(format!("impl_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn process()\nprocess\n")?;

    let file_b = dir_b.path().join(format!("impl_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn process()\nprocess\n")?;

    let root_a = dir_a.path().to_str().context("Invalid path A")?;
    let root_b = dir_b.path().to_str().context("Invalid path B")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[root_a, root_b])?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 920,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "process" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep two-defs failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Both files should appear with the symbol
    assert!(
        text.contains("process"),
        "Expected process in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("impl_a.{MOCK_LANG_A}")),
        "Expected impl_a in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("impl_b.{MOCK_LANG_A}")),
        "Expected impl_b in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that URI-only (`OneOf::Right`) workspace/symbol results are
/// resolved via `workspaceSymbol/resolve`. Uses `--no-empty-query` to force
/// per-query lookup combined with `--resolve-provider` so results need resolve.
#[test]
fn test_grep_resolve_fallback_path() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("fallback.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn resolve_fallback()\nresolve_fallback\n")?;

    let lsp = mockls_lsp_arg(
        MOCK_LANG_A,
        "--scan-roots --resolve-provider --no-empty-query",
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 930,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "resolve_fallback" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep resolve fallback failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol should be found via ripgrep + prepareRename
    assert!(
        text.contains("resolve_fallback"),
        "Expected resolve_fallback in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("fallback.{MOCK_LANG_A}")),
        "Expected file name in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that the same symbol name found by two different language servers
/// produces a single `#` heading with `##` sub-headings from each server.
#[test]
fn test_grep_cross_server_same_symbol() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file_a = dir.path().join(format!("shared.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn cross_server_fn()\ncross_server_fn\n")?;

    let file_b = dir.path().join(format!("shared.{MOCK_LANG_B}"));
    std::fs::write(&file_b, "fn cross_server_fn()\ncross_server_fn\n")?;

    let lsp_a = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let lsp_b = mockls_lsp_arg(MOCK_LANG_B, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;

    let mut bridge = BridgeProcess::spawn(&[&lsp_a, &lsp_b], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 940,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "cross_server_fn" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep cross-server failed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol should appear with both files
    assert!(
        text.contains("cross_server_fn"),
        "Expected cross_server_fn in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("shared.{MOCK_LANG_A}")),
        "Expected shared.{MOCK_LANG_A} in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("shared.{MOCK_LANG_B}")),
        "Expected shared.{MOCK_LANG_B} in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enriched output for functions includes a "Called by:" section
/// listing the enclosing caller name.
#[test]
fn test_grep_enrichment_incoming_calls() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("calls.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn callee_fn()\nfn caller_fn()\n  callee_fn\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 950,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "callee_fn" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep incoming calls should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for incoming calls")?;

    // Enrichment runs and renders the result
    assert!(
        text.contains("callee_fn"),
        "Expected callee_fn in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("calls.{MOCK_LANG_A}")),
        "Expected file name in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enrichment for structs runs without error.
#[test]
fn test_grep_enrichment_implementations() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // mockls routes textDocument/implementation to handle_references,
    // so any reference location will appear as an implementation entry.
    let test_file = dir.path().join(format!("impls.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "struct MyStruct\nMyStruct\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 960,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "MyStruct" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep implementations should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for implementations")?;

    // Enrichment runs and renders the result
    assert!(
        text.contains("MyStruct"),
        "Expected MyStruct in output, got:\n{text}"
    );

    Ok(())
}

/// Verifies that enrichment for types with subtypes runs without error.
#[test]
fn test_grep_enrichment_subtypes() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let test_file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &test_file,
        "interface Animal\nstruct Dog extends Animal\nclass Cat implements Animal\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 970,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Animal" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep subtypes should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text for subtypes")?;

    // Enrichment runs and renders the result
    assert!(
        text.contains("Animal"),
        "Expected Animal in output, got:\n{text}"
    );

    Ok(())
}

/// Symbol index finds methods inside impl blocks with correct kind
/// and enclosing scope. No bootstrap or workspace/symbol needed.
#[test]
fn test_symbol_index_finds_methods() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Struct with a method inside it — documentSymbol should find the
    // method with kind "method" and the enclosing struct name as scope.
    let file = dir.path().join(format!("widget.{MOCK_LANG_A}"));
    std::fs::write(&file, "struct Widget {\nfn widget_method\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5000,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "widget_method" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "grep should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tree-sitter index should find the method with kind label
    assert!(
        text.contains("widget_method"),
        "Expected widget_method in output, got:\n{text}"
    );
    // Method should have a kind label from tree-sitter
    assert!(
        text.contains("<Function>") || text.contains("<Method>"),
        "Expected kind label for method, got:\n{text}"
    );
    // Method should be scoped under Widget
    assert!(
        text.contains("Widget"),
        "Expected enclosing struct scope, got:\n{text}"
    );

    Ok(())
}

// ─── SEARCHv2 grep pipeline tests (ticket 06a) ─────────────────────────

/// Pattern matching a known symbol — no grammar installed, so the
/// prepareRename path (no symbol index data) (prepareRename) identifies symbols.
#[test]
fn test_grep_basic_hits() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("greet.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn say_hello()\nsay_hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3000,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "say_hello" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Should find the symbol with file and line reference
    assert!(
        text.contains("say_hello"),
        "Expected say_hello in output, got:\n{text}"
    );
    assert!(
        text.contains(&format!("greet.{MOCK_LANG_A}")),
        "Expected filename in output, got:\n{text}"
    );

    Ok(())
}

/// Grep with `glob` scoping — only matching files appear.
#[test]
fn test_grep_glob_scoping() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src_dir = dir.path().join("src");
    std::fs::create_dir(&src_dir)?;
    let file_a = src_dir.join(format!("a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn scope_target()\nscope_target\n")?;
    let file_b = dir.path().join(format!("b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn scope_target()\nscope_target\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3001,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": {
                "pattern": "scope_target",
                "glob": "src/**"
            }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains(&format!("a.{MOCK_LANG_A}")),
        "Expected src/a file in output, got:\n{text}"
    );
    // The glob scope limits which files are searched for definitions.
    // Enrichment sections (impls, refs) may reference out-of-scope files.
    // Check that b.LANG doesn't appear as a definition line (tab-indented
    // with file path at the end).
    let b_as_def = text.lines().any(|l| {
        let t = l.trim_start_matches('\t');
        t.starts_with("scope_target") && t.contains(&format!("b.{MOCK_LANG_A}"))
    });
    assert!(
        !b_as_def,
        "Expected b file excluded from definition lines, got:\n{text}"
    );

    Ok(())
}

/// Grep with `exclude` — test files excluded from matches.
#[test]
fn test_grep_exclude() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("main.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn excl_func()\nexcl_func\n")?;
    let file_b = dir.path().join(format!("test_main.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn excl_func()\nexcl_func\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3002,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": {
                "pattern": "excl_func",
                "exclude": "**/test_*"
            }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains(&format!("main.{MOCK_LANG_A}")),
        "Expected main file in output, got:\n{text}"
    );
    // The exclude parameter limits which files are searched for definitions.
    // Enrichment sections (impls, refs) may reference excluded files.
    // Check that test_main doesn't appear as a definition line.
    let test_as_def = text.lines().any(|l| {
        let t = l.trim_start_matches('\t');
        t.starts_with("excl_func") && t.contains(&format!("test_main.{MOCK_LANG_A}"))
    });
    assert!(
        !test_as_def,
        "Expected test file excluded from definition lines, got:\n{text}"
    );

    Ok(())
}

/// `foo|bar` pattern produces two independent result sections.
#[test]
fn test_grep_alternation_split() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_a = dir.path().join(format!("alt_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn alt_alpha()\nalt_alpha\n")?;
    let file_b = dir.path().join(format!("alt_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn alt_beta()\nalt_beta\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3004,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alt_alpha|alt_beta" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("alt_alpha"),
        "Expected alt_alpha in output, got:\n{text}"
    );
    assert!(
        text.contains("alt_beta"),
        "Expected alt_beta in output, got:\n{text}"
    );

    Ok(())
}

/// `(foo|bar)_baz` pattern is a single result section (not split).
#[test]
fn test_grep_alternation_nested() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("nested.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn alpha_baz()\nalpha_baz\nfn beta_baz()\nbeta_baz\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3005,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "(alpha|beta)_baz" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Both matches should appear — the nested alternation is one arm
    assert!(
        text.contains("alpha_baz") && text.contains("beta_baz"),
        "Expected both alpha_baz and beta_baz in single section, got:\n{text}"
    );

    Ok(())
}

/// No-grammar file, pattern matches only keywords. Keywords filtered out
/// via `prepareRename` returning null should not appear in output.
/// Searches for the keyword `struct` which mockls recognizes and returns
/// null for via prepareRename.
#[test]
fn test_grep_prepare_rename_keyword() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("kw.{MOCK_LANG_A}"));
    // Only the keyword `struct` matches — mockls returns null for keywords
    std::fs::write(&file, "struct MyType\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Search for the keyword `struct` (not the symbol name)
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3006,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "^struct$" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // The keyword `struct` is filtered out by prepareRename returning null.
    // Only the keyword itself matched (not the symbol name), so output is empty.
    assert_eq!(
        text, "No results found",
        "Expected 'No results found' when only keywords match, got:\n{text}"
    );

    Ok(())
}

/// Symbol kind labels use `<Kind>` angle brackets from `documentSymbol`.
#[test]
fn test_grep_kind_brackets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // File with matching language extension so documentSymbol populates the index
    let file = dir.path().join(format!("kinds.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn my_func\nstruct MyStruct\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3010,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "my_func|MyStruct" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tree-sitter kind labels should use <Kind> angle brackets
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind label, got:\n{text}"
    );
    assert!(
        text.contains("<Struct>"),
        "Expected <Struct> kind label, got:\n{text}"
    );
    // Must NOT use [Kind] square brackets (old format)
    assert!(
        !text.contains("[Function]") && !text.contains("[Struct]"),
        "Expected angle brackets <Kind>, not square brackets [Kind], got:\n{text}"
    );

    Ok(())
}

/// Reference hit at a non-definition line reports enclosing tree-sitter structure.
/// Uses the mock grammar's brace-delimited block syntax: `fn outer { target }`.
#[test]
fn test_grep_reference_enclosing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // fn outer spans lines 0-2, "target" on line 1 is enclosed by it
    let file = dir.path().join(format!("enclosing.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn outer {\ntarget\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 3011,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "target" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Reference hit with enclosing structure: `:line <Kind> name:span`
    assert!(
        text.contains("<Function>") && text.contains("outer"),
        "Expected enclosing <Function> outer in output, got:\n{text}"
    );
    // Span should show the enclosing function's range
    assert!(
        text.contains(":1-3"),
        "Expected enclosing span :1-3, got:\n{text}"
    );

    Ok(())
}

/// Verify that LSP messages triggered by grep carry a `parent_id`
/// in the database (CLI tool queries get a UUID for event correlation).
#[test]
fn test_grep_parent_id_threading() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn hello()\nhello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Call grep via IPC (intercepted by send()).
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "hello" }
        }
    }))?;
    let _response = bridge.recv()?;

    // Open the database and verify parent_id threading
    let db_path = PathBuf::from(bridge.state_home())
        .join("catenary")
        .join("catenary.db");
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .context("open test database")?;

    // Find the parent_id on the IPC tool/grep hook event.
    // This UUID is shared with LSP messages from the grep pipeline.
    let call_uuid: String = conn
        .query_row(
            "SELECT parent_id FROM messages \
             WHERE type = 'hook' AND method = 'tool/grep' \
             AND parent_id IS NOT NULL LIMIT 1",
            [],
            |row| row.get(0),
        )
        .context("find tool/grep parent_id (call UUID)")?;

    // LSP messages from the grep pipeline should carry this parent_id
    let lsp_with_parent: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE type = 'lsp' AND parent_id = ?1",
            [&call_uuid],
            |row| row.get(0),
        )
        .context("count LSP messages with parent_id")?;

    assert!(
        lsp_with_parent > 0,
        "Expected LSP messages with parent_id={call_uuid} from grep, found 0"
    );

    Ok(())
}

/// Verify that glob no longer produces LSP messages (filesystem-only
/// since 08a — defensive maps with LSP symbols added in 08b).
#[test]
fn test_glob_parent_id_threading() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn hello()\nhello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "glob",
            "arguments": { "pattern": test_file.to_str().context("path")? }
        }
    }))?;
    let response = bridge.recv()?;
    let result = &response["result"];
    let content = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Glob returns line count header only (no LSP calls).
    assert!(
        content.contains("(2 lines)"),
        "Should show line count, got: {content}"
    );

    Ok(())
}

// ─── 06b: Grep rendering ────────────────────────────────────────────────

/// Name grouping: definitions with enclosing structures and spans.
#[test]
fn test_grep_name_grouping() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Create multiple .mock files with definitions and references
    let tests_dir = dir.path().join("tests");
    std::fs::create_dir(&tests_dir)?;
    let file_a = tests_dir.join(format!("alpha.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn test_alpha {\ntest_alpha\n}\n")?;
    let file_b = tests_dir.join(format!("beta.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn test_beta {\ntest_alpha\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4000,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "test_alpha" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Name group at column 0 (no leading whitespace)
    let first_line = text.lines().next().unwrap_or("");
    assert!(
        !first_line.starts_with('\t') && !first_line.starts_with(' '),
        "Name group should be at column 0, got:\n{text}"
    );

    // Enclosing structures with spans
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind label, got:\n{text}"
    );

    // Directory grouping
    assert!(
        text.contains("tests/"),
        "Expected tests/ directory grouping, got:\n{text}"
    );

    Ok(())
}

/// Basic grep: definition and reference lines with line numbers.
#[test]
fn test_grep_basic_output() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("data.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn say_hello()\nsay_hello\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4001,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "say_hello" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol appears in output with line numbers
    assert!(
        text.contains("say_hello"),
        "Expected symbol in output, got:\n{text}"
    );
    // Bare hit lines use `:line` format
    assert!(
        text.contains(':'),
        "Expected line numbers in output, got:\n{text}"
    );

    Ok(())
}

/// Narrow pattern: name at column 0 with kind label.
#[test]
fn test_grep_narrow_pattern() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("narrow.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn unique_symbol_xyz\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4002,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "unique_symbol_xyz" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Name at column 0, <Kind> label present
    assert!(
        text.contains("unique_symbol_xyz"),
        "Expected name in output, got:\n{text}"
    );
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind, got:\n{text}"
    );

    Ok(())
}

/// Single-line structure: `:line <Kind> name:line` (no range).
#[test]
fn test_grep_single_line_structure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Single-line definition (no brace block)
    let file = dir.path().join(format!("single.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn one_liner\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4003,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "one_liner" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("<Function>") && text.contains("one_liner"),
        "Expected function definition, got:\n{text}"
    );
    // Single-line: `:1` not `:1-1`
    assert!(
        text.contains(":1") && !text.contains(":1-1"),
        "Single-line structure should show :line not :start-end, got:\n{text}"
    );

    Ok(())
}

/// No blank line separators between name groups.
#[test]
fn test_grep_no_blank_lines() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("multi.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn alpha_one\nfn beta_two\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4004,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "alpha_one|beta_two" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Each alternation arm produces its own output section
    assert!(
        text.contains("alpha_one"),
        "Expected alpha_one, got:\n{text}"
    );
    assert!(text.contains("beta_two"), "Expected beta_two, got:\n{text}");

    // No blank lines within a single arm's output
    for arm_text in [&text] {
        let lines: Vec<&str> = arm_text.lines().collect();
        for window in lines.windows(2) {
            assert!(
                !(window[0].is_empty() && window[1].is_empty()),
                "Found consecutive blank lines in output:\n{text}"
            );
        }
    }

    Ok(())
}

/// Multi-server priority chain for `prepareRename`: first server errors,
/// second server succeeds. The symbol should still appear in output.
///
/// Uses two mockls servers for the same language. Server A has
/// `--fail-on textDocument/prepareRename`; server B works normally.
/// No grammar installed, so the prepareRename path (no symbol index data) exercises
/// `prepare_rename_check` priority chain fallthrough.
#[test]
fn test_grep_prepare_rename_priority_chain() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(
            root.join(format!("chain.{MOCK_LANG_A}")),
            "fn chain_symbol\nchain_symbol\n",
        )?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[server.mockls-fail]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--scan-roots\", \"--fail-on\", \"textDocument/prepareRename\"]\n\n\
                 [server.mockls-ok]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--scan-roots\"]\n\n\
                 [language.{MOCK_LANG_A}]\n\
                 servers = [\"mockls-fail\", \"mockls-ok\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 4100,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "chain_symbol" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Server A errors on prepareRename, server B succeeds.
    // The symbol should appear despite the first server failing.
    assert!(
        text.contains("chain_symbol"),
        "Expected chain_symbol in output (priority chain fallthrough), got:\n{text}"
    );

    Ok(())
}

// ─── SEARCHv2 enrichment tests (ticket 07a) ───────────────────────────

/// Enrich a function: `outgoing_calls` and `ref_lines` are populated.
/// Uses the prepareRename path (no symbol index data) (mockls, no tree-sitter grammar installed).
/// Enrichment runs via the pipeline.
#[test]
fn test_enrich_ungated_function() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // callee_fn defined on L0, caller_fn defined on L1, caller_fn calls callee_fn on L2
    let file = dir.path().join(format!("func.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn callee_fn()\nfn caller_fn()\n  callee_fn\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5100,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "callee_fn" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich_ungated_function should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tool completes successfully with enrichment
    assert!(
        text.contains("callee_fn"),
        "Expected callee_fn in output, got:\n{text}"
    );

    Ok(())
}

/// Enrich a type: implementations, supertypes, subtypes are populated.
/// Uses the prepareRename path (no symbol index data).
#[test]
fn test_enrich_ungated_type() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "interface Vehicle\nstruct Car extends Vehicle\nclass Truck implements Vehicle\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5110,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Vehicle" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich_ungated_type should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("Vehicle"),
        "Expected Vehicle in output, got:\n{text}"
    );

    Ok(())
}

/// Symbol index path: `documentSymbol`-identified symbol skips
/// `prepareRename`. mockls provides `documentSymbol` data for files
/// with recognized declaration keywords.
#[test]
fn test_enrich_symbol_index_path() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // File with matching language extension
    let file = dir.path().join(format!("ts_true.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn my_symbol\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5120,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "my_symbol" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich symbol index path should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol index identified the symbol — enrichment runs without prepareRename
    assert!(
        text.contains("my_symbol"),
        "Expected my_symbol in output, got:\n{text}"
    );
    assert!(
        text.contains("<Function>"),
        "Expected symbol kind label, got:\n{text}"
    );

    Ok(())
}

/// `prepareRename` path on a symbol: file has `documentSymbol` definitions,
/// so the symbol index classifies the hit. Enrichment proceeds.
#[test]
fn test_enrich_prepare_rename_symbol() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("sym.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn enrichable_sym\nenrichable_sym\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5130,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "enrichable_sym" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich prepare_rename symbol should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // prepareRename confirmed symbol, enrichment ran
    assert!(
        text.contains("enrichable_sym"),
        "Expected enrichable_sym in output, got:\n{text}"
    );
    // Enrichment should produce edges (mockls routes implementation to
    // references, so impls section may absorb refs via dedup).
    assert!(
        text.contains("impls:") || text.contains("refs:"),
        "Expected enrichment section, got:\n{text}"
    );

    Ok(())
}

/// Keyword matching on a line that IS a symbol definition: with
/// `documentSymbol`-based indexing, the hit at the keyword position
/// is classified as a symbol definition (the line has a symbol), so
/// the symbol appears in output rather than being filtered.
#[test]
fn test_enrich_prepare_rename_keyword() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // File with a function definition — `documentSymbol` will report
    // `my_symbol` at line 0. A grep for `^fn ` hits line 0, which the
    // symbol index recognizes as a definition line.
    let file = dir.path().join(format!("kw.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn my_symbol\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5140,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "^fn " }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // The line is a symbol definition — the index identifies it, so
    // the symbol appears in output (not filtered as keyword).
    assert!(
        text.contains("my_symbol"),
        "Expected my_symbol in output, got:\n{text}"
    );

    Ok(())
}

/// Keyword-only matches filtered on prepareRename path (no symbol index data).
/// When the symbol index has no data for a file (no `documentSymbol`
/// definitions), hits go through `prepare_rename_check`. mockls returns
/// null for declaration keywords (`fn`, `struct`, etc.), which
/// `prepare_rename_check` interprets as keyword → filtered from output.
#[test]
fn test_keyword_filtered_no_grammar() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // File with no declaration-keyword-at-line-start patterns, so
    // mockls's `documentSymbol` returns empty → symbol index has no
    // data for this file → prepareRename path (no symbol index data) → `prepare_rename_check`.
    // The `fn` keyword appears mid-line, not as a definition.
    let file = dir.path().join(format!("kw_filter.{MOCK_LANG_A}"));
    std::fs::write(&file, "just some fn keyword\nanother fn here\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "fn" }))?;

    // `fn` is a declaration keyword — mockls's prepareRename returns
    // null at the keyword position. All hits should be filtered.
    assert!(
        text.contains("No results found"),
        "Expected keyword hits to be filtered, got:\n{text}"
    );

    Ok(())
}

/// `PrepareRename` enrichment path: file has no `documentSymbol` definitions
/// (no recognized declaration keywords at line start), so hits go through
/// `prepare_rename_check` classification. Symbols that pass get enrichment
/// (refs, impls) via `enrich_at_position`.
#[test]
fn test_enrich_prepare_rename_path() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // No `fn`/`struct`/etc. at line start → mockls returns empty
    // documentSymbol → symbol index empty → prepareRename path.
    // `myTarget` is a renameable word (not a keyword), so it passes
    // prepare_rename_check and gets enrichment.
    let file = dir.path().join(format!("pr_enrich.{MOCK_LANG_A}"));
    std::fs::write(&file, "call myTarget here\nuse myTarget again\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "myTarget" }))?;

    assert!(
        text.contains("myTarget"),
        "Expected myTarget in output, got:\n{text}"
    );
    // Enrichment should run: mockls returns references for myTarget
    // (all occurrences across documents). At minimum, impls or refs
    // section should be present.
    assert!(
        text.contains("impls:") || text.contains("refs:"),
        "Expected enrichment section on prepareRename path, got:\n{text}"
    );

    Ok(())
}

/// Deprecated subtype: TypeEdge.deprecated is set from tags.
/// Enrichment runs for the interface; mockls returns deprecated subtypes
/// when the declaration line contains @deprecated.
#[test]
fn test_enrich_deprecated_type_edge() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("depr.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "interface Shape\nstruct OldSquare extends Shape @deprecated\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5150,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Shape" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich deprecated type edge should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Tool completes with deprecated subtypes collected and rendered
    assert!(
        text.contains("Shape"),
        "Expected Shape in output, got:\n{text}"
    );
    // Subtypes section should contain the deprecated subtype
    assert!(
        text.contains("subtypes:"),
        "Expected subtypes: section from fetch_type_hierarchy, got:\n{text}"
    );
    assert!(
        text.contains("OldSquare"),
        "Expected OldSquare in subtypes section, got:\n{text}"
    );

    Ok(())
}

/// Function with callees: `outgoing_calls` has correct names, kinds, files, lines.
/// Uses mockls which implements outgoing calls by scanning for known function
/// names called within the body.
#[test]
fn test_enrich_outgoing_calls() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // helper_a and helper_b defined, then main_fn calls them
    let file = dir.path().join(format!("out.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn helper_a()\nfn helper_b()\nfn main_fn()\n  helper_a\n  helper_b\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 5160,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "main_fn" }
        }
    }))?;

    let response = bridge.recv()?;
    let result = &response["result"];
    assert!(
        result["isError"].is_null() || result["isError"] == false,
        "enrich outgoing calls should succeed: {response:?}"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Enrichment — outgoing calls visible
    assert!(
        text.contains("main_fn"),
        "Expected main_fn in output, got:\n{text}"
    );
    assert!(
        text.contains("calls:"),
        "Expected calls: section for outgoing calls, got:\n{text}"
    );
    assert!(
        text.contains("helper_a"),
        "Expected helper_a in calls section, got:\n{text}"
    );
    assert!(
        text.contains("helper_b"),
        "Expected helper_b in calls section, got:\n{text}"
    );

    Ok(())
}

// ─── SEARCHv2 enriched rendering tests (ticket 07b) ────────────────────

/// Grammar-path enrichment: verify calls appear for simple functions.
/// Regression test for the document lifecycle bug where `didClose` between
/// enrichment methods caused mockls to lose pre-indexed state.
#[test]
fn test_grep_grammar_path_calls() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("gpcalls.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn helper_gp\nfn main_gp {\nhelper_gp\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 7777,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "main_gp" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol index: kind label present
    assert!(
        text.contains("<Function> main_gp"),
        "Expected <Function> main_gp, got:\n{text}"
    );
    // Enrichment: outgoing calls populated
    assert!(
        text.contains("calls:"),
        "Expected calls: section on grammar path, got:\n{text}"
    );
    assert!(
        text.contains("helper_gp"),
        "Expected helper_gp in calls, got:\n{text}"
    );

    Ok(())
}

/// Enriched:`calls:` section with outgoing calls and `<Function>` labels.
#[test]
fn test_grep_enriched() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // callee defined first, then caller with callee in body.
    // mockls scans the caller's body for known function names.
    let file = dir.path().join(format!("enrich.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn callee_t1\nfn caller_t1 {\ncallee_t1\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6000,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "caller_t1" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Page header followed by bare root path and name at depth 0
    assert!(
        text.starts_with("[page "),
        "Expected [page N/M] header, got:\n{text}"
    );
    assert!(
        !text.contains("Root: "),
        "Should not contain Root: prefix, got:\n{text}"
    );
    assert!(
        text.contains("caller_t1"),
        "Expected caller_t1 name group, got:\n{text}"
    );
    // Symbol index: kind label
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind label, got:\n{text}"
    );
    // Outgoing calls section
    assert!(
        text.contains("calls:"),
        "Expected calls: section, got:\n{text}"
    );
    assert!(
        text.contains("callee_t1"),
        "Expected callee_t1 in calls, got:\n{text}"
    );

    Ok(())
}

/// Enrichment cache: second grep for the same pattern returns identical
/// enriched output. The first call populates the cache; the second hits it.
#[test]
fn test_grep_enrichment_cache_hit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("cache.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn callee_cache\nfn caller_cache {\ncallee_cache\n}\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First grep — populates cache.
    let text1 = bridge.call_tool_text("grep", &json!({ "pattern": "caller_cache" }))?;

    // Sanity: first call produces enriched output.
    assert!(
        text1.contains("calls:"),
        "First grep should be enriched, got:\n{text1}"
    );

    // Second grep — should hit cache and produce identical output.
    let text2 = bridge.call_tool_text("grep", &json!({ "pattern": "caller_cache" }))?;

    assert_eq!(
        text1, text2,
        "Second grep should match first (cache hit).\nFirst:\n{text1}\nSecond:\n{text2}"
    );

    Ok(())
}

/// Out-of-root grep via absolute glob: enrichment works but is not cached.
/// Files outside workspace roots reach `enrich_at_position` through absolute
/// glob override. `resolve_root` returns `None`, so the enrichment cache is
/// bypassed. Second call must still return correct results (no stale state).
#[test]
fn test_grep_enrichment_cache_skip_out_of_root() -> Result<()> {
    let root_dir = tempfile::tempdir()?;
    let root = root_dir.path().to_str().context("root path")?;
    let oor_dir = tempfile::tempdir()?;

    // Put a file in the workspace root (triggers server startup).
    let root_file = root_dir.path().join(format!("root.{MOCK_LANG_A}"));
    std::fs::write(&root_file, "fn root_fn\n")?;

    // Put a file outside workspace roots.
    let oor_file = oor_dir.path().join(format!("outside.{MOCK_LANG_A}"));
    std::fs::write(&oor_file, "fn callee_oor\nfn caller_oor {\ncallee_oor\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Grep with absolute glob targeting the out-of-root directory.
    let abs_glob = format!("{}/**", oor_dir.path().display());
    let text1 = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "caller_oor", "glob": abs_glob }),
    )?;

    assert!(
        text1.contains("caller_oor"),
        "Out-of-root grep should find hits, got:\n{text1}"
    );

    // Second call — must still work (no stale cache, no crash).
    let text2 = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "caller_oor", "glob": abs_glob }),
    )?;

    assert!(
        text2.contains("caller_oor"),
        "Second out-of-root grep should still find hits, got:\n{text2}"
    );

    Ok(())
}

/// Type hierarchy: subtypes section present for interface pattern.
#[test]
fn test_grep_type_hierarchy() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Use `struct` for both — the mock grammar only supports fn/struct.
    // mockls still handles `extends` for type hierarchy on structs.
    let file = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "struct Vehicle_t1\nstruct Car_t1 extends Vehicle_t1\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6010,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Vehicle_t1" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    assert!(
        text.contains("Vehicle_t1"),
        "Expected Vehicle_t1 in output, got:\n{text}"
    );
    // Symbol index: kind label
    assert!(
        text.contains("<Struct>"),
        "Expected <Struct> kind label, got:\n{text}"
    );
    // mockls returns subtypes for types with `extends`
    assert!(
        text.contains("subtypes:"),
        "Expected subtypes: section, got:\n{text}"
    );
    // Subtype name should appear within the subtypes section
    assert!(
        text.contains("Car_t1"),
        "Expected Car_t1 in subtypes section, got:\n{text}"
    );

    Ok(())
}

/// Path syntax: `<Struct> Container/<Function> name  path:line`.
#[test]
fn test_grep_path_syntax() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Nested function inside struct — exercises `/`-separated scope path
    let file = dir.path().join(format!("path.{MOCK_LANG_A}"));
    std::fs::write(&file, "struct Container_ps {\nfn inner_ps\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6020,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "inner_ps" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // `/`-separated path syntax with scope
    assert!(
        text.contains("<Struct> Container_ps/<Function> inner_ps"),
        "Expected scoped path syntax, got:\n{text}"
    );
    let expected_path = format!("path.{MOCK_LANG_A}:2");
    assert!(
        text.contains(&expected_path),
        "Expected {expected_path}, got:\n{text}"
    );

    Ok(())
}

/// Refs: lines ascending within file for same-file references.
#[test]
fn test_grep_refs_sort() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Symbol defined on L0, referenced on L2 and L4 (same file).
    // mockls finds these via textDocument/references.
    let file = dir.path().join(format!("sort.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn sorted_sym\nfn other {\nsorted_sym\nfn yet_another {\nsorted_sym\n}\n}\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6030,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "sorted_sym" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Should have enrichment with definition and reference info
    assert!(
        text.contains("<Function>") && text.contains("sorted_sym"),
        "Expected enriched output, got:\n{text}"
    );
    // References section: mockls returns references for sorted_sym
    // across all occurrences in the document.
    assert!(
        text.contains("refs:"),
        "Expected refs: section from fetch_references, got:\n{text}"
    );

    Ok(())
}

/// Outgoing calls sorted alphabetically.
#[test]
fn test_grep_outgoing_calls_sorted() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // main calls beta and alpha — should appear alpha before beta in calls:
    let file = dir.path().join(format!("sorted_calls.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn alpha_callee\nfn beta_callee\nfn main_caller {\nbeta_callee\nalpha_callee\n}\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6040,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "main_caller" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol index: kind label
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind label, got:\n{text}"
    );
    assert!(
        text.contains("calls:"),
        "Expected calls: section, got:\n{text}"
    );

    // alpha should appear before beta (alphabetical sort)
    if let Some(calls_pos) = text.find("calls:") {
        let calls_section = &text[calls_pos..];
        let alpha_pos = calls_section.find("alpha_callee");
        let beta_pos = calls_section.find("beta_callee");
        if let (Some(a), Some(b)) = (alpha_pos, beta_pos) {
            assert!(a < b, "Expected alpha before beta in calls, got:\n{text}");
        }
    }
    // Call edges should carry kind labels from mockls
    assert!(
        text.contains("<Function>") && text.contains("alpha_callee"),
        "Expected <Function> alpha_callee in calls, got:\n{text}"
    );

    Ok(())
}

/// Deprecated: `<Kind, deprecated>` in output.
#[test]
fn test_grep_deprecated() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Use `struct` for both — the mock grammar only supports fn/struct.
    let file = dir.path().join(format!("depr.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "struct Shape_t1\nstruct OldSquare_t1 extends Shape_t1 @deprecated\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6050,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Shape_t1" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol index: kind label
    assert!(
        text.contains("<Struct>"),
        "Expected <Struct> kind label in output, got:\n{text}"
    );
    assert!(
        text.contains("deprecated"),
        "Expected deprecated tag in output, got:\n{text}"
    );

    Ok(())
}

/// Paging: many symbols exceed budget → paged output, not demotion.
#[test]
fn test_grep_paged_integration() -> Result<()> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        let mut content = String::new();
        for i in 0..50 {
            use std::fmt::Write;
            let _ = writeln!(content, "fn demote_sym_{i}");
        }
        std::fs::write(root.join(format!("demote.{MOCK_LANG_A}")), &content)?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[tools.grep]\nbudget = 200\n\n\
                 [server.mockls]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--scan-roots\"]\n\n\
                 [language.{MOCK_LANG_A}]\nservers = [\"mockls\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;

    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6060,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "demote_sym" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Page header present, multiple pages expected
    assert!(
        text.starts_with("[page 1/"),
        "Expected [page 1/N] header, got:\n{text}"
    );
    // Should still contain results
    assert!(
        text.contains("demote_sym"),
        "Expected results present, got:\n{text}"
    );
    // Not all 50 symbols on one page
    let sym_count = (0..50)
        .filter(|i| text.contains(&format!("demote_sym_{i}")))
        .count();
    assert!(
        sym_count < 50,
        "Expected paged output (not all 50 symbols), got {sym_count}"
    );

    // Request page 2
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6061,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "demote_sym", "page": 2 }
        }
    }))?;

    let response2 = bridge.recv()?;
    let text2 = response2["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text page 2")?;

    assert!(
        text2.starts_with("[page 2/"),
        "Expected [page 2/N] header, got:\n{text2}"
    );

    Ok(())
}

/// Fish-eye: rich symbol (with calls) gets full format, lean gets single line.
#[test]
fn test_grep_fish_eye() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // rich_fn calls lean_fn. Pattern targets only rich_fn.
    let file = dir.path().join(format!("fisheye.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn lean_fisheye\nfn rich_fisheye {\nlean_fisheye\n}\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6070,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "rich_fisheye" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Symbol index: kind labels on both rich and lean symbols
    assert!(
        text.contains("<Function>"),
        "Expected <Function> kind labels, got:\n{text}"
    );
    // rich_fisheye should appear with calls section
    assert!(
        text.contains("rich_fisheye"),
        "Expected rich_fisheye, got:\n{text}"
    );
    assert!(
        text.contains("calls:"),
        "Expected calls: on rich symbol, got:\n{text}"
    );
    assert!(
        text.contains("lean_fisheye"),
        "Expected lean_fisheye in calls, got:\n{text}"
    );

    Ok(())
}

/// Property order: calls → impls → supertypes → subtypes → refs.
#[test]
fn test_grep_property_order() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Function with calls and refs
    let file = dir.path().join(format!("order.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn helper_ord\nfn main_ord {\nhelper_ord\n}\nmain_ord\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6080,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "main_ord" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // If both calls and refs exist, calls should come first
    if text.contains("calls:") && text.contains("refs:") {
        let calls_pos = text.find("calls:").context("calls pos")?;
        let refs_pos = text.find("refs:").context("refs pos")?;
        assert!(
            calls_pos < refs_pos,
            "Expected calls: before refs:, got:\n{text}"
        );
    }

    Ok(())
}

/// Name grouping: bare name at depth 0, definitions indented below.
#[test]
fn test_grep_enriched_name_grouping() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("group.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn grouped_sym\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6090,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "grouped_sym" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    let lines: Vec<&str> = text.lines().collect();
    // First line: [page N/M] header
    assert!(
        !lines.is_empty() && lines[0].starts_with("[page "),
        "Expected [page N/M] header in first line, got:\n{text}"
    );
    // Second line: blank (separator after page header)
    // Third line: bare root path (no Root: prefix)
    assert!(
        !text.contains("Root: "),
        "Should not contain Root: prefix, got:\n{text}"
    );
    // Definition at depth 0 (no name header, no leading tab)
    let has_def = lines
        .iter()
        .any(|l| !l.starts_with('\t') && l.contains("<Function>") && l.contains("grouped_sym"));
    assert!(has_def, "Expected definition at depth 0, got:\n{text}");

    Ok(())
}

/// Cross-definition dedup: impl suppressed when listed in struct's impls.
#[test]
fn test_grep_cross_def_dedup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // struct + impl block matching the same name. mockls routes implementation
    // to references, so the struct's impls section lists the impl location.
    // The impl definition should be suppressed in the output.
    let file = dir.path().join(format!("dedup.{MOCK_LANG_A}"));
    std::fs::write(&file, "struct Dedup_t1\nDedup_t1\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6100,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "Dedup_t1" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // The struct definition should be present
    assert!(
        text.contains("<Struct> Dedup_t1"),
        "Expected struct definition, got:\n{text}"
    );
    // No standalone name header — definition line carries the name.
    let bare_name_lines: Vec<&str> = text.lines().filter(|l| *l == "Dedup_t1").collect();
    assert!(
        bare_name_lines.is_empty(),
        "Expected no bare name header, got {} in:\n{text}",
        bare_name_lines.len()
    );

    Ok(())
}

/// Refs dedup: impl lines excluded from `refs:` when in `impls:`.
/// Uses prepareRename path (no symbol index data) so mockls has all documents open for enrichment.
#[test]
fn test_grep_refs_dedup_labeled() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // struct defined, then a reference on L1. mockls routes implementation
    // to references, so the same line may appear in both impls and refs.
    // The refs section should exclude lines already in impls.
    let file = dir.path().join(format!("dedup_refs.{MOCK_LANG_A}"));
    std::fs::write(&file, "struct DeduRef\nDeduRef\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6110,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "DeduRef" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // If impls section exists, lines in it should not also appear in refs
    if text.contains("impls:") && text.contains("refs:") {
        let impls_start = text.find("impls:").context("impls")?;
        let refs_start = text.find("refs:").context("refs")?;
        let impls_section = &text[impls_start..refs_start];
        let refs_section = &text[refs_start..];

        // Extract line numbers from impls section
        for line in impls_section.lines() {
            if let Some(colon_pos) = line.trim().strip_prefix(':') {
                let num_str: String = colon_pos.chars().take_while(char::is_ascii_digit).collect();
                if !num_str.is_empty() {
                    // This line number should not appear in refs
                    let refs_has_line = refs_section.lines().any(|rl| {
                        rl.trim().starts_with(&format!(":{num_str} "))
                            || rl.trim() == format!(":{num_str}")
                    });
                    assert!(
                        !refs_has_line,
                        "Line :{num_str} in impls should not also appear in refs:\n{text}"
                    );
                }
            }
        }
    }

    Ok(())
}

/// Incoming calls merge: callers appear in `refs:`, not a separate section.
/// Uses prepareRename path (no symbol index data) for reliable enrichment.
#[test]
fn test_grep_incoming_calls_merge() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // target defined on L0, caller on L1, caller calls target on L2
    let file = dir.path().join(format!("incoming.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn target_inc()\nfn caller_inc()\n  target_inc\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6120,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "target_inc" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // No separate `callers:` section — incoming calls merge into refs
    assert!(
        !text.contains("callers:"),
        "Expected no callers: section (merged into refs), got:\n{text}"
    );
    // Incoming calls should have been merged into refs section
    assert!(
        text.contains("refs:"),
        "Expected refs: section with merged incoming calls, got:\n{text}"
    );

    Ok(())
}

/// Impls structure: `impls:` has file-grouped entries with tree-sitter spans.
/// Uses prepareRename path (no symbol index data) for reliable enrichment.
#[test]
fn test_grep_impls_structure() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("impls.{MOCK_LANG_A}"));
    std::fs::write(&file, "struct ImplStr\nImplStr\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6130,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "ImplStr" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // mockls routes textDocument/implementation to handle_references
    // (same word-search logic), so fetch_implementations returns
    // locations for ImplStr. The impls section must be present.
    assert!(
        text.contains("impls:"),
        "Expected impls: section from fetch_implementations, got:\n{text}"
    );
    let impls_start = text.find("impls:").context("impls")?;
    let impls_section = &text[impls_start..];
    // Should contain a file path and at least one `:line` entry
    assert!(
        impls_section.contains(&format!("impls.{MOCK_LANG_A}")) || impls_section.contains(':'),
        "impls section should have file-grouped entries, got:\n{text}"
    );

    Ok(())
}

/// Single-line ref: `:hit <Kind> name:line` (no range when start == end).
#[test]
fn test_grep_single_line_ref() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Single-line function defined on L0, referenced on L1 inside another fn.
    // The enclosing fn on L1 is also single-line (no brace block).
    let file = dir.path().join(format!("single_ref.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn target_sl\nfn user_sl\ntarget_sl\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 6140,
        "method": "tools/call",
        "params": {
            "name": "grep",
            "arguments": { "pattern": "target_sl" }
        }
    }))?;

    let response = bridge.recv()?;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .context("Missing text")?;

    // Definition should have tree-sitter kind
    assert!(
        text.contains("<Function> target_sl"),
        "Expected <Function> target_sl, got:\n{text}"
    );
    // Single-line definitions show `:line` not `:start-end`
    assert!(
        !text.contains(":1-1"),
        "Single-line structure should show :line not :start-end, got:\n{text}"
    );

    Ok(())
}

// ─── Cancellation tests ──────────────────────────────────────────────

/// `notifications/cancelled` for a non-existent request is a no-op.
///
/// The bridge should not crash when it receives a cancellation for a
/// request ID that was never registered (e.g., a late or stale
/// cancellation). Verifies the bridge still responds to ping afterward.
#[test]
fn test_mcp_cancel_nonexistent_is_noop() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Send cancellation for a request that never existed.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 9999 }
    }))?;

    // Bridge should still work.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 9901,
        "method": "ping"
    }))?;
    let ping_response = bridge.recv()?;
    assert!(
        ping_response.get("result").is_some(),
        "bridge should respond to ping after stale cancellation: {ping_response}"
    );

    Ok(())
}

// ── Turn-boundary roots refresh ─────────────────────────────────────

/// `PreAgent` hook does NOT trigger `roots/list` when the client lacks roots
/// capability.
#[test]
fn test_turn_boundary_no_roots_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("dir")?;
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;

    // Initialize WITHOUT roots capability.
    bridge.initialize()?;

    let socket_path = bridge.wait_for_ipc_socket()?;

    // Send PreAgent hook.
    ipc_request(&socket_path, &json!({"method": "pre-agent/turn-start"}))?;

    // Send a tool call. Without roots capability, the bridge should NOT
    // send roots/list — the response should be the tool call result directly.
    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 600,
        "method": "ping"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("result").is_some(),
        "should get ping result directly (no roots/list): {response}"
    );
    // Confirm it's the ping response, not a roots/list request.
    assert!(
        response.get("method").is_none(),
        "should be a response, not a request: {response}"
    );

    Ok(())
}
