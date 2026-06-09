// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for CLI list, monitor, config, and doctor commands.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use common::{ServerProcess, isolate_env};

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

#[test]
fn test_list_shows_row_numbers() -> Result<()> {
    // Start a server to ensure at least one session exists
    let mut server = ServerProcess::spawn()?;
    let _session_id = server.wait_ready()?;

    // Run catenary list
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .args(["debug", "list"])
        .env("CATENARY_STATE_DIR", server.state_dir_path())
        .output()
        .context("Failed to run list command")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Check for row number column header
    assert!(
        stdout.contains('#'),
        "List output should contain # column header"
    );

    // Check for numbered rows (should have at least "1" for our session)
    let lines: Vec<&str> = stdout.lines().collect();
    // Skip header and separator, find data lines
    let data_lines: Vec<&str> = lines
        .iter()
        .skip(2)
        .filter(|l| !l.trim().is_empty())
        .copied()
        .collect();

    assert!(
        !data_lines.is_empty(),
        "Should have at least one session row"
    );

    // First data line should start with "1" (row number)
    let first_row = data_lines[0].trim();
    assert!(
        first_row.starts_with('1'),
        "First row should start with row number 1, got: {first_row}"
    );
    Ok(())
}

#[test]
fn test_list_shows_language_servers_line() -> Result<()> {
    // Start a server
    let mut server = ServerProcess::spawn()?;
    let _session_id = server.wait_ready()?;

    // Run catenary list
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .args(["debug", "list"])
        .env("CATENARY_STATE_DIR", server.state_dir_path())
        .output()
        .context("Failed to run list command")?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Languages are displayed on a second line per session, not as a column header
    assert!(
        stdout.contains("CLIENT"),
        "List output should contain CLIENT column header"
    );
    assert!(
        stdout.contains("WORKSPACE"),
        "List output should contain WORKSPACE column header"
    );
    Ok(())
}

#[test]
fn test_monitor_by_row_number_starts() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let _session_id = server.wait_ready()?;

    // Start monitor with row number "1" - we just verify it successfully starts
    // monitoring some session (row number resolution works)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.args(["debug", "monitor"]).arg("1");
    cmd.env("CATENARY_STATE_DIR", server.state_dir_path());
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Read the first line which should show "Monitoring session ..."
    let line = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();

    // Kill and wait before asserting
    let _ = child.kill();
    let _ = child.wait();

    // Verify the monitor started (just check it says "Monitoring session")
    assert!(
        line.contains("Monitoring session"),
        "Monitor should start monitoring a session with row number, got: {line}"
    );
    Ok(())
}

#[test]
fn test_monitor_invalid_row_number_fails() -> Result<()> {
    // Verify that an invalid row number (999) fails appropriately.
    // "999" is tried as row number (out of range), then as session ID prefix
    // (no match), so the row-number error is reported.
    let state_dir = tempfile::tempdir().context("Failed to create state tempdir")?;
    let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
        .args(["debug", "monitor"])
        .arg("999")
        .env("CATENARY_STATE_DIR", state_dir.path())
        .output()
        .context("Failed to run monitor command")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("out of range") || stderr.contains("Row number"),
        "Should report row number out of range, got: {stderr}"
    );
    Ok(())
}

#[test]
fn test_monitor_numeric_session_id_resolves() -> Result<()> {
    use std::sync::mpsc;

    // Regression test: session IDs are hex strings that may be all digits
    // (e.g., "025586387"). resolve_session_id must not treat these as row
    // numbers and bail with "out of range".
    let mut server = ServerProcess::spawn()?;
    let session_id = server.wait_ready()?;

    // Start monitor using the full session ID — this must work regardless
    // of whether the ID happens to be all digits.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.args(["debug", "monitor"]).arg(&session_id);
    cmd.env("CATENARY_STATE_DIR", server.state_dir_path());
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    let header = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();

    // Capture stderr before asserting, for diagnostics
    let _ = child.kill();
    let output = child.wait_with_output().context("wait_with_output")?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        header.contains("Monitoring session"),
        "Monitor should start successfully with session ID '{session_id}', \
         got header: '{header}', stderr: '{stderr}'"
    );
    Ok(())
}

#[test]
#[ignore = "observability ticket 02 retired the messages-table firehose (now JSONL); \
            `catenary monitor` reads JSONL/state.json after tickets 03/06"]
fn test_monitor_raw_flag() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.wait_ready()?;

    // Start monitor with --raw flag
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.args(["debug", "monitor"]).arg(&session_id).arg("--raw");
    cmd.env("CATENARY_STATE_DIR", server.state_dir_path());
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a request to generate an event
    let request = json!({
        "jsonrpc": "2.0",
        "id": 99999,
        "method": "ping"
    });
    server.send(&request)?;
    let _response = server.recv()?;

    // Read monitor output with timeout
    let mut found_json = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            // Raw mode should produce pretty-printed JSON with braces
            if line.contains('{') || line.contains('}') || line.contains("\"jsonrpc\"") {
                found_json = true;
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(found_json, "Raw mode should output JSON formatted messages");
    Ok(())
}

#[test]
fn test_monitor_nocolor_flag() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.wait_ready()?;

    // Start monitor with --nocolor flag
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.args(["debug", "monitor"])
        .arg(&session_id)
        .arg("--nocolor");
    cmd.env("CATENARY_STATE_DIR", server.state_dir_path());
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a request to generate an event
    let request = json!({
        "jsonrpc": "2.0",
        "id": 88888,
        "method": "ping"
    });
    server.send(&request)?;
    let _response = server.recv()?;

    // Collect output with a timeout
    let mut output = String::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            output.push_str(&line);
            if output.len() > 100 {
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    // Check for absence of ANSI escape codes
    // ANSI escape codes start with \x1b[ or \033[
    assert!(
        !output.contains("\x1b["),
        "Output should not contain ANSI escape codes with --nocolor flag"
    );
    Ok(())
}

#[test]
#[ignore = "observability ticket 02 retired the messages-table firehose (now JSONL); \
            `catenary monitor` reads JSONL/state.json after tickets 03/06"]
fn test_monitor_filter_flag() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.wait_ready()?;

    // Start monitor with filter for "ping"
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.args(["debug", "monitor"])
        .arg(&session_id)
        .arg("--filter")
        .arg("ping");
    cmd.env("CATENARY_STATE_DIR", server.state_dir_path());
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a ping request
    let ping_request = json!({
        "jsonrpc": "2.0",
        "id": 77777,
        "method": "ping"
    });
    server.send(&ping_request)?;
    let _response = server.recv()?;

    // Read monitor output with timeout
    let mut found_ping = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100))
            && line.contains("ping")
        {
            found_ping = true;
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(found_ping, "Filter should allow ping events through");
    Ok(())
}

#[test]
#[ignore = "observability ticket 02 retired the messages-table firehose (now JSONL); \
            `catenary monitor` reads JSONL/state.json after tickets 03/06"]
fn test_monitor_uses_arrows() -> Result<()> {
    use std::sync::mpsc;

    // Start a server
    let mut server = ServerProcess::spawn()?;
    let session_id = server.wait_ready()?;

    // Start monitor (without --raw)
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    cmd.args(["debug", "monitor"])
        .arg(&session_id)
        .arg("--nocolor");
    cmd.env("CATENARY_STATE_DIR", server.state_dir_path());
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("Failed to spawn monitor")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to take monitor stdout")?;

    // Use a thread with channel for non-blocking reads
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }
            let _ = tx.send(line.clone());
            line.clear();
        }
    });

    // Skip the "Monitoring session..." line
    let _ = rx.recv_timeout(Duration::from_secs(5));

    // Send a request
    let request = json!({
        "jsonrpc": "2.0",
        "id": 66666,
        "method": "ping"
    });
    server.send(&request)?;
    let _response = server.recv()?;

    // Read monitor output and check for arrows with timeout
    let mut found_incoming_arrow = false;
    let mut found_outgoing_arrow = false;

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            if line.contains('→') {
                found_incoming_arrow = true;
            }
            if line.contains('←') {
                found_outgoing_arrow = true;
            }
            if found_incoming_arrow && found_outgoing_arrow {
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        found_incoming_arrow,
        "Should use → arrow for incoming messages"
    );
    assert!(
        found_outgoing_arrow,
        "Should use ← arrow for outgoing messages"
    );
    Ok(())
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
            "[server.mockls-test]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [language.test]\n\
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
            "[server.mockls-test]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [language.test]\n\
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
            "[server.mockls-test]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [language.test]\n\
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
            "[server.alpha-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"alpha\"]\n\n\
             [server.beta-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"beta\"]\n\n\
             [server.gamma-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"gamma\"]\n\n\
             [language.alpha]\n\
             servers = [\"alpha-server\"]\n\n\
             [language.beta]\n\
             servers = [\"beta-server\"]\n\n\
             [language.gamma]\n\
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
            "[server.good-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [server.bad-server]\n\
             command = \"nonexistent-binary-xyz-12345\"\n\
             args = []\n\n\
             [language.good]\n\
             servers = [\"good-server\"]\n\n\
             [language.bad]\n\
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
            "[server.zulu-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"zulu\"]\n\n\
             [server.alpha-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"alpha\"]\n\n\
             [server.mike-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"mike\"]\n\n\
             [language.zulu]\n\
             servers = [\"zulu-server\"]\n\n\
             [language.alpha]\n\
             servers = [\"alpha-server\"]\n\n\
             [language.mike]\n\
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
            "[server.fast-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"fast\"]\n\n\
             [server.hanging-server]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"hang\", \"--hang-on\", \"initialize\"]\n\n\
             [language.fast]\n\
             servers = [\"fast-server\"]\n\n\
             [language.hang]\n\
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
            "[server.mockls-pipe]\n\
             command = \"{mockls_bin}\"\n\
             args = [\"test\"]\n\n\
             [language.test]\n\
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
/// loud `no files matched` on stdout. The `catenary glob` binary talks
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
        stdout.contains("no files matched"),
        "stdout should loudly report zero matches, got:\n{stdout}"
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

// ── sed preview overflow file ──────────────────────────────────────

/// End-to-end: a bare `catenary sed` preview that truncates spills the complete
/// diff to a per-invocation `sed-<uuid>.txt` under the isolated runtime dir, and
/// the preview points the agent at it (cli-prerelease ticket 11a). Exercises the
/// daemon-side UUID minting + `runtime_dir()` wiring the unit tests can't reach.
#[test]
fn test_sed_preview_writes_overflow_file() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;

    // More matched files than the in-memory render cap (MAX_PREVIEW_FILES = 200),
    // so the preview truncates and spills the full set to disk.
    let root = tempfile::tempdir()?;
    let root_str = root.path().to_str().context("root path")?;
    let total = 205;
    for i in 0..total {
        std::fs::write(root.path().join(format!("f{i:04}.txt")), "foo\n")?;
    }

    // Start a daemon bound to this state dir (no LSP servers needed — sed is pure
    // filesystem).
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

    // Bare preview (no --in-place) sweeping the whole dir.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, state_home);
    cmd.current_dir(root.path())
        .args(["sed", "foo", "bar", root_str])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd.output().context("failed to run catenary sed")?;
    assert!(
        output.status.success(),
        "sed preview must exit 0, got {:?}; stderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The daemon wrote exactly one sed-<uuid>.txt under the isolated runtime dir.
    let overflow_dir = common::xdg_runtime_dir(state_dir.path()).join("catenary");
    let mut sed_files: Vec<_> = std::fs::read_dir(&overflow_dir)
        .context("read overflow dir")?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("sed-"))
        })
        .collect();
    assert_eq!(
        sed_files.len(),
        1,
        "exactly one overflow file written; stdout:\n{stdout}"
    );
    let on_disk = sed_files.remove(0);
    let name = on_disk
        .file_name()
        .and_then(|n| n.to_str())
        .context("overflow file name")?;

    // The preview points the agent at that file by name…
    assert!(
        stdout.contains("full diff at") && stdout.contains(name),
        "preview points at the on-disk overflow file ({name}); stdout:\n{stdout}"
    );
    // …and the file holds the complete set (one diff section per matched file,
    // beyond what the bounded in-memory preview rendered).
    let contents = std::fs::read_to_string(&on_disk)?;
    assert_eq!(
        contents.matches(" (1 match)").count(),
        total,
        "overflow file holds every matched file's diff"
    );

    drop(bridge);
    Ok(())
}
