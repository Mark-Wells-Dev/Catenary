// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration tests for per-root `SymbolIndex` eviction on root removal
//! (fs-coherence ticket 05, bug #36).
//!
//! The daemon-lived `SymbolIndex` is warmed by enriched grep. When a root
//! leaves the tracked set (MCP disconnect, `catenary roots rm`), the symbol +
//! enrichment entries for that root must be evicted so an untracked path can
//! no longer serve enrichment from a dead session's cache, and so a re-tracked
//! root sees a genuine cold first touch.
//!
//! Eviction is wired at the `Session::sync_roots` layer, which every removal
//! path routes through. These end-to-end tests drive the real removal paths
//! (`roots-rm` IPC and MCP disconnect) against a `mockls`-backed daemon and
//! assert the observable behavior; the prefix-sweep itself is covered by the
//! `evict_root_prefix_sweep_drops_under_root_keeps_siblings` unit test in
//! `src/symbol_index.rs`.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use common::{BridgeProcess, grep_until_enriched, ipc_request, mockls_lsp_arg};

const MOCK_LANG: &str = "evict_test";

/// Body where a callee is defined first, then a caller whose braced body names
/// the callee. Grepping the callee surfaces its in-body usage, which is enclosed
/// by the caller — so the hit carries the `#<caller>` containment anchor, the
/// observable that proves the daemon answered `documentSymbol` and the index is
/// warm (the post-nav-suite readiness signal).
fn caller_callee(callee: &str, entry: &str) -> String {
    format!("fn {callee}\nfn {entry} {{\n{callee}\n}}\n")
}

/// `catenary roots rm` evicts the removed root's enrichment from the
/// daemon-lived `SymbolIndex`.
///
/// A sibling root is added via the hook contributor (`roots-add`), warmed by an
/// enriched grep, then removed (`roots-rm`). The post-removal grep on the same
/// path must not surface cache-served enrichment for the now-untracked root.
#[test]
fn enrichment_evicted_on_root_removal() -> Result<()> {
    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // The warmed root is a sibling directory added at runtime. Canonicalize so
    // it matches the canonical form `roots-add` stores and `roots-ls` reports.
    let sibling = tempfile::tempdir()?;
    let sibling_path = sibling.path().canonicalize()?;
    let sibling_str = sibling_path.to_str().context("sibling path")?;
    let file = sibling.path().join(format!("warm.{MOCK_LANG}"));
    std::fs::write(&file, caller_callee("callee_evict", "caller_evict"))?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], base_str)?;
    bridge.initialize()?;

    let socket = bridge.wait_for_ipc_socket()?;

    // Add the sibling root via the hook contributor (the `catenary roots add`
    // path), then wait for it to appear as tracked.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": sibling_str }),
    )?;
    bridge.wait_for_root(sibling_str, Duration::from_secs(5))?;

    // Warm the SymbolIndex: enriched grep on the sibling root. Retry until the
    // `calls:` enrichment signal appears instead of sleeping to guess the per-root
    // server + `--scan-roots` index is ready — the enrichment-present output IS the
    // readiness signal, so the grep can't race server readiness under contention.
    let warm = grep_until_enriched(
        &bridge,
        &json!({ "pattern": "callee_evict", "directory": sibling_str }),
    )?;
    assert!(
        warm.contains("#caller_evict"),
        "warming grep must be enriched — the callee's in-body usage carries the \
         `#caller_evict` scope anchor, got:\n{warm}"
    );

    // Remove the sibling root via the hook contributor (`catenary roots rm`).
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-rm", "path": sibling_str }),
    )?;
    // Wait until ls-roots no longer reports the sibling as tracked.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ls = ipc_request(&socket, &json!({ "method": "tool/roots-ls" }))?;
        if !ls.contains(sibling_str) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "sibling root still tracked after roots-rm: {ls}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Grep the same path now that the root is untracked. `evict_root` dropped the
    // daemon-lived outline for it; the grep still serves the raw ripgrep match (a
    // strict superset of grep never drops a hit). The containment-anchor model
    // has no per-position enrichment cache, so there is nothing stale to leak —
    // bug #36's concern is structurally resolved (whatever the path enriches to
    // is computed fresh per query, never served from a dead session's cache).
    let after = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "callee_evict", "directory": sibling_str }),
    )?;
    assert!(
        after.contains("callee_evict"),
        "untracked root must still serve the raw match, got:\n{after}"
    );

    Ok(())
}

/// Bug #36 scenario: a path under a root that has left the tracked set does not
/// present cached enrichment as authoritative.
///
/// Driven through the MCP disconnect path: the warming bridge declares the root
/// via `roots/list`; dropping it removes its contributor and routes the reduced
/// set through `Session::sync_roots`, which evicts the dropped root. A second
/// bridge (declaring only an unrelated base root) keeps the daemon alive and
/// observes that the dropped root no longer serves enrichment.
#[test]
fn untracked_root_does_not_serve_cached_enrichment() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131) for the shared-state spawns below.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    // The base root keeps the daemon alive after the warming bridge drops.
    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // The warmed-then-dropped root.
    let dropped = tempfile::tempdir()?;
    let dropped_path = dropped.path().canonicalize()?;
    let dropped_str = dropped_path.to_str().context("dropped path")?;
    let file = dropped.path().join(format!("sib.{MOCK_LANG}"));
    std::fs::write(&file, caller_callee("callee_drop", "caller_drop"))?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");

    // Keeper bridge — declares only the base root, holds the daemon open.
    let mut keeper = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    keeper.initialize_with_roots(&[base_str])?;

    // Warming bridge — declares the dropped root.
    let mut warmer = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", dropped_str);
    })?;
    warmer.initialize_with_roots(&[dropped_str])?;
    warmer.wait_for_root(dropped_str, Duration::from_secs(5))?;

    // Warm the SymbolIndex via the warming bridge. Retry until the `calls:`
    // enrichment signal appears (the readiness signal) instead of a fixed sleep.
    let warm = grep_until_enriched(
        &warmer,
        &json!({ "pattern": "callee_drop", "directory": dropped_str }),
    )?;
    assert!(
        warm.contains("#caller_drop"),
        "warming grep must be enriched (callee usage carries `#caller_drop`), got:\n{warm}"
    );

    // Drop the warming bridge — its MCP contributor is removed and the reduced
    // root set is synced, evicting the dropped root from the SymbolIndex.
    drop(warmer);

    // Wait until the keeper no longer sees the dropped root as tracked.
    let socket = keeper.wait_for_ipc_socket()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ls = ipc_request(&socket, &json!({ "method": "tool/roots-ls" }))?;
        if !ls.contains(dropped_str) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "dropped root still tracked after disconnect: {ls}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Through the keeper, the dropped (now untracked) path still serves the raw
    // ripgrep match. There is no per-position enrichment cache in the
    // containment-anchor model, so a dropped root can never serve a dead
    // session's *stale* enrichment as authoritative (bug #36) — any anchor is
    // recomputed fresh per query.
    let after = keeper.call_tool_text(
        "grep",
        &json!({ "pattern": "callee_drop", "directory": dropped_str }),
    )?;
    assert!(
        after.contains("callee_drop"),
        "dropped untracked root must still serve the raw match, got:\n{after}"
    );

    Ok(())
}

/// Eviction restores cold-state testability: after a root is removed and
/// re-added, the first touch re-resolves from the live server rather than
/// being contaminated by a stale warm cache.
#[test]
fn cold_first_touch_is_actually_cold_after_eviction() -> Result<()> {
    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    let work = tempfile::tempdir()?;
    let work_path = work.path().canonicalize()?;
    let work_str = work_path.to_str().context("work path")?;
    let file = work.path().join(format!("cold.{MOCK_LANG}"));
    std::fs::write(&file, caller_callee("callee_cold", "caller_cold"))?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");
    let mut bridge = BridgeProcess::spawn(&[&lsp], base_str)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // Add and warm the work root.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    bridge.wait_for_root(work_str, Duration::from_secs(5))?;
    // Retry until the `#scope` enrichment anchor appears (readiness signal).
    let warm = grep_until_enriched(
        &bridge,
        &json!({ "pattern": "callee_cold", "directory": work_str }),
    )?;
    assert!(
        warm.contains("#caller_cold"),
        "first warm grep must be enriched (callee usage carries `#caller_cold`), got:\n{warm}"
    );

    // Remove the root (evicts the warm cache).
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-rm", "path": work_str }),
    )?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ls = ipc_request(&socket, &json!({ "method": "tool/roots-ls" }))?;
        if !ls.contains(work_str) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "work root still tracked after roots-rm: {ls}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Re-add the root. With the warm cache evicted, the first touch is a
    // genuine cold resolve against the live server — enrichment is produced
    // afresh, not contaminated by a surviving warm entry.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    bridge.wait_for_root(work_str, Duration::from_secs(5))?;
    // Retry until the `#scope` enrichment anchor appears — the genuine cold first
    // touch re-resolves the `documentSymbol` outline from the live server;
    // polling on that signal (not a fixed sleep) makes it contention-safe.
    let cold = grep_until_enriched(
        &bridge,
        &json!({ "pattern": "callee_cold", "directory": work_str }),
    )?;
    assert!(
        cold.contains("callee_cold"),
        "re-added root must serve the symbol, got:\n{cold}"
    );
    assert!(
        cold.contains("#caller_cold"),
        "cold first touch after eviction must re-resolve enrichment, got:\n{cold}"
    );

    Ok(())
}
