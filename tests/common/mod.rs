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
/// override test-specific settings. Clears `PATH` so built-in server
/// defaults (julia-language-server, cmake-language-server, etc.) fail
/// the binary check immediately instead of spawning real processes.
/// Tests that need specific binaries use absolute paths.
///
/// All integration test subprocesses (bridge, `catenary install`, etc.)
/// must call this. Callers set `CATENARY_SERVERS`, `CATENARY_ROOTS`, or
/// `CATENARY_CONFIG` explicitly after this call.
pub fn isolate_env(cmd: &mut Command, root: &str) {
    cmd.env("XDG_CONFIG_HOME", root);
    cmd.env("XDG_STATE_HOME", root);
    cmd.env("XDG_DATA_HOME", root);
    cmd.env("PATH", "");
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
    /// `None` when using a shared (externally-owned) state dir.
    _state_dir: Option<tempfile::TempDir>,
    /// Isolated workspace root dir, owned by this process.
    root_dir: Option<tempfile::TempDir>,
    /// Working directory for IPC tool queries.
    ///
    /// Set from `CATENARY_ROOTS` during spawn. Used as the `cwd` field
    /// in `tool/grep` and `tool/glob` IPC requests.
    ipc_cwd: Option<PathBuf>,
}

impl BridgeProcess {
    /// Spawns with `CATENARY_SERVERS` and a single workspace root.
    pub fn spawn(lsp_commands: &[&str], root: &str) -> Result<Self> {
        Self::spawn_multi_root(lsp_commands, &[root])
    }

    /// Spawns with `CATENARY_SERVERS` and multiple workspace roots.
    pub fn spawn_multi_root(lsp_commands: &[&str], roots: &[&str]) -> Result<Self> {
        let cwd = roots.first().map(PathBuf::from);
        let mut proc = Self::spawn_with(|cmd| {
            if !lsp_commands.is_empty() {
                cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));
            }
            let roots_val = std::env::join_paths(roots).unwrap_or_default();
            cmd.env("CATENARY_ROOTS", &roots_val);
        })?;
        proc.ipc_cwd = cwd;
        Ok(proc)
    }

    /// Spawns with `CATENARY_SERVERS`, a single workspace root, and a
    /// pre-start setup callback for state directory initialization.
    pub fn spawn_with_pre_start(
        lsp_commands: &[&str],
        root: &str,
        pre_start: impl FnOnce(&str) -> Result<()>,
    ) -> Result<Self> {
        let cwd = PathBuf::from(root);
        let mut proc = Self::spawn_with_setup(
            |cmd| {
                if !lsp_commands.is_empty() {
                    cmd.env("CATENARY_SERVERS", lsp_commands.join(";"));
                }
                cmd.env("CATENARY_ROOTS", root);
            },
            pre_start,
        )?;
        proc.ipc_cwd = Some(cwd);
        Ok(proc)
    }

    /// Spawns using a TOML config file with an isolated workspace root.
    ///
    /// Creates a fresh tempdir for the workspace root. `setup` receives the
    /// root path, populates it with test files, and returns the config path.
    /// The tempdir lives as long as this `BridgeProcess`.
    pub fn spawn_with_config(setup: impl FnOnce(&Path) -> Result<PathBuf>) -> Result<Self> {
        let root_dir = tempfile::tempdir().context("Failed to create root dir")?;
        let config_path = setup(root_dir.path())?;
        let mut proc = Self::spawn_with(|cmd| {
            cmd.env("CATENARY_CONFIG", &config_path);
            cmd.env("CATENARY_ROOTS", root_dir.path());
        })?;
        proc.root_dir = Some(root_dir);
        Ok(proc)
    }

    /// Spawns using a TOML config file with additional `CATENARY_SERVERS`
    /// entries merged at runtime. Creates an isolated workspace root like
    /// [`spawn_with_config`].
    pub fn spawn_with_config_and_servers(
        lsp_commands: &[&str],
        setup: impl FnOnce(&Path) -> Result<PathBuf>,
    ) -> Result<Self> {
        let root_dir = tempfile::tempdir().context("Failed to create root dir")?;
        let config_path = setup(root_dir.path())?;
        let servers = lsp_commands.join(";");
        let mut proc = Self::spawn_with(|cmd| {
            cmd.env("CATENARY_CONFIG", &config_path);
            if !servers.is_empty() {
                cmd.env("CATENARY_SERVERS", &servers);
            }
            cmd.env("CATENARY_ROOTS", root_dir.path());
        })?;
        proc.root_dir = Some(root_dir);
        Ok(proc)
    }

    /// Spawns using an externally-owned state directory.
    ///
    /// The caller keeps the state dir alive. Multiple bridges can share
    /// the same `state_home` to connect to the same daemon — the first
    /// bridge starts the daemon, subsequent bridges connect to it.
    pub fn spawn_in_state(state_home: &str, configure: impl FnOnce(&mut Command)) -> Result<Self> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static BRIDGE_SEQ: AtomicU32 = AtomicU32::new(0);

        let state_home = state_home.to_string();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, &state_home);
        configure(&mut cmd);

        let seq = BRIDGE_SEQ.fetch_add(1, Ordering::Relaxed);
        let stderr_path = PathBuf::from(&state_home).join(format!("bridge_stderr_{seq}.log"));
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
            _state_dir: None,
            root_dir: None,
            ipc_cwd: None,
        })
    }

    /// Shared spawn: creates state dir, isolates env, lets `configure`
    /// set `CATENARY_*` vars (after `isolate_env` cleared them), then
    /// redirects stderr and starts the process.
    ///
    /// Prefer `spawn_with_config` for config-based tests — it creates
    /// an isolated workspace root automatically. Use this directly only
    /// when you need a custom root layout (e.g. root marker tests).
    pub fn spawn_with(configure: impl FnOnce(&mut Command)) -> Result<Self> {
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
            _state_dir: Some(state_dir),
            root_dir: None,
            ipc_cwd: None,
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

    /// Returns the daemon IPC socket path for this test's XDG scope.
    pub fn ipc_socket_path(&self) -> PathBuf {
        PathBuf::from(&self.state_home)
            .join("catenary")
            .join("catenary.sock")
    }

    /// Waits for the daemon IPC socket to appear (up to 5 seconds).
    pub fn wait_for_ipc_socket(&self) -> Result<PathBuf> {
        let path = self.ipc_socket_path();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            if std::time::Instant::now() > deadline {
                bail!("IPC socket not found at {} within 5s", path.display());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(path)
    }

    /// Polls `ls-roots` until the given path appears as a tracked root.
    ///
    /// Used after MCP roots/list sync to ensure the daemon has processed
    /// the new root before IPC tool queries run on a separate connection.
    pub fn wait_for_root(&self, root: &str, timeout: Duration) -> Result<()> {
        let socket_path = self.wait_for_ipc_socket()?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let response = ipc_request(&socket_path, &json!({"method": "tool/roots-ls"}))?;
            if response.contains(root) {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                bail!("root {root} not found in ls-roots within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Enters editing mode, accumulates a file, then runs `done_editing`
    /// via the handoff protocol to retrieve diagnostics.
    pub fn call_diagnostics(&self, file: &str) -> Result<String> {
        let socket_path = self.wait_for_ipc_socket()?;

        // Enter editing mode via CLI path
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-start",
                "agent_id": ""
            }),
        )?;

        // Accumulate file via PreToolUse file tracking
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-state",
                "tool_name": "Edit",
                "file_path": file,
                "agent_id": ""
            }),
        )?;

        // Prepare handoff (drains files, deposits in slot)
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-stop",
                "agent_id": ""
            }),
        )?;

        // Run diagnostics via handoff slot
        let text = ipc_request(
            &socket_path,
            &json!({
                "method": "tool/editing-stop"
            }),
        )?;

        Ok(text)
    }

    /// Enters editing mode, accumulates multiple files, then runs
    /// `done_editing` via the handoff protocol to retrieve batched
    /// diagnostics.
    pub fn call_diagnostics_multi(&self, files: &[&str]) -> Result<String> {
        let socket_path = self.wait_for_ipc_socket()?;

        // Enter editing mode via CLI path
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-start",
                "agent_id": ""
            }),
        )?;

        // Accumulate all files via PreToolUse file tracking
        for file in files {
            ipc_request(
                &socket_path,
                &json!({
                    "method": "pre-tool/editing-state",
                    "tool_name": "Edit",
                    "file_path": file,
                    "agent_id": ""
                }),
            )?;
        }

        // Prepare handoff (drains files, deposits in slot)
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-stop",
                "agent_id": ""
            }),
        )?;

        // Run diagnostics via handoff slot
        let text = ipc_request(
            &socket_path,
            &json!({
                "method": "tool/editing-stop"
            }),
        )?;

        Ok(text)
    }

    /// Resolves the working directory for IPC tool queries.
    ///
    /// Priority: explicit `directory` arg > `ipc_cwd` (from spawn roots) >
    /// `root_dir` (from `spawn_with_config`). When no cwd is available,
    /// the field is omitted and the daemon falls back to workspace roots.
    fn resolve_ipc_cwd(&self, obj: &mut serde_json::Map<String, Value>) {
        // Map the old MCP `directory` parameter to `cwd`.
        if let Some(dir) = obj.remove("directory") {
            obj.entry("cwd").or_insert(dir);
        }

        if obj.get("cwd").is_none() {
            let path = self
                .ipc_cwd
                .as_deref()
                .or_else(|| self.root_dir.as_ref().map(tempfile::TempDir::path));
            if let Some(p) = path {
                obj.insert("cwd".to_string(), json!(p.to_string_lossy().as_ref()));
            }
            // No fallback — omitting cwd makes the daemon search all
            // workspace roots (the pre-cwd-scoping behavior).
        }
    }

    /// Calls grep via the IPC socket and returns the output text.
    ///
    /// `args` should contain the grep parameters (`pattern`, and optionally
    /// `glob`, `exclude`, `page`, `include_gitignored`, `include_hidden`).
    /// A `directory` field, if present, is used as the cwd for pattern
    /// resolution (matching the old MCP `directory` parameter semantics).
    pub fn call_grep(&self, args: &Value) -> Result<String> {
        let socket_path = self.wait_for_ipc_socket()?;
        let mut request = args.clone();
        let obj = request.as_object_mut().context("args must be an object")?;
        obj.insert("method".to_string(), json!("tool/grep"));
        self.resolve_ipc_cwd(obj);

        let response = ipc_tool_request(&socket_path, &Value::Object(obj.clone()))?;
        let parsed: Value =
            serde_json::from_str(&response).context("failed to parse grep response")?;
        let output = parsed
            .get("output")
            .and_then(|o| o.as_str())
            .context("no output in grep response")?;
        Ok(output.to_string())
    }

    /// Calls glob via the IPC socket and returns the output text.
    ///
    /// `args` should contain the glob parameters (`pattern`, and optionally
    /// `exclude`, `page`, `include_gitignored`, `include_hidden`).
    /// A `directory` field, if present, is used as the cwd for pattern
    /// resolution.
    pub fn call_glob(&self, args: &Value) -> Result<String> {
        let socket_path = self.wait_for_ipc_socket()?;
        let mut request = args.clone();
        let obj = request.as_object_mut().context("args must be an object")?;
        obj.insert("method".to_string(), json!("tool/glob"));
        self.resolve_ipc_cwd(obj);

        let response = ipc_tool_request(&socket_path, &Value::Object(obj.clone()))?;
        let parsed: Value =
            serde_json::from_str(&response).context("failed to parse glob response")?;
        let output = parsed
            .get("output")
            .and_then(|o| o.as_str())
            .context("no output in glob response")?;
        Ok(output.to_string())
    }

    /// Calls a tool via the IPC socket and returns the raw result object.
    ///
    /// Wraps the IPC text output in an MCP-style content structure for
    /// backward compatibility with existing test assertions.
    pub fn call_tool(&self, name: &str, args: &Value) -> Result<Value> {
        let text = match name {
            "grep" => self.call_grep(args)?,
            "glob" => self.call_glob(args)?,
            _ => bail!("unknown tool: {name} (MCP no longer serves tools)"),
        };
        Ok(json!({
            "content": [{"type": "text", "text": text}]
        }))
    }

    /// Calls a tool via the IPC socket and returns the text output.
    pub fn call_tool_text(&self, name: &str, args: &Value) -> Result<String> {
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

    /// Returns the isolated workspace root path.
    ///
    /// Only available when spawned via `spawn_with_config` or
    /// `spawn_with_config_and_servers`.
    pub fn root_path(&self) -> &Path {
        self.root_dir
            .as_ref()
            .expect("root_path() requires spawn_with_config")
            .path()
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
        let state_home = state_dir.path().to_str().context("state dir path")?;

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, state_home);
        cmd.env("CATENARY_ROOTS", ".");

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
            .args(["debug", "query"])
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

/// Sends an IPC tool query (grep/glob) and reads the response line.
///
/// Unlike [`ipc_request`], this does NOT shutdown the write side after
/// sending — the daemon's tool handler races the query against client
/// disconnect, and a write-shutdown would trigger the disconnect branch
/// before the response is sent.
pub fn ipc_tool_request(socket_path: &Path, request: &Value) -> Result<String> {
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).context("connect to IPC socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    writeln!(stream, "{request}").context("write to IPC socket")?;

    // Read response line (the daemon writes JSON + newline then shuts down).
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("read from IPC socket")?;
    Ok(line)
}

/// Sends a one-shot IPC request to the hook server. Returns the response.
pub fn ipc_request(socket_path: &Path, request: &Value) -> Result<String> {
    use std::io::Read as _;
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).context("connect to notify socket")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    writeln!(stream, "{request}").context("write to notify socket")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("read from notify socket")?;
    Ok(response)
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
