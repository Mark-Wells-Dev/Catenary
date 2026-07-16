// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end coverage for the `PostToolUse` secret-redaction backstop (misc 201,
//! component 3; the live-pin fix).
//!
//! Drives the real `catenary hook post-tool-use --format=claude` binary as a
//! subprocess with an isolated env (`isolate_env`) and NO daemon running — the
//! redaction leg is hook-process-local (it parses stdin, scans `tool_response`,
//! and prints the rewrite), so no daemon is needed.
//!
//! The contract these pin (docs: <https://code.claude.com/docs/en/hooks>):
//! - the rewrite is **shape-preserving**: `updatedToolOutput` matches the shape of
//!   `tool_response` for the tool that ran (Bash: `{stdout, stderr, exit_code}`;
//!   Read: `{file_path, contents}`), with only the secret-bearing string leaf
//!   swapped for its marker — the live-pin failure was a bare-string rewrite of a
//!   Bash object, which the client ignored, leaking the raw PEM;
//! - a clean output emits NOTHING (byte-identical passthrough).

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::isolate_env;

/// A fake PEM private-key armor block with dummy base64 (never a real key) — the
/// exact shape the live pin `cat`-ed.
const PEM_ARMOR: &str =
    "-----BEGIN PRIVATE KEY-----\nMIIabcDUMMYbase64PADDING\n-----END PRIVATE KEY-----";

/// Drive `catenary hook post-tool-use --format=claude` with the given payload on
/// stdin, NO daemon running, returning the hook's stdout.
fn run_post_tool_use(root: &str, payload: &Value) -> Result<String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.args(["hook", "post-tool-use", "--format=claude"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn `hook post-tool-use`")?;
    {
        let mut stdin = child.stdin.take().context("hook stdin")?;
        writeln!(stdin, "{payload}").context("write hook payload")?;
    }
    let out = child.wait_with_output().context("wait for hook")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse the hook's stdout as the `hookSpecificOutput` rewrite envelope, or `None`
/// when the hook emitted nothing (a clean passthrough → empty stdout).
fn rewrite_envelope(stdout: &str) -> Option<Value> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

#[test]
fn bash_tool_response_with_pem_gets_shape_preserving_rewrite() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_str().context("root utf-8")?;

    // Bash `tool_response`: a structured object with stdout/stderr/exit_code.
    let payload = json!({
        "session_id": "s-bash",
        "cwd": root,
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "cat key.pem" },
        "tool_response": {
            "stdout": format!("here is the key:\n{PEM_ARMOR}\n"),
            "stderr": "",
            "exit_code": 0,
        },
    });

    let stdout = run_post_tool_use(root, &payload)?;
    let env = rewrite_envelope(&stdout)
        .with_context(|| format!("expected a rewrite envelope, got: {stdout:?}"))?;

    // Correctly-shaped rewrite: hookEventName + updatedToolOutput.
    let hso = &env["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], "PostToolUse");
    let updated = &hso["updatedToolOutput"];

    // SHAPE-PRESERVING: updatedToolOutput is an OBJECT matching the Bash shape,
    // not a flattened string (the live-pin bug). exit_code stays the number 0,
    // stderr stays "".
    assert!(
        updated.is_object(),
        "updatedToolOutput must match the Bash object shape, got: {updated}"
    );
    assert_eq!(updated["exit_code"], json!(0));
    assert_eq!(updated["stderr"], json!(""));

    // The PEM in stdout is redacted; the key material never survives; the
    // surrounding non-secret text is preserved.
    let out_stdout = updated["stdout"]
        .as_str()
        .context("stdout stays a string")?;
    assert!(out_stdout.contains("[REDACTED: private key]"));
    assert!(
        !out_stdout.contains("MIIabc"),
        "key material leaked: {out_stdout}"
    );
    assert!(out_stdout.starts_with("here is the key:\n"));

    Ok(())
}

#[test]
fn read_tool_response_with_pem_gets_shape_preserving_rewrite() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_str().context("root utf-8")?;

    // Read `tool_response`: a structured object with file_path/contents.
    let payload = json!({
        "session_id": "s-read",
        "cwd": root,
        "hook_event_name": "PostToolUse",
        "tool_name": "Read",
        "tool_input": { "file_path": "/home/me/key.pem" },
        "tool_response": {
            "file_path": "/home/me/key.pem",
            "contents": PEM_ARMOR,
        },
    });

    let stdout = run_post_tool_use(root, &payload)?;
    let env = rewrite_envelope(&stdout)
        .with_context(|| format!("expected a rewrite envelope, got: {stdout:?}"))?;

    let hso = &env["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], "PostToolUse");
    let updated = &hso["updatedToolOutput"];

    // SHAPE-PRESERVING: an object with the SAME keys; file_path (a non-secret
    // string) is byte-identical, contents is redacted.
    assert!(
        updated.is_object(),
        "updatedToolOutput must be an object: {updated}"
    );
    assert_eq!(updated["file_path"], json!("/home/me/key.pem"));
    let contents = updated["contents"]
        .as_str()
        .context("contents stays a string")?;
    assert!(contents.contains("[REDACTED: private key]"));
    assert!(
        !contents.contains("MIIabc"),
        "key material leaked: {contents}"
    );

    Ok(())
}

#[test]
fn clean_bash_tool_response_emits_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_str().context("root utf-8")?;

    // A clean Bash response — no secret anywhere. The hook must emit NOTHING
    // (byte-identical passthrough, no updatedToolOutput).
    let payload = json!({
        "session_id": "s-clean",
        "cwd": root,
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": "npm test" },
        "tool_response": {
            "stdout": "PASS src/auth.test.ts\n  ok should validate token",
            "stderr": "",
            "exit_code": 0,
        },
    });

    let stdout = run_post_tool_use(root, &payload)?;
    assert!(
        stdout.trim().is_empty(),
        "a clean output must emit nothing, got: {stdout:?}"
    );

    Ok(())
}

#[test]
fn clean_read_tool_response_emits_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().to_str().context("root utf-8")?;

    let payload = json!({
        "session_id": "s-clean-read",
        "cwd": root,
        "hook_event_name": "PostToolUse",
        "tool_name": "Read",
        "tool_input": { "file_path": "/home/me/config.json" },
        "tool_response": {
            "file_path": "/home/me/config.json",
            "contents": "{\n  \"version\": \"1.0.0\"\n}",
        },
    });

    let stdout = run_post_tool_use(root, &payload)?;
    assert!(
        stdout.trim().is_empty(),
        "a clean Read output must emit nothing, got: {stdout:?}"
    );

    Ok(())
}
