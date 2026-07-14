// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for per-server behavior casing (misc 157).
//!
//! The engine-internal per-server table
//! (`catenary_cli::lsp::server_behavior`) shapes each server's `initialize`:
//! rust-analyzer is cased to never receive the `textDocument.diagnostic` client
//! capability and never be sent `textDocument/diagnostic` (advertised pull *or*
//! best-effort probe), while every other server receives today's shape unchanged.
//!
//! These tests drive the whole daemon (not the pure builder) so the casing is
//! verified on the wire: `--log-init-params` captures the initialize request
//! params, and `--request-log` records every request the daemon issued.

mod common;

use anyhow::{Context, Result};
use serde_json::Value;

use common::{BridgeProcess, mockls_lsp_arg, read_merged_log};

/// The cased server name: the engine table suppresses pull for `rust-analyzer`.
const CASED: &str = "rust-analyzer";
/// An un-cased but BLESSED server: the `mockls-event` persona (event discipline,
/// no pull suppression; diagnostics-debt 04c). It must be blessed, not merely
/// unknown — an enrichment-only (unverified) server would ALSO lose the
/// `diagnostic` capability, which would conflate "un-cased" with "unverified" and
/// defeat this file's isolation of the rust-analyzer casing as the sole variable.
const UNCASED: &str = "mockls-event";

/// Counts request-log lines whose `method` equals `method`. mockls's
/// `--request-log` appends one `{"method":"..."}` object per handled request;
/// `call_diagnostics` runs the pipeline to completion before returning, so the
/// count is authoritative with no sleep or poll.
fn count_request_method(log: &str, method: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v.get("method").and_then(Value::as_str) == Some(method))
        .count()
}

/// A server cased to suppress pull receives an `initialize` WITHOUT the
/// `textDocument.diagnostic` client capability.
#[test]
fn cased_server_initialize_omits_diagnostic_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let init_log = dir.path().join("init_params.json");
    let init_arg = init_log.to_str().context("init log path")?;
    let file = dir.path().join(format!("test.{CASED}"));
    std::fs::write(&file, "code\n")?;

    let lsp = mockls_lsp_arg(CASED, &format!("--log-init-params {init_arg}"));
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("root")?)?;
    bridge.initialize()?;

    // Drive one diagnostics run so the server is spawned and initialized.
    let _ = bridge.call_diagnostics(file.to_str().context("file")?)?;

    let params_json = std::fs::read_to_string(&init_log)
        .context("mockls --log-init-params should have written the initialize params")?;
    let params: Value = serde_json::from_str(&params_json)?;
    let text_doc = &params["capabilities"]["textDocument"];

    assert!(
        text_doc.get("diagnostic").is_none(),
        "cased server ({CASED}) must not receive textDocument.diagnostic; got: {text_doc}",
    );
    // The rest of the capability shape is intact — only `diagnostic` is dropped.
    assert!(
        text_doc.get("definition").is_some(),
        "cased server must still receive the other capabilities; got: {text_doc}",
    );
    assert!(
        text_doc.get("publishDiagnostics").is_some(),
        "cased server keeps publishDiagnostics; got: {text_doc}",
    );

    Ok(())
}

/// An un-cased server receives today's shape unchanged — `textDocument.diagnostic`
/// present.
#[test]
fn uncased_server_initialize_carries_diagnostic_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let init_log = dir.path().join("init_params.json");
    let init_arg = init_log.to_str().context("init log path")?;
    let file = dir.path().join(format!("test.{UNCASED}"));
    std::fs::write(&file, "code\n")?;

    let lsp = mockls_lsp_arg(UNCASED, &format!("--log-init-params {init_arg}"));
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("root")?)?;
    bridge.initialize()?;

    let _ = bridge.call_diagnostics(file.to_str().context("file")?)?;

    let params_json = std::fs::read_to_string(&init_log)
        .context("mockls --log-init-params should have written the initialize params")?;
    let params: Value = serde_json::from_str(&params_json)?;
    let text_doc = &params["capabilities"]["textDocument"];

    assert!(
        text_doc.get("diagnostic").is_some(),
        "un-cased server ({UNCASED}) must still advertise textDocument.diagnostic; got: {text_doc}",
    );

    Ok(())
}

/// A pull-suppressed server is never sent `textDocument/diagnostic` even when it
/// advertises `diagnosticProvider` and never pushes — the case gates both the
/// advertised-pull path (via `supports_pull_diagnostics`) and the best-effort
/// probe, so the never-heard file resolves without any pull on the wire.
#[test]
fn pull_suppressed_server_never_receives_diagnostic_request() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let rlog_arg = rlog.to_str().context("rlog path")?;
    let file = dir.path().join(format!("test.{CASED}"));
    std::fs::write(&file, "code\n")?;

    // The mockls advertises pull AND suppresses push: absent the casing, the
    // daemon would pull (advertised or best-effort). The casing must block it.
    let lsp = mockls_lsp_arg(
        CASED,
        &format!("--pull-diagnostics --no-push-diagnostics --request-log {rlog_arg}"),
    );
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("root")?)?;
    bridge.initialize()?;

    let _ = bridge.call_diagnostics(file.to_str().context("file")?)?;

    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "textDocument/diagnostic"),
        0,
        "a pull-suppressed server must never be sent textDocument/diagnostic; log:\n{rlog_text}",
    );

    Ok(())
}
