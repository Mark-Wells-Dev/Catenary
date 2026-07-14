// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the whole-root diagnostics scope
//! (`catenary diagnostics .`, workstream 37 ticket 04).
//!
//! Routing is capability-and-scope-based: a whole tracked root served by a
//! `workspace/diagnostic`-capable server takes one workspace pull (no per-file
//! `didOpen` churn, clean files collapsed to a count); a sub-root path set, or a
//! server without the capability, falls back to the per-file fan-out.

mod common;

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};

use common::{BridgeProcess, POLL_BACKSTOP, POLL_SPACING, mockls_lsp_arg, read_merged_log};

// The `mockls-event` persona is the blessed base (diagnostics-debt 04c): its
// bundle is empty, so a test that exercises the workspace/scan behaviour passes
// `--workspace-diagnostics --scan-roots` explicitly (extending the base) and the
// no-capability fallback test passes neither — the persona blesses without
// forcing any discipline. The value doubles as the server key, language, and
// file extension.
const MOCK_LANG: &str = "mockls-event";

/// Counts JSON-RPC request-log lines whose `method` equals `method`.
fn count_request_method(log: &str, method: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v.get("method").and_then(serde_json::Value::as_str) == Some(method))
        .count()
}

/// Whether the notification log records any entry for `method`.
fn has_notification_method(log: &str, method: &str) -> bool {
    count_notification_method(log, method) > 0
}

/// Counts notification-log entries for `method`.
fn count_notification_method(log: &str, method: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v.get("method").and_then(serde_json::Value::as_str) == Some(method))
        .count()
}

/// Polls the (merged) notification log until the per-root server is fully ready
/// with capabilities set.
///
/// The daemon's eager health probe opens and closes the first covered file
/// *after* it has read the initialize response and set the server's
/// capabilities, so the probe's `textDocument/didClose` is a positive signal
/// that a `workspace/diagnostic`-capable client is live — stronger than the
/// `__instance_root` init marker, which mockls writes before the daemon has
/// processed the capabilities (a race the whole-root route would lose by falling
/// back to fan-out).
fn wait_for_server_ready(nlog_base: &Path) {
    let deadline = Instant::now() + POLL_BACKSTOP;
    loop {
        if has_notification_method(&read_merged_log(nlog_base), "textDocument/didClose") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "mockls server not ready within backstop; log:\n{}",
            read_merged_log(nlog_base)
        );
        std::thread::sleep(POLL_SPACING);
    }
}

// ─── Whole root + capable server → one workspace/diagnostic ─────────

/// `catenary diagnostics .` against a `workspace/diagnostic`-capable server
/// takes exactly one workspace pull: no per-file `didOpen`, and the receipt
/// collapses the clean files to a count while listing the dirty ones.
#[test]
fn whole_root_capable_uses_one_workspace_request() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let nlog = logs.path().join("notifications.jsonl");

    // Two dirty files (content marker `DIRTY`), three clean ones.
    for name in ["bad0", "bad1"] {
        std::fs::write(dir.path().join(format!("{name}.{MOCK_LANG}")), "DIRTY\n")?;
    }
    for name in ["ok0", "ok1", "ok2"] {
        std::fs::write(dir.path().join(format!("{name}.{MOCK_LANG}")), "fine\n")?;
    }

    let rlog_arg = rlog.to_str().context("rlog path")?;
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--workspace-diagnostics --scan-roots \
             --request-log {rlog_arg} --notification-log {nlog_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    wait_for_server_ready(&nlog);

    let receipt = bridge.call_diagnostics_scoped(&[root])?;

    // The dirty files list their diagnostics; the clean files collapse.
    assert!(
        receipt.contains("workspace diagnostic"),
        "dirty diagnostics missing from receipt:\n{receipt}"
    );
    assert!(
        receipt.contains("[error]"),
        "error severity missing from receipt:\n{receipt}"
    );
    assert!(
        receipt.contains("3 files clean"),
        "clean files should collapse to a count:\n{receipt}"
    );
    assert!(
        !receipt.contains("ok0"),
        "clean filenames must not appear when collapsed:\n{receipt}"
    );

    // Exactly one workspace pull, and no per-file open churn.
    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "workspace/diagnostic"),
        1,
        "expected exactly one workspace/diagnostic request; log:\n{rlog_text}"
    );
    assert_eq!(
        count_request_method(&rlog_text, "textDocument/diagnostic"),
        0,
        "whole-root scope must not per-file pull; log:\n{rlog_text}"
    );
    // No per-file open churn: the whole-root pull opens nothing. (The daemon's
    // one-shot eager health probe on server spawn may open a single file, so the
    // bound is "far fewer than the five covered files", not zero — a fan-out
    // would open all five.)
    let nlog_text = read_merged_log(&nlog);
    assert!(
        count_notification_method(&nlog_text, "textDocument/didOpen") <= 1,
        "whole-root workspace pull must not open documents per file; log:\n{nlog_text}"
    );

    Ok(())
}

// ─── No capability → fan-out ────────────────────────────────────────

/// `catenary diagnostics .` against a server WITHOUT
/// `workspace/diagnostic` support falls back to the per-file fan-out: the root
/// expands to its covered files, which are opened and diagnosed per-file.
#[test]
fn whole_root_no_capability_fans_out() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let nlog = logs.path().join("notifications.jsonl");

    std::fs::write(dir.path().join(format!("a.{MOCK_LANG}")), "code\n")?;
    std::fs::write(dir.path().join(format!("b.{MOCK_LANG}")), "code\n")?;

    let rlog_arg = rlog.to_str().context("rlog path")?;
    let nlog_arg = nlog.to_str().context("nlog path")?;
    // Default push diagnostics, NO --workspace-diagnostics.
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!("--request-log {rlog_arg} --notification-log {nlog_arg}"),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    let receipt = bridge.call_diagnostics_scoped(&[root])?;

    assert!(
        receipt.contains("mock diagnostic"),
        "fan-out diagnostics missing from receipt:\n{receipt}"
    );

    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "workspace/diagnostic"),
        0,
        "no-capability server must never see workspace/diagnostic; log:\n{rlog_text}"
    );
    let nlog_text = read_merged_log(&nlog);
    assert!(
        has_notification_method(&nlog_text, "textDocument/didOpen"),
        "fan-out fallback must open the expanded files; log:\n{nlog_text}"
    );

    Ok(())
}

// ─── Sub-root path set → fan-out (even when capable) ────────────────

/// A sub-root directory scope fans out even when the covering server is
/// workspace-diagnostic-capable: scope, not just capability, gates the route.
#[test]
fn sub_root_directory_fans_out_despite_capability() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub)?;
    let logs = tempfile::tempdir()?;
    let rlog = logs.path().join("requests.jsonl");
    let nlog = logs.path().join("notifications.jsonl");

    std::fs::write(sub.join(format!("a.{MOCK_LANG}")), "code\n")?;
    std::fs::write(sub.join(format!("b.{MOCK_LANG}")), "code\n")?;

    let rlog_arg = rlog.to_str().context("rlog path")?;
    let nlog_arg = nlog.to_str().context("nlog path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--workspace-diagnostics --scan-roots \
             --request-log {rlog_arg} --notification-log {nlog_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    wait_for_server_ready(&nlog);

    let sub_arg = sub.to_str().context("sub path")?;
    let receipt = bridge.call_diagnostics_scoped(&[sub_arg])?;

    // Fan-out opens the files, so the PUSH diagnostic ("mock diagnostic")
    // appears — distinct from the workspace handler's "workspace diagnostic".
    assert!(
        receipt.contains("mock diagnostic"),
        "sub-root fan-out should produce per-file diagnostics:\n{receipt}"
    );

    let rlog_text = read_merged_log(&rlog);
    assert_eq!(
        count_request_method(&rlog_text, "workspace/diagnostic"),
        0,
        "a sub-root scope must fan out, never workspace-pull; log:\n{rlog_text}"
    );
    let nlog_text = read_merged_log(&nlog);
    assert!(
        has_notification_method(&nlog_text, "textDocument/didOpen"),
        "sub-root fan-out must open the sub files; log:\n{nlog_text}"
    );

    Ok(())
}
