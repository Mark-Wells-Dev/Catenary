// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Process-level contract tests for the reserved hook-event shims
//! (full-surface registration, pre-v2 maintainer ruling).
//!
//! Every event registered ahead of its behavior terminates in
//! `cli::hooks::run_reserved_shim`, whose contract is: drain stdin to EOF,
//! answer in the host dialect's empty form (Claude: silence; Antigravity: the
//! documented empty object `{}`), and exit 0 — on well-formed JSON and on
//! garbage alike, with no daemon anywhere in sight (these tests spawn no
//! bridge; an isolated environment has no socket to reach).

mod common;

use std::io::Write;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result};

use common::isolate_env;

/// Spawn `catenary hook <event> --format=<format>` in an isolated environment,
/// feed it `stdin_bytes`, and collect its output.
fn run_shim(root: &str, event: &str, format: &str, stdin_bytes: &[u8]) -> Result<Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    let format_flag = format!("--format={format}");
    cmd.args(["hook", event, format_flag.as_str()]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn hook shim")?;
    {
        let mut stdin = child.stdin.take().context("shim stdin")?;
        stdin.write_all(stdin_bytes).context("write shim stdin")?;
    }
    child.wait_with_output().context("wait for shim")
}

/// The shim contract, end to end: exit 0 with zero output, for a well-formed
/// Claude Code payload, for garbage (including invalid UTF-8), and for empty
/// stdin. A sample of shims across the surface stands in for all 18 — they
/// share one handler, and the parity tests in `src/main.rs` pin the rest.
#[test]
fn reserved_shims_exit_zero_silently() -> Result<()> {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path().to_str().expect("tempdir path");

    let json: &[u8] = br#"{"session_id":"s1","hook_event_name":"PostToolUse","cwd":"/tmp"}"#;
    let garbage: &[u8] = &[0xFF, 0xFE, b'{', 0x00, b'x'];
    let empty: &[u8] = b"";

    for event in ["setup", "post-tool-use", "notification", "pre-compact"] {
        for stdin_bytes in [json, garbage, empty] {
            let out = run_shim(root, event, "claude", stdin_bytes)?;
            assert!(
                out.status.success(),
                "hook {event} must exit 0 (stderr: {})",
                String::from_utf8_lossy(&out.stderr),
            );
            assert!(
                out.stdout.is_empty(),
                "hook {event} must print nothing on stdout (got: {})",
                String::from_utf8_lossy(&out.stdout),
            );
            assert!(
                out.stderr.is_empty(),
                "hook {event} must print nothing on stderr (got: {})",
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }
    Ok(())
}

/// The Antigravity dialect: its hook contract is JSON-in/JSON-out, so these
/// events answer the documented empty object `{}` on stdout (and nothing else)
/// on every stdin shape. `post-invocation` is a reserved shim; `post-tool-use`
/// is the real reconcile-bracket handler (root-ownership stage 5), whose
/// Antigravity floor is the same empty answer — the reconcile is Claude/OpenCode
/// Bash-only, so an Antigravity `post-tool-use` reconciles nothing and answers
/// `{}` exactly like the shim it grew out of.
#[test]
fn antigravity_shims_answer_the_empty_object() -> Result<()> {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path().to_str().expect("tempdir path");

    let json: &[u8] = br#"{"stepIdx":5,"error":"","conversationId":"c1"}"#;
    let garbage: &[u8] = &[0xFF, 0xFE, b'{', 0x00, b'x'];
    let empty: &[u8] = b"";

    for event in ["post-tool-use", "post-invocation"] {
        for stdin_bytes in [json, garbage, empty] {
            let out = run_shim(root, event, "antigravity", stdin_bytes)?;
            assert!(
                out.status.success(),
                "hook {event} must exit 0 (stderr: {})",
                String::from_utf8_lossy(&out.stderr),
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                "{}",
                "hook {event} must answer the empty object",
            );
            assert!(
                out.stderr.is_empty(),
                "hook {event} must print nothing on stderr (got: {})",
                String::from_utf8_lossy(&out.stderr),
            );
        }
    }
    Ok(())
}
