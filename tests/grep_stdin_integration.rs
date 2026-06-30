// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for `catenary grep` stdin mode (pipeable-output ticket 04).
//!
//! `… | catenary grep PAT` runs a plain ripgrep pass over the stream: no daemon,
//! no enrichment, no `#scope` — but the **same flags** as file mode (it differs
//! only in enrichment, never in capability). These tests spawn the binary with a
//! real piped stdin (a FIFO, so ripgrep's `is_readable_stdin` fires) and assert
//! the stream output, bypassing the command filter that still blocks `catenary
//! grep` downstream of a pipe (retired in ticket 05).

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use common::isolate_env;

/// Spawns `catenary grep <args>` with `input` piped to stdin and returns
/// `(stdout, success)`. No daemon is started — stdin mode is entirely CLI-local,
/// so these tests also prove it needs no running daemon.
fn grep_stdin(args: &[&str], input: &str) -> (String, bool) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    // Rule: isolate the subprocess env before any CATENARY_* override. Stdin mode
    // never reaches the daemon, but isolation keeps the test off the user's XDG
    // dirs regardless.
    isolate_env(&mut cmd, tmp.path().to_str().expect("tempdir path"));
    cmd.arg("grep")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn catenary grep");
    {
        let mut stdin = child.stdin.take().expect("child stdin");
        stdin.write_all(input.as_bytes()).expect("write stdin");
        // Drop closes the write end → the child sees EOF.
    }
    let output = child.wait_with_output().expect("wait for catenary grep");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.success(),
    )
}

#[test]
fn stdin_plain_matches_lines() {
    let (stdout, ok) = grep_stdin(&["beta"], "alpha\nbeta\ngamma\n");
    assert!(ok, "stdin grep must exit 0");
    assert_eq!(stdout, "beta\n");
}

#[test]
fn stdin_no_match_is_empty_and_exits_zero() {
    let (stdout, ok) = grep_stdin(&["zzz"], "alpha\nbeta\n");
    assert!(ok, "a no-match stdin grep must still exit 0");
    assert_eq!(stdout, "");
}

#[test]
fn stdin_carries_smart_case_default() {
    // Lowercase pattern → case-insensitive (matches uppercase text).
    let (insensitive, _) = grep_stdin(&["alpha"], "ALPHA\nbeta\n");
    assert_eq!(insensitive, "ALPHA\n");
    // Pattern with an uppercase letter → case-sensitive (no match).
    let (sensitive, ok) = grep_stdin(&["Alpha"], "alpha\nbeta\n");
    assert!(ok);
    assert_eq!(sensitive, "");
}

#[test]
fn stdin_carries_ignore_case_flag() {
    let (stdout, _) = grep_stdin(&["-i", "Alpha"], "alpha\nbeta\n");
    assert_eq!(stdout, "alpha\n");
}

#[test]
fn stdin_carries_fixed_strings_flag() {
    // `-F` makes `.` a literal dot, not "any char".
    let (stdout, _) = grep_stdin(&["-F", "a.c"], "a.c\nabc\n");
    assert_eq!(stdout, "a.c\n");
}

#[test]
fn stdin_carries_invert_flag() {
    let (stdout, _) = grep_stdin(&["-v", "beta"], "alpha\nbeta\ngamma\n");
    assert_eq!(stdout, "alpha\ngamma\n");
}

#[test]
fn stdin_carries_context_flag() {
    let (stdout, _) = grep_stdin(&["-A", "1", "beta"], "alpha\nbeta\ngamma\n");
    assert_eq!(stdout, "beta\ngamma\n");
}

#[test]
fn stdin_count_reports_matching_lines() {
    let (stdout, ok) = grep_stdin(&["--count", "a"], "a\nba\nc\n");
    assert!(ok);
    assert_eq!(stdout, "2 matches\n");
}

#[test]
fn stdin_files_with_matches_prints_standard_input() {
    let (matched, _) = grep_stdin(&["-l", "beta"], "alpha\nbeta\n");
    assert_eq!(matched, "(standard input)\n");
    // No match → nothing printed (GNU `grep -l` convention for a stream).
    let (missed, ok) = grep_stdin(&["-l", "zzz"], "alpha\nbeta\n");
    assert!(ok);
    assert_eq!(missed, "");
}
