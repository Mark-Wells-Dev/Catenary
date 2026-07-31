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

/// `isolate_env` must point every isolated base at a *distinct* subdir of the
/// root, so a subprocess writing under the wrong base can no longer silently
/// land in the one shared directory. Regression guard for the split.
///
/// The home base (`CATENARY_HOME_DIR`, bug 149) rides the same guard: it has no
/// XDG counterpart, but it is where the home-rooted host-CLI artifacts land, and
/// collapsing it onto another base would blind the detector exactly there.
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
        "CATENARY_HOME_DIR",
    ]
    .iter()
    .map(|var| envs.get(*var).expect("base dir set by isolate_env"))
    .collect();

    // Every base dir lives under the root...
    for dir in &dirs {
        assert!(dir.starts_with(root), "{dir} should be under root {root}");
    }

    // ...and all five resolve to distinct paths.
    let distinct: HashSet<&&String> = dirs.iter().collect();
    assert_eq!(
        distinct.len(),
        5,
        "the isolated base dirs should be distinct, got: {dirs:?}"
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

/// A config with a quarantined `[commands]` section (bug 110): doctor must raise
/// a finding naming the section and its error(s), pointing at the fix — the
/// doctor read of the loud degrade the grep/glob/hook only summarize.
#[test]
fn test_doctor_surfaces_quarantined_commands_section() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let config_dir = common::xdg_config_home(tmp.path()).join("catenary");
    std::fs::create_dir_all(&config_dir)?;
    // The incident shape: deny.sqlite3 references a command absent from allow.
    std::fs::write(
        config_dir.join("config.toml"),
        "[commands]\nallow = [\"git\"]\n\n[commands.deny]\nsqlite3 = [\"-cmd\"]\n",
    )?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    cmd.arg("doctor").arg("--nocolor");

    let output = cmd.output().context("Failed to run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Doctor booted (did not refuse on the config error — quarantine, not abort)
    // and named the quarantined section with its error.
    assert!(
        stdout.contains("[commands] quarantined"),
        "doctor must surface the quarantined [commands] section, got:\n{stdout}"
    );
    assert!(
        stdout.contains("sqlite3"),
        "the finding must carry the cross-reference error, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Config error:"),
        "a quarantined section must NOT be rendered as a fatal config error, got:\n{stdout}"
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
             path = \"{mockls_bin}\"\n\
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
             path = \"{mockls_bin}\"\n\
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
             path = \"{mockls_bin}\"\n\
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
             path = \"{mockls_bin}\"\n\
             args = [\"alpha\"]\n\n\
             [lsp.server.beta-server]\n\
             path = \"{mockls_bin}\"\n\
             args = [\"beta\"]\n\n\
             [lsp.server.gamma-server]\n\
             path = \"{mockls_bin}\"\n\
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
             path = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [lsp.server.bad-server]\n\
             path = \"nonexistent-binary-xyz-12345\"\n\
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
             path = \"{mockls_bin}\"\n\
             args = [\"zulu\"]\n\n\
             [lsp.server.alpha-server]\n\
             path = \"{mockls_bin}\"\n\
             args = [\"alpha\"]\n\n\
             [lsp.server.mike-server]\n\
             path = \"{mockls_bin}\"\n\
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
             path = \"{mockls_bin}\"\n\
             args = [\"fast\"]\n\n\
             [lsp.server.hanging-server]\n\
             path = \"{mockls_bin}\"\n\
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
             path = \"{mockls_bin}\"\n\
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

/// A grep name operand that does not exist is a soft condition: grep must exit
/// 0 with a loud `path does not exist` — never a non-zero exit that would cancel
/// sibling tool calls in a parallel batch (`bugs/13`). Under the VERBS streams
/// ruling this teaching rides **stderr** (stdout is results only); an explicit
/// `2>/dev/null` is consent to lose it. A bogus path with no glob metacharacter
/// resolves client-side, so no daemon is required.
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("path does not exist"),
        "stderr should loudly report the missing path, got:\n{stderr}"
    );
    Ok(())
}

#[test]
fn test_glob_nonexistent_path_exits_zero_loud() -> Result<()> {
    // Under the VERBS one-verb form the glob positional is a pattern, always: a
    // metachar-free absent is not a `path does not exist` (that is grep's
    // name-operand teaching) but a pattern that matched nothing — the loud
    // `no matches for pattern` report, on stderr, exit 0 (the bug-13 soft
    // condition). stdout stays empty (the zero-match shape).
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
        "catenary glob on a non-existent pattern must exit 0, got {:?}",
        output.status.code()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().is_empty(),
        "zero-match stdout must be empty (results only), got:\n{stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no matches for pattern"),
        "stderr should loudly report the zero-match pattern, got:\n{stderr}"
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
/// loud `no matches for pattern` on **stderr** (the VERBS streams ruling — the
/// zero-match shape is empty stdout, exit 0, teaching on stderr). The
/// `catenary glob` binary talks to a live daemon (the pattern is expanded
/// daemon-side), so a sibling tool call in the same parallel batch is never
/// cancelled (`bugs/13`).
#[test]
fn test_glob_quoted_zero_match_exits_zero_loud() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131) for the shared-state spawn below.
    let _daemon_guard = common::DaemonGuard::new(state_home);

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
        stdout.trim().is_empty(),
        "zero-match stdout must be empty (results only), got:\n{stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no matches for pattern: **/*.rs (relative patterns anchor at cwd)"),
        "stderr should loudly report the zero-match pattern, got:\n{stderr}"
    );

    drop(bridge);
    Ok(())
}

// ── glob arity is grammar (VERBS moment 1) ────────────────────────

/// `catenary glob` takes exactly one pattern: the bare form and N>1 are usage
/// errors — teaching on stderr, exit 2 (clap's invalid-arg class). N>1 is the
/// shape an unquoted pattern the shell expanded leaves.
#[test]
fn test_glob_arity_bare_and_multi_exit_2() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let home = tmp.path().to_str().context("tempdir path")?;

    // Bare form.
    let mut bare = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut bare, home);
    bare.current_dir(tmp.path())
        .args(["glob"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = bare.output().context("run bare glob")?;
    assert_eq!(out.status.code(), Some(2), "bare glob exits 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("takes one pattern") && stderr.contains("nullglob"),
        "bare form teaches the nullglob rationale on stderr, got:\n{stderr}"
    );

    // N>1 (the shell-expansion shape).
    let mut multi = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut multi, home);
    multi
        .current_dir(tmp.path())
        .args(["glob", "a.rs", "b.rs"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = multi.output().context("run multi glob")?;
    assert_eq!(out.status.code(), Some(2), "N>1 glob exits 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("got 2 arguments") && stderr.contains("*.rs"),
        "N>1 names the likely expansion on stderr, got:\n{stderr}"
    );
    Ok(())
}

// ── invalid regex is a usage error (bug 105) ──────────────────────

/// Bug 105: an invalid grep pattern is a **usage error** — the parse error
/// prints on stderr and the command exits **2**, on the bare AND `--count`
/// forms (ripgrep parity). Never a zero indistinguishable from a genuine
/// no-match, never a swallowed parse error the exit code hides. Runs through
/// the daemon-less in-process path (no daemon), which sets the same `error`
/// field the daemon-served path does.
#[test]
fn test_grep_invalid_regex_is_usage_error_exit_2() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    // An unclosed character class is an uncompilable regex.
    for extra in [&[][..], &["--count"][..]] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
        cmd.current_dir(tmp.path())
            .args(["grep", "a["])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().context("failed to run catenary grep")?;
        assert_eq!(
            output.status.code(),
            Some(2),
            "invalid regex must exit 2 (form: {extra:?}); stderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains("0 matches"),
            "the count leg must not swallow the parse error into a zero (form: {extra:?}), got:\n{stdout}"
        );
        assert!(
            stdout.trim().is_empty(),
            "no results on stdout for an uncompilable pattern (form: {extra:?}), got:\n{stdout}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.to_lowercase().contains("regex") || stderr.to_lowercase().contains("pattern"),
            "the parse error prints on stderr (form: {extra:?}), got:\n{stderr}"
        );
    }
    Ok(())
}

// ── --count path counting ─────────────────────────────────────────

/// End-to-end: `catenary glob '<dir>/*' --count` reports "N paths" for the
/// directory's listed entries. Under the one-verb form a directory's listing is
/// `glob 'dir/*'` (the positional is a pattern); the count is the pattern's
/// match set. The `glob` binary talks to a live daemon; no LSP server is needed
/// because the count is pure filesystem.
#[test]
fn test_glob_count_reports_paths() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131) for the shared-state spawn below.
    let _daemon_guard = common::DaemonGuard::new(state_home);

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

    let dir_pattern = format!("{root_str}/*");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(root.path())
        .args(["glob", &dir_pattern, "--count"])
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
        "expected the three matched files counted, got:\n{stdout}"
    );

    drop(bridge);
    Ok(())
}

// ── stdout is results only; --count is the sole tally ─────────────

/// End-to-end (retargeted for the VERBS streams ruling): `catenary glob
/// 'src/**/*.rs'` prints **results only** on stdout — no cardinality header
/// leads the output (the header retired; `--count` is the sole tally). The
/// first line is a result, and `--count` reports the same match set. This pins
/// the bug-184 divergence class shut structurally: one bookkeeper.
#[test]
fn test_glob_pattern_header_matches_count_via_binary() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131) for the shared-state spawn below.
    let _daemon_guard = common::DaemonGuard::new(state_home);

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

    // The rendered pattern glob carries no header — results only on stdout.
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
    assert!(
        !stdout.contains("files match") && !stdout.contains("file matches"),
        "stdout carries no cardinality header (results only), got:\n{stdout}"
    );
    let first_line = stdout.lines().next().unwrap_or_default();
    assert!(
        first_line.contains("a.rs") || first_line.contains("cwd:"),
        "the first line is a result, not a banner, got:\n{stdout}"
    );

    // `--count` is the sole tally and reports the same match set.
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
        "--count reports the match set, got:\n{count_stdout}"
    );

    drop(bridge);
    Ok(())
}

// ── grep serves large files uncapped (misc 140 / bug 62) ──────────

/// End-to-end regression for bug 62: `catenary grep` on a pure-UTF-8 file well
/// over the retired 10 MB size cap used to render `0 matches in 0 files`
/// (skipped-by-size), indistinguishable from a genuine no-match. Classification
/// is now content-based (misc 140, decision 029), so the file is searched to EOF
/// and matched from every entry path — named, quoted glob, and directory walk —
/// and `catenary glob` renders it with a line count, not a byte size.
///
/// The fixture is a synthetic >10 MB multi-line pure-UTF-8 file built in the
/// tempdir, so the suite never depends on the system path the bug was sighted
/// against.
#[test]
fn test_grep_searches_large_utf8_file_uncapped() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131) for the shared-state spawn below.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;

    // A >10 MB pure-UTF-8 file with no NUL bytes: `"const x = 1;\n"` (13 bytes)
    // repeated, so every line matches `const` and a genuine search returns the
    // full tally. 900_000 lines ≈ 11.7 MB, comfortably over the retired cap.
    let lines = 900_000;
    let big = root.path().join("big.js");
    std::fs::write(&big, "const x = 1;\n".repeat(lines))?;
    let big_str = big.to_str().context("big path")?;
    assert!(
        std::fs::metadata(&big)?.len() > 10 * 1024 * 1024,
        "fixture must exceed the retired 10 MB cap"
    );

    // A neighbouring searchable file and a genuine no-match control.
    let small = root.path().join("small.js");
    std::fs::write(&small, "const y = 2;\n")?;
    let plain = root.path().join("plain.txt");
    std::fs::write(&plain, "nothing to see here\n")?;

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
        let output = cmd.output().context("failed to run catenary")?;
        assert!(
            output.status.success(),
            "catenary must exit 0, got {:?}; stderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    };

    let want_count = format!("{lines} matches in 1 files");

    // (a) Named path: the large file is searched to EOF, every line matched, no
    //     skip suffix (the size cap is gone).
    let count_named = run(&["grep", "const", big_str, "--count"])?;
    assert_eq!(
        count_named.trim(),
        want_count,
        "a named large pure-UTF-8 file is searched in full, got:\n{count_named}"
    );
    assert!(
        !count_named.contains("skipped"),
        "a large pure-UTF-8 file is never skipped, got:\n{count_named}"
    );

    // (b) Quoted glob: `big*.js` expands daemon-side to the large file, searched
    //     the same way.
    let count_glob = run(&["grep", "const", "big*.js", "--count"])?;
    assert_eq!(
        count_glob.trim(),
        want_count,
        "a quoted-glob large file is searched in full, got:\n{count_glob}"
    );

    // (c) Directory walk: pathless grep over the cwd finds the large file among
    //     its neighbours (`-l` lists it without dumping 900k lines).
    let walked = run(&["grep", "const", "-l"])?;
    assert!(
        walked.contains("big.js"),
        "a directory walk searches the large file, got:\n{walked}"
    );

    // `catenary glob` renders the large text file with a line count, not a byte
    // size (the enrichment size gate is gone too).
    let glob_big = run(&["glob", "big.js"])?;
    assert!(
        glob_big.contains(&format!("{lines} lines")),
        "glob shows the large file's line count, got:\n{glob_big}"
    );
    assert!(
        !glob_big.contains(" MB"),
        "glob shows a line count, not a byte size, got:\n{glob_big}"
    );

    // A genuine no-match still renders the plain zero — no skip noise.
    let plain_str = plain.to_str().context("plain path")?;
    let count_plain = run(&["grep", "needle-absent", plain_str, "--count"])?;
    assert_eq!(
        count_plain.trim(),
        "0 matches in 0 files",
        "a true no-match keeps the plain zero, no skip suffix, got:\n{count_plain}"
    );

    drop(bridge);
    Ok(())
}

// ── non-TTY stdout delivers full output (bugs/15) ─────────────────

/// Regression guard for `bugs/15`: a `catenary` command whose stdout is a
/// **pipe** (non-TTY — the harness-backgrounded capture case) must deliver its
/// full output, not an empty capture. The command's `Output` writer wraps
/// `io::stdout()` (a `LineWriter`), and every line it emits ends in `\n`, so the
/// bytes reach the pipe on the normal (non-`process::exit`) return path — no
/// exit-time flush is required. This test pins that contract at the process
/// boundary so a future change (e.g. wrapping `Output` in an unflushed
/// `BufWriter`) that would drop the tail is caught.
///
/// `catenary version` is chosen deliberately: it is hermetic (no live daemon
/// needed) and, under `isolate_env`, no socket exists so the daemon probe
/// resolves to `NotRunning`, giving a deterministic two-line receipt on stdout.
#[test]
fn test_version_piped_stdout_delivers_full_output() -> Result<()> {
    let tmp = tempfile::tempdir()?;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, tmp.path().to_str().context("tempdir path")?);
    // stdout as a pipe (not a TTY) — the exact capture geometry a backgrounding
    // harness uses.
    cmd.arg("version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().context("failed to run catenary version")?;
    assert!(
        output.status.success(),
        "catenary version must exit 0, got {:?}; stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The capture is non-empty — the reported symptom (empty output file) must
    // not recur.
    assert!(
        !stdout.trim().is_empty(),
        "piped (non-TTY) stdout must carry the version receipt, not an empty capture"
    );
    // The full receipt lands: the CLI version line first, then the daemon line.
    // No daemon is reachable under `isolate_env`, so the second line is the
    // not-running verdict — the whole receipt, both lines, must arrive.
    let mut lines = stdout.lines();
    let cli_line = lines.next().unwrap_or_default();
    assert!(
        cli_line.starts_with("catenary "),
        "first line is the CLI version, got:\n{stdout}"
    );
    assert_eq!(
        lines.next().unwrap_or_default(),
        "daemon: not running",
        "the daemon line lands too (no daemon under isolate_env), got:\n{stdout}"
    );

    Ok(())
}

// ── bug 112: the glob directory `dir/*` note fuses into stdout under piping ──

/// Builds a workspace whose `**` glob resolves to a directory (firing the
/// teaching-moment-4 directory note) plus a large, multi-line body
/// (many files, each long) so the stdout stream is well over the kernel pipe
/// buffer — the geometry under which `io::Stdout`'s line buffering used to split
/// the body across syscalls a merged-fd stderr hint could interleave. Returns
/// `(root, state_home_str)`; the daemon is left running on `bridge`, which the
/// caller must keep alive for the duration of the glob run.
fn bug112_workspace(root: &std::path::Path) -> Result<()> {
    // A subdirectory the `**` pattern matches — this is what fires the dir hint.
    let sub = root.join("resolver");
    std::fs::create_dir_all(&sub)?;
    // Enough long files that the rendered listing dwarfs the 64 KiB pipe buffer,
    // forcing the multi-syscall write path the bug rode.
    let long_line = "// a reasonably long source line to inflate the body\n".repeat(40);
    for f in 0..60 {
        std::fs::write(root.join(format!("file_{f:02}.rs")), &long_line)?;
        std::fs::write(sub.join(format!("inner_{f:02}.rs")), &long_line)?;
    }
    Ok(())
}

/// A substring unique to the note the glob emits when its pattern resolves a
/// directory (teaching moment 4, misc 222) — it appears in no result row. This
/// substring must appear ONLY on stderr, never fused into a stdout result line.
const GLOB_HINT_MARKER: &str = "summarized above";

/// The prefix every glob teaching note (moment 3 and moment 4) opens with. A
/// hint-carrying line must START here — a result row (absolute path or indented
/// outline row) never does, so a note welded onto a result line is detectable.
const GLOB_NOTE_PREFIX: &str = "note:";

/// Deterministic contract regression for bug 112: an enriched `catenary glob`
/// whose `**` pattern matches a directory emits the directory note — and that
/// note rides **stderr only**. The stdout capture holds zero hint bytes
/// and every stdout line is a well-formed result line (an absolute path listing
/// row or an indented outline/summary row), never prose.
///
/// Separate stdout/stderr pipes make the assertion deterministic: it pins the
/// stream contract (advisory on stderr, results on stdout) independent of the
/// interleaving race, so a regression that routed the hint back onto stdout — or
/// split it across the streams — fails here every run.
#[test]
fn test_glob_listing_hint_rides_stderr_only() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131) for the shared-state spawn below.
    let _daemon_guard = common::DaemonGuard::new(state_home);
    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    bug112_workspace(root.path())?;

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

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(root.path())
        .args(["glob", "**"])
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
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The hint fired — otherwise the test is not exercising the bug's path.
    assert!(
        stderr.contains(GLOB_HINT_MARKER),
        "the directory match must emit the listing hint on stderr; stderr:\n{stderr}"
    );
    // ...and it is present ONLY on stderr: not one hint byte reaches stdout.
    // Both the human hint marker AND its embedded command fragment
    // (`catenary glob '<dir>/*'`) must be absent from stdout — the fusion welded
    // that fragment onto a result line.
    assert!(
        !stdout.contains(GLOB_HINT_MARKER) && !stdout.contains("catenary glob '"),
        "the listing hint must never appear on stdout (bug 112); stdout head:\n{}",
        stdout.lines().take(20).collect::<Vec<_>>().join("\n"),
    );

    // The stdout body is non-empty (the workspace resolved a listing) — the test
    // is exercising a real, large result body, not an empty stream.
    assert!(
        stdout.contains("file_00.rs") && stdout.contains("inner_00.rs"),
        "stdout must carry the workspace listing; head:\n{}",
        stdout.lines().take(10).collect::<Vec<_>>().join("\n"),
    );

    drop(bridge);
    Ok(())
}

/// Stream-discipline guard for bug 112 under the real merged-fd geometry: run
/// `catenary glob '**'` with stdout and stderr sharing ONE physical pipe (exactly
/// how a backgrounding agent harness captures a command with `2>&1`), and assert
/// the directory note always lands on its OWN line — never fused mid-line
/// into a stdout result row — and that no other line carries the hint's command
/// fragment.
///
/// The fusion was a buffering interleave (line-buffered stdout vs. immediate
/// stderr on a shared fd), an intermittent race. This loops a bounded 24
/// iterations so a probabilistic regression has repeated chances to surface. With
/// the fix — the body written as one atomic `write_all` and flushed before any
/// advisory — the two streams cannot interleave: stdout is fully drained before
/// the hint is written.
///
/// Reliability note (beta-tester honesty): under this harness's `std::io::pipe`
/// geometry the pre-fix code did NOT reproduce the fusion in 24-iteration and
/// N=4 stress runs — Rust's `LineWriter` flushes the body up to its last newline
/// in one blocking `write_all` before the sequential stderr write, so the
/// interleave window the maintainer observed against the live rust-analyzer daemon
/// (a ~48 KB enriched body) did not open here. This test therefore pins the
/// CONTRACT (hint on its own line under merged fds) rather than deterministically
/// reproducing the timing; the atomic-write mechanism itself is pinned
/// deterministically by `write_block_appends_single_newline` in `cli::mod`.
#[test]
fn test_glob_listing_hint_never_fuses_in_merged_stream() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131) for the shared-state spawn below.
    let _daemon_guard = common::DaemonGuard::new(state_home);
    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    bug112_workspace(root.path())?;

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

    for iter in 0..24 {
        // Merge the child's stdout and stderr into ONE physical pipe — the exact
        // `2>&1` geometry a backgrounding harness captures, and the only stream
        // where a stderr hint can fuse into a stdout result. Both fds are handed
        // the same pipe writer (no `unsafe`, no PATH-dependent `sh`); the parent
        // reads the merged bytes off the reader.
        let (mut reader, writer) = std::io::pipe().context("create merge pipe")?;
        let writer_dup = writer.try_clone().context("clone merge pipe writer")?;

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, state_home);
        cmd.current_dir(root.path())
            .args(["glob", "**"])
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(writer_dup));
        let mut child = cmd.spawn().context("failed to spawn merged glob")?;

        // Drop the parent's writer ends so the reader sees EOF once the child
        // exits — otherwise `read_to_string` would block on the parent's own
        // dangling write fds.
        drop(cmd);
        let mut merged = String::new();
        std::io::Read::read_to_string(&mut reader, &mut merged).context("read merged stream")?;
        let status = child.wait().context("wait for merged glob")?;
        assert!(
            status.success(),
            "iter {iter}: merged glob must exit 0, got {:?}; merged:\n{}",
            status.code(),
            merged.lines().take(10).collect::<Vec<_>>().join("\n"),
        );

        // The hint fired this run (the merged stream carries stderr too).
        assert!(
            merged.contains(GLOB_HINT_MARKER),
            "iter {iter}: the merged stream must carry the listing hint; head:\n{}",
            merged.lines().take(10).collect::<Vec<_>>().join("\n"),
        );
        for line in merged.lines() {
            if line.contains(GLOB_HINT_MARKER) {
                // A line touching the hint must BE the hint line, whole — starting
                // at the `note:` prefix, never a result row with the hint welded
                // onto its front or tail (the fusion signature). The hint is the
                // only line that may carry the `summarized above` / `catenary glob '`
                // fragments.
                assert!(
                    line.starts_with(GLOB_NOTE_PREFIX),
                    "iter {iter}: the hint fused into a result line (bug 112); line:\n{line}"
                );
            } else {
                // No non-hint line may carry the hint's command fragment either —
                // a split hint could deposit `catenary glob '` without the leading
                // marker.
                assert!(
                    !line.contains("catenary glob '"),
                    "iter {iter}: a result line carries the hint's command fragment (fusion); line:\n{line}"
                );
            }
        }
    }

    drop(bridge);
    Ok(())
}

// ── worktree diff/land teaching stubs (wf-03) ───────────────────────────

/// The retired `worktree diff` / `worktree land` verbs are transition-period
/// teaching stubs: any invocation shape (bare, with the old flags) prints the
/// git-native landing flow on stderr and exits `2` — distinct from success and
/// from generic error `1`. No daemon is needed; the stub never touches one.
/// (The stubs get deleted in a later release — this test goes with them.)
#[test]
fn worktree_diff_and_land_stubs_teach_and_exit_2() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let root = tmp.path().to_str().expect("tempdir path");

    for argv in [
        vec!["worktree", "diff", "/some/worktree"],
        vec!["worktree", "diff", "/some/worktree", "--name-only"],
        vec!["worktree", "land", "/some/worktree"],
        vec!["worktree", "land", "/some/worktree", "--keep"],
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, root);
        let out = cmd.args(&argv).output().expect("run worktree stub");

        assert_eq!(
            out.status.code(),
            Some(2),
            "{argv:?} must exit 2, got {:?}",
            out.status.code(),
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let verb = argv[1];
        assert!(
            stderr.contains(&format!("`catenary worktree {verb}` is retired")),
            "{argv:?} must name the retired verb; stderr:\n{stderr}"
        );
        for step in [
            "commit the work in the worktree",
            "git diff main...<branch>",
            "git merge --squash <branch>",
            "catenary worktree rm <path>",
            "merge bracket transfers any unpaid worker debt automatically",
            "catenary diagnostics",
        ] {
            assert!(
                stderr.contains(step),
                "{argv:?} teaching missing {step:?}; stderr:\n{stderr}"
            );
        }
    }
}
