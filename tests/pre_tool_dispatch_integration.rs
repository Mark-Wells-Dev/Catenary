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
//! `catenary diagnostics` is DENIED *before* the editing-stop prepare drains
//! the tracked set — so a denied piped form can never silently clear pending
//! diagnostics ("denied *and* cleared").

mod common;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, diagnostics_output, ipc_request, ipc_request_long, mockls_lsp_arg};

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

/// Driving the real hook: a piped `catenary diagnostics` denies before the
/// prepare-drain, so the tracked set survives and a later bare run still
/// reports its diagnostics. If the deny fired *after* the drain (the bug the
/// ticket-11 ordering prevents), the bare run would find an empty set.
#[test]
fn piped_diagnostics_denied_before_prepare_drain() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("track.{MOCK_LANG}"));
    std::fs::write(&file, "echo hello\n")?;
    let root = dir.path().to_str().context("root path")?;
    let file_str = file.to_str().context("file path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // Enter editing mode and accumulate the covered file (raw IPC, agent "").
    ipc_request(
        &socket,
        &json!({"method": "pre-tool/editing-start", "agent_id": ""}),
    )?;
    ipc_request(
        &socket,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": file_str,
            "agent_id": ""
        }),
    )?;

    // Drive the REAL run_pre_tool with a PIPED diagnostics command. The matcher
    // denies it (regime 1) and the dispatch returns *before* contacting the
    // daemon — no editing-stop prepare, so the tracked set is untouched.
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

    // A bare `catenary diagnostics` now routes (the Diagnostics arm): the hook
    // stages the editing-stop prepare, draining the *surviving* set into the
    // handoff slot. An allow is silent (empty stdout).
    let bare = bridge.run_pre_tool_bash("catenary diagnostics")?;
    assert!(
        parse_decision(&bare).is_none(),
        "bare diagnostics should be allowed (no deny), got: {bare}",
    );

    // Claim the staged handoff. A non-empty result proves the set survived the
    // piped deny and was drained by the bare run; had the piped form drained it,
    // the bare prepare would have found nothing to report.
    let claimed = ipc_request_long(
        &socket,
        bridge.daemon_pid(),
        &json!({"method": "tool/editing-stop"}),
    )?;
    let diag = diagnostics_output(&claimed);
    assert!(
        diag.contains("mock diagnostic"),
        "tracked set must survive the piped deny and drain on the bare run, got:\n{diag}",
    );

    Ok(())
}
