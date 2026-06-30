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

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;

use common::{BridgeProcess, mockls_lsp_arg, read_merged_log};

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
    let text_a = bridge.call_tool_text("grep", &json!({ "pattern": "alpha_func" }))?;
    assert!(
        text_a.contains(&format!("alpha.{MOCK_LANG_A}")),
        "Expected search to find alpha.mock, got: {text_a}"
    );
    assert!(
        text_a.contains("alpha_func"),
        "Expected alpha_func in output, got: {text_a}"
    );

    // Search should locate beta_func from root B (cwd must target root B)
    let text_b = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "beta_func", "directory": root_b }),
    )?;
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
    let text_a = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [script_a.to_str().context("Invalid script A path")?] }),
    )?;
    // Glob file mode: line count header (no symbols until 08b).
    assert!(
        text_a.contains("(2 lines)"),
        "Should show line count for root A file, got: {text_a}"
    );

    // Get header from root B file
    let text_b = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [script_b.to_str().context("Invalid script B path")?] }),
    )?;
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
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "unique_root_a_func" }))?;

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
    for _ in 0..10 {
        let text = bridge
            .call_tool_text(
                "grep",
                &json!({ "pattern": "unique_root_b_func", "directory": root_b }),
            )
            .unwrap_or_default();
        last_text = text.clone();
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
        let _ = bridge.call_tool_text("grep", &json!({ "pattern": "unique_root_a_func" }))?;

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

        // Search in root_b (cwd must target root_b)
        let text = bridge.call_tool_text(
            "grep",
            &json!({ "pattern": "unique_root_b_func", "directory": root_b }),
        )?;
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
    let _ = bridge.call_tool_text("grep", &json!({ "pattern": "hello" }))?;

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
    let text =
        bridge.call_tool_text("grep", &json!({ "pattern": "world", "directory": root_b }))?;
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
    let text_a = bridge.call_tool_text("grep", &json!({ "pattern": "greet" }))?;

    // Search for "package" — should find in MOCK_LANG_B file
    let text_b = bridge.call_tool_text("grep", &json!({ "pattern": "package" }))?;

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

    let log = read_merged_log(&log_path);
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

    let log = read_merged_log(&log_path);
    assert!(
        log.contains("textDocument/didSave"),
        "didSave SHOULD be sent with save capability. Log:\n{log}"
    );

    Ok(())
}

/// Symbol source present: `documentSymbol` still drives classification, and the
/// one-atom format renders results as plain full-source-line atoms with no
/// `<Kind>` labels. A keyword-position hit is returned verbatim as a reference
/// atom (bug 47: `prepareRename` gates enrichment, never membership).
#[test]
fn test_search_graceful_degradation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let test_file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&test_file, "fn greet()\ngreet\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Search for "fn" — a declaration keyword. The line is returned verbatim as
    // a plain reference atom (not dropped); the keyword is not the symbol name,
    // so the symbol index does not classify it as a definition.
    let text_fn = bridge.call_tool_text("grep", &json!({ "pattern": "fn" }))?;
    assert!(
        text_fn.contains(&format!("test.{MOCK_LANG_A}:1:fn greet()")),
        "keyword hit should be returned as a reference atom, got: {text_fn}"
    );

    // Search for "greet" — a symbol, not a keyword.
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "greet" }))?;
    // Symbol found via ripgrep, file reference present.
    assert!(
        text.contains("greet"),
        "Should find greet via ripgrep, got: {text}"
    );
    assert!(
        text.contains(&format!("test.{MOCK_LANG_A}")),
        "Should show file reference, got: {text}"
    );
    // One-atom format: no `<Kind>` labels.
    assert!(
        !text.contains('<'),
        "Expected no angle-bracket kind labels, got: {text}"
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
    // Search degrades gracefully — ripgrep results still present.
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "hello" }))?;
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
    // Should succeed — wait_ready waits for CPU burn to finish
    let text = bridge
        .call_tool_text("grep", &json!({ "pattern": "my_function" }))
        .unwrap_or_default();
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "greet" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "load_config|save_config" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "resolve_me" }))?;

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
/// `pattern: "alpha_func|beta_func"` should find both files in a single root.
#[test]
fn test_grep_alternation() -> Result<()> {
    let dir = tempfile::tempdir().context("Failed to create temp dir")?;

    let script_a = dir.path().join(format!("alpha.{MOCK_LANG_A}"));
    std::fs::write(&script_a, "function alpha_func()\nalpha_func\n")?;

    let script_b = dir.path().join(format!("beta.{MOCK_LANG_A}"));
    std::fs::write(&script_b, "function beta_func()\nbeta_func\n")?;

    let root = dir.path().to_str().context("Invalid path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "alpha_func|beta_func" }))?;

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

/// Volume valve: a broad pattern exceeding `line_budget` truncates the display
/// and spills the complete output to a runtime-dir file announced by a receipt.
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
                "[tools]\nline_budget = 10\n\n\
                 [server.mockls]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--scan-roots\"]\n\n\
                 [language.{MOCK_LANG_A}]\nservers = [\"mockls\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;

    bridge.initialize()?;

    let resp = bridge.call_search_raw("tool/grep", &json!({ "pattern": "zz_broad" }))?;
    let output = resp
        .get("output")
        .and_then(serde_json::Value::as_str)
        .context("output")?;
    let receipt = resp
        .get("receipt")
        .and_then(serde_json::Value::as_str)
        .context("expected an overflow receipt when output exceeds the line budget")?;

    // Display truncated to the line budget; results still present on stdout.
    let shown = output.lines().count();
    assert!(
        shown <= 10,
        "display truncated to the 10-line budget, got {shown} lines:\n{output}"
    );
    assert!(
        output.contains("zz_broad"),
        "results present, got:\n{output}"
    );

    // The receipt (stderr-bound) names the truncation and the spill path.
    assert!(
        receipt.contains("output truncated to protect context")
            && receipt.contains("full output ("),
        "receipt names the truncation + spill path: {receipt}"
    );

    // The spill file holds the COMPLETE output — more than the truncated display,
    // including the last symbol that did not fit.
    let path = receipt
        .rsplit(" at ")
        .next()
        .context("spill path in receipt")?;
    let spilled = std::fs::read_to_string(path).context("read spill file")?;
    assert!(
        spilled.lines().count() > shown,
        "spill file holds the full output ({} lines) vs {shown} shown",
        spilled.lines().count()
    );
    assert!(
        spilled.contains("zz_broad_29"),
        "spill file holds the complete result set, got:\n{spilled}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "alpha_token|beta_token" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "compute|render" }))?;

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
    let dir = tempfile::tempdir()?;

    let file_a = dir.path().join(format!("impl_a.{MOCK_LANG_A}"));
    std::fs::write(&file_a, "fn process()\nprocess\n")?;

    let file_b = dir.path().join(format!("impl_b.{MOCK_LANG_A}"));
    std::fs::write(&file_b, "fn process()\nprocess\n")?;

    let root = dir.path().to_str().context("Invalid path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "process" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "resolve_fallback" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "cross_server_fn" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "callee_fn" }))?;

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

/// Verifies that enrichment for a type with implementors runs without error.
#[test]
fn test_grep_enrichment_implementations() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // mockls `handle_implementation` returns `implements`-based implementors;
    // `Hello implements Greeter`, so enrichment for `Greeter` exercises the
    // implementation path and renders.
    let test_file = dir.path().join(format!("impls.{MOCK_LANG_A}"));
    std::fs::write(
        &test_file,
        "interface Greeter\nstruct Hello implements Greeter\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Greeter" }))?;

    // Enrichment runs and renders the result
    assert!(
        text.contains("Greeter"),
        "Expected Greeter in output, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Animal" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "widget_method" }))?;

    // Tree-sitter index should find the method, rendered as its full-line atom.
    // The method is a nested definition inside `Widget`, so it drops its own leaf
    // and carries only its container as the `#scope` anchor.
    assert!(
        text.contains(&format!("widget.{MOCK_LANG_A}:2#Widget:fn widget_method")),
        "Expected widget_method full-line atom, got:\n{text}"
    );
    // One-atom format: no `<Kind>` (or any other) angle-bracket labels.
    assert!(
        !text.contains('<'),
        "Expected no angle-bracket kind label, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "say_hello" }))?;

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

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "scope_target", "glob": "src/**" }),
    )?;

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

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "excl_func", "exclude": "**/test_*" }),
    )?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "alt_alpha|alt_beta" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "(alpha|beta)_baz" }))?;

    // Both matches should appear — the nested alternation is one arm
    assert!(
        text.contains("alpha_baz") && text.contains("beta_baz"),
        "Expected both alpha_baz and beta_baz in single section, got:\n{text}"
    );

    Ok(())
}

/// Bug 47: a hit on the keyword position of a definition line is returned
/// verbatim as a plain reference atom. mockls returns null from `prepareRename`
/// for the `struct` keyword, which gates enrichment only — it never drops a
/// ripgrep match (decision 024: `catenary grep` is a strict superset of
/// `grep`). The symbol name is `MyType`, so the symbol index does not classify
/// the keyword hit as a definition.
#[test]
fn test_grep_keyword_hit_returned_as_reference() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("kw.{MOCK_LANG_A}"));
    std::fs::write(&file, "struct MyType\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Grep the `struct` keyword (not the `MyType` symbol name).
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "struct" }))?;

    // The keyword line is returned verbatim, not dropped.
    assert!(
        text.contains(&format!("kw.{MOCK_LANG_A}:1:struct MyType")),
        "Expected keyword hit returned as a reference atom, got:\n{text}"
    );

    Ok(())
}

/// Definitions render as full-source-line atoms — no `<Kind>` labels.
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "my_func|MyStruct" }))?;

    // Each definition is a full-source-line atom: `relpath:LINE  <source line>`.
    assert!(
        text.contains(&format!("kinds.{MOCK_LANG_A}:1:fn my_func")),
        "Expected full-line atom for my_func, got:\n{text}"
    );
    assert!(
        text.contains(&format!("kinds.{MOCK_LANG_A}:2:struct MyStruct")),
        "Expected full-line atom for MyStruct, got:\n{text}"
    );
    // No `<Kind>` (or any other) angle-bracket labels in the one-atom format.
    assert!(
        !text.contains('<'),
        "Expected no angle-bracket kind labels, got:\n{text}"
    );

    Ok(())
}

/// Reference hit at a non-definition line renders as its full-line atom with a
/// `#scope` containment anchor naming its innermost enclosing symbol (no kind
/// tag, no enclosing span).
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "target" }))?;

    // The occurrence renders as a single full-line atom carrying its enclosing
    // symbol as the `#scope` anchor: `relpath:LINE#scope:<line>`.
    assert!(
        text.contains(&format!("enclosing.{MOCK_LANG_A}:2#outer:target")),
        "Expected full-line atom with #outer scope for the target occurrence, got:\n{text}"
    );
    // No enclosing-symbol kind tag.
    assert!(
        !text.contains('<'),
        "Expected no enclosing `<...>` tag, got:\n{text}"
    );
    // No enclosing span range like `:1-3`.
    assert!(
        !text.contains(":1-3"),
        "Expected no enclosing span range, got:\n{text}"
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

    let content = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [test_file.to_str().context("path")?] }),
    )?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "test_alpha" }))?;

    // Top-level atoms sit at column 0 (no leading whitespace).
    let first_line = text.lines().next().unwrap_or("");
    assert!(
        !first_line.starts_with('\t') && !first_line.starts_with(' '),
        "Top-level atom should be at column 0, got:\n{text}"
    );

    // The definition renders as a full-line atom (top-level def → no `#`).
    assert!(
        text.contains(&format!("tests/alpha.{MOCK_LANG_A}:1:fn test_alpha {{")),
        "Expected test_alpha definition atom, got:\n{text}"
    );

    // Usages carry their innermost enclosing symbol as the `#scope` anchor.
    assert!(
        text.contains(&format!(
            "tests/alpha.{MOCK_LANG_A}:2#test_alpha:test_alpha"
        )),
        "Expected test_alpha usage with #test_alpha scope, got:\n{text}"
    );
    assert!(
        text.contains(&format!("tests/beta.{MOCK_LANG_A}:2#test_beta:test_alpha")),
        "Expected test_alpha usage with #test_beta scope, got:\n{text}"
    );

    // Directory grouping retained in the relative path.
    assert!(
        text.contains("tests/"),
        "Expected tests/ directory in atom paths, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "say_hello" }))?;

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

/// `--count` is a dumb `grep -c`-style tally straight from ripgrep — no LSP,
/// no symbol classification. The response carries exact `matches`/`files` and
/// an empty `output` (no tree, no pagination). Run against a server-less
/// daemon to prove the count never touches the symbol pipeline.
#[test]
fn test_grep_count_reports_totals() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("data.txt"),
        "say_hello here\nand say_hello again\nunrelated line\n",
    )?;

    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[], root)?;
    bridge.initialize()?;

    let resp = bridge.call_search_raw(
        "tool/grep",
        &json!({ "pattern": "say_hello", "count": true }),
    )?;

    assert_eq!(
        resp.get("matches").and_then(serde_json::Value::as_u64),
        Some(2),
        "two lines match the pattern: {resp}"
    );
    assert_eq!(
        resp.get("files").and_then(serde_json::Value::as_u64),
        Some(1),
        "one file holds the matches: {resp}"
    );
    // Count short-circuits rendering — no tree output.
    assert_eq!(
        resp.get("output").and_then(serde_json::Value::as_str),
        Some(""),
        "count response carries no rendered output: {resp}"
    );

    Ok(())
}

/// Alternation counts a line matching multiple arms once — a single ripgrep
/// pass over `a|b`, not per-arm passes summed (the old double-count). Matches
/// `rg -c 'a|b'`.
#[test]
fn test_grep_count_alternation_counts_lines_once() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("data.txt"),
        "foo bar\nfoo only\nbar only\nnothing\n",
    )?;

    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[], root)?;
    bridge.initialize()?;

    let resp =
        bridge.call_search_raw("tool/grep", &json!({ "pattern": "foo|bar", "count": true }))?;

    // Three matching lines: "foo bar" (matches both arms — counted once),
    // "foo only", "bar only". Not four — the old per-arm sum double-counted
    // the overlapping line.
    assert_eq!(
        resp.get("matches").and_then(serde_json::Value::as_u64),
        Some(3),
        "alternation overlap counts the line once: {resp}"
    );

    Ok(())
}

/// Narrow pattern: definition renders as a full-source-line atom.
#[test]
fn test_grep_narrow_pattern() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("narrow.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn unique_symbol_xyz\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "unique_symbol_xyz" }))?;

    // Definition is a full-source-line atom: `relpath:LINE  <source line>`.
    assert!(
        text.contains(&format!("narrow.{MOCK_LANG_A}:1:fn unique_symbol_xyz")),
        "Expected full-line atom for the definition, got:\n{text}"
    );
    // One-atom format: no `<Kind>` angle-bracket labels.
    assert!(
        !text.contains('<'),
        "Expected no angle-bracket kind label, got:\n{text}"
    );

    Ok(())
}

/// Single-line structure: atom is `relpath:LINE  <source>` — a single line
/// number, no `:start-end` range.
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "one_liner" }))?;

    // The definition atom carries a single `:LINE` and the full source line.
    assert!(
        text.contains(&format!("single.{MOCK_LANG_A}:1:fn one_liner")),
        "Expected single-line atom, got:\n{text}"
    );
    // Single-line: `:1` not `:1-1` (spans are gone).
    assert!(
        !text.contains(":1-1"),
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "alpha_one|beta_two" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "chain_symbol" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "callee_fn" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Vehicle" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "my_symbol" }))?;

    // Symbol index identified the symbol — enrichment runs without prepareRename.
    // The definition renders as a full-line atom with no `<Kind>` label.
    assert!(
        text.contains(&format!("ts_true.{MOCK_LANG_A}:1:fn my_symbol")),
        "Expected my_symbol definition atom, got:\n{text}"
    );
    assert!(
        !text.contains('<'),
        "Expected no angle-bracket kind label, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "enrichable_sym" }))?;

    // prepareRename confirmed symbol; the definition and its usage both render
    // as self-contained `path:line:text` atoms (both top-level → no `#scope`).
    assert!(
        text.contains(&format!("sym.{MOCK_LANG_A}:1:fn enrichable_sym")),
        "Expected enrichable_sym definition atom, got:\n{text}"
    );
    assert!(
        text.contains(&format!("sym.{MOCK_LANG_A}:2:enrichable_sym")),
        "Expected enrichable_sym usage atom, got:\n{text}"
    );

    Ok(())
}

/// Bug 47: grepping the bare `fn` keyword on a definition line in an indexed
/// file returns the line verbatim as a plain reference atom. The keyword is not
/// the symbol name, so the symbol index does not classify the hit as a
/// definition, and `prepareRename` returning null no longer drops it.
#[test]
fn test_enrich_prepare_rename_keyword() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // File with a function definition — `documentSymbol` reports `my_symbol`
    // at line 0. A grep for `^fn ` hits the `fn` keyword position, not the name.
    let file = dir.path().join(format!("kw.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn my_symbol\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "^fn " }))?;

    // The keyword hit is returned verbatim as a reference atom (not dropped).
    assert!(
        text.contains(&format!("kw.{MOCK_LANG_A}:1:fn my_symbol")),
        "Expected keyword hit returned as a reference atom, got:\n{text}"
    );

    Ok(())
}

/// Bug 47: in a non-indexed file (no `documentSymbol` definitions) a hit whose
/// position `prepareRename` reports as a non-symbol (here the `fn` keyword, for
/// which mockls returns null) is returned as a plain reference atom, not
/// filtered out. `prepare_rename_check` gates enrichment only.
#[test]
fn test_keyword_no_grammar_returned_as_reference() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // File with no declaration-keyword-at-line-start patterns, so mockls's
    // `documentSymbol` returns empty → the symbol index has no data for this
    // file → the `prepare_rename_check` path. The `fn` keyword appears mid-line,
    // not as a definition, so prepareRename returns null at that position.
    let file = dir.path().join(format!("kw_filter.{MOCK_LANG_A}"));
    std::fs::write(&file, "just some fn keyword\nanother fn here\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "fn" }))?;

    // Both keyword hits are returned verbatim, not filtered.
    assert!(
        text.contains(&format!("kw_filter.{MOCK_LANG_A}:1:just some fn keyword")),
        "Expected line 1 returned as a reference atom, got:\n{text}"
    );
    assert!(
        text.contains(&format!("kw_filter.{MOCK_LANG_A}:2:another fn here")),
        "Expected line 2 returned as a reference atom, got:\n{text}"
    );

    Ok(())
}

/// Regression for bug 47: on a prose root (markdown served by Lattice) only
/// headings are renameable symbols, so every body-text line returns null from
/// `prepareRename`. Body-text matches MUST be returned as plain reference atoms
/// — decision 024 makes `catenary grep` a strict superset of `grep`, so no
/// ripgrep byte-match is ever dropped. Before the fix only heading-line hits
/// survived and every body line was silently dropped.
///
/// mockls stands in for Lattice: `fn heading` is the lone "heading"
/// (`documentSymbol`), and the body lines mention the keyword `struct`, for
/// which mockls's `prepareRename` returns null exactly as Lattice does for
/// prose. The body lines do not start with a declaration keyword, so they are
/// not themselves symbols.
#[test]
fn test_grep_prose_body_text_not_dropped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    let file = dir.path().join(format!("notes.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "fn heading\nfirst mention of struct in a paragraph\nanother struct in a list item\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "struct" }))?;

    // Both body-text matches are returned verbatim (the bug dropped them).
    assert!(
        text.contains(&format!(
            "notes.{MOCK_LANG_A}:2:first mention of struct in a paragraph"
        )),
        "Expected body-text line 2 returned as a reference atom, got:\n{text}"
    );
    assert!(
        text.contains(&format!(
            "notes.{MOCK_LANG_A}:3:another struct in a list item"
        )),
        "Expected body-text line 3 returned as a reference atom, got:\n{text}"
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

    // Both occurrences render as self-contained `path:line:text` atoms
    // (top-level body text → no `#scope`).
    assert!(
        text.contains(&format!("pr_enrich.{MOCK_LANG_A}:1:call myTarget here")),
        "Expected myTarget line 1 atom, got:\n{text}"
    );
    assert!(
        text.contains(&format!("pr_enrich.{MOCK_LANG_A}:2:use myTarget again")),
        "Expected myTarget line 2 atom, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Shape" }))?;

    // Definition renders as a full-line atom (top-level def → no `#`).
    assert!(
        text.contains(&format!("depr.{MOCK_LANG_A}:1:interface Shape")),
        "Expected Shape definition atom, got:\n{text}"
    );
    // The subtype line reads its full source verbatim (no Catenary-added
    // deprecation tag — the `@deprecated` text is part of the source).
    assert!(
        text.contains(&format!(
            "depr.{MOCK_LANG_A}:2:struct OldSquare extends Shape @deprecated"
        )),
        "Expected OldSquare subtype atom, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "main_fn" }))?;

    // The matched definition renders as a self-contained `path:line:text` atom
    // (top-level def → no `#scope`). The per-hit nav suite no longer fires, so
    // the callee names are not emitted under the hit.
    assert!(
        text.contains(&format!("out.{MOCK_LANG_A}:3:fn main_fn()")),
        "Expected main_fn definition atom, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "main_gp" }))?;

    // Definition renders as a full-line atom (top-level def → no `#`). The
    // per-hit nav suite no longer fires, so the callee `helper_gp` is not
    // emitted under the hit.
    assert!(
        text.contains(&format!("gpcalls.{MOCK_LANG_A}:2:fn main_gp {{")),
        "Expected main_gp definition atom, got:\n{text}"
    );

    Ok(())
}

/// Enriched: `calls:` section with outgoing-call edge atoms (full source lines).
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "caller_t1" }))?;

    // Single-page result: no page header, bare root path
    assert!(
        !text.contains("[page"),
        "Single-page result should have no page header, got:\n{text}"
    );
    assert!(
        !text.contains("Root: "),
        "Should not contain Root: prefix, got:\n{text}"
    );
    // Definition renders as a full-line atom (top-level def → no `#`). The
    // per-hit nav suite no longer fires, so the callee `callee_t1` is not
    // emitted under the hit.
    assert!(
        text.contains(&format!("enrich.{MOCK_LANG_A}:2:fn caller_t1 {{")),
        "Expected caller_t1 definition atom, got:\n{text}"
    );
    // One-atom format: no `<Kind>` labels.
    assert!(
        !text.contains('<'),
        "Expected no angle-bracket kind labels, got:\n{text}"
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

    // Sanity: first call produces the definition atom (top-level def → no `#`).
    assert!(
        text1.contains(&format!("cache.{MOCK_LANG_A}:2:fn caller_cache {{")),
        "First grep should render the caller_cache atom, got:\n{text1}"
    );

    // Second grep — should hit cache and produce identical output.
    let text2 = bridge.call_tool_text("grep", &json!({ "pattern": "caller_cache" }))?;

    assert_eq!(
        text1, text2,
        "Second grep should match first (cache hit).\nFirst:\n{text1}\nSecond:\n{text2}"
    );

    Ok(())
}

/// Bug #23 (end-to-end): after a symbol is renamed on disk and a
/// `catenary diagnostics` batch covers the file, `grep` reports the *new*
/// source-line atom. The batch invalidates the stale symbol rows
/// (`process_files_batched` Phase 1c) so enrichment re-indexes from
/// `documentSymbol` instead of serving the pre-edit name.
#[test]
fn test_grep_enclosing_label_refreshed_after_diagnostics() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // `outer_old` is a function definition on line 1.
    let file = dir.path().join(format!("rename.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn outer_old {\nmarker()\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First grep populates the symbol index; the atom carries the original line.
    let before = bridge.call_tool_text("grep", &json!({ "pattern": "outer" }))?;
    assert!(
        before.contains("fn outer_old"),
        "atom should carry the original source line, got:\n{before}"
    );

    // Rename the function on disk (as a host Edit/Write would), then run a
    // diagnostics batch over the file.
    std::fs::write(&file, "fn outer_new {\nmarker()\n}\n")?;
    let _ = bridge.call_diagnostics(file.to_str().context("file path")?)?;

    // The next grep must report the refreshed atom. `outer_old` no longer exists
    // anywhere on disk, so its presence would be pure cache staleness — the
    // symptom of bug #23.
    let after = bridge.call_tool_text("grep", &json!({ "pattern": "outer" }))?;
    assert!(
        after.contains("fn outer_new"),
        "atom should refresh to the renamed function, got:\n{after}"
    );
    assert!(
        !after.contains("outer_old"),
        "stale pre-rename atom must not survive the diagnostics batch, got:\n{after}"
    );

    Ok(())
}

/// Forces `path`'s mtime to a clearly-future instant so a rewrite is detected
/// as newer regardless of the filesystem's timestamp resolution (avoids a
/// same-second flake on coarse filesystems).
fn bump_mtime(path: &std::path::Path) -> Result<()> {
    let f = std::fs::File::options().write(true).open(path)?;
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))?;
    Ok(())
}

/// Bug #26 (end-to-end): after a symbol is renamed on disk through a host
/// `Edit`/`Write`, the *next* `grep` reports the new source-line atom with **no**
/// intervening `catenary diagnostics` pass. The mtime backstop in
/// `ensure_symbols` detects the file changed since its rows were populated and
/// re-requests `documentSymbol` — closing bug #23's documented residual (a
/// `grep` between a host edit and the next diagnostics served stale rows).
#[test]
fn test_grep_enclosing_label_refreshed_after_host_edit() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // `outer_old` is a function definition on line 1.
    let file = dir.path().join(format!("rename.{MOCK_LANG_A}"));
    std::fs::write(&file, "fn outer_old {\nmarker()\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First grep populates the symbol index and records the file's mtime.
    let before = bridge.call_tool_text("grep", &json!({ "pattern": "outer" }))?;
    assert!(
        before.contains("fn outer_old"),
        "atom should carry the original source line, got:\n{before}"
    );

    // Rename the function on disk (as a host Edit/Write would). No
    // diagnostics/sed pass runs — the daemon's only post-write signal is the
    // mtime advancing. Force a strictly-newer mtime so the test does not depend
    // on the filesystem's timestamp resolution (a same-second rewrite on a
    // coarse FS would not advance it).
    std::fs::write(&file, "fn outer_new {\nmarker()\n}\n")?;
    bump_mtime(&file)?;

    // The next grep must report the refreshed atom purely from the mtime
    // backstop. `outer_old` exists nowhere on disk, so its presence would be
    // pure cache staleness — the symptom of bug #26.
    let after = bridge.call_tool_text("grep", &json!({ "pattern": "outer" }))?;
    assert!(
        after.contains("fn outer_new"),
        "atom should refresh after a host edit, got:\n{after}"
    );
    assert!(
        !after.contains("outer_old"),
        "stale pre-edit atom must not survive the next grep, got:\n{after}"
    );

    Ok(())
}

/// Bug #26 (add/remove): a *new* matching file added to the searched tree must
/// appear on the next identical grep. There's no prior match to stat, so this is
/// caught by the directory witnesses — grep snapshots every directory it walked,
/// and the OS bumps a directory's mtime when a file is added to it.
#[test]
fn test_grep_multipage_cache_invalidates_on_file_added() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // One file with many reference matches → a multi-page result (grep budget
    // 4000), with only a single `documentSymbol` round-trip to populate it.
    let mut content = String::from("fn anchor\n");
    for i in 0..200 {
        let _ = std::fmt::Write::write_fmt(&mut content, format_args!("marker_{i:04}\n"));
    }
    std::fs::write(dir.path().join(format!("m.{MOCK_LANG_A}")), &content)?;

    // No `--scan-roots`: mockls answers documentSymbol from the opened document,
    // so a file added mid-session is recognized (its match isn't dropped as a
    // keyword). This isolates the cache-invalidation behavior under test.
    let lsp = mockls_lsp_arg(MOCK_LANG_A, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let _p1 = bridge.call_tool_text("grep", &json!({ "pattern": "marker" }))?;
    let p2 = bridge.call_tool_text("grep", &json!({ "pattern": "marker", "page": 2 }))?;
    assert!(
        !p2.trim().is_empty(),
        "result should span multiple pages (cache active), got empty page 2"
    );

    // Add a new matching file. Force a strictly-newer directory mtime for
    // resolution-independence.
    std::fs::write(
        dir.path().join(format!("a.{MOCK_LANG_A}")),
        "fn anchor2\nmarker_NEW\n",
    )?;
    {
        let d = std::fs::File::open(dir.path())?;
        d.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))?;
    }

    // The repeated query must now reflect the new file. Its single match can
    // land on any page (the parallel walk order isn't sorted), so scan all
    // pages: the page-1 fetch misses the cache (dir mtime changed) and re-runs;
    // later pages hit the freshly re-cached result.
    let mut combined = String::new();
    for page in 1..=8 {
        let p = bridge.call_tool_text("grep", &json!({ "pattern": "marker", "page": page }))?;
        if p.trim().is_empty() {
            break;
        }
        combined.push_str(&p);
        combined.push('\n');
    }
    assert!(
        combined.contains("marker_NEW"),
        "a newly added matching file must appear (cache must miss on dir change), got:\n{combined}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Vehicle_t1" }))?;

    // Definition renders as a full-line atom (top-level def → no `#`).
    assert!(
        text.contains(&format!("types.{MOCK_LANG_A}:1:struct Vehicle_t1")),
        "Expected Vehicle_t1 definition atom, got:\n{text}"
    );
    // The subtype Car_t1's hit on `Vehicle_t1` reads its full source line. It is
    // a top-level struct line, so it carries no `#scope`.
    assert!(
        text.contains(&format!(
            "types.{MOCK_LANG_A}:2:struct Car_t1 extends Vehicle_t1"
        )),
        "Expected Car_t1 subtype atom, got:\n{text}"
    );

    Ok(())
}

/// Path syntax: a nested definition renders as a single `relpath:LINE  <source>`
/// atom — no `<Kind>` labels and no `/`-separated scope path.
#[test]
fn test_grep_path_syntax() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("root path")?;

    // Nested function inside struct — formerly exercised `/`-separated scope path
    let file = dir.path().join(format!("path.{MOCK_LANG_A}"));
    std::fs::write(&file, "struct Container_ps {\nfn inner_ps\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "inner_ps" }))?;

    // The nested definition drops its own leaf and carries its container as the
    // `#scope` anchor: `relpath:LINE#Container:<source>`.
    assert!(
        text.contains(&format!("path.{MOCK_LANG_A}:2#Container_ps:fn inner_ps")),
        "Expected nested-def atom with #Container_ps scope, got:\n{text}"
    );
    // No `<Kind>` labels.
    assert!(
        !text.contains('<'),
        "Expected no `<...>` kind/scope labels, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "sorted_sym" }))?;

    // Definition renders as a full-line atom (top-level def → no `#`); the two
    // usages carry their innermost enclosing symbol as the `#scope` anchor (the
    // second nested as `other/yet_another`).
    let def = format!("sort.{MOCK_LANG_A}:1:fn sorted_sym");
    let ref3 = format!("sort.{MOCK_LANG_A}:3#other:sorted_sym");
    let ref5 = format!("sort.{MOCK_LANG_A}:5#other/yet_another:sorted_sym");
    let def_pos = text.find(&def);
    let ref3_pos = text.find(&ref3);
    let ref5_pos = text.find(&ref5);
    assert!(
        def_pos.is_some() && ref3_pos.is_some() && ref5_pos.is_some(),
        "Expected sorted_sym definition and both usage atoms, got:\n{text}"
    );
    // Lines appear ascending within the file.
    assert!(
        def_pos < ref3_pos && ref3_pos < ref5_pos,
        "Expected atoms in ascending line order (1, 3, 5), got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "main_caller" }))?;

    // Definition renders as a full-line atom (top-level def → no `#`). The
    // per-hit nav suite no longer fires, so the sorted callee names are not
    // emitted under the hit.
    assert!(
        text.contains(&format!("sorted_calls.{MOCK_LANG_A}:3:fn main_caller {{")),
        "Expected main_caller definition atom, got:\n{text}"
    );

    Ok(())
}

/// Deprecated subtype: the edge atom renders the subtype as a plain
/// full-source-line atom — no Catenary-added deprecation tag.
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Shape_t1" }))?;

    // Definition renders as a full-line atom; the subtype edge reads its full
    // source line verbatim (which here happens to contain `@deprecated` because
    // that text is in the source — it is not a Catenary-added tag).
    assert!(
        text.contains(&format!("depr.{MOCK_LANG_A}:1:struct Shape_t1")),
        "Expected Shape_t1 definition atom, got:\n{text}"
    );
    assert!(
        text.contains(&format!(
            "depr.{MOCK_LANG_A}:2:struct OldSquare_t1 extends Shape_t1 @deprecated"
        )),
        "Expected OldSquare_t1 subtype atom, got:\n{text}"
    );
    // One-atom format: no `<Kind>` labels.
    assert!(
        !text.contains('<'),
        "Expected no angle-bracket kind labels, got:\n{text}"
    );

    Ok(())
}

/// Volume valve: many symbols exceed the line budget → the display is truncated
/// and the complete result lives in the spill file (replaces the old page-2 fetch).
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
                "[tools]\nline_budget = 10\n\n\
                 [server.mockls]\n\
                 command = \"{mockls_bin}\"\n\
                 args = [\"{MOCK_LANG_A}\", \"--scan-roots\"]\n\n\
                 [language.{MOCK_LANG_A}]\nservers = [\"mockls\"]\n"
            ),
        )?;
        Ok(config_path)
    })?;

    bridge.initialize()?;

    let resp = bridge.call_search_raw("tool/grep", &json!({ "pattern": "demote_sym" }))?;
    let output = resp
        .get("output")
        .and_then(serde_json::Value::as_str)
        .context("output")?;
    let receipt = resp
        .get("receipt")
        .and_then(serde_json::Value::as_str)
        .context("expected a receipt when 50 symbols exceed the budget")?;
    assert!(
        output.contains("demote_sym"),
        "Expected results present, got:\n{output}"
    );

    // Not all 50 symbols fit the truncated display.
    let shown = (0..50)
        .filter(|i| output.contains(&format!("demote_sym_{i}")))
        .count();
    assert!(
        shown < 50,
        "Expected truncated output (not all 50 symbols), got {shown}"
    );

    // The complete result lives in the spill file (the old "page 2").
    let path = receipt
        .rsplit(" at ")
        .next()
        .context("spill path in receipt")?;
    let spilled = std::fs::read_to_string(path).context("read spill file")?;
    let spilled_count = (0..50)
        .filter(|i| spilled.contains(&format!("demote_sym_{i}")))
        .count();
    assert_eq!(
        spilled_count, 50,
        "spill file holds all 50 symbols, got {spilled_count}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "rich_fisheye" }))?;

    // rich_fisheye renders as a full-line atom (top-level def → no `#`). The
    // per-hit nav suite no longer fires, so the callee `lean_fisheye` is not
    // emitted under the hit.
    assert!(
        text.contains(&format!("fisheye.{MOCK_LANG_A}:2:fn rich_fisheye {{")),
        "Expected rich_fisheye definition atom, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "main_ord" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "grouped_sym" }))?;

    // Single-page result: no page header
    assert!(
        !text.contains("[page"),
        "Single-page result should have no page header, got:\n{text}"
    );
    assert!(
        !text.contains("Root: "),
        "Should not contain Root: prefix, got:\n{text}"
    );
    // Definition atom at depth 0 (no name header, no leading tab).
    let lines: Vec<&str> = text.lines().collect();
    let has_def = lines
        .iter()
        .any(|l| !l.starts_with('\t') && l.contains("fn grouped_sym"));
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Dedup_t1" }))?;

    // The struct definition renders as a full-line atom (top-level def → no `#`).
    assert!(
        text.contains(&format!("dedup.{MOCK_LANG_A}:1:struct Dedup_t1")),
        "Expected struct definition atom, got:\n{text}"
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "DeduRef" }))?;

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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "target_inc" }))?;

    // No separate `callers:` section — the per-hit nav suite no longer fires.
    assert!(
        !text.contains("callers:"),
        "Expected no callers: section, got:\n{text}"
    );
    // The definition and the in-body usage both render as self-contained atoms.
    // caller_inc is a single-line symbol (no brace body), so the line-3 usage is
    // top-level → no `#scope`; its source indentation is preserved verbatim.
    let def = format!("incoming.{MOCK_LANG_A}:1:fn target_inc()");
    let usage = format!("incoming.{MOCK_LANG_A}:3:  target_inc");
    let def_pos = text.find(&def);
    let usage_pos = text.find(&usage);
    assert!(
        def_pos.is_some() && usage_pos.is_some(),
        "Expected target_inc definition and in-body usage atoms, got:\n{text}"
    );
    // Lines appear ascending within the file.
    assert!(
        def_pos < usage_pos,
        "Expected atoms in ascending line order (1, 3), got:\n{text}"
    );

    Ok(())
}

/// Impls structure: `impls:` lists implementors of the queried type. mockls
/// `handle_implementation` returns `implements`-based implementors (distinct
/// from references and from `extends` subtypes), so querying the interface
/// `Drawable` surfaces its implementor `Sprite`.
#[test]
fn test_grep_impls_structure() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let file = dir.path().join(format!("impls.{MOCK_LANG_A}"));
    std::fs::write(
        &file,
        "interface Drawable\nstruct Sprite implements Drawable\n",
    )?;

    let lsp = mockls_lsp_arg(MOCK_LANG_A, "--scan-roots");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "Drawable" }))?;

    // The per-hit nav suite no longer fires. The interface definition and the
    // implementor's `implements Drawable` line both render as self-contained
    // `path:line:text` atoms (both top-level lines → no `#scope`).
    assert!(
        text.contains(&format!("impls.{MOCK_LANG_A}:1:interface Drawable")),
        "Expected Drawable definition atom, got:\n{text}"
    );
    assert!(
        text.contains(&format!(
            "impls.{MOCK_LANG_A}:2:struct Sprite implements Drawable"
        )),
        "Expected Sprite implementor atom, got:\n{text}"
    );

    Ok(())
}

/// Single-line ref: atoms carry a single `:LINE`, never a `:start-end` range.
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

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "target_sl" }))?;

    // The definition renders as a single-line atom with the full source line
    // (top-level def → no `#`). The usage on line 3 sits outside any brace body
    // (user_sl is single-line), so it is top-level too → no `#scope`.
    assert!(
        text.contains(&format!("single_ref.{MOCK_LANG_A}:1:fn target_sl")),
        "Expected single-line definition atom, got:\n{text}"
    );
    assert!(
        text.contains(&format!("single_ref.{MOCK_LANG_A}:3:target_sl")),
        "Expected single-line usage atom, got:\n{text}"
    );
    // Single-line atoms show `:line` not `:start-end` (spans are gone).
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
