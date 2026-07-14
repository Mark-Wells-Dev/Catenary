// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for LSP client functionality using mockls.

use anyhow::Result;
use catenary_cli::logging::LoggingServer;
use tempfile::tempdir;

fn test_logging() -> LoggingServer {
    LoggingServer::new()
}

const MOCK_LANG_A: &str = "yX4Za";

#[tokio::test]
async fn test_mockls_initialize() -> Result<()> {
    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    let result = client.initialize(&[dir.path().to_path_buf()], None).await?;

    let caps = &result["capabilities"];
    assert!(caps.get("hoverProvider").is_some_and(|v| !v.is_null()));
    assert!(caps.get("definitionProvider").is_some_and(|v| !v.is_null()));

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_mockls_initialize_workspace_folders() -> Result<()> {
    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--workspace-folders"],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    let result = client.initialize(&[dir.path().to_path_buf()], None).await?;

    assert!(
        result["capabilities"]
            .get("hoverProvider")
            .is_some_and(|v| !v.is_null())
    );
    assert!(client.supports_workspace_folders());

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_mockls_document_lifecycle() -> Result<()> {
    let dir = tempdir()?;
    let script_path = dir.path().join(format!("lifecycle.{MOCK_LANG_A}"));
    std::fs::write(&script_path, "let MY_VAR\n")?;

    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let uri = format!("file://{}", script_path.display());

    // Open
    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Change
    client.did_change(&uri, 2, "let MY_VAR\nMY_VAR\n").await?;

    // Close
    client.did_close(&uri).await?;

    client.shutdown().await?;
    Ok(())
}

/// Verifies client capabilities sent during `initialize` (Gap 7).
///
/// mockls `--log-init-params` writes the full initialize request params
/// to a file. We parse it and assert the capabilities Catenary advertises.
///
/// Uses a **blessed** server name (`clangd`) so the full diagnostics-capable
/// shape is advertised — including `publishDiagnostics` (diagnostics-debt 04b:
/// an unverified server has its diagnostics advertisement withheld, which is
/// covered by `enrichment_only_server_gets_no_diagnostics_advertisement`). The
/// mockls binary and the language id stay `MOCK_LANG_A`; only Catenary's internal
/// server identity is the blessed name.
#[tokio::test]
async fn test_client_capabilities() -> Result<()> {
    let dir = tempdir()?;
    let init_log = dir.path().join("init_params.json");
    let bin = env!("CARGO_BIN_EXE_mockls");

    let log_path = init_log.to_str().ok_or_else(|| anyhow::anyhow!("path"))?;
    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--log-init-params", log_path],
        MOCK_LANG_A,
        "clangd",
        test_logging(),
        None,
        None,
        "",
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let params_json = std::fs::read_to_string(&init_log)?;
    let params: serde_json::Value = serde_json::from_str(&params_json)?;
    let caps = &params["capabilities"];
    let text_doc = &caps["textDocument"];

    // Capabilities that MUST be present
    assert!(
        text_doc.get("synchronization").is_some(),
        "synchronization capability missing"
    );
    assert!(
        text_doc.get("definition").is_some(),
        "definition capability missing"
    );
    assert!(
        text_doc.get("references").is_some(),
        "references capability missing"
    );
    assert!(
        text_doc.get("documentSymbol").is_some(),
        "documentSymbol capability missing"
    );
    assert!(
        text_doc.get("callHierarchy").is_some(),
        "callHierarchy capability missing"
    );
    assert!(
        text_doc.get("typeHierarchy").is_some(),
        "typeHierarchy capability missing"
    );
    assert!(
        text_doc.get("implementation").is_some(),
        "implementation capability missing"
    );
    assert!(
        text_doc.get("declaration").is_some(),
        "declaration capability missing"
    );
    assert!(
        text_doc.get("typeDefinition").is_some(),
        "typeDefinition capability missing"
    );

    // `publishDiagnostics.versionSupport` must be true
    assert_eq!(
        text_doc["publishDiagnostics"]["versionSupport"], true,
        "publishDiagnostics.versionSupport must be true"
    );

    // `window.workDoneProgress` must be true
    assert_eq!(
        caps["window"]["workDoneProgress"], true,
        "window.workDoneProgress must be true"
    );

    // Capabilities that MUST NOT be present (tools were removed)
    assert!(
        text_doc.get("hover").is_none(),
        "hover capability should not be advertised"
    );
    assert!(
        text_doc.get("codeAction").is_some(),
        "codeAction capability SHOULD be advertised"
    );
    assert!(
        text_doc.get("rename").is_none(),
        "rename capability should not be advertised"
    );
    assert!(
        text_doc.get("completion").is_none(),
        "completion capability should not be advertised"
    );
    assert!(
        text_doc.get("signatureHelp").is_none(),
        "signatureHelp capability should not be advertised"
    );
    assert!(
        text_doc.get("formatting").is_none(),
        "formatting capability should not be advertised"
    );

    client.shutdown().await?;
    Ok(())
}

/// diagnostics-debt 04b, deliverable 1: an **unverified** server (a custom name
/// absent from the blessed manifest) is advertised **no diagnostics capability** —
/// neither `textDocument.diagnostic` (pull) nor `textDocument.publishDiagnostics`
/// (push). Every non-diagnostics capability survives, so its grep/glob enrichment
/// is untouched. This is the "no advertisement" acceptance pinned at the wire.
#[tokio::test]
async fn enrichment_only_server_gets_no_diagnostics_advertisement() -> Result<()> {
    let dir = tempdir()?;
    let init_log = dir.path().join("init_params.json");
    let bin = env!("CARGO_BIN_EXE_mockls");

    let log_path = init_log.to_str().ok_or_else(|| anyhow::anyhow!("path"))?;
    // `yX4Za` is absent from the blessed manifest ⇒ unverified ⇒ enrichment-only.
    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--log-init-params", log_path],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let params_json = std::fs::read_to_string(&init_log)?;
    let params: serde_json::Value = serde_json::from_str(&params_json)?;
    let text_doc = &params["capabilities"]["textDocument"];

    // No diagnostics advertisement — both channels withheld.
    assert!(
        text_doc.get("diagnostic").is_none(),
        "an enrichment-only server must not be advertised the pull capability",
    );
    assert!(
        text_doc.get("publishDiagnostics").is_none(),
        "an enrichment-only server must not be advertised the push capability",
    );
    // Enrichment (navigation) capabilities survive — grep/glob is untouched.
    assert!(
        text_doc.get("definition").is_some(),
        "definition (enrichment) must still be advertised",
    );
    assert!(
        text_doc.get("documentSymbol").is_some(),
        "documentSymbol (enrichment) must still be advertised",
    );

    client.shutdown().await?;
    Ok(())
}

// ── Settle tests ─────────────────────────────────────────────────────

/// Verifies that `await_idle` waits through a `Busy` → `Healthy` lifecycle
/// transition and returns `Settled` once the tree is quiet.
///
/// mockls `--indexing-delay 200` sends `$/progress` begin, sleeps 200ms,
/// then sends `$/progress` end. `await_idle` sees `Busy` → skips tree
/// walking, then resumes after `Healthy` and detects quiet.
#[tokio::test]
async fn test_settle_waits_through_busy_to_healthy() -> Result<()> {
    use catenary_cli::lsp::settle::{IdleDetector, SettleResult, await_idle};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--indexing-delay", "200"],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    // await_idle starts while mockls is still in its indexing delay (Busy).
    let server = Arc::clone(client.server());
    let cancel = CancellationToken::new();
    let detector = IdleDetector::after_activity(0);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        await_idle(&server, detector, cancel, MOCK_LANG_A),
    )
    .await
    .expect("await_idle should complete within 5s");

    assert_eq!(result, SettleResult::Settled);

    client.shutdown().await?;
    Ok(())
}

/// Verifies that `await_idle` detects a quiet tree after brief CPU activity.
///
/// mockls `--cpu-on-initialized 100` burns CPU for 100ms on the
/// `initialized` notification. `await_idle`'s tree walks catch the activity,
/// then detect silence once the burn ends.
#[tokio::test]
async fn test_settle_returns_settled_on_quiet_tree() -> Result<()> {
    use catenary_cli::lsp::settle::{IdleDetector, SettleResult, await_idle};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--cpu-on-initialized", "100"],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    // await_idle starts while mockls is burning CPU from initialized.
    let server = Arc::clone(client.server());
    let cancel = CancellationToken::new();
    let detector = IdleDetector::after_activity(0);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        await_idle(&server, detector, cancel, MOCK_LANG_A),
    )
    .await
    .expect("await_idle should complete within 5s");

    assert_eq!(result, SettleResult::Settled);

    client.shutdown().await?;
    Ok(())
}

/// Verifies that `Connection::request` retries on `ContentModified` (-32801).
///
/// mockls `--content-modified-once` returns `ContentModified` on the first
/// `textDocument/definition` request, then succeeds on retry.
#[tokio::test]
async fn test_content_modified_retry() -> Result<()> {
    let dir = tempdir()?;
    let script_path = dir.path().join(format!("retry.{MOCK_LANG_A}"));
    std::fs::write(&script_path, "fn hello\nhello\n")?;

    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--content-modified-once"],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    let uri = format!("file://{}", script_path.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "fn hello\nhello\n")
        .await?;

    // First internal attempt gets ContentModified, retry succeeds
    let result = client.definition(&uri, 1, 0).await?;
    assert!(
        result.get("uri").is_some() || result.get(0).is_some(),
        "definition should return a location after retry"
    );

    client.shutdown().await?;
    Ok(())
}

/// Verifies lifecycle transitions: Initializing → Probing → Healthy.
///
/// After `initialize()`, server is `Probing`. The first successful tool
/// request transitions it to `Healthy`.
#[tokio::test]
async fn test_lifecycle_probing_to_healthy_on_tool_request() -> Result<()> {
    use catenary_cli::lsp::state::ServerLifecycle;

    let dir = tempdir()?;
    let script_path = dir.path().join(format!("probe.{MOCK_LANG_A}"));
    std::fs::write(&script_path, "fn hello\nhello\n")?;

    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    // Before init: Initializing
    assert_eq!(client.lifecycle(), ServerLifecycle::Initializing);

    client.initialize(&[dir.path().to_path_buf()], None).await?;

    // After init: Probing
    assert_eq!(client.lifecycle(), ServerLifecycle::Probing);

    let uri = format!("file://{}", script_path.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "fn hello\nhello\n")
        .await?;

    // Tool request succeeds → Probing → Healthy
    let _result = client.definition(&uri, 1, 0).await?;
    assert_eq!(client.lifecycle(), ServerLifecycle::Healthy);

    client.shutdown().await?;
    Ok(())
}

/// Verifies the health probe transitions Probing → Healthy.
#[tokio::test]
async fn test_health_probe_transitions_to_healthy() -> Result<()> {
    use catenary_cli::lsp::state::ServerLifecycle;

    let dir = tempdir()?;
    let script_path = dir.path().join(format!("probe.{MOCK_LANG_A}"));
    std::fs::write(&script_path, "fn hello\nhello\n")?;

    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        None,
        "",
    )?;

    client.initialize(&[dir.path().to_path_buf()], None).await?;
    assert_eq!(client.lifecycle(), ServerLifecycle::Probing);

    let uri = format!("file://{}", script_path.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "fn hello\nhello\n")
        .await?;

    // Health probe sends documentSymbol → Probing → Healthy
    assert!(client.run_health_probe(&uri).await);
    assert_eq!(client.lifecycle(), ServerLifecycle::Healthy);

    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn test_server_env_passed_to_process() -> Result<()> {
    let dir = tempdir()?;
    let bin = env!("CARGO_BIN_EXE_mockls");

    let mut env = std::collections::HashMap::new();
    env.insert(
        "CATENARY_TEST_ENV_VAR".to_string(),
        "hello_from_config".to_string(),
    );

    let mut client = catenary_cli::lsp::LspClient::spawn(
        bin,
        &[MOCK_LANG_A, "--report-env", "CATENARY_TEST_ENV_VAR"],
        MOCK_LANG_A,
        MOCK_LANG_A,
        test_logging(),
        None,
        Some(&env),
        "",
    )?;

    let result = client.initialize(&[dir.path().to_path_buf()], None).await?;

    let version = result["serverInfo"]["version"]
        .as_str()
        .expect("serverInfo.version should be present");
    assert_eq!(version, "hello_from_config");

    client.shutdown().await?;
    Ok(())
}

/// Smoke test: spawn real rust-analyzer via rustup and complete the
/// initialize handshake.
///
/// Ignored by default — requires a working Rust stable toolchain with
/// rust-analyzer installed. Run with:
///     `make test-ignored T=test_real_rust_analyzer_initialize`
#[tokio::test]
#[ignore = "requires real rust-analyzer"]
async fn test_real_rust_analyzer_initialize() -> Result<()> {
    let dir = tempdir()?;

    // Create a minimal Cargo.toml so rust-analyzer has a valid project.
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::create_dir_all(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/lib.rs"), "")?;

    let mut client = catenary_cli::lsp::LspClient::spawn(
        "rustup",
        &["run", "stable", "rust-analyzer"],
        "rust",
        "rust-analyzer",
        test_logging(),
        None,
        None,
        "",
    )?;

    let result = client.initialize(&[dir.path().to_path_buf()], None).await?;

    assert!(
        result.get("capabilities").is_some(),
        "initialize response should contain capabilities"
    );

    client.shutdown().await?;
    Ok(())
}
