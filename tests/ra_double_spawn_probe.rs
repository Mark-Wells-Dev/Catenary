// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Confirms that the per-root LSP double-spawn which makes the mockls-based
//! `ws31_review_r4` flake under load is a FAST-MOCK artifact, not a real daemon
//! behavior that bites production servers.
//!
//! Drives the FULL daemon path (`BridgeProcess` → roots-add/rm → on-demand grep,
//! racing the eager `spawn_all` against the on-demand spawn for one root) with the
//! REAL `rust-analyzer`, and counts the daemon's OWN spawn telemetry
//! (`info!("Spawning LSP server …")`) read from the JSONL firehose — a
//! production-grade signal with zero dependence on the mock request log. If any
//! single re-add window shows >1 spawn for one root, the daemon launched two real
//! rust-analyzer processes for one project (production-reachable). It does not:
//! a real server's slow `initialize` is held under the spawn lock, so a racing
//! second trigger blocks and dedups — the narrow window only opens for mockls's
//! near-instant init. Verified GREEN over 60 re-add cycles under CPU saturation.
//!
//! Ignored by default (requires a real rust-analyzer toolchain). Run with:
//!   `make test-ignored T=ra_double_spawn_on_readd_probe`

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "shared `common` test helpers use expect for readable assertions"
)]

mod common;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use common::{BridgeProcess, ipc_request};

/// Counts daemon spawn-telemetry lines scoped to `root`.
fn count_spawns(log: &str, root: &str) -> usize {
    log.lines()
        .filter(|l| l.contains("Spawning LSP server") && l.contains(root))
        .count()
}

/// Recursively concatenates every file under `dir` (the JSONL firehose tree).
fn read_tree(dir: &std::path::Path, out: &mut String) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                read_tree(&p, out);
            } else if let Ok(t) = std::fs::read_to_string(&p) {
                out.push_str(&t);
                out.push('\n');
            }
        }
    }
}

#[test]
#[ignore = "requires real rust-analyzer"]
#[allow(
    clippy::too_many_lines,
    reason = "linear probe: setup + cycle loop + asserts"
)]
fn ra_double_spawn_on_readd_probe() -> Result<()> {
    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // Throwaway cargo project so rust-analyzer has a real workspace to index.
    let work = tempfile::tempdir()?;
    std::fs::write(
        work.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::create_dir_all(work.path().join("src"))?;
    std::fs::write(
        work.path().join("src/lib.rs"),
        "pub fn callee() {}\npub fn caller() { callee(); }\n",
    )?;
    let work_path = work.path().canonicalize()?;
    let work_str = work_path.to_str().context("work path")?;

    // Drive via spawn_with so PATH (cleared by isolate_env) is restored — the
    // rustup proxy needs it; HOME is inherited so RUSTUP_HOME resolves.
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("CATENARY_SERVERS", "rust:rustup run stable rust-analyzer");
        cmd.env("CATENARY_ROOTS", base_str);
        cmd.env("PATH", "/usr/bin:/bin");
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;
    // The daemon's tracing has no stderr fmt layer — `info!` events land in the
    // JSONL firehose under XDG_CACHE_HOME (= <state_home>/cache in isolation).
    let cache_root = std::path::PathBuf::from(bridge.state_home()).join("cache");
    let read_log = || {
        let mut s = String::new();
        read_tree(&cache_root, &mut s);
        s
    };

    // Establish the first RA instance for the work root.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    bridge.wait_for_root(work_str, Duration::from_secs(30))?;
    let _ = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "callee", "directory": work_str }),
    );

    let mut window_counts = Vec::new();
    for _cycle in 0..60 {
        // Remove + wait until untracked (the per-root RA is torn down).
        ipc_request(
            &socket,
            &json!({ "method": "tool/roots-rm", "path": work_str }),
        )?;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let ls = ipc_request(&socket, &json!({ "method": "tool/roots-ls" }))?;
            if !ls.contains(work_str) {
                break;
            }
            if std::time::Instant::now() > deadline {
                anyhow::bail!("work root still tracked after roots-rm");
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let before = count_spawns(&read_log(), work_str);

        // Re-add (fires the fire-and-forget eager spawn_all) and immediately
        // grep (on-demand spawn) to race the two spawn triggers for one root.
        ipc_request(
            &socket,
            &json!({ "method": "tool/roots-add", "path": work_str }),
        )?;
        for _ in 0..4 {
            let _ = bridge.call_tool_text(
                "grep",
                &json!({ "pattern": "callee", "directory": work_str }),
            );
        }
        bridge.wait_for_root(work_str, Duration::from_secs(30))?;

        // Poll until at least one spawn lands in this window.
        let wdeadline = std::time::Instant::now() + Duration::from_secs(12);
        loop {
            if count_spawns(&read_log(), work_str).saturating_sub(before) >= 1 {
                break;
            }
            if std::time::Instant::now() > wdeadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Settle to catch a late second spawn.
        std::thread::sleep(Duration::from_millis(750));
        let window = count_spawns(&read_log(), work_str).saturating_sub(before);
        window_counts.push(window);
    }

    let total: usize = window_counts.iter().sum();
    let log = read_log();
    let spawn_lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("Spawning LSP server"))
        .collect();
    let diag = format!(
        "per-re-add-window spawn counts (scoped to work root) = {window_counts:?}, total={total}\n\
         all 'Spawning LSP server' lines ({}):\n{}",
        spawn_lines.len(),
        spawn_lines.join("\n"),
    );
    // Sanity: a silent pass must not hide "RA never actually re-spawned" (e.g.
    // binary not found). Each re-add window should produce >= 1 spawn.
    assert!(
        total >= window_counts.len(),
        "probe did not exercise re-spawns — RA may not have launched.\n{diag}"
    );
    let maxw = window_counts.iter().copied().max().unwrap_or(0);
    assert!(
        maxw <= 1,
        "real rust-analyzer DOUBLE-SPAWNED on re-add. Any window >1 ⇒ the daemon \
         launched 2 rust-analyzer processes for one root in one add cycle ⇒ \
         production-reachable, not a mock-harness artifact.\n{diag}"
    );
    Ok(())
}
