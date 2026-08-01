// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Regression coverage for misc 172: `catenary grep` output must be *current*
//! and must *disclose its scope*.
//!
//! The sighting ("grep served pre-edit content after Edit-tool edits") turned
//! out not to be a cache at all — the misc-92 result cache was already deleted
//! (`ccb7971`). The worker's grep used **relative** glob arguments
//! (`'src/**/*.rs'`) while its shell cwd had been reset to the main checkout,
//! so the daemon correctly searched the *main tree* and returned its unedited
//! content — rendered cwd-relative, with no scope line, indistinguishable from
//! worktree hits. Two contracts are pinned here:
//!
//! 1. **Currency** — a repeat of the same query against the same long-lived
//!    daemon, across an on-disk edit, returns post-edit content at post-edit
//!    line numbers (guards against any future result cache replaying pre-edit
//!    output for an untracked root — the ticket's original suspect).
//! 2. **Scope disclosure** — results whose scope was anchored at the cwd
//!    (relative patterns, or pathless) open with the `cwd:` anchor line, so a
//!    wrong-cwd search is detectable from its output alone.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "the shared tests/common module uses expect for readable assertions"
)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use common::{BridgeProcess, isolate_env, xdg_state_home};

/// Runs the `catenary` binary for `subargs` with cwd = `cwd`, isolated to
/// `state_home`, and returns `(stdout, stderr, success)`.
fn run_cli(state_home: &str, cwd: &Path, subargs: &[&str]) -> Result<(String, String, bool)> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(cwd)
        .args(subargs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().context("run catenary binary")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    ))
}

/// A repeat of the byte-identical query against the *same live daemon*, across
/// an on-disk rewrite, must serve the post-edit content at the post-edit line
/// number — never a replay of the first answer.
///
/// The searched tree is deliberately **not** a registered root (the misc-172
/// sighting condition: an untracked worktree with at most an ephemeral
/// holder), so any query-keyed cache that relies on tracked-root generation
/// bumps for invalidation would serve the first answer here and fail this
/// test.
#[test]
fn repeat_query_after_edit_serves_post_edit_content() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?.to_string();
    // Panic-safe daemon teardown (bug 131) for the shared-state spawn below.
    let _daemon_guard = common::DaemonGuard::new(&state_home);

    // A daemon with no roots and no LSP servers — the tree stays untracked.
    let mut bridge = BridgeProcess::spawn_in_state(&state_home, |_cmd| {})?;
    bridge.initialize()?;

    // Wait for the IPC socket the search binary connects to.
    let ipc_sock = xdg_state_home(state_dir.path())
        .join("catenary")
        .join("catenary.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ipc_sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ipc_sock.exists(), "daemon IPC socket should appear");

    let root = tempfile::tempdir()?;
    let probe = root.path().join("probe.rs");
    std::fs::write(
        &probe,
        "// probe\nfn original() {\n    let marker = \"M172_original_line3\";\n}\n",
    )?;

    let probe_str = probe.to_str().context("probe path")?;
    let query = ["grep", "M172_", probe_str];

    let (first, _, ok) = run_cli(&state_home, root.path(), &query)?;
    assert!(ok, "first grep must exit 0");
    assert!(
        first.contains("M172_original_line3"),
        "first query sees the original content: {first}"
    );
    assert!(
        first.contains(":3"),
        "original content sits on line 3: {first}"
    );

    // The edit: content replaced, marker line shifted 3 → 5.
    std::fs::write(
        &probe,
        "// probe\n// shift\n// shift\nfn edited() {\n    let marker = \"M172_edited_line5\";\n}\n",
    )?;

    let (second, _, ok) = run_cli(&state_home, root.path(), &query)?;
    assert!(ok, "repeat grep must exit 0");
    assert!(
        second.contains("M172_edited_line5"),
        "the repeat of the same query must serve post-edit content: {second}"
    );
    assert!(
        second.contains(":5"),
        "post-edit content sits on line 5: {second}"
    );
    assert!(
        !second.contains("M172_original_line3"),
        "pre-edit content must never survive into a repeat query: {second}"
    );

    drop(bridge);
    Ok(())
}

/// The misc-172 sighting shape, end to end: a grep with a **relative** glob
/// runs from a cwd that is a *different tree* than the one the agent edited.
/// The hits legitimately come from the cwd tree — and the output must open
/// with the `cwd:` anchor so that provenance is visible. An absolute-scope
/// query over the edited tree stays anchor-free and current.
#[test]
fn relative_scope_results_disclose_the_cwd_anchor() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    // Tree A — the shell cwd (the "main checkout"): still has the old text.
    let tree_a = tempfile::tempdir()?;
    std::fs::create_dir_all(tree_a.path().join("src"))?;
    std::fs::write(
        tree_a.path().join("src/paths.rs"),
        "fn a() {\n    let x = M172_OLD_TEXT;\n}\n",
    )?;

    // Tree B — the "worktree" the agent actually edited.
    let tree_b = tempfile::tempdir()?;
    std::fs::create_dir_all(tree_b.path().join("src"))?;
    std::fs::write(
        tree_b.path().join("src/paths.rs"),
        "fn b() {\n    let x = M172_NEW_TEXT;\n}\n",
    )?;

    // The sighting command shape: relative glob, cwd = tree A. The expected
    // anchor uses the canonical path (the CLI reads its cwd via `getcwd`,
    // which resolves symlinked prefixes — macOS `/var` → `/private/var`) and
    // applies the CLI's `~` home compression. That compression resolves home
    // through `paths::home_dir()`, so under `isolate_env` the reference point is
    // the isolated home base, not the operator's real `$HOME` (misc 229) — the
    // test derives it the same way so both sides agree wherever `TMPDIR` lives.
    let (stdout, _, ok) = run_cli(state_home, tree_a.path(), &["grep", "M172_", "src/**/*.rs"])?;
    assert!(ok, "relative-scope grep must exit 0");
    let canon = tree_a.path().canonicalize()?;
    let compressed = canon
        .strip_prefix(common::catenary_home(state_home))
        .map_or_else(
            |_| canon.display().to_string(),
            |rest| format!("~/{}", rest.display()),
        );
    let cwd_line = format!("cwd: {compressed}");
    assert!(
        stdout.starts_with(&cwd_line),
        "cwd-anchored results must open with the `cwd:` anchor line\n\
         expected prefix: {cwd_line}\ngot:\n{stdout}"
    );
    assert!(
        stdout.contains("M172_OLD_TEXT"),
        "the hits are honestly the cwd tree's content: {stdout}"
    );
    assert!(
        !stdout.contains("M172_NEW_TEXT"),
        "no other tree's content is substituted (bug 31): {stdout}"
    );

    // The absolute-scope control: same daemon-less machinery, edited tree
    // named absolutely — current content, and no anchor line grows.
    let tree_b_src = tree_b.path().join("src");
    let tree_b_str = tree_b_src.to_str().context("tree b src")?;
    let (stdout, _, ok) = run_cli(state_home, tree_a.path(), &["grep", "M172_", tree_b_str])?;
    assert!(ok, "absolute-scope grep must exit 0");
    assert!(
        stdout.contains("M172_NEW_TEXT"),
        "absolute scope serves the named tree's current content: {stdout}"
    );
    assert!(
        !stdout.starts_with("cwd:"),
        "absolute-only scopes stay anchor-free (byte-identical): {stdout}"
    );

    Ok(())
}
