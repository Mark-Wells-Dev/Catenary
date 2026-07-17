// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! The `catenary grep` lint sink, end to end through the CLI binary (ws43-04).
//!
//! The acceptance pins that must hold at the BINARY seam, not just the library
//! one: with the daemon STOPPED (an isolated state dir holds no socket), a
//! lint-covered file's hits come back lint-annotated — pool-less lint work
//! requires no daemon — and a missing linter degrades to pass-through with one
//! stderr advisory, never an error and never a dropped hit. The linter is the
//! hermetic `mocklint` (`--format shellcheck`), so no real shellcheck is
//! needed.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use common::{BridgeProcess, isolate_env, xdg_config_home};

/// A repo fixture: a `.git` marker (the CLI-side lint router's root discovery)
/// and a shell script with two `needle` lines.
fn repo_with_script() -> Result<tempfile::TempDir> {
    let repo = tempfile::tempdir().context("repo tempdir")?;
    std::fs::create_dir(repo.path().join(".git")).context("git marker")?;
    std::fs::write(repo.path().join("build.sh"), "needle $HOME\nneedle done\n")
        .context("write script")?;
    Ok(repo)
}

/// Writes a user config whose one `[linter.rule.shellcheck]` runs `command`.
fn write_linter_config(dir: &Path, command: &str, args: &[&str]) -> Result<std::path::PathBuf> {
    let rendered_args = args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[linter.rule.shellcheck]\n\
             command = \"{command}\"\n\
             args = [{rendered_args}]\n\
             patterns = [\"**/*.sh\"]\n",
        ),
    )
    .context("write linter config")?;
    Ok(path)
}

/// Runs `catenary grep needle` in `repo` against an isolated state dir (no
/// daemon has ever started there — the no-daemon arm by construction).
fn run_grep_no_daemon(state: &Path, config: &Path, repo: &Path) -> Result<std::process::Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state.to_str().context("state path")?);
    cmd.env("CATENARY_CONFIG", config);
    cmd.args(["grep", "needle"])
        .current_dir(repo)
        // /dev/null stdin: NOT a readable stream, so the CLI takes the
        // filesystem path, never stdin mode.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().context("run catenary grep")
}

/// The no-daemon pin: a lint-covered file's hits come back lint-annotated with
/// the daemon STOPPED — the diagnostic line carries its `source/code` trail
/// and the verified clean line renders covered, not `#?`.
#[test]
fn lint_covered_hits_annotate_with_the_daemon_stopped() -> Result<()> {
    let state = tempfile::tempdir().context("state tempdir")?;
    let repo = repo_with_script()?;
    let config = write_linter_config(
        state.path(),
        env!("CARGO_BIN_EXE_mocklint"),
        &[
            "--format",
            "shellcheck",
            "--diag",
            "SC2086|1|8|Double quote to prevent globbing",
        ],
    )?;

    let output = run_grep_no_daemon(state.path(), &config, repo.path())?;
    assert!(
        output.status.success(),
        "daemon-less lint-annotated grep exits 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("build.sh:1#shellcheck/SC2086:needle $HOME"),
        "the lint diagnostic annotates its hit with no daemon at all:\n{stdout}",
    );
    assert!(
        stdout.contains("build.sh:2:needle done"),
        "the verified clean line renders covered (no `#?`):\n{stdout}",
    );
    assert!(
        !stdout.contains("build.sh:2#?"),
        "a lint-verified line never wears the could-not-enrich marker:\n{stdout}",
    );
    Ok(())
}

/// Linter absence: pass-through hits plus exactly one stderr advisory — never
/// an error, never a dropped hit.
#[test]
fn absent_linter_passes_through_with_one_stderr_advisory() -> Result<()> {
    let state = tempfile::tempdir().context("state tempdir")?;
    let repo = repo_with_script()?;
    let config = write_linter_config(state.path(), "/nonexistent/catenary-missing-linter", &[])?;

    let output = run_grep_no_daemon(state.path(), &config, repo.path())?;
    assert!(
        output.status.success(),
        "a missing linter is a degrade, never an error; stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("build.sh:1#?:needle $HOME") && stdout.contains("build.sh:2#?:needle done"),
        "every hit survives, in the pass-through spelling:\n{stdout}",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("not found").count(),
        1,
        "exactly one advisory names the missing linter:\n{stderr}",
    );
    assert!(
        stderr.contains("shellcheck"),
        "the advisory names the linter:\n{stderr}",
    );
    Ok(())
}

/// The daemon-present wiring: with a live daemon serving the annotation
/// stream, lint-covered hits STILL annotate through the local linter sink
/// (mixed batches split by coverage), and the uncovered file's hits ride the
/// daemon (no covering server ⇒ the `#?` pass-through spelling).
#[test]
fn lint_sink_rides_beside_a_live_daemon() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        // A repository root the CLI-side router can discover, holding one
        // lint-covered file and one uncovered file.
        std::fs::create_dir(root.join(".git"))?;
        std::fs::write(root.join("aaa.txt"), "needle text\n")?;
        std::fs::write(root.join("build.sh"), "needle $HOME\nneedle done\n")?;
        // The daemon's config: empty (defaults) — the lint sink is CLI-side.
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "")?;
        Ok(config_path)
    })?;
    bridge.initialize()?;
    // The CLI must actually reach the daemon — otherwise this would silently
    // exercise the (already-pinned) no-daemon arm instead of the mixed one.
    bridge.wait_for_ipc_socket()?;

    // The grep CLI subprocess loads the isolated user config
    // (`<config-home>/catenary/config.toml`) — point its shellcheck rule at
    // the hermetic mocklint.
    let config_dir = xdg_config_home(bridge.state_home()).join("catenary");
    std::fs::create_dir_all(&config_dir).context("mkdir user config dir")?;
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[linter.rule.shellcheck]\n\
             command = \"{}\"\n\
             args = [\"--format\", \"shellcheck\", \"--diag\", \
             \"SC2086|1|8|Double quote to prevent globbing\"]\n\
             patterns = [\"**/*.sh\"]\n",
            env!("CARGO_BIN_EXE_mocklint"),
        ),
    )
    .context("write user config")?;

    let body = bridge.call_grep(&serde_json::json!({ "pattern": "needle" }))?;
    assert!(
        body.contains("build.sh:1#shellcheck/SC2086:needle $HOME"),
        "lint-covered hits annotate locally beside a live daemon:\n{body}",
    );
    assert!(
        body.contains("build.sh:2:needle done"),
        "the lint-verified clean line renders covered:\n{body}",
    );
    assert!(
        body.contains("aaa.txt:1#?:needle text"),
        "the uncovered file's hits ride the daemon (no covering server):\n{body}",
    );
    Ok(())
}
