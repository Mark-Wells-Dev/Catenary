// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Cross-cutting integration tests for `LoggingServer`.
//!
//! Tests in this file exercise multi-sink dispatch and protocol message
//! round-trip through the full tracing Layer pipeline — scenarios that span
//! multiple tickets and don't fit naturally inside a single module's test
//! suite. (The user-notification queue retired in tui-rework 04, so its
//! threshold/dedup tests are gone with it.)
//!
//! The firehose half is captured by an in-memory `MessageRecorder` (the
//! observability rewrite retired the `messages` DB sink); the JSONL write
//! path itself is covered by `jsonl_sink`'s own unit tests.
//!
//! Each test uses `tracing::subscriber::with_default` (scoped per-test)
//! to avoid global subscriber conflicts in parallel test execution.

use std::time::Duration;

use anyhow::Result;
use catenary_mcp::source::Source;
use tempfile::tempdir;
use tracing_subscriber::layer::SubscriberExt;

use catenary_mcp::logging::LoggingServer;
use catenary_mcp::logging::test_support::{
    MessageRecorder, MsgRow, message_count, query_all_messages,
};

const MOCK_LANG_A: &str = "yX4Za";

// ── Multi-sink dispatch ────────────────────────────────────────────────

/// Verify that events route through a single `LoggingServer` Layer to the
/// message recorder with the correct `type` classification.
#[test]
fn multi_sink_dispatch_routes_correctly() {
    let recorder = MessageRecorder::new();

    let server = LoggingServer::new();
    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![recorder.clone()]);

        // Protocol event (kind="lsp") → recorder with type "lsp".
        tracing::info!(
            kind = "lsp",
            method = "textDocument/hover",
            server = "rust-analyzer",
            payload = "{}",
            "outgoing"
        );

        // Warn event without kind → recorder with type "internal".
        tracing::warn!(source = Source::LspLifecycle.as_str(), "server crashed");

        // Debug event without kind → recorder with type "internal".
        tracing::debug!("verbose trace");
    });

    let msgs = query_all_messages(&recorder);

    // All 3 events go to the recorder.
    assert_eq!(msgs.len(), 3, "expected 3 rows, got {}", msgs.len());

    // Protocol event is type "lsp".
    assert_eq!(msgs[0].r#type, "lsp");
    assert_eq!(msgs[0].method, "textDocument/hover");

    // Internal events are type "internal".
    assert_eq!(msgs[1].r#type, "internal");
    assert_eq!(msgs[2].r#type, "internal");
}

// ── Protocol message round-trip ────────────────────────────────────────

/// Spawn mockls, fire an LSP request, and verify the full scope chain:
/// MCP tool call's correlation ID appears as the LSP request's `parent_id`.
#[tokio::test]
async fn lsp_request_scope_chain() -> Result<()> {
    let recorder = MessageRecorder::new();
    let server = LoggingServer::new();
    server.activate(vec![recorder.clone()]);

    let subscriber = tracing_subscriber::registry().with(server.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        server.clone(),
        None,
        None,
        "",
    )?;
    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Simulate an MCP tool call context by setting parent_id.
    let mcp_parent = "scope-mcp".to_string();
    client.set_parent_id(Some(mcp_parent.clone()));

    let _def = client.definition(&uri, 0, 4).await?;

    let msgs = query_all_messages(&recorder);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(
        def_msgs.len() >= 2,
        "expected request + response, got {}",
        def_msgs.len()
    );

    // Request carries the MCP parent_id.
    assert_eq!(
        def_msgs[0].parent_id.as_deref(),
        Some(mcp_parent.as_str()),
        "request parent_id should be the MCP parent UUID"
    );

    // Both carry the same parent_id (pair-merge key).
    assert!(
        def_msgs[0].parent_id.is_some(),
        "request should have parent_id"
    );
    assert_eq!(
        def_msgs[0].parent_id, def_msgs[1].parent_id,
        "request and response should share parent_id"
    );

    drop(guard);
    Ok(())
}

/// Verify that `pair_merge` semantics are preserved: request and response
/// share the same `parent_id`. Without a tool-call scope, both are `None`.
#[tokio::test]
async fn pair_merge_still_works() -> Result<()> {
    let recorder = MessageRecorder::new();
    let server = LoggingServer::new();
    server.activate(vec![recorder.clone()]);

    let subscriber = tracing_subscriber::registry().with(server.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        server.clone(),
        None,
        None,
        "",
    )?;
    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    let _def = client.definition(&uri, 0, 4).await?;

    // Find the definition request/response pair.
    let msgs = query_all_messages(&recorder);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(def_msgs.len() >= 2, "expected at least request + response");

    // Without a tool-call scope, both request and response have
    // the same parent_id (both None — no scope UUID was provided).
    assert_eq!(
        def_msgs[0].parent_id, def_msgs[1].parent_id,
        "request and response should share the same parent_id"
    );

    drop(guard);
    Ok(())
}

/// Verify that the unified sink routes all events without duplication:
/// protocol events get `type = "lsp"|"mcp"|"hook"`, internal events
/// get `type = "internal"`.
#[test]
fn unified_sink_type_column_correct() {
    let recorder = MessageRecorder::new();
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![recorder.clone()]);

        // 3 protocol events.
        for kind in &["lsp", "mcp", "hook"] {
            tracing::info!(kind = *kind, method = "test", payload = "{}", "protocol");
        }

        // 2 non-protocol events.
        tracing::warn!("trace event 1");
        tracing::info!("trace event 2");
    });

    let msgs = query_all_messages(&recorder);

    let protocol_count = msgs
        .iter()
        .filter(|m| matches!(m.r#type.as_str(), "lsp" | "mcp" | "hook"))
        .count();
    let internal_count = msgs.iter().filter(|m| m.r#type == "internal").count();

    assert_eq!(protocol_count, 3, "3 protocol events");
    assert_eq!(internal_count, 2, "2 internal events");
    assert_eq!(msgs.len(), 5, "no duplication");
}

/// Verify that `LoggingServer::buffered_len` reports correctly during
/// the bootstrap phase, and that all buffered events are drained to
/// sinks on activation.
#[test]
fn bootstrap_buffer_drains_to_all_sinks() {
    let recorder = MessageRecorder::new();
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        // Bootstrap: events buffered.
        tracing::warn!(source = Source::ConfigParse.as_str(), "bad TOML");
        tracing::warn!(
            source = Source::ConfigParse.as_str(),
            server = "x",
            "bad key"
        );
        assert_eq!(server.buffered_len(), 2);

        // Activate: buffer drains to sinks.
        server.activate(vec![recorder.clone()]);
        assert_eq!(server.buffered_len(), 0);
    });

    // Recorder got both events.
    assert_eq!(message_count(&recorder), 2);
}

/// Verify that LSP server stderr output is captured as attributed tracing
/// events recorded with `method = "stderr"` and the server name attached.
#[tokio::test]
async fn stderr_captured_with_source_and_server() -> Result<()> {
    let recorder = MessageRecorder::new();
    let server = LoggingServer::new();
    server.activate(vec![recorder.clone()]);

    let subscriber = tracing_subscriber::registry().with(server.clone());
    let guard = tracing::subscriber::set_default(subscriber);

    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");
    let stderr_text = "mockls: test stderr capture line";
    let mut client = catenary_mcp::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--stderr-message", stderr_text],
        MOCK_LANG_A,
        MOCK_LANG_A,
        server.clone(),
        None,
        None,
        "",
    )?;
    client.initialize(&[dir.path().to_path_buf()], None).await?;

    // Poll for the stderr event to appear (async reader task).
    let mut stderr_row: Option<MsgRow> = None;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if let Some(row) = query_all_messages(&recorder).into_iter().find(|m| {
            m.r#type == "lsp" && m.level == "debug" && m.payload.contains("test stderr capture")
        }) {
            stderr_row = Some(row);
            break;
        }
    }

    let row = stderr_row.expect("stderr line should appear in the recorder");

    assert_eq!(row.server, MOCK_LANG_A, "server should be the mockls name");
    assert_eq!(row.method, "stderr", "method should be 'stderr'");
    assert!(
        row.payload.contains(stderr_text),
        "payload should contain the stderr text, got: {}",
        row.payload
    );

    drop(guard);
    Ok(())
}
