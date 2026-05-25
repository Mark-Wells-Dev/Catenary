// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Cross-cutting integration tests for `LoggingServer`.
//!
//! Tests in this file exercise multi-sink dispatch, notification queue
//! threshold/dedup, and protocol message round-trip through the full
//! tracing Layer pipeline — scenarios that span multiple tickets and
//! don't fit naturally inside a single module's test suite.
//!
//! Each test uses `tracing::subscriber::with_default` (scoped per-test)
//! to avoid global subscriber conflicts in parallel test execution.

use anyhow::Result;
use catenary_mcp::source::Source;
use tempfile::tempdir;
use tracing_subscriber::layer::SubscriberExt;

use catenary_mcp::logging::message_db::MessageDbSink;
use catenary_mcp::logging::notification_queue::NotificationQueueSink;
use catenary_mcp::logging::test_support::{
    MsgRow, logging_test_db, message_count, query_all_messages,
};
use catenary_mcp::logging::{LoggingServer, Severity};

const MOCK_LANG_A: &str = "yX4Za";

// ── Multi-sink dispatch ────────────────────────────────────────────────

/// Verify that both sinks (notification queue, message DB) receive their
/// respective events through a single `LoggingServer` Layer.
#[test]
fn multi_sink_dispatch_routes_correctly() {
    let db = logging_test_db();
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let message_db = MessageDbSink::new(db.clone(), "s1".into());

    let server = LoggingServer::new();
    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![notifications.clone(), message_db]);

        // Protocol event (kind="lsp") → message DB with type "lsp".
        tracing::info!(
            kind = "lsp",
            method = "textDocument/hover",
            server = "rust-analyzer",
            payload = "{}",
            "outgoing"
        );

        // Warn event without kind → message DB with type "internal" + notification queue.
        tracing::warn!(source = Source::LspLifecycle.as_str(), "server crashed");

        // Debug event without kind → message DB with type "internal" only (below notification threshold).
        tracing::debug!("verbose trace");
    });

    let msgs = query_all_messages(&db);

    // All 3 events go to the unified message DB.
    assert_eq!(msgs.len(), 3, "expected 3 DB rows, got {}", msgs.len());

    // Protocol event is type "lsp".
    assert_eq!(msgs[0].r#type, "lsp");
    assert_eq!(msgs[0].method, "textDocument/hover");

    // Internal events are type "internal".
    assert_eq!(msgs[1].r#type, "internal");
    assert_eq!(msgs[2].r#type, "internal");

    // Notification queue: 1 warn event.
    let drained = notifications.drain();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].message, "server crashed");
}

/// Verify that notification queue threshold filtering works end-to-end
/// through the tracing Layer.
#[test]
fn notification_threshold_filters_through_layer() {
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![notifications.clone()]);

        tracing::debug!("below threshold");
        tracing::info!("below threshold");
        tracing::warn!(server = "a", "at threshold");
        tracing::error!(server = "b", "above threshold");
    });

    let drained = notifications.drain();
    assert_eq!(drained.len(), 2, "only warn + error should enqueue");
    assert_eq!(drained[0].severity, Severity::Warn);
    assert_eq!(drained[1].severity, Severity::Error);
}

/// Verify dedup works through the Layer — identical messages dedup,
/// different servers do not.
#[test]
fn notification_dedup_through_layer() {
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![notifications.clone()]);

        tracing::warn!(server = "ra", "server offline");
        tracing::warn!(server = "ra", "server offline"); // dedup
        tracing::warn!(server = "pylsp", "server offline"); // different server
    });

    let drained = notifications.drain();
    assert_eq!(
        drained.len(),
        2,
        "identical message with same server should dedup"
    );
}

// ── Protocol message round-trip ────────────────────────────────────────

/// Spawn mockls, fire an LSP request, and verify the full scope chain:
/// MCP tool call's correlation ID appears as the LSP request's `parent_id`.
#[tokio::test]
async fn lsp_request_scope_chain() -> Result<()> {
    let db = logging_test_db();
    let message_db = MessageDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();
    server.activate(vec![message_db]);

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

    let msgs = query_all_messages(&db);
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
    let db = logging_test_db();
    let message_db = MessageDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();
    server.activate(vec![message_db]);

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
    let msgs = query_all_messages(&db);
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
    let db = logging_test_db();
    let message_db = MessageDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();

    let subscriber = tracing_subscriber::registry().with(server.clone());
    tracing::subscriber::with_default(subscriber, || {
        server.activate(vec![message_db]);

        // 3 protocol events.
        for kind in &["lsp", "mcp", "hook"] {
            tracing::info!(kind = *kind, method = "test", payload = "{}", "protocol");
        }

        // 2 non-protocol events.
        tracing::warn!("trace event 1");
        tracing::info!("trace event 2");
    });

    let msgs = query_all_messages(&db);

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
    let db = logging_test_db();
    let notifications = NotificationQueueSink::new(Severity::Warn);
    let message_db = MessageDbSink::new(db.clone(), "s1".into());
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
        server.activate(vec![notifications.clone(), message_db]);
        assert_eq!(server.buffered_len(), 0);
    });

    // Message DB got both events.
    assert_eq!(message_count(&db), 2);

    // Notification queue got both (both are warn, distinct keys).
    let drained = notifications.drain();
    assert_eq!(drained.len(), 2);
}

/// Verify that LSP server stderr output is captured as attributed tracing
/// events stored in the message DB with `source = "lsp.stderr"`.
#[tokio::test]
async fn stderr_captured_with_source_and_server() -> Result<()> {
    let db = logging_test_db();
    let message_db = MessageDbSink::new(db.clone(), "s1".into());
    let server = LoggingServer::new();
    server.activate(vec![message_db]);

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
    )?;
    client.initialize(&[dir.path().to_path_buf()], None).await?;

    // Poll for the stderr event to appear (async reader task).
    let mut found = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let c = db.lock().expect("lock");
        let count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM messages \
                 WHERE type = 'lsp' AND level = 'debug' \
                   AND payload LIKE '%test stderr capture%'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        drop(c);
        if count > 0 {
            found = true;
            break;
        }
    }
    assert!(found, "stderr line should appear in message DB");

    // Verify server attribution, method, and payload.
    let c = db.lock().expect("lock");
    let (stored_server, stored_method, stored_payload): (String, String, String) = c
        .query_row(
            "SELECT server, method, payload FROM messages \
             WHERE type = 'lsp' AND level = 'debug' \
               AND payload LIKE '%test stderr capture%' \
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query stderr row");
    drop(c);

    assert_eq!(
        stored_server, MOCK_LANG_A,
        "server should be the mockls name"
    );
    assert_eq!(stored_method, "stderr", "method should be 'stderr'");
    assert!(
        stored_payload.contains(stderr_text),
        "payload should contain the stderr text, got: {stored_payload}"
    );

    drop(guard);
    Ok(())
}
