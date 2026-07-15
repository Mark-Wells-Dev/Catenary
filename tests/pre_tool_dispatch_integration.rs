// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end ordering guard for the `PreToolUse` hook dispatch
//! (`run_pre_tool`).
//!
//! The unit-level `composition_table` (in `command_filter`) *models*
//! `run_pre_tool`'s dispatch over the pure filter functions. This test drives
//! the real `catenary hook pre-tool` binary against a live daemon to prove the
//! load-bearing ordering (cli-prerelease ticket 11 / ADR 013): a piped
//! `catenary diagnostics` is DENIED by the client-side matcher (bare-only),
//! while a bare form is allowed and serves. Root-ownership stage 3 retired the
//! two-phase prepare-drain (the serve now reads the durable ledger), so the
//! surviving guard is the bare-only pipe deny — a piped form never reaches the
//! serve.

mod common;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, diagnostics_output, ipc_request_long, mockls_lsp_arg};

// The `mockls-event` persona is the blessed base (diagnostics-debt 04c):
// renaming the key to a manifest persona makes the mock a diagnostics source
// (replacing the retired bless-list wildcard) while its empty
// behavior bundle keeps the wire behavior identical.
const MOCK_LANG: &str = "mockls-event";

/// Parse a Claude `PreToolUse` hook stdout into `(decision, reason)`, or `None`
/// when it is not a deny envelope (an allow is silent → empty stdout).
fn parse_decision(stdout: &str) -> Option<(String, String)> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let out = v.get("hookSpecificOutput")?;
    let decision = out.get("permissionDecision")?.as_str()?.to_string();
    let reason = out
        .get("permissionDecisionReason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((decision, reason))
}

/// Driving the real hook: a piped `catenary diagnostics | head` is DENIED by the
/// client-side matcher (bare-only), never reaching the serve; a bare
/// `catenary diagnostics` is allowed; and a scoped serve still reports the file's
/// diagnostics. The deny is the ordering guard — a piped form cannot slip through
/// to run the serve.
#[test]
fn piped_diagnostics_denied_bare_serves() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("track.{MOCK_LANG}"));
    std::fs::write(&file, "echo hello\n")?;
    let root = dir.path().to_str().context("root path")?;
    let file_str = file.to_str().context("file path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // Drive the REAL run_pre_tool with a PIPED diagnostics command. The matcher
    // denies it (regime 1, bare-only) before any daemon contact.
    let piped = bridge.run_pre_tool_bash("catenary diagnostics | head")?;
    let (decision, reason) =
        parse_decision(&piped).context("piped diagnostics should produce a deny envelope")?;
    assert_eq!(
        decision, "deny",
        "piped diagnostics must be denied: {piped}"
    );
    assert!(
        reason.contains("runtime-dir") || reason.contains("preview") || reason.contains("pipe"),
        "deny reason should be the diagnostics pipe-deny, got: {reason}",
    );

    // A bare `catenary diagnostics` is allowed (the Diagnostics arm — the owner
    // gate on an unlocked root allows). An allow is silent (empty stdout).
    let bare = bridge.run_pre_tool_bash("catenary diagnostics")?;
    assert!(
        parse_decision(&bare).is_none(),
        "bare diagnostics should be allowed (no deny), got: {bare}",
    );

    // A scoped serve names the file and reports its diagnostics (served regardless
    // of ledger state — root-ownership stage 3). The piped deny never touched it.
    let served = ipc_request_long(
        &socket,
        bridge.daemon_pid(),
        &json!({"method": "tool/editing-stop", "files": [file_str]}),
    )?;
    let diag = diagnostics_output(&served);
    assert!(
        diag.contains("mock diagnostic"),
        "the scoped serve reports the file's diagnostics, got:\n{diag}",
    );

    Ok(())
}
