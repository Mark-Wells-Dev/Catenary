// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Bounded query enrichment (misc 197 stage 1).
//!
//! `catenary grep`/`glob` must never go SILENT under a wedged/busy server. The
//! enrichment path (`ensure_and_wait_for_paths`) can block unboundedly on a
//! server's spawn/settle; the query variant bounds that wait
//! (`QUERY_ENRICHMENT_BUDGET`, ~5 s) and serves the results UNENRICHED past the
//! bound. Decision 025's additive doctrine: results are complete either way,
//! only enrichment degrades.
//!
//! This drives a real daemon whose server sits behind a long `--response-delay`
//! so its `initialize` handshake never completes within the test window. A grep
//! against it must return COMPLETE matches within a bound far below the stall,
//! rather than hanging until the server happens to answer.

mod common;

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use common::BridgeProcess;

const MOCK_LANG: &str = "mockls-event";

/// The server stalls its `initialize` response this long — far above the
/// query enrichment budget (~5 s). A grep that waited for the settle would take
/// at least this long; a bounded one returns in a few seconds.
const STALL_MS: u64 = 30_000;

/// Promptness bound for the bounded query. Generously above the enrichment
/// budget (~5 s) plus daemon/ripgrep overhead and CPU contention, yet far below
/// [`STALL_MS`] — so a grep that returns under this bound provably did NOT wait
/// the full server stall (the pre-fix silent hang).
const PROMPT_BOUND: Duration = Duration::from_secs(20);

/// A grep against a server whose `initialize` handshake stalls for 30 s must
/// return its COMPLETE matches within the enrichment bound (~5 s), not hang
/// until the settle finishes. Only enrichment degrades past the bound; the
/// ripgrep matches themselves are complete regardless of server state.
#[test]
fn grep_returns_complete_results_when_server_settle_stalls() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let root = dir.path().to_path_buf();
    // Two matched files, so "complete" means BOTH hits come back — an
    // enrichment bound must never drop a match.
    let file_a = root.join(format!("alpha.{MOCK_LANG}"));
    let file_b = root.join(format!("beta.{MOCK_LANG}"));
    std::fs::write(&file_a, "fn needle_here() {}\n")?;
    std::fs::write(&file_b, "let needle_here = 1\n")?;

    // The server sleeps 30 s before answering EVERY request — including
    // `initialize`, so its handshake never completes within the test. The
    // enrichment ensure would block on that spawn/settle unbounded pre-fix.
    let lsp = common::mockls_lsp_arg(MOCK_LANG, &format!("--response-delay {STALL_MS}"));
    let root_str = root.to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root_str)?;
    bridge.initialize_with_roots(&[root_str])?;

    // Grep for the needle. Ripgrep (step 1) finds both hits before the
    // enrichment ensure (step 2) is even reached, so the matches are complete no
    // matter what the server does; the bound only decides whether we WAIT for
    // enrichment. This must return promptly, not after the 30 s stall.
    let started = Instant::now();
    let grep_out = bridge.call_grep(&json!({
        "pattern": "needle_here",
        "directory": root_str,
    }))?;
    let elapsed = started.elapsed();

    // Complete results: BOTH matched files present.
    assert!(
        grep_out.contains(&format!("alpha.{MOCK_LANG}")),
        "grep must return the alpha hit unenriched. Got: {grep_out}"
    );
    assert!(
        grep_out.contains(&format!("beta.{MOCK_LANG}")),
        "grep must return the beta hit unenriched. Got: {grep_out}"
    );
    // Returned within the bound, not after the server's 30 s stall — proof the
    // enrichment wait was bounded rather than pinned on the settle.
    assert!(
        elapsed < PROMPT_BOUND,
        "grep took {elapsed:?} — it hung on the stalled server settle instead of \
         serving unenriched at the bound; expected under {PROMPT_BOUND:?} \
         (server stall is {STALL_MS} ms)"
    );

    Ok(())
}
