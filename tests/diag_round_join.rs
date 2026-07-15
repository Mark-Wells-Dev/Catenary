// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Same-identity diagnose join (misc 197 stage 1).
//!
//! The incident: the host harness auto-backgrounds a slow `catenary
//! diagnostics` and retries it, so several concurrent rounds stack for ONE
//! agent. Left unbounded they all fan out to the shared LSP pool at once, and
//! the whole daemon can go quiet for an extended stretch.
//!
//! The fix admits one round per editing identity at a time: a second
//! same-identity round WAITS for the in-flight one, then runs its own with a
//! one-line note ("another diagnose was in flight; this run followed it"). It
//! never stacks a second concurrent execution.
//!
//! These tests drive a real daemon and fire overlapping scoped diagnose calls:
//! - `same_identity_diagnose_rounds_serialize_with_note` proves two overlapping
//!   SAME-identity rounds serialize — exactly one carries the follow note, i.e.
//!   one waited for the other rather than running concurrently.
//! - `different_identity_diagnose_rounds_do_not_join` pins the control: two
//!   overlapping DIFFERENT-identity rounds neither serialize on each other nor
//!   carry the note — today's cross-identity behavior is untouched.

mod common;

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::json;

use common::BridgeProcess;

/// Empty-behavior blessed persona (default mockls push) — a diagnostics source
/// whose receipt carries the "mock diagnostic" line the assertions key on.
const MOCK_LANG: &str = "mockls-event";

/// Flycheck burn in ticks (centiseconds of CPU): ~6 s. Long enough that the
/// first round provably holds the diagnose seat while the second round is fired
/// and reaches the join wait, short enough to keep the test brisk. Wall time
/// only grows under load, so the overlap window never shrinks below this.
const FLYCHECK_TICKS: u64 = 600;

/// The one-line note a followed round's receipt carries (mirrors the daemon's
/// `DIAG_FOLLOWED_NOTE`).
const FOLLOWED_NOTE: &str = "another diagnose was in flight; this run followed it";

/// Runs the scoped `catenary diagnostics <file>` serve. Root-ownership stage 3
/// retired the identity handoff and re-keyed the diagnose seat (misc 197) from
/// identity to ROOT: one diagnose round per root at a time. The serve names its
/// file, and the daemon resolves the file's lock root (the fixture carries a
/// `.git` marker, so it resolves) to take the per-root seat — two rounds over the
/// same root serialize. `agent_id` no longer distinguishes seats (kept for the
/// caller's readability of which round is which).
fn scoped_diagnostics(
    socket: &Path,
    daemon_pid: Option<u32>,
    _agent_id: &str,
    file: &Path,
) -> Result<String> {
    let text = common::ipc_request_long(
        socket,
        daemon_pid,
        &json!({
            "method": "tool/editing-stop",
            "files": [file.to_str().context("file path")?],
        }),
    )?;
    Ok(common::diagnostics_output(&text))
}

/// Polls the mockls notification log until at least `count` `didSave` events
/// have landed — each round's pipeline reaches didSave before its flycheck
/// burn, so this is the "a round is provably executing" signal.
fn wait_for_did_save_count(log_path: &Path, count: usize) -> Result<()> {
    let deadline = Instant::now() + common::POLL_BACKSTOP;
    while Instant::now() < deadline {
        let saves = common::read_merged_log(log_path)
            .matches("\"textDocument/didSave\"")
            .count();
        if saves >= count {
            return Ok(());
        }
        std::thread::sleep(common::POLL_SPACING);
    }
    anyhow::bail!(
        "never observed {count} didSave events — mockls log at {}",
        log_path.display()
    )
}

/// Joins a diagnose thread, mapping a panic to an error with the role name.
fn join_receipt(handle: std::thread::JoinHandle<Result<String>>, role: &str) -> Result<String> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{role} diagnose thread panicked"))?
        .with_context(|| format!("{role} diagnose failed"))
}

/// Builds a daemon over one root whose server runs a CPU-burning flycheck on
/// didSave (so each diagnose round holds its seat for a controlled duration),
/// with its notification log at `<root>/mockls.jsonl`.
fn spawn_bridge_with_flycheck(root: &Path) -> Result<(BridgeProcess, std::path::PathBuf)> {
    let log_path = root.join("mockls.jsonl");
    let log_arg = log_path.to_str().context("log path")?;
    let mockc = env!("CARGO_BIN_EXE_mockc");
    let lsp = common::mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--advertise-save --flycheck-command {mockc} \
             --flycheck-ticks {FLYCHECK_TICKS} --notification-log {log_arg}"
        ),
    );
    let root_str = root.to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_str)?;
    bridge.initialize_with_roots(&[root_str])?;
    Ok((bridge, log_path))
}

/// Two overlapping SAME-ROOT diagnose rounds must serialize: the second waits for
/// the first, then runs its own — exactly ONE carries the follow note. Never two
/// interleaved concurrent executions. Root-ownership stage 3 re-keyed the diagnose
/// seat (misc 197) from identity to ROOT, so this is the same-root case; the
/// fixture carries a `.git` marker so both files resolve to one lock root.
#[test]
fn same_root_diagnose_rounds_serialize_with_note() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let root = dir.path().to_path_buf();
    // A repo marker so both files resolve to the same lock root — the per-root
    // diagnose seat (stage 3) then serializes the two rounds.
    std::fs::create_dir_all(root.join(".git"))?;
    let file_a = root.join(format!("a.{MOCK_LANG}"));
    let file_b = root.join(format!("b.{MOCK_LANG}"));
    std::fs::write(&file_a, "echo a\n")?;
    std::fs::write(&file_b, "echo b\n")?;

    let (bridge, log_path) = spawn_bridge_with_flycheck(&root)?;
    let socket = bridge.wait_for_ipc_socket()?;
    let daemon_pid = bridge.daemon_pid();

    // Round 1 as agent `join-agent`: diagnoses file A. The pipeline reaches
    // didSave, the burn begins, and the diagnose seat for this identity is held
    // for the burn's duration.
    let first = {
        let socket = socket.clone();
        let file = file_a;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "join-agent", &file))
    };
    // Wait until round 1 is provably executing (its didSave landed), then fire
    // round 2 under the SAME identity while round 1 still holds the seat.
    wait_for_did_save_count(&log_path, 1)?;
    let second = {
        let file = file_b;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "join-agent", &file))
    };

    let first_out = join_receipt(first, "first")?;
    let second_out = join_receipt(second, "second")?;

    // Both rounds complete with real receipts — the join delays the second, it
    // does not drop it.
    assert!(
        first_out.contains("mock diagnostic"),
        "first round's receipt should carry diagnostics. Got: {first_out}"
    );
    assert!(
        second_out.contains("mock diagnostic"),
        "second round's receipt should carry diagnostics. Got: {second_out}"
    );

    // Exactly one round carried the follow note. The round that entered the seat
    // first never waited (no note); the one that arrived while a round was in
    // flight waited and earned the note. If the two had run concurrently, the
    // seat would have been free for both and neither would carry it — so the note
    // appearing on precisely one is the serialization proof.
    let first_followed = first_out.contains(FOLLOWED_NOTE);
    let second_followed = second_out.contains(FOLLOWED_NOTE);
    assert!(
        first_followed ^ second_followed,
        "exactly one same-identity round must carry the follow note (serialized), \
         but first_followed={first_followed} second_followed={second_followed}. \
         first: {first_out}\nsecond: {second_out}"
    );

    Ok(())
}

/// Two overlapping DIFFERENT-ROOT diagnose rounds must NOT join: each root has
/// its own diagnose seat (root-ownership stage 3), so neither waits on the other
/// and neither carries the follow note. This is the cross-root behavior the
/// per-root seat leaves independent (the cross-identity-same-root case cannot
/// happen — one cook per kitchen).
#[test]
fn different_root_diagnose_rounds_do_not_join() -> Result<()> {
    let dir_a = common::canonical_tempdir()?;
    let dir_b = common::canonical_tempdir()?;
    let root_a = dir_a.path().to_path_buf();
    let root_b = dir_b.path().to_path_buf();
    // Each root carries its own repo marker so its files resolve to a distinct
    // lock root — distinct per-root diagnose seats.
    std::fs::create_dir_all(root_a.join(".git"))?;
    std::fs::create_dir_all(root_b.join(".git"))?;
    let file_a = root_a.join(format!("a.{MOCK_LANG}"));
    let file_b = root_b.join(format!("b.{MOCK_LANG}"));
    std::fs::write(&file_a, "echo a\n")?;
    std::fs::write(&file_b, "echo b\n")?;

    // One daemon serving both roots, with a flycheck-burning server so each round
    // holds its seat for a controlled duration.
    let log_a = root_a.join("mockls.jsonl");
    let log_arg = log_a.to_str().context("log path")?;
    let mockc = env!("CARGO_BIN_EXE_mockc");
    let lsp = common::mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--advertise-save --flycheck-command {mockc} \
             --flycheck-ticks {FLYCHECK_TICKS} --notification-log {log_arg}"
        ),
    );
    let alpha = root_a.to_str().context("root a")?;
    let beta = root_b.to_str().context("root b")?;
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[alpha, beta])?;
    bridge.initialize_with_roots(&[alpha, beta])?;
    let socket = bridge.wait_for_ipc_socket()?;
    let daemon_pid = bridge.daemon_pid();

    // Round 1 over root A; once it is provably executing, round 2 over root B
    // fires while round 1 still burns. Distinct roots → distinct seats.
    let first = {
        let socket = socket.clone();
        let file = file_a;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "round-a", &file))
    };
    wait_for_did_save_count(&log_a, 1)?;
    let second = {
        let file = file_b;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "round-b", &file))
    };

    let first_out = join_receipt(first, "first")?;
    let second_out = join_receipt(second, "second")?;

    assert!(
        first_out.contains("mock diagnostic"),
        "first (root A) round's receipt should carry diagnostics. Got: {first_out}"
    );
    assert!(
        second_out.contains("mock diagnostic"),
        "second (root B) round's receipt should carry diagnostics. Got: {second_out}"
    );
    // Distinct roots keep distinct seats — neither round waited on the other, so
    // neither carries the follow note.
    assert!(
        !first_out.contains(FOLLOWED_NOTE),
        "different-root first round must NOT carry the follow note. Got: {first_out}"
    );
    assert!(
        !second_out.contains(FOLLOWED_NOTE),
        "different-root second round must NOT carry the follow note. Got: {second_out}"
    );

    Ok(())
}
