// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for CLI list, monitor, config, and doctor commands.

mod common;

use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use common::isolate_env;

/// `isolate_env` must point the four XDG base dirs at *distinct* subdirs
/// of the root, so a subprocess writing under the wrong base can no
/// longer silently land in the one shared directory. Regression guard
/// for the split.
#[test]
fn isolate_env_distinct_subdirs() {
    use std::collections::{HashMap, HashSet};

    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path().to_str().expect("tempdir path");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);

    // `.env()` entries appear with a value; `.env_remove()` entries
    // appear with `None` and are skipped here.
    let envs: HashMap<String, String> = cmd
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v?.to_str()?.to_owned())))
        .collect();

    let dirs: Vec<&String> = [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ]
    .iter()
    .map(|var| envs.get(*var).expect("XDG base dir set by isolate_env"))
    .collect();

    // Every base dir lives under the root...
    for dir in &dirs {
        assert!(dir.starts_with(root), "{dir} should be under root {root}");
    }

    // ...and all four resolve to distinct paths.
    let distinct: HashSet<&&String> = dirs.iter().collect();
    assert_eq!(
        distinct.len(),
        4,
        "the four XDG base dirs should be distinct, got: {dirs:?}"
    );
}

// ── catenary config ─────────────────────────────────────────────

#[test]
fn test_config_outputs_valid_toml() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("config");

    let output = cmd.output().context("Failed to run catenary config")?;
    assert!(output.status.success(), "catenary config should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    toml::from_str::<toml::Value>(&stdout)
        .with_context(|| format!("catenary config output is not valid TOML:\n{stdout}"))?;
    Ok(())
}

#[test]
fn test_config_contains_allowlist_sections() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("config");

    let output = cmd.output().context("Failed to run catenary config")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("# [commands]"),
        "output should contain commented-out [commands] section"
    );
    assert!(stdout.contains("allow"), "output should contain allow key");
    assert!(
        stdout.contains("pipeline"),
        "output should contain pipeline key"
    );
    assert!(
        stdout.contains("client_enforcement_only"),
        "output should contain client_enforcement_only option"
    );
    Ok(())
}

// ── catenary doctor suggestions ─────────────────────────────────

#[test]
fn test_doctor_suggests_config_when_no_config_file() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("doctor").arg("--nocolor");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Suggestions section should appear at the bottom
    assert!(
        stdout.contains("Suggestions:"),
        "doctor should show Suggestions section when no config exists, got:\n{stdout}"
    );
    assert!(
        stdout.contains("catenary config"),
        "doctor should suggest `catenary config`, got:\n{stdout}"
    );
    assert!(
        stdout.contains("No config file found"),
        "doctor should mention missing config file, got:\n{stdout}"
    );

    // Suggestions should be the last section
    let suggestions_pos = stdout
        .rfind("Suggestions:")
        .context("Suggestions: not found")?;
    let filter_pos = stdout
        .rfind("Command filter:")
        .context("Command filter: not found")?;
    assert!(
        suggestions_pos > filter_pos,
        "Suggestions should appear after Command filter"
    );
    Ok(())
}

#[test]
fn test_doctor_no_suggestions_when_config_with_commands() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(
        config_dir.join("config.toml"),
        "[commands]\nallow = [\"git\"]\n",
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("doctor").arg("--nocolor");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        !stdout.contains("Suggestions:"),
        "doctor should not show Suggestions when config with commands exists, got:\n{stdout}"
    );
    Ok(())
}

#[test]
fn test_doctor_suggests_commands_when_config_exists_without_commands() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(config_dir.join("config.toml"), "# no commands section\n")?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("doctor").arg("--nocolor");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("Suggestions:"),
        "doctor should show Suggestions when config has no [commands], got:\n{stdout}"
    );
    assert!(
        stdout.contains("No [commands] section"),
        "doctor should mention missing [commands] section, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("No config file found"),
        "should not say config file is missing when it exists, got:\n{stdout}"
    );
    Ok(())
}

// ── catenary doctor single-server mode ──────────────────────────

#[test]
fn test_doctor_single_server_found() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.mockls-test]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [lsp.language.test]\n\
             servers = [\"mockls-test\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "mockls-test", "--nocolor"]);

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show verbose sections
    assert!(
        stdout.contains("Command:"),
        "verbose doctor should show Command section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Binary:"),
        "verbose doctor should show Binary section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Spawn:"),
        "verbose doctor should show Spawn section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Initialize request:"),
        "verbose doctor should show Initialize request section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Initialize response:"),
        "verbose doctor should show Initialize response section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Capabilities:"),
        "verbose doctor should show Capabilities section, got:\n{stdout}"
    );
    assert!(
        stdout.contains(mockls_bin),
        "verbose doctor should show resolved binary path, got:\n{stdout}"
    );

    // Should NOT show the summary-mode sections
    assert!(
        !stdout.contains("Hooks:"),
        "verbose doctor should not show Hooks section, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Languages:"),
        "verbose doctor should not show Languages section, got:\n{stdout}"
    );

    assert!(
        output.status.success(),
        "doctor single-server should exit 0, got: {}",
        output.status
    );
    Ok(())
}

#[test]
fn test_doctor_single_server_not_found() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.mockls-test]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [lsp.language.test]\n\
             servers = [\"mockls-test\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "nonexistent-server", "--nocolor"]);

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("Unknown server: 'nonexistent-server'"),
        "should report unknown server, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Configured servers:"),
        "should list configured servers, got:\n{stdout}"
    );
    assert!(
        stdout.contains("mockls-test"),
        "should include mockls-test in configured servers, got:\n{stdout}"
    );

    assert!(
        output.status.success(),
        "doctor unknown server should still exit 0, got: {}",
        output.status
    );
    Ok(())
}

#[test]
fn test_doctor_no_args_unchanged() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.mockls-test]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [lsp.language.test]\n\
             servers = [\"mockls-test\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "--nocolor"]);

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Summary mode should show the standard sections
    assert!(
        stdout.contains("Servers:"),
        "summary doctor should show Servers section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Languages:"),
        "summary doctor should show Languages section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Hooks:"),
        "summary doctor should show Hooks section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Command filter:"),
        "summary doctor should show Command filter section, got:\n{stdout}"
    );

    assert!(
        output.status.success(),
        "doctor no-args should exit 0, got: {}",
        output.status
    );
    Ok(())
}

// ── catenary doctor parallel probing ──────────────────────────

#[test]
fn test_doctor_parallel_all_ready() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.alpha-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"alpha\"]\n\n\
             [lsp.server.beta-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"beta\"]\n\n\
             [lsp.server.gamma-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"gamma\"]\n\n\
             [lsp.language.alpha]\n\
             servers = [\"alpha-server\"]\n\n\
             [lsp.language.beta]\n\
             servers = [\"beta-server\"]\n\n\
             [lsp.language.gamma]\n\
             servers = [\"gamma-server\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "--nocolor"]);

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // All three should report ready
    assert!(
        stdout.contains("alpha-server")
            && stdout.contains("beta-server")
            && stdout.contains("gamma-server"),
        "all server names should appear, got:\n{stdout}"
    );

    // Count ready markers
    let ready_count = stdout.matches("ready").count();
    assert!(
        ready_count >= 3,
        "all 3 servers should be ready, found {ready_count} ready markers, got:\n{stdout}"
    );

    // Languages section should cross-reference
    assert!(
        stdout.contains("Languages:"),
        "should show Languages section, got:\n{stdout}"
    );

    assert!(
        output.status.success(),
        "doctor should exit 0, got: {}",
        output.status
    );
    Ok(())
}

#[test]
fn test_doctor_parallel_one_fails() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.good-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [lsp.server.bad-server]\n\
             command = \"nonexistent-binary-xyz-12345\"\n\
             args = []\n\n\
             [lsp.language.good]\n\
             servers = [\"good-server\"]\n\n\
             [lsp.language.bad]\n\
             servers = [\"bad-server\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "--nocolor"]);

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Good server should show ready
    assert!(
        stdout.contains("good-server") && stdout.contains("ready"),
        "good-server should be ready, got:\n{stdout}"
    );

    // Bad server should show command not found
    assert!(
        stdout.contains("bad-server") && stdout.contains("command not found"),
        "bad-server should show command not found, got:\n{stdout}"
    );

    assert!(
        output.status.success(),
        "doctor should exit 0 even with failures, got: {}",
        output.status
    );
    Ok(())
}

#[test]
fn test_doctor_output_sorted() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    // Names chosen to verify alphabetical ordering regardless of completion order
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.zulu-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"zulu\"]\n\n\
             [lsp.server.alpha-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"alpha\"]\n\n\
             [lsp.server.mike-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"mike\"]\n\n\
             [lsp.language.zulu]\n\
             servers = [\"zulu-server\"]\n\n\
             [lsp.language.alpha]\n\
             servers = [\"alpha-server\"]\n\n\
             [lsp.language.mike]\n\
             servers = [\"mike-server\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "--nocolor"]);

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Find positions of each server name in output
    let alpha_pos = stdout
        .find("alpha-server")
        .context("alpha-server not found")?;
    let mike_pos = stdout
        .find("mike-server")
        .context("mike-server not found")?;
    let zulu_pos = stdout
        .find("zulu-server")
        .context("zulu-server not found")?;

    assert!(
        alpha_pos < mike_pos && mike_pos < zulu_pos,
        "servers should appear in alphabetical order: alpha({alpha_pos}) < mike({mike_pos}) < zulu({zulu_pos})"
    );

    assert!(output.status.success());
    Ok(())
}

#[test]
fn test_doctor_parallel_timeout() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.fast-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"fast\"]\n\n\
             [lsp.server.hanging-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"hang\", \"--hang-on\", \"initialize\"]\n\n\
             [lsp.language.fast]\n\
             servers = [\"fast-server\"]\n\n\
             [lsp.language.hang]\n\
             servers = [\"hanging-server\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "--nocolor"]);
    // Use a short timeout for testing (3 seconds instead of 5 minutes)
    cmd.env("CATENARY_DOCTOR_TIMEOUT_SECS", "3");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Fast server should complete normally
    assert!(
        stdout.contains("fast-server") && stdout.contains("ready"),
        "fast-server should be ready, got:\n{stdout}"
    );

    // Hanging server should show timeout
    assert!(
        stdout.contains("hanging-server") && stdout.contains("initialize timed out"),
        "hanging-server should show timed out, got:\n{stdout}"
    );

    assert!(
        output.status.success(),
        "doctor should exit 0 even with timeouts, got: {}",
        output.status
    );
    Ok(())
}

#[test]
fn test_doctor_piped_no_ansi() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[lsp.server.mockls-pipe]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [lsp.language.test]\n\
             servers = [\"mockls-pipe\"]\n"
        ),
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["doctor", "--nocolor"]);
    // stdout is piped (not a TTY) — this is the default for Command::output()

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout_bytes = &output.stdout;

    // No ANSI escape sequences should be present (ESC = 0x1B)
    assert!(
        !stdout_bytes.contains(&0x1B),
        "piped output should contain no ANSI escape sequences"
    );

    // Should still contain the server result
    let stdout = String::from_utf8_lossy(stdout_bytes);
    assert!(
        stdout.contains("mockls-pipe") && stdout.contains("ready"),
        "piped output should still show server results, got:\n{stdout}"
    );

    assert!(output.status.success());
    Ok(())
}

// ── non-existent path tests ───────────────────────────────────────

/// A plain path that does not exist is a soft condition: grep/glob must
/// exit 0 with a loud `path does not exist` on stdout, never a non-zero
/// exit that would cancel sibling tool calls in a parallel batch
/// (`bugs/13`). A bogus path with no glob metacharacter resolves
/// client-side, so no daemon is required.
#[test]
fn test_grep_nonexistent_path_exits_zero_loud() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let bogus = tmp.path().join("no_such_dir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["grep", "pattern"])
        .arg(&bogus)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().context("failed to run catenary grep")?;
    assert!(
        output.status.success(),
        "catenary grep on a non-existent path must exit 0, got {:?}",
        output.status.code()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("path does not exist"),
        "stdout should loudly report the missing path, got:\n{stdout}"
    );
    Ok(())
}

#[test]
fn test_glob_nonexistent_path_exits_zero_loud() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let bogus = tmp.path().join("no_such_dir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.args(["glob"])
        .arg(&bogus)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().context("failed to run catenary glob")?;
    assert!(
        output.status.success(),
        "catenary glob on a non-existent path must exit 0, got {:?}",
        output.status.code()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("path does not exist"),
        "stdout should loudly report the missing path, got:\n{stdout}"
    );
    Ok(())
}

// ── --help exit code tests ────────────────────────────────────────

/// Agent-facing subcommands must exit 0 on `--help` so that parallel
/// tool calls (e.g., `catenary grep --help` and `catenary glob --help`
/// in the same turn) don't cancel each other on a non-zero exit code.
#[test]
fn test_help_exits_zero_for_agent_subcommands() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    for subcmd in ["grep", "glob", "editing", "roots"] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
        cmd.args([subcmd, "--help"]);
        let output = cmd
            .output()
            .with_context(|| format!("failed to run catenary {subcmd} --help"))?;
        assert!(
            output.status.success(),
            "catenary {subcmd} --help should exit 0, got {:?}",
            output.status.code()
        );
    }
    Ok(())
}

// ── quoted-glob exit-code contract ────────────────────────────────

/// End-to-end: a quoted glob that matches nothing must exit 0 with a
/// loud `no matches for pattern` on stdout. The `catenary glob` binary talks
/// to a live daemon (the pattern is expanded daemon-side), so a sibling
/// tool call in the same parallel batch is never cancelled (`bugs/13`).
#[test]
fn test_glob_quoted_zero_match_exits_zero_loud() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    std::fs::write(root.path().join("only.txt"), "x")?;

    // Start a daemon bound to this state dir (no LSP servers needed).
    let mut bridge = common::BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;

    // Wait for the IPC socket the `glob` binary will connect to.
    let ipc_sock = common::xdg_state_home(state_dir.path())
        .join("catenary")
        .join("catenary.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ipc_sock.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }

    // Run the `glob` binary against the same daemon, cwd = workspace root.
    // The arg reaches the process literally (no shell expansion), exactly
    // as a quoted glob would.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(root.path())
        .args(["glob", "**/*.rs"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().context("failed to run catenary glob")?;

    assert!(
        output.status.success(),
        "quoted zero-match glob must exit 0, got {:?}; stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no matches for pattern: **/*.rs (relative patterns anchor at cwd)"),
        "stdout should loudly report the zero-match pattern, got:\n{stdout}"
    );

    drop(bridge);
    Ok(())
}

// ── --count path counting ─────────────────────────────────────────

/// End-to-end: `catenary glob <dir> --count` reports "N paths" for the
/// directory's listed entries. The `glob` binary talks to a live daemon;
/// no LSP server is needed because the count is pure filesystem.
#[test]
fn test_glob_count_reports_paths() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    std::fs::write(root.path().join("a.txt"), "x")?;
    std::fs::write(root.path().join("b.txt"), "y")?;
    std::fs::write(root.path().join("c.txt"), "z")?;

    // Start a daemon bound to this state dir (no LSP servers needed).
    let mut bridge = common::BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;

    // Wait for the IPC socket the `glob` binary will connect to.
    let ipc_sock = common::xdg_state_home(state_dir.path())
        .join("catenary")
        .join("catenary.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ipc_sock.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(root.path())
        .args(["glob", root_str, "--count"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd
        .output()
        .context("failed to run catenary glob --count")?;

    assert!(
        output.status.success(),
        "glob --count must exit 0, got {:?}; stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        "3 paths",
        "expected the three listed files counted, got:\n{stdout}"
    );

    drop(bridge);
    Ok(())
}

// ── pattern cardinality header ────────────────────────────────────

/// End-to-end: `catenary glob 'src/**/*.rs'` opens its output with the
/// `N files match <pattern>` cardinality header (misc 121), spelled as the
/// agent typed it, and the count agrees with `--count`. The header lands on the
/// first line so a `| head`-truncated view still shows the true count even when
/// the first file's outline is large.
#[test]
fn test_glob_pattern_header_matches_count_via_binary() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    std::fs::create_dir_all(root.path().join("src"))?;
    std::fs::write(root.path().join("src/a.rs"), "fn a() {}\n")?;
    std::fs::write(root.path().join("src/b.rs"), "fn b() {}\n")?;
    std::fs::write(root.path().join("src/c.rs"), "fn c() {}\n")?;
    std::fs::write(root.path().join("src/notes.txt"), "notes\n")?;

    // Start a daemon bound to this state dir (no LSP servers needed).
    let mut bridge = common::BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;

    // Wait for the IPC socket the `glob` binary will connect to.
    let ipc_sock = common::xdg_state_home(state_dir.path())
        .join("catenary")
        .join("catenary.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ipc_sock.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }

    // The rendered pattern glob opens with the header on line 1.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(root.path())
        .args(["glob", "src/**/*.rs"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().context("failed to run catenary glob")?;
    assert!(
        output.status.success(),
        "glob must exit 0, got {:?}; stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or_default();
    assert_eq!(
        first_line, "3 files match src/**/*.rs",
        "the cardinality header (original spelling) leads the output, got:\n{stdout}"
    );

    // The header count agrees with `--count`.
    let mut count_cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut count_cmd, state_home);
    count_cmd
        .current_dir(root.path())
        .args(["glob", "src/**/*.rs", "--count"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let count_output = count_cmd
        .output()
        .context("failed to run catenary glob --count")?;
    let count_stdout = String::from_utf8_lossy(&count_output.stdout);
    assert_eq!(
        count_stdout.trim(),
        "3 paths",
        "the header count matches --count, got:\n{count_stdout}"
    );

    drop(bridge);
    Ok(())
}

// ── grep skip honesty (misc 135 / bug 62) ─────────────────────────

/// End-to-end regression for bug 62: `catenary grep` on an explicitly named
/// file over the 10 MB binary-scan cap used to render `0 matches in 0 files`
/// (and empty default output), indistinguishable from a genuine no-match. The
/// file is now reported as skipped — never silent — while a real no-match and a
/// genuinely searchable file are unaffected.
///
/// The fixture is a synthetic >10 MB single-line pure-UTF-8 file built in the
/// tempdir, so the suite never depends on the system path the bug was sighted
/// against.
#[test]
fn test_grep_skip_over_size_cap_is_reported_not_silent() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;

    // A >10 MB single-line UTF-8 file with no NUL bytes: `const` thousands of
    // times, so a genuine search would match — the skip is the only reason for
    // zero. `"const x=1; "` is 11 bytes; 1_100_000 copies ≈ 11.5 MB, one line.
    let big = root.path().join("big.js");
    std::fs::write(&big, "const x=1; ".repeat(1_100_000))?;
    let big_str = big.to_str().context("big path")?;
    assert!(
        std::fs::metadata(&big)?.len() > 10 * 1024 * 1024,
        "fixture must exceed the 10 MB binary-scan cap"
    );

    // A small, searchable control and a genuine no-match control.
    let small = root.path().join("small.js");
    std::fs::write(&small, "const y=2;\n")?;
    let small_str = small.to_str().context("small path")?;
    let plain = root.path().join("plain.txt");
    std::fs::write(&plain, "nothing to see here\n")?;
    let plain_str = plain.to_str().context("plain path")?;

    let mut bridge = common::BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_ROOTS", root_str);
    })?;
    bridge.initialize()?;

    let ipc_sock = common::xdg_state_home(state_dir.path())
        .join("catenary")
        .join("catenary.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ipc_sock.exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }

    let run = |args: &[&str]| -> Result<String> {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, state_home);
        cmd.current_dir(root.path())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().context("failed to run catenary grep")?;
        assert!(
            output.status.success(),
            "catenary grep must exit 0 on a soft skip, got {:?}; stderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };

    // 1. `--count` on the oversized file: reported as skipped, never conflated
    //    with a no-match.
    let count_big = run(&["grep", "const", big_str, "--count"])?;
    assert_eq!(
        count_big.trim(),
        "0 matches in 0 files (1 skipped: too large (>10 MB))",
        "an oversized named file counts as skipped, not a no-match, got:\n{count_big}"
    );

    // 2. Default output on the oversized file: a per-file skip line names it
    //    (cwd == root, so the path renders relative).
    let default_big = run(&["grep", "const", big_str])?;
    assert!(
        default_big.contains("skipped (too large (>10 MB)): big.js"),
        "default output must carry a per-file skip line naming the file, got:\n{default_big}"
    );

    // 3. The searchable control still matches — no skip, unchanged tally.
    let count_small = run(&["grep", "const", small_str, "--count"])?;
    assert_eq!(
        count_small.trim(),
        "1 matches in 1 files",
        "a sub-cap file is searched normally, got:\n{count_small}"
    );

    // 4. A genuine no-match still renders the plain zero — no skip noise.
    let count_plain = run(&["grep", "const", plain_str, "--count"])?;
    assert_eq!(
        count_plain.trim(),
        "0 matches in 0 files",
        "a true no-match keeps the plain zero, no skip suffix, got:\n{count_plain}"
    );

    drop(bridge);
    Ok(())
}
