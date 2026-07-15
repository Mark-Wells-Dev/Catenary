// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Regression tests for bug 104: one agent's in-flight diagnose must not
//! serialize sibling agents' daemon queries across roots.
//!
//! The incident mechanism: `settle_and_save` holds a server's client mutex
//! across the whole post-open/post-didSave settle (by design — no interleaved
//! traffic during a settle), and several `LspClientManager` paths awaited a
//! client lock while holding the `clients` registry mutex. One agent's
//! diagnose parked on a slow-settling server therefore wedged the registry,
//! and every manager-touching query daemon-wide — greps, globs, and other
//! agents' diagnostics on *unrelated roots* — queued behind it for the
//! settle's whole duration (~65 min in the wild).
//!
//! The test mirrors the incident's shape: two roots (the per-agent worktree
//! layout), one holding agent whose diagnose is pinned open by a long
//! CPU-burning flycheck under a `$/progress` bracket, one sibling agent whose
//! diagnose routes to that same held instance (pre-fix: the registry wedge).
//! Concurrent queries scoped to the *other* root — grep, glob, and a third
//! agent's diagnostics — must complete promptly regardless.
//!
//! Same-root queries are out of scope here: waiting on the root's own busy
//! server (e.g. `covering_watchers` locking each client under the queried
//! root) is genuinely root-bound and survives this fix.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use common::BridgeProcess;

// Blessed personas as server keys (diagnostics-debt 04c): membership in the seed
// manifest is what makes a mock a diagnostics source. Two roots run two live
// servers at once, so they need two DISTINCT persona keys. `mockls-event` (empty
// behavior bundle — default mockls push) carries root A's flycheck discipline
// unchanged; `mockls-pull` makes root B's server a diagnostics source via the
// pull channel, still emitting the "mock diagnostic" the prober asserts.
const MOCK_LANG_A: &str = "mockls-event";
const MOCK_LANG_B: &str = "mockls-pull";

/// Promptness bound for queries scoped to the un-held root. Generous against
/// CPU contention (a normal probe completes in well under a second), but far
/// below the holder's flycheck burn, so a probe that queues behind the
/// sibling's diagnose (the bug) blows through it unambiguously.
const PROMPT_BOUND: Duration = Duration::from_secs(12);

/// Flycheck burn in ticks (centiseconds of CPU): 20 s. The holder's client
/// mutex is held for at least this long — wall time only grows under load —
/// dwarfing [`PROMPT_BOUND`] so the pre-fix failure mode cannot slip under it.
const FLYCHECK_TICKS: u64 = 2_000;

/// Runs the scoped `catenary diagnostics <file>` serve. Root-ownership stage 3
/// retired the identity handoff — a single scoped `tool/editing-stop` naming the
/// file serves it. `agent_id` is retained only for the caller's readability of
/// which round is which (it no longer keys anything on the wire).
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

/// Polls mockls A's notification log until the holder's `didSave` lands — the
/// moment `settle_and_save` is provably inside its post-didSave settle with
/// the client mutex held (the lock is taken before the post-open settle and
/// released only when the batch ends).
fn wait_for_did_save(log_path: &Path) -> Result<()> {
    let deadline = Instant::now() + common::POLL_BACKSTOP;
    while Instant::now() < deadline {
        if common::read_merged_log(log_path).contains("\"textDocument/didSave\"") {
            return Ok(());
        }
        std::thread::sleep(common::POLL_SPACING);
    }
    anyhow::bail!(
        "holder never reached didSave — mockls A log at {}",
        log_path.display()
    )
}

/// While one agent's diagnose is parked in a slow settle on its root's server
/// and a sibling agent's diagnose is queued on that same held instance,
/// queries scoped to a different root — grep, glob, and a third agent's
/// diagnostics — must complete promptly (bug 104).
#[test]
fn sibling_diagnose_does_not_serialize_other_roots() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let root_a = dir.path().join("agent_a");
    let root_b = dir.path().join("agent_b");
    std::fs::create_dir(&root_a)?;
    std::fs::create_dir(&root_b)?;
    let held_file = root_a.join(format!("held.{MOCK_LANG_A}"));
    let sibling_file = root_a.join(format!("sibling.{MOCK_LANG_A}"));
    let other_file = root_b.join(format!("other.{MOCK_LANG_B}"));
    std::fs::write(&held_file, "echo held\n")?;
    std::fs::write(&sibling_file, "echo sibling\n")?;
    std::fs::write(&other_file, "needle_b\n")?;

    let log_path = dir.path().join("mockls_a_notifications.jsonl");
    let log_arg = log_path.to_str().context("log path")?;
    let mockc = env!("CARGO_BIN_EXE_mockc");

    // Server A: didSave spawns a 20 s CPU-burning flycheck under a
    // `$/progress` bracket, so the post-didSave settle — and with it the
    // diagnose's client-mutex hold — spans the whole burn. Server B: plain.
    let lsp_a = common::mockls_lsp_arg(
        MOCK_LANG_A,
        &format!(
            "--advertise-save --flycheck-command {mockc} \
             --flycheck-ticks {FLYCHECK_TICKS} --notification-log {log_arg}"
        ),
    );
    let lsp_b = common::mockls_lsp_arg(MOCK_LANG_B, "");
    let held_root = root_a.to_str().context("root a path")?;
    let probe_root = root_b.to_str().context("root b path")?;
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp_a, &lsp_b], &[held_root, probe_root])?;
    bridge.initialize_with_roots(&[held_root, probe_root])?;

    let socket = bridge.wait_for_ipc_socket()?;
    let daemon_pid = bridge.daemon_pid();

    // Holder: agent `bug104-holder` diagnoses its root-A file. The pipeline
    // reaches didSave, the flycheck burn begins, and root A's client mutex
    // stays held until the burn's progress bracket closes.
    let holder = {
        let socket = socket.clone();
        let file = held_file;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "bug104-holder", &file))
    };
    wait_for_did_save(&log_path)?;

    // Sibling: agent `bug104-sibling` diagnoses another root-A file — it
    // routes to the held instance. Pre-fix, its server lookup awaited the
    // held client mutex UNDER the registry lock, wedging every manager
    // lookup daemon-wide; post-fix it waits on the client alone (root-bound,
    // by design). Give it a beat to reach that lock.
    let sibling = {
        let socket = socket.clone();
        let file = sibling_file;
        std::thread::spawn(move || scoped_diagnostics(&socket, daemon_pid, "bug104-sibling", &file))
    };
    std::thread::sleep(Duration::from_secs(2));

    // Probe 1 — grep scoped to root B (enrichment goes through the manager).
    let started = Instant::now();
    let grep_out = bridge.call_grep(&json!({
        "pattern": "needle_b",
        "directory": probe_root,
    }))?;
    let grep_elapsed = started.elapsed();
    assert!(
        grep_out.contains("needle_b"),
        "grep should find the root-B hit. Got: {grep_out}"
    );
    assert!(
        grep_elapsed < PROMPT_BOUND,
        "grep took {grep_elapsed:?} — queued behind the sibling's diagnose \
         (bug 104); bound {PROMPT_BOUND:?}"
    );

    // Probe 2 — glob over root B (outline coverage goes through the manager).
    let started = Instant::now();
    let glob_out = bridge.call_glob(&json!({
        "paths": [probe_root],
        "directory": probe_root,
    }))?;
    let glob_elapsed = started.elapsed();
    assert!(
        glob_out.contains(&format!("other.{MOCK_LANG_B}")),
        "glob should list the root-B file. Got: {glob_out}"
    );
    assert!(
        glob_elapsed < PROMPT_BOUND,
        "glob took {glob_elapsed:?} — queued behind the sibling's diagnose \
         (bug 104); bound {PROMPT_BOUND:?}"
    );

    // Probe 3 — a third agent's diagnostics on the root-B file (the full
    // pipeline, against the un-held server).
    let started = Instant::now();
    let diag_out = scoped_diagnostics(&socket, daemon_pid, "bug104-prober", &other_file)?;
    let diag_elapsed = started.elapsed();
    assert!(
        diag_out.contains("mock diagnostic"),
        "prober's receipt should carry mockls B's diagnostic. Got: {diag_out}"
    );
    assert!(
        diag_elapsed < PROMPT_BOUND,
        "diagnostics took {diag_elapsed:?} — queued behind the sibling's \
         diagnose (bug 104); bound {PROMPT_BOUND:?}"
    );

    // Both root-A runs complete once the burns end — the hold is root-bound,
    // never a wedge.
    let holder_out = join_receipt(holder, "holder")?;
    assert!(
        holder_out.contains("mock diagnostic"),
        "holder's receipt should carry diagnostics. Got: {holder_out}"
    );
    let sibling_out = join_receipt(sibling, "sibling")?;
    assert!(
        sibling_out.contains("mock diagnostic"),
        "sibling's receipt should carry diagnostics. Got: {sibling_out}"
    );

    Ok(())
}

/// Joins a diagnose thread, mapping a panic to an error with the role name.
fn join_receipt(handle: std::thread::JoinHandle<Result<String>>, role: &str) -> Result<String> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{role} diagnose thread panicked"))?
        .with_context(|| format!("{role} diagnose failed"))
}
