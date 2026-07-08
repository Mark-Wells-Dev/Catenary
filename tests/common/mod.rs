// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared integration test utilities.
//!
//! Each integration test file is a separate compilation unit.
//! `mod common;` imports this module to share [`isolate_env`],
//! [`BridgeProcess`], [`ServerProcess`], and IPC helpers without
//! copy-pasting.

#![allow(dead_code, reason = "each test crate compiles common separately")]

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

// ── Environment isolation ────────────────────────────────────────────

/// Isolates a subprocess from the user's environment.
///
/// Points each XDG base dir at a *distinct* subdir of the given root —
/// `XDG_CONFIG_HOME` → `<root>/config`, `XDG_STATE_HOME` → `<root>/state`,
/// `XDG_DATA_HOME` → `<root>/data`, `XDG_CACHE_HOME` → `<root>/cache`,
/// `XDG_RUNTIME_DIR` → `<root>/runtime` — so the process uses the test's
/// tempdir instead of `~/.config`, `~/.local/state`, `~/.local/share`,
/// `~/.cache`, or the host runtime dir. Keeping the bases distinct makes
/// `isolate_env` a mislocation detector: code that writes under the *wrong*
/// base no longer silently lands in the one shared directory. The cache base
/// homes the JSONL firehose (`db::cache_dir()`).
///
/// Clears all `CATENARY_*` env vars that could leak from the user's
/// shell and override test-specific settings. Clears `PATH` so built-in
/// server defaults (julia-language-server, cmake-language-server, etc.)
/// fail the binary check immediately instead of spawning real processes.
/// Tests that need specific binaries use absolute paths.
///
/// All integration test subprocesses (bridge, `catenary install`, etc.)
/// must call this. Callers set `CATENARY_SERVERS`, `CATENARY_ROOTS`, or
/// `CATENARY_CONFIG` explicitly after this call.
///
/// Test-side code that resolves a daemon path (socket, DB, log via
/// `db::state_dir`) or writes a file the subprocess reads (config via
/// `config_sources()`) must derive it through [`xdg_state_home`] /
/// [`xdg_config_home`] so both sides agree on the split layout.
pub fn isolate_env(cmd: &mut Command, root: &str) {
    cmd.env("XDG_CONFIG_HOME", xdg_config_home(root));
    cmd.env("XDG_STATE_HOME", xdg_state_home(root));
    cmd.env("XDG_DATA_HOME", xdg_data_home(root));
    cmd.env("XDG_CACHE_HOME", xdg_cache_home(root));
    cmd.env("XDG_RUNTIME_DIR", xdg_runtime_dir(root));
    cmd.env("PATH", "");
    // Clear every inherited `CATENARY_*` var so the user's shell can't leak
    // settings (state/runtime/config dirs, server defs, `CATENARY_NOTIFY`,
    // `CATENARY_LOG_RETENTION_DAYS`, `CATENARY_DOCTOR_TIMEOUT_SECS`, …) into the
    // subprocess. Prefix-based, not a hand-maintained list, so a newly-added var
    // is covered for free — an enumerated list silently drifted before (e.g.
    // `CATENARY_RUNTIME_DIR`, added by ticket 11, went unguarded). Callers re-add
    // `CATENARY_SERVERS` / `CATENARY_ROOTS` / `CATENARY_CONFIG` after this call.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("CATENARY_")) {
            cmd.env_remove(&key);
        }
    }
}

/// The `XDG_CONFIG_HOME` subdir [`isolate_env`] configures under `root`.
///
/// `config_sources()` resolves user config at
/// `$XDG_CONFIG_HOME/catenary/config.toml`, so a test writing a config
/// the subprocess must read writes under this path.
pub fn xdg_config_home(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join("config")
}

/// The `XDG_STATE_HOME` subdir [`isolate_env`] configures under `root`.
///
/// `db::state_dir()` resolves here, so the DB, sockets, and daemon log
/// all live under `$XDG_STATE_HOME/catenary/`. Test-side code computing
/// those paths must resolve through this helper.
pub fn xdg_state_home(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join("state")
}

/// The `XDG_DATA_HOME` subdir [`isolate_env`] configures under `root`.
pub fn xdg_data_home(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join("data")
}

/// The `XDG_CACHE_HOME` subdir [`isolate_env`] configures under `root`.
///
/// `db::cache_dir()` resolves here, so the JSONL firehose lives under
/// `$XDG_CACHE_HOME/catenary/`. Test-side code computing firehose paths must
/// resolve through this helper.
pub fn xdg_cache_home(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join("cache")
}

/// The `XDG_RUNTIME_DIR` [`isolate_env`] configures under `root`.
pub fn xdg_runtime_dir(root: impl AsRef<Path>) -> PathBuf {
    root.as_ref().join("runtime")
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

    /// Spawns the daemon with the SHIPPED default config against a fixture root
    /// — the conformance-harness spawn (tui-rework 07).
    ///
    /// Unlike the mock-driven spawns, this sets neither `CATENARY_SERVERS` nor
    /// `CATENARY_CONFIG`: the daemon loads its built-in `defaults/servers.toml`
    /// and `languages.toml`, so the harness exercises the *exact* server command,
    /// language binding, and `workspace/configuration` delivery a user gets (the
    /// pyright-44-minute class — waitv2 findings 5/6 — lives on that delivery
    /// path). It restores the inherited `PATH` that [`isolate_env`] clears so the
    /// real, pinned language-server binary resolves; the XDG bases stay isolated
    /// under a per-test tempdir exactly as every other spawn.
    ///
    /// `root` is the fixture project directory (owned by the caller's `TempDir`,
    /// which must outlive the returned process). It becomes both the sole
    /// workspace root and the IPC `cwd`.
    pub fn spawn_conformance(root: &Path) -> Result<Self> {
        Self::spawn_conformance_with_config(root, None)
    }

    /// Like [`Self::spawn_conformance`], but layers an explicit **user** config
    /// file (`CATENARY_CONFIG`) over the shipped defaults.
    ///
    /// The shipped defaults still load (embedded, lowest priority); the named
    /// file merges on top, so a one-line `[lsp.language.<lang>]` binding
    /// override reroutes the language while every shipped server *definition*
    /// (its command/args) stays intact. This is the layer a routing opt-in must
    /// live in to take effect: a project `.catenary.toml` `[lsp.language.*]`
    /// `servers` list drives classification only, not server dispatch, so it
    /// does not reroute (tui-rework 13 class E — the marksman CI miss). `None`
    /// spawns pure shipped defaults, unchanged.
    pub fn spawn_conformance_with_config(root: &Path, config: Option<&Path>) -> Result<Self> {
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let root_env = root.to_path_buf();
        let config_env = config.map(Path::to_path_buf);
        let mut proc = Self::spawn_with(|cmd| {
            cmd.env("PATH", inherited_path);
            cmd.env("CATENARY_ROOTS", &root_env);
            if let Some(config_path) = config_env {
                cmd.env("CATENARY_CONFIG", config_path);
            }
        })?;
        proc.ipc_cwd = Some(root.to_path_buf());
        Ok(proc)
    }

    /// Gracefully shuts the bridge down and asserts it exited without a kill —
    /// the harness's "shutdown is clean" assertion (tui-rework 07).
    ///
    /// Closes stdin (the same EOF the [`Drop`] path uses as the graceful-shutdown
    /// signal) and reaps the child within `grace`. Reaping here caches the exit
    /// status, so the subsequent `Drop` observes it immediately rather than
    /// repeating the grace wait.
    ///
    /// "Clean" means the bridge **exited on its own** (WIFEXITED) inside `grace`
    /// — neither hung (needing a kill) nor crashed (signal-killed). The exit
    /// *code* is deliberately not asserted: the MCP bridge returns non-zero on
    /// stdin EOF by design (a normal client disconnect), so a nonzero code is
    /// expected, while a signal death (segfault/abort) or a hang is not.
    pub fn shutdown_clean(&mut self, grace: Duration) -> Result<()> {
        self.stdin.take(); // EOF → graceful shutdown.
        let deadline = std::time::Instant::now() + grace;
        loop {
            if let Some(status) = self.child.try_wait().context("wait on bridge")? {
                if status.code().is_some() {
                    return Ok(());
                }
                bail!("bridge was terminated by a signal ({status:?}) — unclean shutdown (crash)");
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                bail!(
                    "bridge did not exit within {grace:?} of stdin close — unclean shutdown (hang)"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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
            let daemon_log = xdg_state_home(&self.state_home)
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
        xdg_state_home(&self.state_home)
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

    /// Resolves the running daemon's PID from its `state.json` session-board
    /// snapshot (`runtime_dir()/catenary/state.json`, field `daemon.pid`).
    ///
    /// The daemon is a *detached grandchild* of this bridge (it is spawned by
    /// `router::spawn_daemon` in its own process group), so its PID is not one
    /// of our direct `Child` handles — the snapshot the daemon writes on startup
    /// is the test-visible source of truth (the same one the TUI reads). The
    /// progress-aware [`ipc_request_long`] wait and the wedged-daemon regression
    /// need this PID to watch the actual daemon process rather than a wall clock.
    ///
    /// Polls the snapshot on [`POLL_SPACING`] until the PID is present and
    /// nonzero, backstopped by [`POLL_BACKSTOP`] (a genuine-hang guard, never the
    /// happy path — the daemon has already answered IPC by the time a caller
    /// needs its PID). Returns `None` only if the daemon never published a PID.
    pub fn daemon_pid(&self) -> Option<u32> {
        let state_json = xdg_runtime_dir(&self.state_home)
            .join("catenary")
            .join("state.json");
        let deadline = std::time::Instant::now() + POLL_BACKSTOP;
        loop {
            if let Some(pid) = read_daemon_pid(&state_json) {
                return Some(pid);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(POLL_SPACING);
        }
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

        // Run diagnostics via handoff slot. Uses the progress-aware wait
        // because the diagnostics pipeline (settle + flycheck) can take tens of
        // seconds under CPU contention from parallel tests.
        let text = ipc_request_long(
            &socket_path,
            self.daemon_pid(),
            &json!({
                "method": "tool/editing-stop"
            }),
        )?;

        Ok(diagnostics_output(&text))
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

        // Run diagnostics via handoff slot. Uses the progress-aware wait
        // because the diagnostics pipeline (settle + flycheck) can take tens of
        // seconds under CPU contention from parallel tests.
        let text = ipc_request_long(
            &socket_path,
            self.daemon_pid(),
            &json!({
                "method": "tool/editing-stop"
            }),
        )?;

        Ok(diagnostics_output(&text))
    }

    /// Runs the **scoped** `catenary diagnostics <paths>` form (ws37 tickets
    /// 02/04): prepares the handoff, then consumes it with an explicit `files`
    /// set — a whole-root `.`, a sub-root directory, or explicit files — without
    /// accumulating any edited set first. Returns the rendered receipt text.
    pub fn call_diagnostics_scoped(&self, paths: &[&str]) -> Result<String> {
        let socket_path = self.wait_for_ipc_socket()?;

        // Enter editing mode via CLI path (parity with the accumulating helpers;
        // the scoped pull names its own set, so nothing is accumulated).
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-start",
                "agent_id": ""
            }),
        )?;

        // Prepare handoff (stages the slot the consume step drains).
        ipc_request(
            &socket_path,
            &json!({
                "method": "pre-tool/editing-stop",
                "agent_id": ""
            }),
        )?;

        // Consume with the explicit scoped `files` set.
        let text = ipc_request_long(
            &socket_path,
            self.daemon_pid(),
            &json!({
                "method": "tool/editing-stop",
                "files": paths,
            }),
        )?;

        Ok(diagnostics_output(&text))
    }

    /// Drives the real `catenary hook pre-tool` binary (the `run_pre_tool`
    /// dispatch) for a Claude `Bash` `tool_input.command`, against this test's
    /// daemon, and returns the hook's stdout — a deny JSON, or empty on allow.
    ///
    /// Unlike the raw `pre-tool/*` IPC helpers, this exercises the actual
    /// dispatch in `run_pre_tool` (regime 1 matcher → routed action / deny →
    /// regime 2), which is the only place the ordering between a piped-
    /// diagnostics deny and the editing-stop prepare-drain is decided.
    ///
    /// The payload carries no `session_id`/`agent_id`, so the hook resolves to
    /// agent `""` / session `None` — matching the raw-IPC editing setup used in
    /// [`call_diagnostics`](Self::call_diagnostics), so both share one
    /// editing-state key.
    pub fn run_pre_tool_bash(&self, command: &str) -> Result<String> {
        let payload = json!({
            "tool_name": "Bash",
            "tool_input": { "command": command },
        });
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
        isolate_env(&mut cmd, &self.state_home);
        cmd.args(["hook", "pre-tool", "--format=claude"]);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawn `hook pre-tool`")?;
        {
            let mut stdin = child.stdin.take().context("hook stdin")?;
            writeln!(stdin, "{payload}").context("write hook payload")?;
        }
        let out = child.wait_with_output().context("wait for hook")?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
    /// `paths`, `exclude`, `page`, `include_gitignored`, `include_hidden`).
    /// A `directory` field, if present, is used as the cwd.
    pub fn call_grep(&self, args: &Value) -> Result<String> {
        let socket_path = self.wait_for_ipc_socket()?;
        let mut request = args.clone();
        let obj = request.as_object_mut().context("args must be an object")?;
        obj.insert("method".to_string(), json!("tool/grep"));
        self.resolve_ipc_cwd(obj);

        let response =
            ipc_tool_request(&socket_path, self.daemon_pid(), &Value::Object(obj.clone()))?;
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
    /// `args` should contain the glob parameters (`paths`, and optionally
    /// `exclude`, `page`, `include_gitignored`, `include_hidden`).
    /// A `directory` field, if present, is used as the cwd.
    pub fn call_glob(&self, args: &Value) -> Result<String> {
        let socket_path = self.wait_for_ipc_socket()?;
        let mut request = args.clone();
        let obj = request.as_object_mut().context("args must be an object")?;
        obj.insert("method".to_string(), json!("tool/glob"));
        self.resolve_ipc_cwd(obj);

        let response =
            ipc_tool_request(&socket_path, self.daemon_pid(), &Value::Object(obj.clone()))?;
        let parsed: Value =
            serde_json::from_str(&response).context("failed to parse glob response")?;
        let output = parsed
            .get("output")
            .and_then(|o| o.as_str())
            .context("no output in glob response")?;
        Ok(output.to_string())
    }

    /// Calls grep/glob via IPC and returns the full parsed response object.
    ///
    /// Unlike [`Self::call_grep`]/[`Self::call_glob`] (which extract only
    /// `output`), this returns the entire response — including the `--count`
    /// fields (`matches`/`files` for grep, `paths` for glob). `method` is the
    /// IPC method string (`tool/grep` or `tool/glob`).
    pub fn call_search_raw(&self, method: &str, args: &Value) -> Result<Value> {
        let socket_path = self.wait_for_ipc_socket()?;
        let mut request = args.clone();
        let obj = request.as_object_mut().context("args must be an object")?;
        obj.insert("method".to_string(), json!(method));
        self.resolve_ipc_cwd(obj);

        let response =
            ipc_tool_request(&socket_path, self.daemon_pid(), &Value::Object(obj.clone()))?;
        serde_json::from_str(&response).context("failed to parse search response")
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

// ── IPC helpers ──────────────────────────────────────────────────────

/// Sends an IPC tool query (grep/glob) and reads the response, watching the
/// daemon process for progress instead of a wall clock (misc 136, applying the
/// bug-59 ruling misc 130 landed for the diagnostics wait).
///
/// The old flat 30s `set_read_timeout` per attempt was a pressure-dependent
/// wall budget of the very family bug 59 condemned: the file's own
/// [`POLL_BACKSTOP`] note records cold searches exceeding 60s under 3×
/// overcommit — a working-but-saturated daemon (per-root server respawn +
/// `--scan-roots` reindex) could blow the flat budget and fail a *working*
/// search. Delegating to [`ipc_request_progress_aware`] on the default
/// [`IPC_NO_PROGRESS_BUDGET`] swaps that clock for the same `catenary-proc`
/// no-progress budget the diagnostics wait uses: a saturated-but-working daemon
/// never times out, and only a genuinely wedged one fails fast.
///
/// Like [`ipc_request_progress_aware`] (and unlike [`ipc_request`]) it does NOT
/// shut down the write side after sending — the daemon's tool handler races the
/// query against client disconnect (bug 24), and a write-shutdown would trigger
/// the disconnect branch before the response is sent.
///
/// `daemon_pid` is the process to watch (from [`BridgeProcess::daemon_pid`]),
/// threaded by the grep/glob callers exactly as the diagnostics callers thread
/// it into [`ipc_request_long`].
pub fn ipc_tool_request(
    socket_path: &Path,
    daemon_pid: Option<u32>,
    request: &Value,
) -> Result<String> {
    ipc_request_progress_aware(socket_path, daemon_pid, request, IPC_NO_PROGRESS_BUDGET)
}

/// Extract the diagnostics text from a `tool/editing-stop` response.
///
/// The daemon returns a `{"status","output"}` JSON envelope (ticket 11); tests
/// assert on the rendered `output`. Falls back to the raw response if it is not
/// the expected envelope, so assertions get something meaningful either way.
pub fn diagnostics_output(response: &str) -> String {
    serde_json::from_str::<Value>(response.trim())
        .ok()
        .and_then(|v| v.get("output").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| response.to_string())
}

/// Sends a one-shot IPC request to the hook server. Returns the response.
///
/// Uses a 10-second read timeout — sufficient for hook calls that return
/// immediately. For calls that block on the diagnostics pipeline (e.g.,
/// `tool/editing-stop` with flycheck), use [`ipc_request_long`].
pub fn ipc_request(socket_path: &Path, request: &Value) -> Result<String> {
    ipc_request_with_timeout(socket_path, request, Duration::from_secs(10))
}

/// No-progress budget for [`ipc_request_long`]: wall time is only charged
/// against this across *consecutive* [`IPC_PROGRESS_POLL`] windows in which the
/// watched daemon shows **zero** progress. A daemon making progress — even a
/// saturated-but-working one whose flycheck child is starved of a core — resets
/// it every window and so never times out; only a genuinely wedged daemon
/// (SIGSTOP, deadlock) burns it down and fails fast.
///
/// Kept small on purpose (order 30–60s per misc 130): it is a hang detector,
/// not a saturation cushion. Bug 59 died here — the old flat 3-minute
/// `set_read_timeout` was a wall clock that inflated with CPU contention and
/// eventually failed a *working* daemon. This mirrors the daemon's own
/// tick-budget settle model (`src/lsp/settle.rs`): measure the process, not the
/// clock.
const IPC_NO_PROGRESS_BUDGET: Duration = Duration::from_secs(45);

/// Poll cadence for the progress-aware wait: the socket read timeout doubles as
/// the daemon-sampling interval, so each blocked read returns within one window
/// and we re-check progress. Short enough that the daemon's own 50 ms settle
/// loop advances its counters within every window; this is cadence, not a
/// readiness guess.
const IPC_PROGRESS_POLL: Duration = Duration::from_millis(200);

/// Progress-aware replacement for the old flat 3-minute `tool/editing-stop`
/// read timeout (misc 130). Blocks on the diagnostics pipeline while the daemon
/// keeps working; fails fast only when the daemon genuinely stops making
/// progress. Delegates to [`ipc_request_progress_aware`] with the default
/// [`IPC_NO_PROGRESS_BUDGET`].
///
/// Crucially it does NOT shut down the write side after sending. The daemon
/// races the pipeline against client disconnect (bug 24); a write-shutdown reads
/// as EOF on the daemon side and would trip the disconnect branch before the
/// response is sent. This mirrors the production `catenary diagnostics` client
/// (`run_done_editing`), which likewise keeps the write half open and reads to
/// EOF with no wall-clock budget — same non-half-closing contract as
/// [`ipc_tool_request`].
///
/// `daemon_pid` is the process to watch (from [`BridgeProcess::daemon_pid`]).
/// `None` (or a PID that can no longer be sampled) means the daemon is
/// unwatchable — every window then reads as no-progress, so an unmonitorable
/// daemon fails within the budget instead of hanging.
pub fn ipc_request_long(
    socket_path: &Path,
    daemon_pid: Option<u32>,
    request: &Value,
) -> Result<String> {
    ipc_request_progress_aware(socket_path, daemon_pid, request, IPC_NO_PROGRESS_BUDGET)
}

/// [`ipc_request_long`] with an explicit no-progress budget.
///
/// Polls the socket on [`IPC_PROGRESS_POLL`] (the read timeout *is* the poll
/// cadence) and, on every window that returns no response bytes, samples the
/// daemon process via [`catenary_proc::ProcessMonitor`]. A window counts as
/// progress when the daemon's cumulative counters (`utime + stime + pfc +
/// voluntary context switches`) advanced, or when its scheduler state is pending
/// work (runnable / uninterruptible I/O) — the same signals the daemon's own
/// settle loop uses. Progress (including any received bytes) resets the budget;
/// only consecutive flat windows charge it, and the wait bails once they sum to
/// `no_progress_budget`.
///
/// Exposed with an explicit budget so the wedged-daemon regression can drive the
/// identical mechanism on a short budget without a multi-second test.
pub fn ipc_request_progress_aware(
    socket_path: &Path,
    daemon_pid: Option<u32>,
    request: &Value,
    no_progress_budget: Duration,
) -> Result<String> {
    use std::io::Read as _;

    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).context("connect to notify socket")?;
    // The read timeout is poll cadence, not a budget: a blocked read returns
    // within one window so we can re-sample the daemon. Do NOT shut down the
    // write side (bug 24 — see the doc comment).
    stream
        .set_read_timeout(Some(IPC_PROGRESS_POLL))
        .context("set poll-cadence read timeout")?;
    writeln!(stream, "{request}").context("write to notify socket")?;

    // Watch the actual daemon process. Prime the monitor so the first in-loop
    // sample yields a real delta (the first `sample()` always reports zero).
    let mut monitor = daemon_pid.and_then(catenary_proc::ProcessMonitor::new);
    if let Some(m) = monitor.as_mut() {
        let _ = m.sample();
    }
    let mut prev_ticks = monitor
        .as_ref()
        .map_or(0, catenary_proc::ProcessMonitor::cumulative_ticks);

    let mut response: Vec<u8> = Vec::new();
    let mut no_progress = Duration::ZERO;
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            // EOF: the daemon wrote the full response and closed the write side.
            Ok(0) => break,
            // Bytes arrived — unambiguous progress. Re-baseline the monitor.
            Ok(n) => {
                response.extend_from_slice(&chunk[..n]);
                no_progress = Duration::ZERO;
                if let Some(m) = monitor.as_mut() {
                    let _ = m.sample();
                    prev_ticks = m.cumulative_ticks();
                }
            }
            // Poll window elapsed with no data: charge the budget only if the
            // daemon itself made no progress this window.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if daemon_progressed(monitor.as_mut(), &mut prev_ticks) {
                    no_progress = Duration::ZERO;
                } else {
                    no_progress += IPC_PROGRESS_POLL;
                    if no_progress >= no_progress_budget {
                        let method = request
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>");
                        bail!(
                            "daemon (pid {daemon_pid:?}) showed no progress for \
                             {no_progress_budget:?} awaiting `{method}` — wedged"
                        );
                    }
                }
            }
            Err(e) => return Err(e).context("read from notify socket"),
        }
    }

    String::from_utf8(response).context("notify socket response was not valid UTF-8")
}

/// Samples the watched daemon once and reports whether it made progress since
/// the previous sample, advancing `prev_ticks` to the new cumulative value.
///
/// Progress = the cumulative counters (`utime + stime + pfc + voluntary context
/// switches`, via [`catenary_proc::ProcessMonitor::cumulative_ticks`]) advanced,
/// **or** the daemon is in a pending-work scheduler state
/// ([`daemon_pending_work`]). An absent monitor (no PID) or a PID that can no
/// longer be sampled (process gone) reports no progress, so an unwatchable
/// daemon fails fast rather than hanging.
fn daemon_progressed(
    monitor: Option<&mut catenary_proc::ProcessMonitor>,
    prev_ticks: &mut u64,
) -> bool {
    let Some(m) = monitor else {
        return false;
    };
    let Some(delta) = m.sample() else {
        return false;
    };
    let now = m.cumulative_ticks();
    let advanced = now > *prev_ticks;
    *prev_ticks = now;
    advanced || daemon_pending_work(delta.state)
}

/// Whether a daemon [`catenary_proc::ProcessState`] counts as pending work for
/// the progress-aware wait — a mirror of `is_pending_work` in
/// `src/lsp/settle.rs`.
///
/// Where scheduler state is observable, a runnable (`Running`) or
/// uninterruptible-I/O (`Blocked`) daemon has pending work regardless of the
/// sampled deltas, so a window in either state is progress even when the
/// counters happen to round flat. A SIGSTOP'd daemon reports `Dead` (stopped),
/// so it is correctly *not* pending. Off observable-scheduler platforms this is
/// always `false` and progress falls back to the cumulative counters alone.
const fn daemon_pending_work(state: catenary_proc::ProcessState) -> bool {
    catenary_proc::SCHEDULER_STATE_OBSERVABLE
        && matches!(
            state,
            catenary_proc::ProcessState::Running | catenary_proc::ProcessState::Blocked
        )
}

/// Parses `daemon.pid` from a `state.json` snapshot (see
/// [`BridgeProcess::daemon_pid`]). Returns `None` if the file is absent,
/// unparseable, or the `pid` field is missing or zero.
fn read_daemon_pid(state_json: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(state_json).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let pid = value.get("daemon")?.get("pid")?.as_u64()?;
    u32::try_from(pid).ok().filter(|&p| p != 0)
}

fn ipc_request_with_timeout(
    socket_path: &Path,
    request: &Value,
    timeout: Duration,
) -> Result<String> {
    use std::io::Read as _;
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket_path).context("connect to notify socket")?;
    stream.set_read_timeout(Some(timeout))?;
    writeln!(stream, "{request}").context("write to notify socket")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .context("read from notify socket")?;
    Ok(response)
}

/// Builds a `CATENARY_SERVERS` spec for [`BridgeProcess::spawn`] using mockls.
///
/// Always passes `--log-pid-suffix`, so every per-root mockls instance writes its
/// OWN `<log>.<pid>` file. Concurrent or transient instances of one language
/// (multi-root, root churn, or a transient double-spawn under load) therefore
/// never share — and destructively truncate or byte-interleave — a single log
/// file. Readers merge `<base>.*` via [`read_merged_log`] (a single-instance test
/// has exactly one pid file, so the merged content matches the old shared file).
/// Callers must NOT also pass `--log-pid-suffix`.
pub fn mockls_lsp_arg(lang: &str, flags: &str) -> String {
    let bin = env!("CARGO_BIN_EXE_mockls");
    if flags.is_empty() {
        format!("{lang}:{bin} {lang} --log-pid-suffix")
    } else {
        format!("{lang}:{bin} {lang} --log-pid-suffix {flags}")
    }
}

/// Reads a mockls log written with `--log-pid-suffix`: merges every
/// `<base>.<pid>` sibling (one writer each ⇒ no torn/truncated lines), plus
/// `<base>` itself if present. Returns `String::new()` when nothing exists yet.
///
/// [`mockls_lsp_arg`] always enables `--log-pid-suffix`, so this is the canonical
/// way to read a notification/request log: a single-instance test has exactly one
/// pid file (merged content == the old shared-file content), and a transient
/// second instance's empty pid file contributes nothing rather than truncating
/// the serving instance's log.
pub fn read_merged_log(base: &Path) -> String {
    let mut buf = std::fs::read_to_string(base).unwrap_or_default();
    let (Some(dir), Some(name)) = (base.parent(), base.file_name().and_then(|n| n.to_str())) else {
        return buf;
    };
    let prefix = format!("{name}.");
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|f| f.starts_with(&prefix))
            })
            .collect();
        paths.sort();
        for p in paths {
            if let Ok(t) = std::fs::read_to_string(&p) {
                buf.push_str(&t);
            }
        }
    }
    buf
}

// ── Contention-resistant signal polling ──────────────────────────────
//
// The maintainer runs many agents rebuilding/testing concurrently, so heavy
// *external* CPU contention is the normal operating condition. A fixed
// `sleep(N ms)` used as a "the server/notification is surely ready by now"
// guess flakes under that load. These helpers replace the guess with
// signal-polling: success completes as soon as the observable signal appears,
// and the deadline is a generous backstop that only trips on a genuine hang
// (≫ any plausible contention stall) — never on the happy path. The short
// spacing between attempts is poll cadence, not a readiness guess.

/// Generous backstop for every signal poll. On a healthy machine the awaited
/// signal appears in milliseconds; this only trips on a real hang.
///
/// Sized to absorb even pathological *external* CPU contention (e.g. several
/// full test suites stress-running concurrently on top of the maintainer's
/// normal multi-agent load), under which a real per-root LSP server respawn +
/// `--scan-roots` reindex can legitimately take well over a minute. It stays
/// under nextest's `slow-timeout` absolute kill (`.config/nextest.toml`:
/// 60s × 5 = 300s), which is the true runaway backstop. A correct test under
/// heavy load completes as soon as its signal appears — just slower — and never
/// trips this; only a genuine hang does. (Measured: the R4 eviction test, whose
/// cold re-grep forces a full server respawn + reindex, needed > 60s under a
/// 3×-overcommit stress; 2 min gives comfortable margin below the 300s kill.)
pub const POLL_BACKSTOP: Duration = Duration::from_mins(2);

/// Spacing between poll attempts — poll cadence, not a readiness guess.
/// Matches the 50 ms cadence of the existing `roots-ls` poll loops.
pub const POLL_SPACING: Duration = Duration::from_millis(50);

/// Reads the notification log and returns every `(uri, type)` pair from every
/// `workspace/didChangeWatchedFiles` notification recorded.
///
/// Mockls writes each notification to its `--notification-log` file with a
/// direct (unbuffered) `writeln!` the moment it processes the notification, so
/// reading the file mid-session reflects everything mockls has handled so far.
pub fn watched_file_changes(log: &str) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for line in log.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("method").and_then(Value::as_str) != Some("workspace/didChangeWatchedFiles") {
            continue;
        }
        let Some(changes) = entry.get("changes").and_then(Value::as_array) else {
            continue;
        };
        for change in changes {
            let uri = change
                .get("uri")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let typ = change.get("type").and_then(Value::as_u64).unwrap_or(0);
            out.push((uri, typ));
        }
    }
    out
}

/// Counts the number of `workspace/didChangeWatchedFiles` notifications (not
/// individual changes) recorded in the log.
pub fn watched_file_notification_count(log: &str) -> usize {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| {
            entry.get("method").and_then(Value::as_str) == Some("workspace/didChangeWatchedFiles")
        })
        .count()
}

/// Reads the per-instance `--log-pid-suffix` file (`<base>.<pid>`) whose
/// `__instance_root` marker equals `root`, or `String::new()` if none exists yet.
///
/// One language spawns one instance per tracked root; each writes its primary
/// workspace root as the first `__instance_root` log line (mockls). This selects
/// the file for a SPECIFIC root so a test can assert against the
/// `parent`-scoped vs `parent/sub`-scoped instance of one language — which the
/// merged view cannot distinguish.
pub fn read_instance_log_for_root(base: &Path, root: &str) -> String {
    let Some(dir) = base.parent() else {
        return String::new();
    };
    let Some(name) = base.file_name().and_then(|n| n.to_str()) else {
        return String::new();
    };
    let prefix = format!("{name}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|f| f.starts_with(&prefix))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let marks_root = text.lines().any(|line| {
            serde_json::from_str::<Value>(line).is_ok_and(|e| {
                e.get("method").and_then(Value::as_str) == Some("__instance_root")
                    && e.get("uri").and_then(Value::as_str) == Some(root)
            })
        });
        if marks_root {
            return text;
        }
    }
    String::new()
}

/// Polls the per-instance log for a SPECIFIC root (see
/// [`read_instance_log_for_root`]) until `pred` holds over its parsed changes,
/// then returns that snapshot.
pub fn poll_instance_log_until<P>(base: &Path, root: &str, mut pred: P) -> Vec<(String, u64)>
where
    P: FnMut(&[(String, u64)]) -> bool,
{
    let deadline = std::time::Instant::now() + POLL_BACKSTOP;
    loop {
        let changes = watched_file_changes(&read_instance_log_for_root(base, root));
        if pred(&changes) || std::time::Instant::now() >= deadline {
            return changes;
        }
        std::thread::sleep(POLL_SPACING);
    }
}

/// Like [`wait_for_change`] but over the per-instance log of a SPECIFIC root
/// (see [`read_instance_log_for_root`]).
pub fn wait_for_change_in_root(base: &Path, root: &str, uri: &str, typ: u64) -> Vec<(String, u64)> {
    poll_instance_log_until(base, root, |changes| {
        changes.iter().any(|(u, t)| u == uri && *t == typ)
    })
}

/// Polls a notification log until the parsed `(uri, type)` changes satisfy
/// `pred`, then returns that snapshot. Re-reads + re-parses the file each
/// attempt (mockls writes it live), so the returned snapshot is the first one
/// for which `pred` held. On a missing/empty log the parsed set is empty.
///
/// Used to gate a *positive* assertion on the observable signal that the
/// expected change has been routed and recorded — not on a fixed delay. The
/// deadline is the generous [`POLL_BACKSTOP`]; the caller should still assert on
/// the returned snapshot so a deadline reached with `pred` unmet fails loudly.
pub fn poll_log_until<P>(log_path: &Path, mut pred: P) -> Vec<(String, u64)>
where
    P: FnMut(&[(String, u64)]) -> bool,
{
    let deadline = std::time::Instant::now() + POLL_BACKSTOP;
    loop {
        let log = read_merged_log(log_path);
        let changes = watched_file_changes(&log);
        if pred(&changes) || std::time::Instant::now() >= deadline {
            return changes;
        }
        std::thread::sleep(POLL_SPACING);
    }
}

/// Polls a notification log until a specific `(uri, type)` change appears, then
/// returns the full snapshot. Convenience over [`poll_log_until`] for the common
/// "wait for this exact change, then assert over the snapshot" case (including a
/// companion-anchored negative assertion: wait for an expected positive change,
/// then assert the unwanted change is absent in the SAME snapshot).
pub fn wait_for_change(log_path: &Path, uri: &str, typ: u64) -> Vec<(String, u64)> {
    poll_log_until(log_path, |changes| {
        changes.iter().any(|(u, t)| u == uri && *t == typ)
    })
}

/// Rewrites `path` with `content` and confirms its mtime strictly advanced past
/// what it was before the write, retrying until it does (or [`POLL_BACKSTOP`]).
///
/// The changed-set engine keys a modification on a strictly-greater mtime, so a
/// test that wants the next walk to diff a file as `Changed` must guarantee the
/// new mtime exceeds the baselined one. This gates on the *observed* mtime
/// advance — the change signal itself — rather than a fixed `sleep` to span the
/// filesystem's mtime granularity, so it is correct on coarse-granularity
/// filesystems and unaffected by CPU contention. On tmpfs (nanosecond mtime)
/// the first write already advances, so the loop runs once.
pub fn rewrite_advancing_mtime(path: &Path, content: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let before = std::fs::metadata(path).map_or(i64::MIN, |m| m.mtime());
    let deadline = std::time::Instant::now() + POLL_BACKSTOP;
    loop {
        std::fs::write(path, content).context("rewrite file to advance mtime")?;
        let now = std::fs::metadata(path).map_or(i64::MIN, |m| m.mtime());
        if now > before || std::time::Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(POLL_SPACING);
    }
}

/// Retries an enriched grep until its output carries a `#scope` containment
/// anchor — a `#` that is not the `#?` degradation marker — on some hit line,
/// then returns that output. A real scope anchor *is* the readiness signal for
/// the (re-)spawned server + its `--scan-roots` `documentSymbol` index: a
/// contention-resistant replacement for `sleep(N) + grep-once` (which guesses
/// the server is ready and races a cold grep against server readiness under
/// load). Polls on the generous [`POLL_BACKSTOP`]; if the deadline passes the
/// last (un-enriched) output is returned so the caller's precondition fails
/// loudly rather than hanging.
pub fn grep_until_enriched(bridge: &BridgeProcess, args: &Value) -> Result<String> {
    let deadline = std::time::Instant::now() + POLL_BACKSTOP;
    loop {
        let out = bridge.call_tool_text("grep", args)?;
        // A hit inside a named scope renders `path:line#scope:raw`; `#?` is the
        // un-enrichable marker. A line carrying a `#` that is not `#?` proves the
        // server answered `documentSymbol` and the index is warm.
        let scoped = out.lines().any(|l| l.contains('#') && !l.contains("#?"));
        if scoped || std::time::Instant::now() >= deadline {
            return Ok(out);
        }
        std::thread::sleep(POLL_SPACING);
    }
}
