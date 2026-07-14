// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for LSP message logging via `LoggingServer` + the
//! in-memory `MessageRecorder` test sink.

use anyhow::Result;

use catenary_cli::logging::test_support::{
    MsgRow, query_all_messages, setup_logging, spawn_initialized_client,
};

const MOCK_LANG_A: &str = "yX4Za";

/// Verify that a hover request/response pair is logged with correct
/// correlation ID linking (both rows share the same `request_id`).
#[tokio::test]
async fn test_lsp_log_request_response() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) =
        spawn_initialized_client(env!("CARGO_BIN_EXE_mockls"), logging, MOCK_LANG_A).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;
    let _def = client.definition(&uri, 0, 4).await?;

    let msgs = query_all_messages(&conn);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(
        def_msgs.len() >= 2,
        "expected at least 2 definition messages (request + response), got {}",
        def_msgs.len()
    );

    // All should be "lsp" type
    for m in &def_msgs {
        assert_eq!(m.r#type, "lsp");
    }

    // Both request and response carry the same parent_id (exchange UUID)
    let request = def_msgs[0];
    let response = def_msgs[1];
    assert_eq!(
        response.parent_id, request.parent_id,
        "response parent_id should match request"
    );

    Ok(())
}

/// Verify that an outbound notification is logged.
#[tokio::test]
async fn test_lsp_log_notification() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) =
        spawn_initialized_client(env!("CARGO_BIN_EXE_mockls"), logging, MOCK_LANG_A).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let X\n")?;
    let uri = format!("file://{}", file.display());

    client.did_open(&uri, MOCK_LANG_A, 1, "let X\n").await?;

    let msgs = query_all_messages(&conn);
    let did_open_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/didOpen")
        .collect();

    assert!(
        !did_open_msgs.is_empty(),
        "expected at least 1 didOpen message"
    );
    assert_eq!(did_open_msgs[0].r#type, "lsp");

    Ok(())
}

/// Verify that inbound notifications (e.g., `textDocument/publishDiagnostics`)
/// are logged.
#[tokio::test]
async fn test_lsp_log_inbound_notification() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) =
        spawn_initialized_client(env!("CARGO_BIN_EXE_mockls"), logging, MOCK_LANG_A).await?;

    // mockls publishes diagnostics for files with "error" in the content
    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "error here\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "error here\n")
        .await?;

    // Give the server time to publish diagnostics
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let msgs = query_all_messages(&conn);
    let diag_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/publishDiagnostics")
        .collect();

    assert!(
        !diag_msgs.is_empty(),
        "expected at least 1 publishDiagnostics message"
    );
    assert_eq!(diag_msgs[0].r#type, "lsp");

    Ok(())
}

/// Verify that `set_parent_id` propagates to request and response messages.
#[tokio::test]
async fn test_lsp_log_parent_id() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (mut client, dir) =
        spawn_initialized_client(env!("CARGO_BIN_EXE_mockls"), logging, MOCK_LANG_A).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Use a UUID as parent (simulating an MCP tool call context)
    let parent_id = "scope-42".to_string();
    client.set_parent_id(Some(parent_id.clone()));

    let _def = client.definition(&uri, 0, 4).await?;

    let msgs = query_all_messages(&conn);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(
        def_msgs.len() >= 2,
        "expected at least 2 definition messages (request + response), got {}",
        def_msgs.len(),
    );

    // Request carries the MCP parent_id
    assert_eq!(def_msgs[0].parent_id.as_deref(), Some(parent_id.as_str()));
    // Response carries the same parent_id (pair-merge via UUID)
    assert_eq!(def_msgs[1].parent_id.as_deref(), Some(parent_id.as_str()),);

    Ok(())
}

/// Verify that the response message in DB has `method` annotated from the
/// pending map (not from the raw JSON-RPC response, which has no method).
#[tokio::test]
async fn test_lsp_log_pending_method_annotation() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) =
        spawn_initialized_client(env!("CARGO_BIN_EXE_mockls"), logging, MOCK_LANG_A).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());

    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;
    let _def = client.definition(&uri, 0, 4).await?;

    let msgs = query_all_messages(&conn);
    let def_msgs: Vec<&MsgRow> = msgs
        .iter()
        .filter(|m| m.method == "textDocument/definition")
        .collect();

    assert!(
        def_msgs.len() >= 2,
        "expected at least 2 definition messages"
    );

    // The response (second message) should have method="textDocument/definition"
    // even though JSON-RPC responses don't carry a method field.
    assert_eq!(
        def_msgs[1].method, "textDocument/definition",
        "response should be annotated with the request method from the pending map"
    );

    Ok(())
}
