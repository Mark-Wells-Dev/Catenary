// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Spawn-path proof for the rust-toolchain pin (misc 176 / bug 92).
//!
//! Drives the FULL daemon path with a STUB `rustup` on `PATH` — no real rustup,
//! no real rust-analyzer — and proves that when the rust engine is spawned
//! through the bare `rust-analyzer` proxy key, the daemon:
//!
//! 1. resolves the root's active toolchain by invoking `rustup show
//!    active-toolchain` with `cwd = the root`, and
//! 2. rewrites the spawn to `rustup run <toolchain> rust-analyzer …` with
//!    `RUSTUP_TOOLCHAIN=<toolchain>` in the child env.
//!
//! The stub records the `run` invocation (its argv and `RUSTUP_TOOLCHAIN`) to a
//! witness file; the test asserts on that witness. This is the integration-shaped
//! coverage the ticket asks for — the child command/env wrapping, proven through
//! the real spawn plumbing, without depending on a real toolchain being present.
//!
//! Unix-only: the stub is a `sh` script made executable via
//! `PermissionsExt` (unsafe-free — no process-env mutation, no new mock binary).

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "shared `common` test helpers use expect for readable assertions"
)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use common::{BridgeProcess, ipc_request};

/// The fixture toolchain the stub `rustup` reports for `show active-toolchain`.
/// Chosen to look like a real toolchain-file pin (`x.y-<triple>`).
const FIXTURE_TOOLCHAIN: &str = "1.95-x86_64-unknown-linux-gnu";

/// Writes a stub `rustup` executable into `bin_dir` that:
/// - answers `show active-toolchain` with [`FIXTURE_TOOLCHAIN`] (plus the
///   `(overridden by …)` annotation a real toolchain-file pin carries — the
///   parser must strip it), and
/// - on `run <tc> <cmd> <args…>` appends `RUSTUP_TOOLCHAIN` and its full argv to
///   `witness`, then exits 0.
///
/// The exit after witnessing means the daemon's `initialize` handshake then
/// fails (no real server), which is irrelevant: the spawn command/env are fixed
/// at spawn time, before any handshake, and that is what we assert on.
fn write_stub_rustup(bin_dir: &std::path::Path, witness: &std::path::Path) -> Result<()> {
    let script = format!(
        r#"#!/bin/sh
# Stub rustup for the rust-toolchain spawn-path test.
if [ "$1" = "show" ] && [ "$2" = "active-toolchain" ]; then
  # Emit a toolchain-file-pin shape; the parser must take the first token.
  echo "{tc} (overridden by '$PWD/rust-toolchain.toml')"
  exit 0
fi
if [ "$1" = "run" ]; then
  # $2 = toolchain, $3.. = the wrapped command + args.
  {{
    echo "RUSTUP_TOOLCHAIN=${{RUSTUP_TOOLCHAIN}}"
    echo "ARGV=$*"
  }} >> "{witness}"
  exit 0
fi
# Any other subcommand: succeed quietly.
exit 0
"#,
        tc = FIXTURE_TOOLCHAIN,
        witness = witness.display(),
    );
    let stub = bin_dir.join("rustup");
    std::fs::write(&stub, script).context("write stub rustup")?;
    let mut perms = std::fs::metadata(&stub)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).context("chmod stub rustup")?;
    Ok(())
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
#[allow(
    clippy::too_many_lines,
    reason = "linear proof: stub setup + daemon drive + witness asserts"
)]
fn rust_analyzer_spawns_through_rustup_run_of_the_resolved_pin() -> Result<()> {
    // Stub bin dir (first on PATH) + witness file the stub appends to.
    let stub_home = tempfile::tempdir()?;
    let bin_dir = stub_home.path().join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let witness = stub_home.path().join("run-witness.txt");
    write_stub_rustup(&bin_dir, &witness)?;
    let bin_dir_str = bin_dir.to_str().context("bin dir path")?.to_string();

    // A rust project root: `.rs` file classifies to the `rust` language, which
    // the shipped defaults bind to the bare `rust-analyzer` server key — the
    // exact condition `should_wrap` gates on. `rust-toolchain.toml` is present
    // so the shape reads as a real pin (the stub ignores its content).
    let work = tempfile::tempdir()?;
    std::fs::write(
        work.path().join("Cargo.toml"),
        "[package]\nname = \"pin\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(
        work.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95\"\n",
    )?;
    std::fs::create_dir_all(work.path().join("src"))?;
    std::fs::write(work.path().join("src/lib.rs"), "pub fn f() {}\n")?;
    let work_path = work.path().canonicalize()?;
    let work_str = work_path.to_str().context("work path")?.to_string();

    // Drive the full daemon. PATH is set to the stub dir FIRST, then the system
    // dirs (so `sh` resolves for the stub). No `CATENARY_SERVERS` override — the
    // shipped `[lsp.server.rust-analyzer]` (bare key) is used, so the spawn goes
    // through the wrap. `HOME` is inherited by `spawn_with` so nothing else in
    // the stub breaks.
    let path_env = format!("{bin_dir_str}:/usr/bin:/bin");
    let base_str = work_str.clone();
    let mut bridge = BridgeProcess::spawn_with(|cmd| {
        cmd.env("PATH", path_env);
        cmd.env("CATENARY_ROOTS", &base_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // Establish the per-root rust-analyzer instance: add the root, then grep to
    // force an on-demand spawn if the eager spawn has not landed yet.
    ipc_request(
        &socket,
        &json!({ "method": "tool/roots-add", "path": work_str }),
    )?;
    let _ = bridge.wait_for_root(&work_str, Duration::from_secs(30));
    for _ in 0..3 {
        let _ = bridge.call_tool_text("grep", &json!({ "pattern": "f", "directory": work_str }));
    }

    // Poll for the stub's witness (the daemon spawns asynchronously; the stub
    // exits fast so `initialize` fails and the server may be re-triggered — each
    // spawn re-appends, which is fine).
    let cache_root = std::path::PathBuf::from(bridge.state_home()).join("cache");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let witnessed = loop {
        if let Ok(text) = std::fs::read_to_string(&witness)
            && text.contains("ARGV=")
        {
            break text;
        }
        if std::time::Instant::now() > deadline {
            let mut log = String::new();
            read_tree(&cache_root, &mut log);
            let spawn_lines: Vec<&str> = log
                .lines()
                .filter(|l| l.contains("rust-toolchain") || l.contains("Spawning LSP server"))
                .collect();
            anyhow::bail!(
                "stub rustup was never invoked with `run` within the deadline.\n\
                 witness exists: {}\nrelevant firehose lines:\n{}",
                witness.exists(),
                spawn_lines.join("\n"),
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    // Proof 1: the daemon invoked `rustup run <resolved> rust-analyzer …`.
    assert!(
        witnessed.contains(&format!("ARGV=run {FIXTURE_TOOLCHAIN} rust-analyzer")),
        "expected `rustup run {FIXTURE_TOOLCHAIN} rust-analyzer …`, got witness:\n{witnessed}",
    );
    // Proof 2: `RUSTUP_TOOLCHAIN` is set in the child env (belt-and-braces for
    // proxied flycheck sub-invocations).
    assert!(
        witnessed.contains(&format!("RUSTUP_TOOLCHAIN={FIXTURE_TOOLCHAIN}")),
        "expected RUSTUP_TOOLCHAIN={FIXTURE_TOOLCHAIN} in the child env, got:\n{witnessed}",
    );

    // Proof 3: the daemon logged the resolution outcome at info! naming the
    // toolchain (the operator-visible surface for a pinned-but-not-installed
    // failure — the error names the toolchain that was attempted).
    let mut log = String::new();
    read_tree(&cache_root, &mut log);
    assert!(
        log.contains("rust-toolchain: spawning rust-analyzer through")
            && log.contains(FIXTURE_TOOLCHAIN),
        "expected an info! spawn-through-rustup-run line naming {FIXTURE_TOOLCHAIN} in the firehose",
    );

    Ok(())
}
