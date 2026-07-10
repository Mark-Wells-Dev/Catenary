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
//! print nothing on stdout or stderr, and exit 0 — on well-formed JSON and on
//! garbage alike, with no daemon anywhere in sight (these tests spawn no
//! bridge; an isolated environment has no socket to reach).

mod common;

use std::io::Write;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result};

use common::isolate_env;

/// Spawn `catenary hook <event> --format=claude` in an isolated environment,
/// feed it `stdin_bytes`, and collect its output.
fn run_shim(root: &str, event: &str, stdin_bytes: &[u8]) -> Result<Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.args(["hook", event, "--format=claude"]);
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
            let out = run_shim(root, event, stdin_bytes)?;
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
