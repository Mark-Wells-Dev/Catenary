// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::print_stdout,
    reason = "profiling test prints summary to stdout"
)]
#![allow(
    clippy::similar_names,
    reason = "utime/stime/pfc counter names are intentionally similar"
)]
//! Intensity profiling integration test.
//!
//! Spawns real LSP servers against fixture projects, runs the settle loop
//! with a recording sink, appends samples to a temp JSONL file (one JSON
//! object per line), and prints per-server summary statistics.
//!
//! Run with:
//! ```text
//! PROFILE_OUT=internal_repo/data/intensity.jsonl make test-ignored T=profile_intensity
//! ```
//!
//! The log accumulates (append) across servers in one run and across runs;
//! delete the file for a clean capture. Any prior `.db` captures stay
//! readable with `sqlite3` — this test no longer writes that format.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use catenary_cli::lsp::LspServer;
use catenary_cli::lsp::settle::{ProfileSample, ProfileSink, profile_loop};

// ── Constants ────────────────────────────────────────────────────────

const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_DURATION: Duration = Duration::from_secs(20);
const STIMULUS_DURATION: Duration = Duration::from_secs(10);
const RECOVERY_DURATION: Duration = Duration::from_secs(10);

/// Delay after initialize before sending didOpen — lets the server begin
/// indexing before we hand it a document.
const INIT_SETTLE: Duration = Duration::from_secs(2);

// ── Server definitions ───────────────────────────────────────────────

struct ServerDef {
    name: &'static str,
    binary: &'static str,
    args: &'static [&'static str],
    language_id: &'static str,
    fixture_dir: &'static str,
    /// File that will receive the bad→clean cycle.
    bad_file: &'static str,
    /// Source of clean content for `bad_file`. If `Some`, the clean
    /// content is read from this file; during stimulus, `bad_file`'s
    /// original content becomes the "bad" payload. If `None`, `bad_file`
    /// is opened as-is (no clean→bad→clean cycle).
    clean_file: Option<&'static str>,
}

const SERVERS: &[ServerDef] = &[
    ServerDef {
        name: "rust-analyzer",
        binary: "rust-analyzer",
        args: &[],
        language_id: "rust",
        fixture_dir: "rust",
        bad_file: "src/main.rs",
        clean_file: Some("src/clean.rs"),
    },
    ServerDef {
        name: "taplo",
        binary: "taplo",
        args: &["lsp", "stdio"],
        language_id: "toml",
        fixture_dir: "toml",
        bad_file: "bad.toml",
        clean_file: None,
    },
    ServerDef {
        name: "lattice",
        binary: "lattice",
        args: &["serve"],
        language_id: "markdown",
        fixture_dir: "markdown",
        bad_file: "doc.md",
        clean_file: None,
    },
    ServerDef {
        name: "bash-language-server",
        binary: "bash-language-server",
        args: &["start"],
        language_id: "shellscript",
        fixture_dir: "bash",
        bad_file: "bad.sh",
        clean_file: None,
    },
    ServerDef {
        name: "vscode-json-language-server",
        binary: "vscode-json-language-server",
        args: &["--stdio"],
        language_id: "json",
        fixture_dir: "json",
        bad_file: "bad.json",
        clean_file: None,
    },
    ServerDef {
        name: "pyright-langserver",
        binary: "pyright-langserver",
        args: &["--stdio"],
        language_id: "python",
        fixture_dir: "python",
        bad_file: "bad.py",
        clean_file: None,
    },
    ServerDef {
        name: "yaml-language-server",
        binary: "yaml-language-server",
        args: &["--stdio"],
        language_id: "yaml",
        fixture_dir: "yaml",
        bad_file: "bad.yaml",
        clean_file: None,
    },
];

// ── Recording sink ───────────────────────────────────────────────────

/// One recorded sample, serialized as a single JSON line.
#[derive(serde::Serialize, serde::Deserialize)]
struct Sample {
    timestamp_ms: i64,
    server: String,
    pid: u32,
    ppid: u32,
    delta_pfc: u64,
    delta_utime: u64,
    delta_stime: u64,
    in_progress: u32,
    process_count: u64,
}

/// Appends samples to a JSONL file, one JSON object per line.
struct RecordingSink {
    out: std::fs::File,
    start: Instant,
}

impl RecordingSink {
    fn new(path: &Path) -> Result<Self> {
        // Append, not truncate: the SERVERS loop opens a fresh sink per server
        // at the same path, so each must accumulate onto the prior server's
        // samples (mirrors the old `CREATE TABLE IF NOT EXISTS` + INSERT).
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open profile log {}", path.display()))?;
        Ok(Self {
            out,
            start: Instant::now(),
        })
    }
}

impl ProfileSink for RecordingSink {
    fn record(&mut self, sample: &ProfileSample) -> bool {
        let elapsed_ms = sample.timestamp.duration_since(self.start).as_millis();

        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            reason = "elapsed ms fits in i64 for any realistic profiling run"
        )]
        let record = Sample {
            timestamp_ms: elapsed_ms as i64,
            server: sample.server.clone(),
            pid: sample.pid,
            ppid: sample.ppid,
            delta_pfc: sample.delta_pfc,
            delta_utime: sample.delta_utime,
            delta_stime: sample.delta_stime,
            in_progress: sample.in_progress_count,
            process_count: sample.process_count as u64,
        };

        let Ok(mut line) = serde_json::to_string(&record) else {
            return false;
        };
        line.push('\n');
        // Write per-line (File is unbuffered) so a killed run still leaves a
        // complete prefix. Recording failure shouldn't crash the loop — stop.
        self.out.write_all(line.as_bytes()).is_ok()
    }
}

// ── LSP transport helpers ────────────────────────────────────────────

async fn send_lsp(stdin: &mut ChildStdin, msg: &Value) -> Result<()> {
    let body = serde_json::to_string(msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).await?;
    stdin.write_all(body.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn recv_lsp(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    // Read headers until blank line.
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .context("Failed to read LSP header line")?;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if trimmed.to_ascii_lowercase().starts_with("content-length:")
            && let Some(val) = trimmed.split(':').nth(1)
        {
            content_length = val.trim().parse().ok();
        }
    }

    let len = content_length.context("Missing Content-Length header")?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("Failed to read LSP body")?;
    serde_json::from_slice(&body).context("Failed to parse LSP JSON")
}

// ── Binary lookup ────────────────────────────────────────────────────

fn find_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

// ── LSP handshake ────────────────────────────────────────────────────

async fn initialize_server(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    root_uri: &str,
) -> Result<Value> {
    let init_params = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {
                        "versionSupport": true
                    },
                    "diagnostic": {
                        "dynamicRegistration": false
                    }
                },
                "window": {
                    "workDoneProgress": true
                }
            },
            "workspaceFolders": [{
                "uri": root_uri,
                "name": "profile"
            }]
        }
    });

    send_lsp(stdin, &init_params).await?;

    // Read messages until we get the initialize response (id: 1).
    let capabilities = loop {
        let msg = tokio::time::timeout(Duration::from_secs(30), recv_lsp(reader))
            .await
            .context("Timeout waiting for initialize response")?
            .context("Failed to read initialize response")?;

        if msg.get("id").and_then(Value::as_i64) == Some(1) {
            break msg
                .get("result")
                .and_then(|r| r.get("capabilities"))
                .cloned()
                .unwrap_or_default();
        }
        // Otherwise it's a notification or server request — skip.
    };

    // Send initialized notification.
    let initialized = json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    });
    send_lsp(stdin, &initialized).await?;

    Ok(capabilities)
}

// ── Reader task ─────────────────────────────────────────────────────

/// Background task that reads all LSP messages from the server, wires
/// `$/progress` notifications into the `LspServer` profile, and responds
/// to `window/workDoneProgress/create` requests.
async fn reader_loop(
    mut reader: BufReader<ChildStdout>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    server: Arc<LspServer>,
    cancel: CancellationToken,
) {
    loop {
        let msg = tokio::select! {
            result = recv_lsp(&mut reader) => {
                match result {
                    Ok(m) => m,
                    Err(_) => return, // server closed stdout
                }
            }
            () = cancel.cancelled() => { return; }
        };

        let method = msg.get("method").and_then(Value::as_str);
        let has_id = msg.get("id").is_some();

        match method {
            Some("$/progress") => {
                let params = msg.get("params").unwrap_or(&Value::Null);
                let kind = params
                    .get("value")
                    .and_then(|v| v.get("kind"))
                    .and_then(Value::as_str);
                if matches!(kind, Some("begin" | "end")) {
                    server.on_notification("$/progress", params);
                }
            }
            Some("window/workDoneProgress/create") if has_id => {
                // Must respond so the server can use progress tokens.
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": null
                });
                let mut guard = stdin.lock().await;
                let _ = send_lsp(&mut guard, &response).await;
            }
            _ => {} // Ignore all other messages.
        }
    }
}

// ── Shutdown ─────────────────────────────────────────────────────────

async fn shutdown_server(stdin: &Arc<tokio::sync::Mutex<ChildStdin>>, child: &mut Child) {
    let mut guard = stdin.lock().await;

    // Send shutdown request.
    let shutdown = json!({
        "jsonrpc": "2.0",
        "id": 9999,
        "method": "shutdown",
        "params": null
    });
    let _ = send_lsp(&mut guard, &shutdown).await;

    // Send exit notification (don't wait for response — reader task
    // may already be cancelled).
    let exit = json!({
        "jsonrpc": "2.0",
        "method": "exit",
        "params": null
    });
    let _ = send_lsp(&mut guard, &exit).await;
    drop(guard);

    // Give the process a moment to exit gracefully, then kill.
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(_) => {}
        Err(_) => {
            let _ = child.kill().await;
        }
    }
}

// ── LSP document helpers ────────────────────────────────────────────

async fn send_did_open(
    stdin: &mut ChildStdin,
    uri: &str,
    language_id: &str,
    text: &str,
) -> Result<()> {
    send_lsp(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }
        }),
    )
    .await
}

async fn send_did_change(
    stdin: &mut ChildStdin,
    uri: &str,
    version: i64,
    text: &str,
) -> Result<()> {
    send_lsp(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }
        }),
    )
    .await
}

async fn send_did_save(stdin: &mut ChildStdin, uri: &str, text: &str) -> Result<()> {
    send_lsp(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didSave",
            "params": {
                "textDocument": { "uri": uri },
                "text": text
            }
        }),
    )
    .await
}

async fn send_did_close(stdin: &mut ChildStdin, uri: &str) -> Result<()> {
    send_lsp(
        stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didClose",
            "params": {
                "textDocument": { "uri": uri }
            }
        }),
    )
    .await
}

// ── Summary output ───────────────────────────────────────────────────

fn print_summary(out_path: &Path) -> Result<()> {
    let content = std::fs::read_to_string(out_path)
        .with_context(|| format!("read profile log {}", out_path.display()))?;
    let samples: Vec<Sample> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("parse sample line"))
        .collect::<Result<_>>()?;

    // Distinct servers, alphabetical (was `SELECT DISTINCT server ORDER BY server`).
    let mut servers: Vec<&str> = samples.iter().map(|s| s.server.as_str()).collect();
    servers.sort_unstable();
    servers.dedup();

    println!("\n{}", "=".repeat(60));
    println!("  INTENSITY PROFILING SUMMARY");
    println!("{}\n", "=".repeat(60));

    for server in servers {
        // Samples for this server in record order (the old `ORDER BY id`).
        let rows: Vec<&Sample> = samples.iter().filter(|s| s.server == server).collect();
        println!("--- {server} ---\n");

        println!("  Total samples: {}", rows.len());

        let max_procs = rows.iter().map(|s| s.process_count).max().unwrap_or(0);
        println!("  Max process count: {max_procs}");

        let sum_pfc = rows
            .iter()
            .map(|s| s.delta_pfc)
            .fold(0u64, u64::saturating_add);
        let sum_utime = rows
            .iter()
            .map(|s| s.delta_utime)
            .fold(0u64, u64::saturating_add);
        let sum_stime = rows
            .iter()
            .map(|s| s.delta_stime)
            .fold(0u64, u64::saturating_add);
        let max_pfc = rows.iter().map(|s| s.delta_pfc).max().unwrap_or(0);
        let max_utime = rows.iter().map(|s| s.delta_utime).max().unwrap_or(0);

        println!("  Total delta_pfc: {sum_pfc}  (max single: {max_pfc})");
        println!("  Total delta_utime: {sum_utime}  (max single: {max_utime})");
        println!("  Total delta_stime: {sum_stime}");

        // in_progress transitions — consecutive samples whose count differs.
        // The Rust equivalent of `LAG(in_progress) OVER (ORDER BY id)`: the
        // first sample has no predecessor and is never counted.
        let transitions = rows
            .windows(2)
            .filter(|w| w[0].in_progress != w[1].in_progress)
            .count();
        println!("  in_progress transitions: {transitions}");

        let mut pids: Vec<u32> = rows.iter().map(|s| s.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        println!("  Distinct PIDs observed: {}", pids.len());

        println!();
    }

    Ok(())
}

// ── Main test ────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "manual profiling test — requires real LSP servers"]
#[allow(
    clippy::too_many_lines,
    reason = "sequential per-server profiling loop with setup/stimulus/teardown phases"
)]
async fn profile_intensity() -> Result<()> {
    let fixtures_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profile");

    // PROFILE_OUT=/path/to/output.jsonl persists the log for later analysis.
    // Without it, a tempdir is used and cleaned up on exit.
    let tmp_dir_guard;
    let out_path = if let Ok(path) = std::env::var("PROFILE_OUT") {
        tmp_dir_guard = None;
        let p = PathBuf::from(&path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        p
    } else {
        let td = tempfile::tempdir()?;
        let p = td.path().join("intensity.jsonl");
        tmp_dir_guard = Some(td);
        p
    };
    // Keep tempdir alive for the duration of the test.
    let _ = &tmp_dir_guard;

    let mut any_server_ran = false;

    for def in SERVERS {
        // Check if binary is available.
        if find_binary(def.binary).is_none() {
            println!("SKIP {}: binary '{}' not found", def.name, def.binary);
            continue;
        }

        println!("PROFILING: {}", def.name);

        // Copy fixtures to a temp dir so we don't pollute the source tree.
        let work_dir = tempfile::tempdir()?;
        let fixture_src = fixtures_dir.join(def.fixture_dir);
        copy_dir_recursive(&fixture_src, work_dir.path())?;

        let root_uri = format!("file://{}", work_dir.path().display());

        // Spawn server.
        let mut child = Command::new(def.binary)
            .args(def.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(work_dir.path())
            .spawn()
            .with_context(|| format!("Failed to spawn {}", def.name))?;

        let child_stdin = child.stdin.take().context("No stdin")?;
        let stdout = child.stdout.take().context("No stdout")?;
        let mut reader = BufReader::new(stdout);

        // Shared stdin — reader task needs it for workDoneProgress/create responses.
        let stdin = Arc::new(tokio::sync::Mutex::new(child_stdin));

        // Initialize.
        let capabilities = {
            let mut guard = stdin.lock().await;
            match tokio::time::timeout(
                Duration::from_secs(30),
                initialize_server(&mut guard, &mut reader, &root_uri),
            )
            .await
            {
                Ok(Ok(caps)) => caps,
                Ok(Err(e)) => {
                    println!("  SKIP: initialize failed: {e}");
                    let _ = child.kill().await;
                    continue;
                }
                Err(_) => {
                    println!("  SKIP: initialize timed out");
                    let _ = child.kill().await;
                    continue;
                }
            }
        };

        let server = Arc::new(LspServer::new(
            def.name.to_string(),
            def.name.to_string(),
            None,
        ));
        server.set_capabilities(capabilities);

        let pid = child
            .id()
            .context(format!("{}: no PID after spawn", def.name))?;
        let Some(mut tree_monitor) = catenary_proc::TreeMonitor::new(pid) else {
            println!("  SKIP: could not create TreeMonitor for PID {pid}");
            let _ = child.kill().await;
            continue;
        };

        // Create recording sink.
        let mut sink = RecordingSink::new(&out_path)?;

        // Cancellation token shared by settle loop and reader task.
        let cancel = CancellationToken::new();

        // ── Spawn reader task (wires progress tokens) ────────────
        let reader_cancel = cancel.clone();
        let reader_server = Arc::clone(&server);
        let reader_stdin = Arc::clone(&stdin);
        let reader_handle = tokio::spawn(async move {
            reader_loop(reader, reader_stdin, reader_server, reader_cancel).await;
        });

        // ── Spawn settle loop ────────────────────────────────────
        let settle_cancel = cancel.clone();
        let settle_server = Arc::clone(&server);
        let server_name = def.name.to_string();
        let settle_handle = tokio::spawn(async move {
            profile_loop(
                &mut tree_monitor,
                &settle_server,
                &server_name,
                SAMPLE_INTERVAL,
                &mut sink,
                settle_cancel,
            )
            .await;
        });

        // ── Let server begin indexing ────────────────────────────
        tokio::time::sleep(INIT_SETTLE).await;

        // ── Read file contents ───────────────────────────────────
        let bad_path = work_dir.path().join(def.bad_file);
        let bad_uri = format!("file://{}", bad_path.display());
        let bad_content = std::fs::read_to_string(&bad_path).unwrap_or_default();

        let clean_content = def.clean_file.map_or_else(
            || bad_content.clone(),
            |clean_file| {
                let clean_path = work_dir.path().join(clean_file);
                std::fs::read_to_string(&clean_path).unwrap_or_default()
            },
        );

        // ── didOpen with clean content ───────────────────────────
        {
            let mut guard = stdin.lock().await;
            send_did_open(&mut guard, &bad_uri, def.language_id, &clean_content).await?;
        }

        // ── Idle baseline ────────────────────────────────────────
        let idle_remaining = IDLE_DURATION.saturating_sub(INIT_SETTLE);
        println!("  idle baseline ({IDLE_DURATION:?})...");
        tokio::time::sleep(idle_remaining).await;

        // ── Stimulus: inject bad content + didSave ───────────────
        println!("  stimulus ({STIMULUS_DURATION:?})...");
        if def.clean_file.is_some() {
            // Write bad content to disk (for flycheck) and update buffer.
            std::fs::write(&bad_path, &bad_content)?;
            let mut guard = stdin.lock().await;
            send_did_change(&mut guard, &bad_uri, 2, &bad_content).await?;
            send_did_save(&mut guard, &bad_uri, &bad_content).await?;
        }
        // For servers without clean_file, the didOpen already opened
        // the bad content — server is already analyzing it.
        tokio::time::sleep(STIMULUS_DURATION).await;

        // ── Recovery: restore clean content + didSave ────────────
        println!("  recovery ({RECOVERY_DURATION:?})...");
        if def.clean_file.is_some() {
            std::fs::write(&bad_path, &clean_content)?;
            let mut guard = stdin.lock().await;
            send_did_change(&mut guard, &bad_uri, 3, &clean_content).await?;
            send_did_save(&mut guard, &bad_uri, &clean_content).await?;
        } else {
            let mut guard = stdin.lock().await;
            send_did_close(&mut guard, &bad_uri).await?;
        }
        tokio::time::sleep(RECOVERY_DURATION).await;

        // ── Teardown ─────────────────────────────────────────────
        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), settle_handle).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), reader_handle).await;

        shutdown_server(&stdin, &mut child).await;

        println!("  done.");
        any_server_ran = true;
    }

    if !any_server_ran {
        bail!("No LSP servers found — at least rust-analyzer should be installed");
    }

    // Print summary.
    print_summary(&out_path)?;

    // Print the log path so the user can inspect it manually.
    println!("Profile log: {}", out_path.display());

    Ok(())
}

// ── Large project test ───────────────────────────────────────────────

/// Longer durations for large-project profiling. rust-analyzer needs
/// 30-60s to index the full Catenary workspace, and flycheck on a
/// 3400-line file can take 10+ seconds.
const LARGE_IDLE: Duration = Duration::from_secs(45);
const LARGE_STIMULUS: Duration = Duration::from_secs(30);
const LARGE_RECOVERY: Duration = Duration::from_secs(30);

/// Type error appended to main.rs as stimulus.
const INJECTED_ERROR: &str = "\nfn _profile_stimulus_error() { let _: i32 = \"oops\"; }\n";

/// Drop guard that restores a file's original content on drop.
/// Ensures the working tree is clean even if the test panics.
struct FileGuard {
    path: PathBuf,
    original: String,
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.original);
    }
}

#[tokio::test]
#[ignore = "manual profiling test — runs rust-analyzer against full Catenary workspace"]
#[allow(
    clippy::too_many_lines,
    reason = "single-server profiling with setup/stimulus/teardown phases"
)]
async fn profile_intensity_large() -> Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_file = workspace.join("src/main.rs");

    if find_binary("rust-analyzer").is_none() {
        bail!("rust-analyzer not found");
    }

    // Read original content and set up restore guard.
    let original = std::fs::read_to_string(&target_file).context("Failed to read src/main.rs")?;
    let _guard = FileGuard {
        path: target_file.clone(),
        original: original.clone(),
    };

    let bad_content = format!("{original}{INJECTED_ERROR}");
    let file_uri = format!("file://{}", target_file.display());
    let root_uri = format!("file://{}", workspace.display());

    // Output log path.
    let tmp_dir_guard;
    let out_path = if let Ok(path) = std::env::var("PROFILE_OUT") {
        tmp_dir_guard = None;
        let p = PathBuf::from(&path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        p
    } else {
        let td = tempfile::tempdir()?;
        let p = td.path().join("intensity_large.jsonl");
        tmp_dir_guard = Some(td);
        p
    };
    let _ = &tmp_dir_guard;

    println!("PROFILING: rust-analyzer (large, Catenary workspace)");

    // Spawn rust-analyzer.
    let mut child = Command::new("rust-analyzer")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(&workspace)
        .spawn()
        .context("Failed to spawn rust-analyzer")?;

    let child_stdin = child.stdin.take().context("No stdin")?;
    let stdout = child.stdout.take().context("No stdout")?;
    let mut reader = BufReader::new(stdout);
    let stdin = Arc::new(tokio::sync::Mutex::new(child_stdin));

    // Initialize.
    let capabilities = {
        let mut guard = stdin.lock().await;
        tokio::time::timeout(
            Duration::from_mins(1),
            initialize_server(&mut guard, &mut reader, &root_uri),
        )
        .await
        .context("Timeout waiting for initialize")?
        .context("Initialize failed")?
    };

    let server = Arc::new(LspServer::new(
        "rust".to_string(),
        "rust-analyzer".to_string(),
        None,
    ));
    server.set_capabilities(capabilities);

    let pid = child.id().context("No PID after spawn")?;
    let mut tree_monitor =
        catenary_proc::TreeMonitor::new(pid).context("Could not create TreeMonitor")?;

    let mut sink = RecordingSink::new(&out_path)?;
    let cancel = CancellationToken::new();

    // Spawn reader task.
    let reader_cancel = cancel.clone();
    let reader_server = Arc::clone(&server);
    let reader_stdin = Arc::clone(&stdin);
    let reader_handle = tokio::spawn(async move {
        reader_loop(reader, reader_stdin, reader_server, reader_cancel).await;
    });

    // Spawn settle loop.
    let settle_cancel = cancel.clone();
    let settle_server = Arc::clone(&server);
    let settle_handle = tokio::spawn(async move {
        profile_loop(
            &mut tree_monitor,
            &settle_server,
            "rust-analyzer-large",
            SAMPLE_INTERVAL,
            &mut sink,
            settle_cancel,
        )
        .await;
    });

    // Let server begin indexing, then open the file.
    tokio::time::sleep(INIT_SETTLE).await;
    {
        let mut guard = stdin.lock().await;
        send_did_open(&mut guard, &file_uri, "rust", &original).await?;
    }

    // Idle baseline — wait for indexing to complete.
    let idle_remaining = LARGE_IDLE.saturating_sub(INIT_SETTLE);
    println!("  idle baseline ({LARGE_IDLE:?})...");
    tokio::time::sleep(idle_remaining).await;

    // Stimulus: inject type error + didSave.
    println!("  stimulus ({LARGE_STIMULUS:?})...");
    std::fs::write(&target_file, &bad_content)?;
    {
        let mut guard = stdin.lock().await;
        send_did_change(&mut guard, &file_uri, 2, &bad_content).await?;
        send_did_save(&mut guard, &file_uri, &bad_content).await?;
    }
    tokio::time::sleep(LARGE_STIMULUS).await;

    // Recovery: restore clean content + didSave.
    println!("  recovery ({LARGE_RECOVERY:?})...");
    std::fs::write(&target_file, &original)?;
    {
        let mut guard = stdin.lock().await;
        send_did_change(&mut guard, &file_uri, 3, &original).await?;
        send_did_save(&mut guard, &file_uri, &original).await?;
    }
    tokio::time::sleep(LARGE_RECOVERY).await;

    // Teardown.
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), settle_handle).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), reader_handle).await;
    shutdown_server(&stdin, &mut child).await;

    println!("  done.");

    print_summary(&out_path)?;
    println!("Profile log: {}", out_path.display());

    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
