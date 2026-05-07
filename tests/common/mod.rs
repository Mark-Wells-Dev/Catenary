// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared integration test utilities.
//!
//! Each integration test file is a separate compilation unit.
//! `mod common;` imports this module to share [`isolate_env`],
//! [`BridgeProcess`], [`ServerProcess`], and IPC helpers without
//! copy-pasting.

#![allow(dead_code, reason = "each test crate compiles common separately")]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

// ── Environment isolation ────────────────────────────────────────────

/// Isolates a subprocess from the user's environment.
///
/// Sets `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, and `XDG_DATA_HOME` to the
/// given root so the process uses the test's tempdir instead of
/// `~/.config`, `~/.local/state`, or `~/.local/share`. Clears all
/// `CATENARY_*` env vars that could leak from the user's shell and
/// override test-specific settings.
///
/// All integration test subprocesses (bridge, `catenary install`, etc.)
/// must call this. Callers set `CATENARY_SERVERS`, `CATENARY_ROOTS`, or
/// `CATENARY_CONFIG` explicitly after this call.
pub fn isolate_env(cmd: &mut Command, root: &str) {
    cmd.env("XDG_CONFIG_HOME", root);
    cmd.env("XDG_STATE_HOME", root);
    cmd.env("XDG_DATA_HOME", root);
    cmd.env_remove("CATENARY_STATE_DIR");
    cmd.env_remove("CATENARY_DATA_DIR");
    cmd.env_remove("CATENARY_CONFIG");
    cmd.env_remove("CATENARY_SERVERS");
    cmd.env_remove("CATENARY_ROOTS");
}

// ── BridgeProcess ────────────────────────────────────────────────────

/// Spawns the Catenary bridge binary and communicates via MCP over
/// stdin/stdout.
///
/// Each instance gets its own `TempDir` for XDG state/config isolation,
/// so bridge-created files (DB, sockets) never leak into the workspace
/// root. Stderr is redirected to a file in the state dir for
/// post-failure inspection.
pub struct BridgeProcess {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    stderr_log: Option<PathBuf>,
    state_home: String,
    /// Internal tempdir for XDG state/config isolation.
    _state_dir: tempfile::TempDir,
}

impl BridgeProcess {
    /// Spawns with `CATENARY_SERVERS` and a single workspace root.
    pub fn spawn(lsp_commands: &[&str], root: &str) -> Result<Self> {
        Self::spawn_multi_root(lsp_commands, &[root])
    }

    /// Spawns with `CATENARY_SERVERS` and multiple workspace roots.
    pub fn spawn_multi_root(lsp_commands: &[&str], roots: &[&str]) -> Result<Self> {
        Self::spawn_with(|cmd| {
            if !lsp_commands.is_empty() {
                cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));
            }
            let roots_val = std::env::join_paths(roots).unwrap_or_default();
            cmd.env("CATENARY_ROOTS", &roots_val);
        })
    }

    /// Spawns with `CATENARY_SERVERS`, a single workspace root, and the
    /// mock grammar pre-installed before the bridge starts. This avoids
    /// the race between `TsIndex::build()` and post-spawn grammar
    /// installation — the grammar is guaranteed to be available when the
    /// tree-sitter index is built during startup.
    pub fn spawn_with_grammar(
        lsp_commands: &[&str],
        root: &str,
        grammar_setup: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Self> {
        Self::spawn_with_setup(
            |cmd| {
                if !lsp_commands.is_empty() {
                    cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));
                }
                cmd.env("CATENARY_ROOTS", root);
            },
            grammar_setup,
        )
    }

    /// Spawns using a TOML config file instead of `CATENARY_SERVERS`.
    ///
    /// Required for multi-server-per-language tests where each server
    /// needs different flags or different `min_severity` settings.
    pub fn spawn_with_config(config_path: &Path, root: &str) -> Result<Self> {
        Self::spawn_with(|cmd| {
            cmd.env("CATENARY_CONFIG", config_path);
            cmd.env("CATENARY_ROOTS", root);
        })
    }

    /// Spawns using a TOML config file with additional `CATENARY_SERVERS`
    /// entries merged at runtime.
    pub fn spawn_with_config_and_servers(
        config_path: &Path,
        lsp_commands: &[&str],
        root: &str,
    ) -> Result<Self> {
        Self::spawn_with(|cmd| {
            cmd.env("CATENARY_CONFIG", config_path);
            if !lsp_commands.is_empty() {
                cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));
            }
            cmd.env("CATENARY_ROOTS", root);
        })
    }

    /// Shared spawn: creates state dir, isolates env, lets `configure`
    /// set `CATENARY_*` vars (after `isolate_env` cleared them), then
    /// redirects stderr and starts the process.
    fn spawn_with(configure: impl FnOnce(&mut Command)) -> Result<Self> {
        Self::spawn_with_setup(configure, |_| Ok(()))
    }

    /// Like [`spawn_with`], but runs `setup` on the isolated state dir
    /// before the subprocess starts. Use this to install grammars or
    /// write config files that must be present when the bridge builds
    /// its tree-sitter index during startup.
    fn spawn_with_setup(
        configure: impl FnOnce(&mut Command),
        setup: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Self> {
        let state_dir = tempfile::tempdir().context("Failed to create state dir")?;
        let state_home = state_dir
            .path()
            .to_str()
            .context("state dir path")?
            .to_string();

        setup(&state_home)?;

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, &state_home);
        configure(&mut cmd);

        let stderr_path = state_dir.path().join("bridge_stderr.log");
        let stderr_file =
            std::fs::File::create(&stderr_path).context("Failed to create stderr log")?;

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file));

        let mut child = cmd.spawn().context("Failed to spawn bridge")?;
        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr_log: Some(stderr_path),
            state_home,
            _state_dir: state_dir,
        })
    }

    pub fn send(&mut self, request: &Value) -> Result<()> {
        let json = serde_json::to_string(request)?;
        let stdin = self.stdin.as_mut().context("Stdin already closed")?;
        writeln!(stdin, "{json}").context("Failed to write to stdin")?;
        stdin.flush().context("Failed to flush stdin")?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Value> {
        let mut line = String::new();
        let stdout = self.stdout.as_mut().context("Stdout already closed")?;
        let n = stdout
            .read_line(&mut line)
            .context("Failed to read from stdout")?;
        if n == 0 {
            // EOF — bridge process died. Read stderr log for diagnostics.
            let stderr_buf = self
                .stderr_log
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            // Also read daemon log if the bridge went through the daemon path.
            let daemon_log = PathBuf::from(&self.state_home)
                .join("catenary")
                .join("daemon.log");
            let daemon_buf = std::fs::read_to_string(&daemon_log).unwrap_or_default();
            let status = self.child.try_wait().ok().flatten();
            bail!(
                "bridge process closed stdout (EOF). exit status: {status:?}, stderr:\n{stderr_buf}\ndaemon log:\n{daemon_buf}"
            );
        }
        serde_json::from_str(&line).context("Failed to parse JSON response")
    }

    pub fn initialize(&mut self) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "integration-test",
                    "version": "1.0.0"
                }
            }
        }))?;

        let response = self.recv()?;
        if response.get("result").is_none() {
            bail!("Initialize failed: {response:?}");
        }

        // Send initialized notification
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;

        // Small delay for notification processing
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    /// Initializes with `roots.listChanged` capability.
    ///
    /// After sending `notifications/initialized`, reads the server's
    /// `roots/list` request from stdout and responds with the given roots.
    pub fn initialize_with_roots(&mut self, roots: &[&str]) -> Result<()> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "roots": { "listChanged": true }
                },
                "clientInfo": {
                    "name": "integration-test",
                    "version": "1.0.0"
                }
            }
        }))?;

        let response = self.recv()?;
        if response.get("result").is_none() {
            bail!("Initialize failed: {response:?}");
        }

        // Send initialized notification — this triggers the roots/list request
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))?;

        // The server should send us a roots/list request
        let roots_request = self.recv()?;
        let method = roots_request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow!("Expected roots/list request, got: {roots_request:?}"))?;
        if method != "roots/list" {
            bail!("Expected roots/list, got {method}");
        }
        let request_id = roots_request
            .get("id")
            .ok_or_else(|| anyhow!("roots/list request missing id"))?
            .clone();

        // Respond with the provided roots
        let root_objects: Vec<Value> = roots
            .iter()
            .map(|r| json!({"uri": format!("file://{r}")}))
            .collect();

        self.send(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": { "roots": root_objects }
        }))?;

        // Small delay for processing
        std::thread::sleep(Duration::from_millis(100));
        Ok(())
    }

    /// Returns the daemon hook socket path for this test's XDG scope.
    pub fn hook_socket_path(&self) -> PathBuf {
        PathBuf::from(&self.state_home)
            .join("catenary")
            .join("catenary-hooks.sock")
    }

    /// Waits for the daemon hook socket to appear (up to 5 seconds).
    pub fn wait_for_hook_socket(&self) -> Result<PathBuf> {
        let path = self.hook_socket_path();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            if std::time::Instant::now() > deadline {
                bail!("Hook socket not found at {} within 5s", path.display());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(path)
    }

    /// Enters editing mode, accumulates a file, then calls `done_editing`
    /// via MCP to retrieve diagnostics from the tool result.
    pub fn call_diagnostics(&mut self, file: &str) -> Result<String> {
        let socket_path = self.wait_for_hook_socket()?;

        // Enter editing mode via IPC
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-state",
                "tool_name": "start_editing",
                "agent_id": ""
            }),
        )?;

        // Accumulate file via IPC
        ipc_request(
            &socket_path,
            &json!({
                "method": "post-tool/diagnostics",
                "file": file,
                "tool": "Edit",
                "agent_id": ""
            }),
        )?;

        // Call done_editing via MCP
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 9000,
            "method": "tools/call",
            "params": {
                "name": "done_editing",
                "arguments": {}
            }
        }))?;

        let response = self.recv()?;
        let text = response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        Ok(text)
    }

    /// Enters editing mode, accumulates multiple files, then calls
    /// `done_editing` via MCP to retrieve batched diagnostics.
    pub fn call_diagnostics_multi(&mut self, files: &[&str]) -> Result<String> {
        let socket_path = self.wait_for_hook_socket()?;

        // Enter editing mode via IPC
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-state",
                "tool_name": "start_editing",
                "agent_id": ""
            }),
        )?;

        // Accumulate all files via IPC
        for file in files {
            ipc_request(
                &socket_path,
                &json!({
                    "method": "post-tool/diagnostics",
                    "file": file,
                    "tool": "Edit",
                    "agent_id": ""
                }),
            )?;
        }

        // Call done_editing via MCP
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 9001,
            "method": "tools/call",
            "params": {
                "name": "done_editing",
                "arguments": {}
            }
        }))?;

        let response = self.recv()?;
        let text = response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        Ok(text)
    }

    /// Calls an MCP tool and returns the raw result object.
    pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value> {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args
            }
        }))?;

        let response = self.recv()?;
        let result = response
            .get("result")
            .context("No result in response")?
            .clone();
        Ok(result)
    }

    /// Calls an MCP tool and returns the first text content item.
    pub fn call_tool_text(&mut self, name: &str, args: &Value) -> Result<String> {
        let result = self.call_tool(name, args)?;
        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .context("No text content in result")?;
        Ok(content.to_string())
    }

    /// Returns the path to the stderr log file, if one exists.
    pub fn stderr_path(&self) -> Option<&Path> {
        self.stderr_log.as_deref()
    }

    /// Returns the state home directory path.
    pub fn state_home(&self) -> &str {
        &self.state_home
    }
}

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        // Close stdin to signal graceful shutdown
        self.stdin.take();

        // Wait for the process to exit naturally (up to 2 seconds)
        for _ in 0..20 {
            if let Ok(Some(_)) = self.child.try_wait() {
                // Clean up stderr log on success — failed tests leave it on disk
                if !std::thread::panicking()
                    && let Some(ref path) = self.stderr_log
                {
                    let _ = std::fs::remove_file(path);
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // If still alive after timeout, kill it
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── ServerProcess ────────────────────────────────────────────────────

/// Spawns the bridge for CLI-focused tests (list, monitor, config, doctor).
///
/// Unlike [`BridgeProcess`], this variant owns its `TempDir` for state
/// isolation and exposes it for subcommand env vars. Fields are non-Option
/// since CLI tests don't need partial-close semantics.
pub struct ServerProcess {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    pub state_dir: tempfile::TempDir,
}

impl ServerProcess {
    pub fn spawn() -> Result<Self> {
        let state_dir = tempfile::tempdir().context("Failed to create state tempdir")?;

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        cmd.env("CATENARY_ROOTS", ".");
        cmd.env("XDG_CONFIG_HOME", state_dir.path());
        cmd.env("CATENARY_STATE_DIR", state_dir.path());
        cmd.env_remove("CATENARY_CONFIG");

        let stderr_path = state_dir.path().join("server_stderr.log");
        let stderr_file =
            std::fs::File::create(&stderr_path).context("Failed to create stderr log")?;

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file));

        let mut child = cmd.spawn().context("Failed to spawn server")?;

        let stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("Failed to get stdout")?);

        Ok(Self {
            child,
            stdin,
            stdout,
            state_dir,
        })
    }

    /// Sends an MCP `initialize` request and reads the response.
    ///
    /// Proves the server is running and the session exists in the DB.
    /// Returns the full instance ID queried from the database.
    pub fn wait_ready(&mut self) -> Result<String> {
        let init_request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0.0.0" }
            }
        });
        self.send(&init_request)?;
        let _response = self.recv()?;

        let output = Command::new(env!("CARGO_BIN_EXE_catenary"))
            .arg("query")
            .arg("--sql")
            .arg("SELECT id FROM sessions LIMIT 1")
            .arg("--format")
            .arg("json")
            .env("CATENARY_STATE_DIR", self.state_dir.path())
            .output()
            .context("Failed to run query command")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: Vec<Value> = serde_json::from_str(stdout.trim())
            .with_context(|| format!("Failed to parse query JSON: {stdout}"))?;
        let id = parsed
            .first()
            .and_then(|obj| obj["id"].as_str())
            .ok_or_else(|| anyhow!("No 'id' field in query output: {stdout}"))?
            .to_string();

        Ok(id)
    }

    pub fn send(&mut self, request: &Value) -> Result<()> {
        let json = serde_json::to_string(request)?;
        writeln!(self.stdin, "{json}").context("Failed to write to stdin")?;
        self.stdin.flush().context("Failed to flush stdin")?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Value> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .context("Failed to read from stdout")?;
        serde_json::from_str(&line).context("Failed to parse JSON response")
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── IPC helpers ──────────────────────────────────────────────────────

/// Sends a one-shot IPC request to the hook server. Ignores the response.
pub fn ipc_request(socket_path: &Path, request: &Value) -> Result<()> {
    use std::io::Read as _;
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).context("connect to notify socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{request}").context("write to notify socket")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    Ok(())
}

/// Builds a `CATENARY_SERVERS` spec for [`BridgeProcess::spawn`] using mockls.
pub fn mockls_lsp_arg(lang: &str, flags: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_mockls");
    if flags.is_empty() {
        format!("{lang}:{bin} {lang}")
    } else {
        format!("{lang}:{bin} {lang} {flags}")
    }
}
