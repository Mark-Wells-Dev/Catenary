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

/// Runs the scoped `catenary diagnostics <file>` flow as `agent_id`: prepare via
/// the `PreToolUse` hook (staging the handoff under this agent's identity), then
/// consume with an explicit `files` set. All agents share the implicit
/// `"default"` session — the incident's shape (same session, per-agent rounds).
/// The prepared handoff carries `(session_id, agent_id)`, so the consume step's
/// round is identity-bearing and takes a diagnose seat.
fn scoped_diagnostics(
    socket: &Path,
    daemon_pid: Option<u32>,
    agent_id: &str,
    file: &Path,
) -> Result<String> {
    common::ipc_request(
        socket,
        &json!({ "method": "pre-tool/editing-start", "agent_id": agent_id }),
    )?;
    common::ipc_request(
        socket,
        &json!({ "method": "pre-tool/editing-stop", "agent_id": agent_id }),
    )?;
    // The consume carries no identity — it takes the identity from the handoff
    // the prepare staged (mirrors the real `catenary diagnostics` client, which
    // is identity-less on the wire).
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

/// Two overlapping SAME-identity diagnose rounds must serialize: the second
/// waits for the first, then runs its own — exactly ONE carries the follow
/// note. Never two interleaved concurrent executions.
#[test]
fn same_identity_diagnose_rounds_serialize_with_note() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let root = dir.path().to_path_buf();
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

/// Two overlapping DIFFERENT-identity diagnose rounds must NOT join: neither
/// waits on the other and neither carries the follow note — the cross-identity
/// behavior the stage-1 change leaves untouched.
#[test]
fn different_identity_diagnose_rounds_do_not_join() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let root = dir.path().to_path_buf();
    let file_a = root.join(format!("a.{MOCK_LANG}"));
    let file_b = root.join(format!("b.{MOCK_LANG}"));
    std::fs::write(&file_a, "echo a\n")?;
    std::fs::write(&file_b, "echo b\n")?;

    let (bridge, log_path) = spawn_bridge_with_flycheck(&root)?;
    let socket = bridge.wait_for_ipc_socket()?;
    let daemon_pid = bridge.daemon_pid();

    // Round 1 as agent `agent-x`; once it is provably executing, round 2 as a
    // DIFFERENT agent `agent-y` fires while round 1 still burns.
    let first = {
        let socket = socket.clone();
        let file = file_a;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "agent-x", &file))
    };
    wait_for_did_save_count(&log_path, 1)?;
    let second = {
        let file = file_b;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "agent-y", &file))
    };

    let first_out = join_receipt(first, "first")?;
    let second_out = join_receipt(second, "second")?;

    assert!(
        first_out.contains("mock diagnostic"),
        "first (agent-x) round's receipt should carry diagnostics. Got: {first_out}"
    );
    assert!(
        second_out.contains("mock diagnostic"),
        "second (agent-y) round's receipt should carry diagnostics. Got: {second_out}"
    );
    // Different identities keep distinct seats — neither round waited on the
    // other, so neither carries the follow note.
    assert!(
        !first_out.contains(FOLLOWED_NOTE),
        "different-identity first round must NOT carry the follow note. Got: {first_out}"
    );
    assert!(
        !second_out.contains(FOLLOWED_NOTE),
        "different-identity second round must NOT carry the follow note. Got: {second_out}"
    );

    Ok(())
}
