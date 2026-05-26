// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Doctor command: check language server health and hook configuration.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::tty::IsTty;

use crate::cli::{ColorConfig, Output};
use crate::lsp;

/// Expected Claude Code hooks, embedded at compile time.
const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../../plugins/catenary/hooks/hooks.json");

/// Expected Gemini CLI hooks, embedded at compile time.
const GEMINI_HOOKS_EXPECTED: &str = include_str!("../../hooks/hooks.json");

/// Expected Antigravity CLI hooks, embedded at compile time.
const ANTIGRAVITY_HOOKS_EXPECTED: &str =
    include_str!("../../plugins/catenary-antigravity/hooks.json");

/// Expected Claude Code SKILL.md, embedded at compile time.
const SKILL_MD_EXPECTED: &str = include_str!("../../plugins/catenary/skills/catenary/SKILL.md");

/// Expected Gemini CLI context file, embedded at compile time.
const GEMINI_CONTEXT_EXPECTED: &str = include_str!("../../gemini-context.md");

/// Expected Antigravity rules file, embedded at compile time.
const ANTIGRAVITY_RULES_EXPECTED: &str =
    include_str!("../../plugins/catenary-antigravity/rules/catenary.md");

/// Migration guidance for users who still have the legacy Python script configured.
const CONSTRAINED_BASH_MIGRATION: &str = "Command filtering is now built into `catenary hook pre-tool`. \
     Remove the constrained_bash.py hook from your settings and use \
     `[commands]` in your Catenary config instead. \
     Run `catenary config` to generate a recommended template.";

/// Default per-server timeout for the initialize probe (5 minutes).
///
/// Julia's `LanguageServer.jl` compiles on first run and can take minutes
/// without a precompiled sysimage. 5 minutes is generous enough to avoid
/// false negatives for legitimately slow servers.
///
/// Override with `CATENARY_DOCTOR_TIMEOUT_SECS` for testing.
fn probe_timeout() -> Duration {
    std::env::var("CATENARY_DOCTOR_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or_else(|| Duration::from_mins(5), Duration::from_secs)
}

/// Threshold after which a still-pending server gets a slow-startup hint.
const SLOW_HINT_DELAY: Duration = Duration::from_secs(5);

/// Result of probing a single server.
struct ServerProbeResult {
    /// Server name (sorted key).
    name: String,
    /// Status line to display (without the name prefix).
    status: ProbeStatus,
    /// Extracted capabilities (empty on failure).
    capabilities: Vec<&'static str>,
    /// `file_patterns` from the server definition (for the status suffix).
    file_patterns: Vec<String>,
}

/// Outcome of a server probe.
enum ProbeStatus {
    /// Server initialized successfully.
    Ready,
    /// Binary not found on `$PATH`.
    BinaryNotFound(String),
    /// Process spawn failed.
    SpawnFailed(String),
    /// Initialize request failed.
    InitializeFailed(String),
    /// Initialize timed out after [`probe_timeout()`].
    TimedOut,
}

impl ServerProbeResult {
    /// Format the status line (everything after the name column).
    fn format_status(&self, colors: &ColorConfig) -> String {
        match &self.status {
            ProbeStatus::Ready => {
                let status = if self.file_patterns.is_empty() {
                    "✓ ready".to_string()
                } else {
                    format!(
                        "✓ ready  file_patterns: [{}]",
                        self.file_patterns
                            .iter()
                            .map(|p| format!("\"{p}\""))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                };
                colors.green(&status)
            }
            ProbeStatus::BinaryNotFound(cmd) => colors.red(&format!("✗ {cmd}: command not found")),
            ProbeStatus::SpawnFailed(e) => colors.red(&format!("✗ spawn failed: {e}")),
            ProbeStatus::InitializeFailed(e) => colors.red(&format!("✗ initialize failed: {e}")),
            ProbeStatus::TimedOut => colors.red("✗ initialize timed out"),
        }
    }
}

/// Polling interval for the work-gate monitor (matches `settle.rs`).
const WORK_GATE_POLL: Duration = Duration::from_millis(50);

/// Probe a single server: binary check → spawn → initialize → capabilities → shutdown.
///
/// If `work_started_tx` is `Some`, spawns a work-gate monitor that polls
/// the server's process tree and sends the server name on the channel
/// once cumulative CPU ticks advance from the pre-initialize baseline.
/// This lets the caller defer slow-startup hints until the server has
/// actually been scheduled CPU time, avoiding false hints under contention.
async fn probe_server(
    name: String,
    command: String,
    args: Vec<String>,
    initialization_options: Option<serde_json::Value>,
    env: Option<HashMap<String, String>>,
    file_patterns: Vec<String>,
    work_started_tx: Option<tokio::sync::mpsc::Sender<String>>,
) -> ServerProbeResult {
    if !binary_exists(&command) {
        return ServerProbeResult {
            name,
            status: ProbeStatus::BinaryNotFound(command),
            capabilities: Vec::new(),
            file_patterns,
        };
    }

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let spawn_result = lsp::LspClient::spawn_quiet(
        &command,
        &args_refs,
        &name,
        &name,
        crate::logging::LoggingServer::new(),
        env.as_ref(),
    );

    let mut client = match spawn_result {
        Ok(client) => client,
        Err(e) => {
            return ServerProbeResult {
                name,
                status: ProbeStatus::SpawnFailed(e.to_string()),
                capabilities: Vec::new(),
                file_patterns,
            };
        }
    };

    // Spawn work-gate monitor: detect when the server actually gets CPU time.
    let gate_cancel = tokio_util::sync::CancellationToken::new();
    if let Some(tx) = work_started_tx {
        let server = std::sync::Arc::clone(client.server());
        let baseline_ticks = server.sample_tree().map_or(0, |s| s.cumulative_ticks);
        let gate_name = name.clone();
        let cancel = gate_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(WORK_GATE_POLL) => {}
                    () = cancel.cancelled() => return,
                }
                let advanced = server
                    .sample_tree()
                    .is_some_and(|s| s.cumulative_ticks > baseline_ticks);
                if advanced {
                    let _ = tx.send(gate_name).await;
                    return;
                }
            }
        });
    }

    let init_result = tokio::time::timeout(
        probe_timeout(),
        client.initialize(&[], initialization_options),
    )
    .await;

    gate_cancel.cancel();

    match init_result {
        Ok(Ok(result)) => {
            let tools =
                extract_capabilities(&result["capabilities"], client.supports_type_hierarchy());
            let _ = client.shutdown().await;
            ServerProbeResult {
                name,
                status: ProbeStatus::Ready,
                capabilities: tools,
                file_patterns,
            }
        }
        Ok(Err(e)) => {
            let _ = client.shutdown().await;
            ServerProbeResult {
                name,
                status: ProbeStatus::InitializeFailed(e.to_string()),
                capabilities: Vec::new(),
                file_patterns,
            }
        }
        Err(_) => {
            let _ = client.shutdown().await;
            ServerProbeResult {
                name,
                status: ProbeStatus::TimedOut,
                capabilities: Vec::new(),
                file_patterns,
            }
        }
    }
}

/// Run the doctor command: check all configured language servers.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded.
#[allow(
    clippy::too_many_lines,
    reason = "Doctor command has sequential output logic"
)]
pub async fn run_doctor(out: &mut Output, project_root: &Path, show_diff: bool) -> Result<()> {
    // Print version header
    let _ = out.writeln(format_args!("Catenary {}", env!("CATENARY_VERSION")));
    let _ = out.writeln(format_args!(""));

    // Check config sources for old-format entries before loading
    doctor_check_config(out);

    // Load configuration — report errors inline instead of bailing
    let config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            let _ = out.writeln(format_args!(
                "{}",
                out.colors.red(&format!("✗ Config error: {e:#}"))
            ));
            let _ = out.writeln(format_args!(""));
            return Ok(());
        }
    };

    // Print config header
    let config_source = std::env::var("CATENARY_CONFIG")
        .ok()
        .unwrap_or_else(|| "default paths".to_string());
    let _ = out.writeln(format_args!(
        "{} {}",
        out.colors.bold("Config:"),
        config_source
    ));
    let _ = out.writeln(format_args!(""));

    // Validation errors
    let validation_errors = config.validate();
    for err in &validation_errors {
        let _ = out.writeln(format_args!("{}", out.colors.red(&format!("✗  {err}"))));
    }

    // Unreferenced server warnings
    let referenced: HashSet<&str> = config
        .language
        .values()
        .flat_map(|lc| lc.servers().iter().map(|b| b.name.as_str()))
        .collect();
    let mut unreferenced: Vec<&str> = config
        .server
        .keys()
        .filter(|name| !referenced.contains(name.as_str()))
        .map(String::as_str)
        .collect();
    unreferenced.sort_unstable();
    for name in &unreferenced {
        let _ = out.writeln(format_args!(
            "{}",
            out.colors.yellow(&format!(
                "⚠  Server '{name}' is defined but not referenced by any [language.*] entry"
            )),
        ));
    }

    // Duplicate extension warnings
    let dup_exts =
        crate::bridge::filesystem_manager::ClassificationTables::find_duplicate_extensions(&config);
    for (ext, first, second) in &dup_exts {
        let _ = out.writeln(format_args!(
            "{}",
            out.colors.yellow(&format!(
                "⚠  Extension '.{ext}' claimed by both [language.{first}] and \
                 [language.{second}] — first wins"
            )),
        ));
    }

    if !validation_errors.is_empty() || !unreferenced.is_empty() || !dup_exts.is_empty() {
        let _ = out.writeln(format_args!(""));
    }

    // ── Project config section ──────────────────────────────────────
    doctor_check_project_config(out, project_root, &config);

    if config.language.is_empty() && config.server.is_empty() {
        let _ = out.writeln(format_args!("No language servers configured."));
        return Ok(());
    }

    // ── Servers section ──────────────────────────────────────────────
    // Spawn all server probes concurrently, updating lines in-place.
    let mut server_names: Vec<String> = config.server.keys().cloned().collect();
    server_names.sort_unstable();

    let max_server_width = server_names.iter().map(String::len).max().unwrap_or(10);

    let is_tty = std::io::stdout().is_tty();

    let _ = out.writeln(format_args!("{}:", out.colors.bold("Servers")));

    // Build index: server name → line offset (distance from bottom).
    // Binary-not-found servers are printed immediately and excluded from
    // the pending set.
    let mut pending_names: Vec<String> = Vec::new();
    let mut immediate_results: Vec<ServerProbeResult> = Vec::new();

    for name in &server_names {
        let server_def = &config.server[name.as_str()];
        if binary_exists(&server_def.command) {
            pending_names.push(name.clone());
        } else {
            immediate_results.push(ServerProbeResult {
                name: name.clone(),
                status: ProbeStatus::BinaryNotFound(server_def.command.clone()),
                capabilities: Vec::new(),
                file_patterns: server_def.file_patterns.clone(),
            });
        }
    }

    // Print binary-not-found results immediately
    for result in &immediate_results {
        let name_display = format!("  {:<max_server_width$}", result.name);
        let _ = out.writeln(format_args!(
            "{name_display}  {}",
            result.format_status(&out.colors)
        ));
    }

    // Print pending lines and spawn concurrent probes
    if is_tty {
        // Print all pending lines with ⏳ status
        for name in &pending_names {
            let name_display = format!("  {name:<max_server_width$}");
            let _ = out.writeln(format_args!("{name_display}  ⏳ checking..."));
        }
    }

    // Spawn probes into a JoinSet.
    // TTY mode gets a work-gate channel so hint timers start only after
    // the server has actually consumed CPU time.
    let pending_count = pending_names.len();
    let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<String>(pending_count.max(1));

    let mut join_set = tokio::task::JoinSet::new();
    for name in &pending_names {
        let server_def = &config.server[name.as_str()];
        let tx = if is_tty { Some(work_tx.clone()) } else { None };
        join_set.spawn(probe_server(
            name.clone(),
            server_def.command.clone(),
            server_def.args.clone(),
            server_def.initialization_options.clone(),
            server_def.env.clone(),
            server_def.file_patterns.clone(),
            tx,
        ));
    }
    drop(work_tx); // Drop the original sender

    // Collect results, updating lines in-place (TTY) or batching (piped).
    let mut completed: HashMap<String, ServerProbeResult> = HashMap::new();
    let mut slow_hinted: HashSet<String> = HashSet::new();

    if is_tty && pending_count > 0 {
        // Per-server hint deadlines, started when the work gate fires.
        let mut hint_deadlines: HashMap<String, tokio::time::Instant> = HashMap::new();

        while completed.len() < pending_count {
            // Find the earliest pending hint deadline.
            let next_deadline = hint_deadlines
                .iter()
                .filter(|(n, _)| !completed.contains_key(*n) && !slow_hinted.contains(*n))
                .map(|(_, &d)| d)
                .min();

            tokio::select! {
                Some(join_result) = join_set.join_next() => {
                    if let Ok(result) = join_result {
                        update_server_line(
                            out,
                            &result,
                            &pending_names,
                            pending_count,
                            max_server_width,
                        );
                        completed.insert(result.name.clone(), result);
                    }
                }
                Some(name) = work_rx.recv() => {
                    // Work gate fired — server has consumed CPU time.
                    // Start the per-server hint timer from now.
                    hint_deadlines.insert(
                        name,
                        tokio::time::Instant::now() + SLOW_HINT_DELAY,
                    );
                }
                () = async {
                    match next_deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Fire hints for all servers past their deadline.
                    let now = tokio::time::Instant::now();
                    for (name, deadline) in &hint_deadlines {
                        if *deadline <= now
                            && !completed.contains_key(name)
                            && slow_hinted.insert(name.clone())
                        {
                            update_server_line_raw(
                                out,
                                name,
                                "⏳ checking... (slow — see your language server's docs)",
                                &pending_names,
                                pending_count,
                                max_server_width,
                            );
                        }
                    }
                }
            }
        }
    } else {
        // Non-TTY: collect all results, then print sequentially.
        while let Some(join_result) = join_set.join_next().await {
            if let Ok(result) = join_result {
                completed.insert(result.name.clone(), result);
            }
        }
        // Print in sorted order
        for name in &pending_names {
            if let Some(result) = completed.get(name) {
                let name_display = format!("  {name:<max_server_width$}");
                let _ = out.writeln(format_args!(
                    "{name_display}  {}",
                    result.format_status(&out.colors)
                ));
            }
        }
    }

    // Build the capabilities map from all results
    let mut server_capabilities: HashMap<&str, Vec<&'static str>> = HashMap::new();
    for result in immediate_results.iter().chain(completed.values()) {
        if !result.capabilities.is_empty() {
            // Borrow the name from server_names (which lives long enough)
            if let Some(name_ref) = server_names.iter().find(|n| **n == result.name) {
                server_capabilities.insert(name_ref.as_str(), result.capabilities.clone());
            }
        }
    }

    // ── Languages section ────────────────────────────────────────────
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Languages")));

    // Build sorted list of (language, server_name) pairs
    let mut lang_entries: Vec<(&str, &str)> = Vec::new();
    for (lang, lc) in &config.language {
        if let Some(binding) = lc.servers().first() {
            lang_entries.push((lang.as_str(), binding.name.as_str()));
        }
    }
    lang_entries.sort_by_key(|(lang, _)| *lang);

    let max_lang_width = lang_entries
        .iter()
        .map(|(l, _)| l.len())
        .max()
        .unwrap_or(10);

    for (lang, target) in &lang_entries {
        let lang_display = format!("  {lang:<max_lang_width$}");
        let _ = out.writeln(format_args!("{lang_display}  → {target}"));
        // Show capabilities from the server, indented
        if let Some(tools) = server_capabilities.get(target)
            && !tools.is_empty()
        {
            let _ = out.writeln(format_args!(
                "{}    {}",
                " ".repeat(max_lang_width + 2),
                out.colors.dim(&tools.join(" ")),
            ));
        }
    }

    // Hooks health section
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Hooks")));
    check_claude_hooks(out, show_diff);
    check_gemini_hooks(out, show_diff);
    check_antigravity_hooks(out, show_diff, project_root);
    check_path_binary(out);

    // Agent instructions section
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Agent instructions")));
    check_claude_instructions(out, show_diff);
    check_gemini_instructions(out, show_diff);
    check_antigravity_instructions(out, show_diff, project_root);

    // Legacy script migration warnings
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Command filter")));
    check_constrained_bash_claude(out);
    check_constrained_bash_gemini(out);
    check_command_filter_config(out, &config);

    // Actionable suggestions at the very bottom so they aren't buried
    let suggestions = collect_suggestions(&config, dirs::config_dir());
    if !suggestions.is_empty() {
        let _ = out.writeln(format_args!(""));
        let _ = out.writeln(format_args!("{}:", out.colors.bold("Suggestions")));
        for suggestion in &suggestions {
            let _ = out.writeln(format_args!("  {}", out.colors.dim(suggestion)));
        }
    }

    Ok(())
}

/// Maximum number of stderr lines to capture in verbose doctor mode.
const STDERR_MAX_LINES: usize = 50;

/// Run the doctor command for a single server with verbose output.
///
/// Probes the named server and prints detailed diagnostic information:
/// resolved command, binary check, stderr capture, initialize exchange,
/// capabilities summary, and exit status.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded.
#[allow(
    clippy::too_many_lines,
    reason = "Verbose doctor has sequential output sections"
)]
pub async fn run_doctor_single(
    out: &mut Output,
    server_name: &str,
    project_root: &Path,
) -> Result<()> {
    let _ = out.writeln(format_args!("Catenary {}", env!("CATENARY_VERSION")));
    let _ = out.writeln(format_args!(""));

    let config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            let _ = out.writeln(format_args!(
                "{}",
                out.colors.red(&format!("✗ Config error: {e:#}"))
            ));
            return Ok(());
        }
    };

    // Merge project config if present
    let merged_config = match crate::config::load_project_config(
        &project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf()),
    ) {
        Ok(Some(pc)) => {
            let mut merged = config.clone();
            for (k, v) in pc.server {
                merged.server.entry(k).or_insert(v);
            }
            merged
        }
        _ => config,
    };

    // Look up server
    let Some(server_def) = merged_config.server.get(server_name) else {
        let _ = out.writeln(format_args!(
            "{}\n",
            out.colors
                .red(&format!("✗ Unknown server: '{server_name}'")),
        ));
        let mut available: Vec<&str> = merged_config.server.keys().map(String::as_str).collect();
        available.sort_unstable();
        let _ = out.writeln(format_args!("Configured servers:"));
        for name in &available {
            let _ = out.writeln(format_args!("  {name}"));
        }
        return Ok(());
    };

    // ── 1. Resolved command ─────────────────────────────────────────
    let command = server_def.command.as_str();
    let args_display = if server_def.args.is_empty() {
        String::new()
    } else {
        format!(" {}", server_def.args.join(" "))
    };
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Command")));
    let _ = out.writeln(format_args!("  {command}{args_display}"));
    let _ = out.writeln(format_args!(""));

    // ── 1b. Root markers ────────────────────────────────────────────
    // Find languages that bind to this server and show their markers.
    let mut shown_markers = false;
    for (lang_name, lang_config) in &merged_config.language {
        if lang_config.servers().iter().any(|b| b.name == server_name)
            && let Some(markers) = lang_config.active_markers()
        {
            if !shown_markers {
                let _ = out.writeln(format_args!("{}:", out.colors.bold("Root markers")));
                shown_markers = true;
            }
            let _ = out.writeln(format_args!("  {lang_name}: {}", markers.join(", ")));
        }
    }
    if shown_markers {
        let _ = out.writeln(format_args!(""));
    }

    // ── 2. Binary check ────────────────────────────────────────────
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Binary")));
    if let Some(path) = resolve_binary(command) {
        let _ = out.writeln(format_args!(
            "  {} {}",
            out.colors.green("✓"),
            path.display()
        ));
    } else {
        let _ = out.writeln(format_args!(
            "  {}",
            out.colors.red(&format!("✗ {command}: command not found")),
        ));
        return Ok(());
    }
    let _ = out.writeln(format_args!(""));

    // ── 3. Spawn ──────────────────────────────────────────────────
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Spawn")));
    let args_refs: Vec<&str> = server_def.args.iter().map(String::as_str).collect();
    let spawn_result = lsp::LspClient::spawn_for_doctor(
        command,
        &args_refs,
        server_name,
        server_name,
        crate::logging::LoggingServer::new(),
        server_def.env.as_ref(),
    );

    let (mut client, child_stderr) = match spawn_result {
        Ok(pair) => {
            let _ = out.writeln(format_args!("  {} process started", out.colors.green("✓")));
            pair
        }
        Err(e) => {
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors.red(&format!("✗ spawn failed: {e}"))
            ));
            return Ok(());
        }
    };

    // Start stderr reader task
    let stderr_task = child_stderr.map(|stderr| {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            let mut output = Vec::new();
            while output.len() < STDERR_MAX_LINES {
                match lines.next_line().await {
                    Ok(Some(line)) => output.push(line),
                    Ok(None) | Err(_) => break,
                }
            }
            output
        })
    });

    let _ = out.writeln(format_args!(""));

    // ── 4. Initialize exchange ──────────────────────────────────────
    let resolved_roots: Vec<PathBuf> = project_root
        .canonicalize()
        .map(|r| vec![r])
        .unwrap_or_default();

    // Build init params for display
    let workspace_folders: Vec<(String, String)> = resolved_roots
        .iter()
        .map(|root| {
            let uri = format!("file://{}", root.display());
            let name = root.file_name().map_or_else(
                || "workspace".to_string(),
                |s| s.to_string_lossy().to_string(),
            );
            (uri, name)
        })
        .collect();
    let folder_refs: Vec<(&str, &str)> = workspace_folders
        .iter()
        .map(|(uri, name)| (uri.as_str(), name.as_str()))
        .collect();
    let init_params = lsp::params::initialize(
        std::process::id(),
        &folder_refs,
        server_def.initialization_options.as_ref(),
    );

    let _ = out.writeln(format_args!("{}:", out.colors.bold("Initialize request")));
    if let Ok(pretty) = serde_json::to_string_pretty(&init_params) {
        for line in pretty.lines() {
            let _ = out.writeln(format_args!("  {line}"));
        }
    }
    let _ = out.writeln(format_args!(""));

    let _ = out.writeln(format_args!("{}:", out.colors.bold("Initialize response")));
    match client
        .initialize(&resolved_roots, server_def.initialization_options.clone())
        .await
    {
        Ok(result) => {
            if let Ok(pretty) = serde_json::to_string_pretty(&result) {
                for line in pretty.lines() {
                    let _ = out.writeln(format_args!("  {line}"));
                }
            }
            let _ = out.writeln(format_args!(""));

            // ── 5. Capabilities summary ─────────────────────────────
            let tools =
                extract_capabilities(&result["capabilities"], client.supports_type_hierarchy());
            let _ = out.writeln(format_args!("{}:", out.colors.bold("Capabilities")));
            if tools.is_empty() {
                let _ = out.writeln(format_args!("  {}", out.colors.dim("(none)")));
            } else {
                for tool in &tools {
                    let _ = out.writeln(format_args!("  {} {tool}", out.colors.green("✓")));
                }
            }
        }
        Err(e) => {
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors.red(&format!("✗ initialize failed: {e}"))
            ));
        }
    }

    // ── 6. Shutdown ────────────────────────────────────────────────
    let _ = client.shutdown().await;
    let _ = out.writeln(format_args!(""));

    // ── 7. Server stderr ───────────────────────────────────────────
    if let Some(task) = stderr_task {
        // Give the task a moment to finish collecting output
        let lines = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();

        if !lines.is_empty() {
            let _ = out.writeln(format_args!("{}:", out.colors.bold("Server stderr")));
            for line in &lines {
                let _ = out.writeln(format_args!("  {line}"));
            }
            if lines.len() >= STDERR_MAX_LINES {
                let _ = out.writeln(format_args!(
                    "  {}",
                    out.colors
                        .dim(&format!("(truncated at {STDERR_MAX_LINES} lines)"))
                ));
            }
            let _ = out.writeln(format_args!(""));
        }
    }

    Ok(())
}

/// Check config source files for old-format entries and print migration guidance.
///
/// Reads each config file as raw TOML (independent of `Config::load`) to detect:
/// - `[server.*]` with no `[language.*]` (old deprecated format)
/// - `[language.*]` entries containing `command`/`args` etc. (intermediate format)
///
/// Prints the equivalent new-format config for each detected old entry.
fn doctor_check_config(out: &mut Output) {
    let sources = crate::config::config_sources();
    let mut found_issues = false;

    for source in &sources {
        let Ok(contents) = std::fs::read_to_string(source) else {
            continue;
        };
        let Ok(raw) = contents.parse::<toml::Value>() else {
            continue;
        };

        let has_server = raw.get("server").is_some();
        let has_language = raw.get("language").is_some();

        // Old deprecated format: [server.*] with command fields and no [language.*]
        if has_server
            && !has_language
            && let Some(table) = raw.get("server").and_then(toml::Value::as_table)
        {
            for (key, entry) in table {
                if let Some(entry_table) = entry.as_table()
                    && entry_table.contains_key("command")
                {
                    found_issues = true;
                    print_migration(out, source, key, entry_table, true);
                }
            }
        }

        // [language.*] entries with removed or stale fields
        if let Some(table) = raw.get("language").and_then(toml::Value::as_table) {
            for (key, entry) in table {
                if let Some(entry_table) = entry.as_table() {
                    // Removed field: inherit
                    if entry_table.contains_key("inherit") {
                        found_issues = true;
                        let target = entry_table
                            .get("inherit")
                            .and_then(toml::Value::as_str)
                            .unwrap_or("?");
                        let _ = out.writeln(format_args!(
                            "{}",
                            out.colors.yellow(&format!(
                                "⚠  {}: [language.{key}] uses removed `inherit` field — \
                                 copy `servers` list from [language.{target}] into \
                                 [language.{key}] instead.",
                                source.display(),
                            )),
                        ));
                    }

                    // Intermediate format: inline server definition fields
                    let has_server_fields = crate::config::SERVER_DEF_KEYS
                        .iter()
                        .any(|k| entry_table.contains_key(*k));
                    if has_server_fields {
                        found_issues = true;
                        print_migration(out, source, key, entry_table, false);
                    }
                }
            }
        }

        // [commands] entries with old denylist-format fields
        if let Some(cmd_table) = raw.get("commands").and_then(toml::Value::as_table) {
            if cmd_table.contains_key("deny_when_first") {
                found_issues = true;
                let _ = out.writeln(format_args!(
                    "{}",
                    out.colors.yellow(&format!(
                        "⚠  {}: [commands] uses removed `deny_when_first` field — \
                         Catenary now uses an allowlist model. \
                         Run `catenary config` for the recommended template.",
                        source.display(),
                    )),
                ));
            }

            if let Some(deny_table) = cmd_table.get("deny").and_then(toml::Value::as_table) {
                for (key, value) in deny_table {
                    if value.is_str() {
                        found_issues = true;
                        let _ = out.writeln(format_args!(
                            "{}",
                            out.colors.yellow(&format!(
                                "⚠  {}: [commands.deny.{key}] has a string value — \
                                 the old guidance-string format is removed. `deny` now \
                                 maps commands to arrays of denied subcommands \
                                 (e.g., `git = [\"grep\", \"ls-files\"]`).",
                                source.display(),
                            )),
                        ));
                        break; // One message per file is enough.
                    }
                }
            }
        }
    }

    if found_issues {
        let _ = out.writeln(format_args!(""));
    }
}

/// Check a project root for `.catenary.toml` and validate its contents.
///
/// Reports unsupported sections, parse errors, and orphan server definitions.
/// Called with `--root` (defaults to cwd).
fn doctor_check_project_config(
    out: &mut Output,
    project_root: &Path,
    user_config: &crate::config::Config,
) {
    let Ok(resolved) = project_root.canonicalize() else {
        return;
    };

    let config_path = resolved.join(".catenary.toml");
    if !config_path.exists() {
        return;
    }

    let _ = out.writeln(format_args!(
        "{} {}",
        out.colors.bold("Project config:"),
        config_path.display(),
    ));

    // Flag deprecated `enabled` key before parsing.
    let has_deprecated_enabled = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|c| c.parse::<toml::Value>().ok())
        .and_then(|raw| raw.get("enabled").map(|_| ()))
        .is_some();

    if has_deprecated_enabled {
        let _ = out.writeln(format_args!(
            "  {}",
            out.colors
                .yellow("⚠  `enabled` is deprecated — rename it to `lsp`"),
        ));
    }

    match crate::config::load_project_config(&resolved) {
        Ok(Some(pc)) => {
            // Count entries
            let lang_count = pc.language.len();
            let server_count = pc.server.len();
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors.green(&format!(
                    "✓ {lang_count} language{}, {server_count} server{}",
                    if lang_count == 1 { "" } else { "s" },
                    if server_count == 1 { "" } else { "s" },
                )),
            ));

            // Orphan server warnings
            for (server_name, server_def) in &pc.server {
                if server_def.command.is_empty() {
                    continue;
                }

                let referenced_by_project = pc
                    .language
                    .values()
                    .any(|lc| lc.servers().iter().any(|b| b.name == *server_name));

                let referenced_by_user = user_config
                    .language
                    .values()
                    .any(|lc| lc.servers().iter().any(|b| b.name == *server_name));

                if !referenced_by_project && !referenced_by_user {
                    let _ = out.writeln(format_args!(
                        "  {}",
                        out.colors.yellow(&format!(
                            "⚠  [server.{server_name}] has a `command` but no \
                             [language.*] references it"
                        )),
                    ));
                }
            }

            // Server ref validation — project language refs must resolve
            // against the combined (user + project) server set.
            for (lang_key, lang_config) in &pc.language {
                for binding in lang_config.servers() {
                    if !pc.server.contains_key(&binding.name)
                        && !user_config.server.contains_key(&binding.name)
                    {
                        let _ = out.writeln(format_args!(
                            "  {}",
                            out.colors.red(&format!(
                                "✗  [language.{lang_key}] references server '{}', \
                                 but no [server.{}] is defined in project or user config",
                                binding.name, binding.name,
                            )),
                        ));
                    }
                }
            }
        }
        Ok(None) => {} // No project config — already handled by the exists check above.
        Err(e) => {
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors
                    .red(&format!("✗ {}: {e:#}", config_path.display())),
            ));
        }
    }

    let _ = out.writeln(format_args!(""));
}

/// Print migration guidance for a single old-format entry.
fn print_migration(
    out: &mut Output,
    source: &Path,
    key: &str,
    entry: &toml::map::Map<String, toml::Value>,
    is_server_section: bool,
) {
    let section = if is_server_section {
        "server"
    } else {
        "language"
    };
    let _ = out.writeln(format_args!(
        "{}",
        out.colors.yellow(&format!(
            "⚠  {}: [{section}.{key}] uses old format — migrate to [language.*] + [server.*]:",
            source.display(),
        )),
    ));

    // Determine server name from command, falling back to the key
    let server_name = entry
        .get("command")
        .and_then(toml::Value::as_str)
        .unwrap_or(key);

    // Build old-format display
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("  Old:"));
    let _ = out.writeln(format_args!("    [{section}.{key}]"));
    for (k, v) in entry {
        let _ = out.writeln(format_args!("    {k} = {v}"));
    }

    // Build new-format display
    let server_fields: Vec<(&str, &toml::Value)> = crate::config::SERVER_DEF_KEYS
        .iter()
        .filter_map(|k| entry.get(*k).map(|v| (*k, v)))
        .collect();
    let lang_fields: Vec<(&str, &toml::Value)> = entry
        .iter()
        .filter(|(k, _)| !crate::config::SERVER_DEF_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("  New:"));
    let _ = out.writeln(format_args!("    [language.{key}]"));
    let _ = out.writeln(format_args!("    servers = [\"{server_name}\"]"));
    for (k, v) in &lang_fields {
        let _ = out.writeln(format_args!("    {k} = {v}"));
    }
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("    [server.{server_name}]"));
    for (k, v) in &server_fields {
        let _ = out.writeln(format_args!("    {k} = {v}"));
    }
    let _ = out.writeln(format_args!(""));
}

/// Return the user config file path if it exists on disk.
///
/// Uses `config_base` as the parent directory (e.g. `~/.config`).
/// Returns `None` when the base is unknown or the file doesn't exist.
fn user_config_path_in(config_base: Option<PathBuf>) -> Option<PathBuf> {
    let path = config_base?.join("catenary").join("config.toml");
    if path.exists() { Some(path) } else { None }
}

/// Collect actionable suggestions based on current config state.
///
/// `config_base` is the platform config directory (from `dirs::config_dir()`).
fn collect_suggestions(
    config: &crate::config::Config,
    config_base: Option<PathBuf>,
) -> Vec<String> {
    let mut suggestions = Vec::new();

    if user_config_path_in(config_base.clone()).is_none() {
        let target = config_base
            .map(|d| d.join("catenary").join("config.toml"))
            .map_or_else(
                || "~/.config/catenary/config.toml".to_string(),
                |p| p.display().to_string(),
            );
        suggestions.push(format!(
            "No config file found. Run `catenary config > {target}` \
             to generate a recommended starting config.",
        ));
    } else if config.resolved_commands.is_none() {
        suggestions.push(
            "No [commands] section in config — all shell commands allowed. \
             Run `catenary config` to see a recommended template."
                .to_string(),
        );
    }

    suggestions
}

/// Update a server's status line in-place using crossterm cursor movement.
///
/// Moves the cursor up to the target line, clears it, prints the new status,
/// and moves back down. Only called when stdout is a TTY.
fn update_server_line(
    out: &mut Output,
    result: &ServerProbeResult,
    pending_names: &[String],
    pending_count: usize,
    max_server_width: usize,
) {
    let status = result.format_status(&out.colors);
    overwrite_line(
        out,
        &result.name,
        &status,
        pending_names,
        pending_count,
        max_server_width,
    );
}

/// Update a server's status line with a raw string (no `ServerProbeResult`).
///
/// Used for the slow-startup hint update.
fn update_server_line_raw(
    out: &mut Output,
    name: &str,
    status: &str,
    pending_names: &[String],
    pending_count: usize,
    max_server_width: usize,
) {
    overwrite_line(
        out,
        name,
        status,
        pending_names,
        pending_count,
        max_server_width,
    );
}

/// Overwrite a server's line in-place via crossterm cursor movement.
///
/// `pending_names` defines the line order; `pending_count` is the total
/// number of pending lines. The cursor is assumed to sit on the line
/// immediately after the last pending line.
#[allow(
    clippy::cast_possible_truncation,
    reason = "server count will never exceed u16::MAX"
)]
fn overwrite_line(
    out: &mut Output,
    name: &str,
    status: &str,
    pending_names: &[String],
    pending_count: usize,
    max_server_width: usize,
) {
    let Some(idx) = pending_names.iter().position(|n| n == name) else {
        return;
    };
    // Lines are printed top-to-bottom, cursor is after the last line.
    // Line at index `idx` is `pending_count - 1 - idx` lines above the cursor.
    let lines_up = (pending_count - 1 - idx) as u16;
    let name_display = format!("  {name:<max_server_width$}");

    if lines_up > 0 {
        let _ = crossterm::execute!(out, crossterm::cursor::MoveUp(lines_up));
    }
    let _ = crossterm::execute!(
        out,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine),
    );
    // \r to return to column 0 after Clear
    let _ = write!(out, "\r{name_display}  {status}");
    if lines_up > 0 {
        let _ = crossterm::execute!(out, crossterm::cursor::MoveDown(lines_up));
    }
    // Return to column 0 on the bottom line
    let _ = write!(out, "\r");
    let _ = out.flush();
}

/// Checks whether a binary can be found on `$PATH`.
fn binary_exists(command: &str) -> bool {
    resolve_binary(command).is_some()
}

/// Resolves a binary command to its full path on `$PATH`.
///
/// Returns `None` if the binary cannot be found.
fn resolve_binary(command: &str) -> Option<PathBuf> {
    // If the command contains a path separator, check it directly
    if command.contains('/') {
        let p = PathBuf::from(command);
        return if p.exists() { Some(p) } else { None };
    }

    // Search PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(command))
        .find(|p| p.is_file())
}

/// Extracts Catenary tool names from LSP server capabilities.
fn extract_capabilities(caps: &serde_json::Value, type_hierarchy: bool) -> Vec<&'static str> {
    let has = |key: &str| caps.get(key).is_some_and(|v| !v.is_null());

    let mut tools = Vec::new();

    if has("hoverProvider") {
        tools.push("hover");
    }
    if has("definitionProvider") {
        tools.push("definition");
    }
    if has("typeDefinitionProvider") {
        tools.push("type_definition");
    }
    if has("implementationProvider") {
        tools.push("implementation");
    }
    if has("referencesProvider") {
        tools.push("references");
    }
    if has("documentSymbolProvider") {
        tools.push("document_symbols");
    }
    if has("workspaceSymbolProvider") {
        tools.push("search");
    }
    if has("codeActionProvider") {
        tools.push("code_actions");
    }
    if has("callHierarchyProvider") {
        tools.push("call_hierarchy");
    }
    if type_hierarchy {
        tools.push("type_hierarchy");
    }

    tools
}

/// Check Claude Code plugin hooks against the embedded expected hooks.
fn check_claude_hooks(out: &mut Output, show_diff: bool) {
    let label = format!("{:<14}", "Claude Code");
    let Ok(home_str) = std::env::var("HOME") else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- cannot determine home directory"),
        ));
        return;
    };
    let home = PathBuf::from(home_str);

    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(plugins_json) = std::fs::read_to_string(&plugins_file) else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- not installed")
        ));
        return;
    };

    let Ok(plugins) = serde_json::from_str::<serde_json::Value>(&plugins_json) else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.yellow("? cannot parse installed_plugins.json"),
        ));
        return;
    };

    // Look up catenary@catenary in plugins.plugins
    let entries = match plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
    {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.dim("- not installed")
            ));
            return;
        }
    };

    // Use the first (most recent) entry
    let entry = &entries[0];
    let version = entry
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let Some(install_path_str) = entry.get("installPath").and_then(serde_json::Value::as_str)
    else {
        let _ = out.writeln(format_args!(
            "  {label}{version:<8}{}",
            out.colors.yellow("? missing installPath"),
        ));
        return;
    };
    let install_path = PathBuf::from(install_path_str);

    // Determine marketplace source type
    let source_type = read_marketplace_source(&home);
    let version_display = source_type
        .as_deref()
        .map_or_else(|| version.to_string(), |src| format!("{version} ({src})"));
    let ver_col = format!("{version_display:<20}");

    // Read installed hooks and compare
    let hooks_path = install_path.join("hooks/hooks.json");
    match std::fs::read_to_string(&hooks_path) {
        Ok(installed) => {
            if normalize_json(&installed) == normalize_json(CLAUDE_HOOKS_EXPECTED) {
                let _ = out.writeln(format_args!(
                    "  {label}{ver_col}{}",
                    out.colors.green("✓ hooks match")
                ));
            } else {
                let _ = out.writeln(format_args!(
                    "  {label}{ver_col}{}",
                    out.colors.red("✗ stale hooks (reinstall: claude plugin uninstall catenary@catenary && claude plugin install catenary@catenary)"),
                ));
                if show_diff {
                    show_unified_diff(
                        out,
                        &pretty_json(&installed),
                        &pretty_json(CLAUDE_HOOKS_EXPECTED),
                        "installed",
                        "expected",
                    );
                }
            }
        }
        Err(_) => {
            let _ = out.writeln(format_args!(
                "  {label}{ver_col}{}",
                out.colors.red("✗ hooks.json not found in plugin cache"),
            ));
        }
    }
}

/// Read the catenary marketplace source type from `known_marketplaces.json`.
fn read_marketplace_source(home: &Path) -> Option<String> {
    let path = home.join(".claude/plugins/known_marketplaces.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&contents).ok()?;
    json.get("catenary")
        .and_then(|c| c.get("source"))
        .and_then(|s| s.get("source"))
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string)
}

/// Check Gemini CLI extension hooks against the embedded expected hooks.
fn check_gemini_hooks(out: &mut Output, show_diff: bool) {
    let label = format!("{:<14}", "Gemini CLI");
    let Ok(home_str) = std::env::var("HOME") else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- cannot determine home directory"),
        ));
        return;
    };
    let home = PathBuf::from(home_str);

    // Look for the extension directory
    let ext_dir = home.join(".gemini/extensions");
    let candidates = ["Catenary", "catenary"];
    let ext_path = candidates
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir());

    let Some(ext_path) = ext_path else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- not installed")
        ));
        return;
    };

    // Read .gemini-extension-install.json to determine install type and source.
    // Gemini CLI writes this metadata file for both linked and installed extensions.
    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let install_meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = install_meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown");

    // For linked extensions, the source field is a local path to the actual
    // extension files. For installed extensions (github-release, etc.), the
    // files are cloned into the extension directory itself.
    let resolved = if install_type == "link" {
        install_meta
            .as_ref()
            .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
            .map_or_else(|| ext_path.clone(), PathBuf::from)
    } else {
        ext_path
    };

    // Read the extension manifest for version info
    let manifest_path = resolved.join("gemini-extension.json");
    let version = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("version")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string)
        });

    let type_label = if install_type == "link" {
        "linked"
    } else {
        "installed"
    };
    let version_display = version
        .as_deref()
        .map_or_else(|| type_label.to_string(), |v| format!("{v} ({type_label})"));
    let ver_col = format!("{version_display:<20}");

    // Read hooks and compare against embedded
    let hooks_path = resolved.join("hooks/hooks.json");
    match std::fs::read_to_string(&hooks_path) {
        Ok(installed) => {
            if normalize_json(&installed) == normalize_json(GEMINI_HOOKS_EXPECTED) {
                let _ = out.writeln(format_args!(
                    "  {label}{ver_col}{}",
                    out.colors.green("✓ hooks match")
                ));
            } else {
                let _ = out.writeln(format_args!(
                    "  {label}{ver_col}{}",
                    out.colors.red("✗ stale hooks (update extension)"),
                ));
                if show_diff {
                    show_unified_diff(
                        out,
                        &pretty_json(&installed),
                        &pretty_json(GEMINI_HOOKS_EXPECTED),
                        "installed",
                        "expected",
                    );
                }
            }
        }
        Err(_) => {
            let _ = out.writeln(format_args!(
                "  {label}{ver_col}{}",
                out.colors.yellow("? hooks.json not found"),
            ));
        }
    }
}

/// Check Antigravity CLI plugin hooks against the embedded expected hooks.
///
/// Searches three discovery paths (first match wins):
/// 1. `<project_root>/.agents/plugins/catenary/` (workspace)
/// 2. `<project_root>/_agents/plugins/catenary/` (workspace)
/// 3. `~/.gemini/config/plugins/catenary/` (global)
fn check_antigravity_hooks(out: &mut Output, show_diff: bool, project_root: &Path) {
    let label = format!("{:<14}", "Antigravity");

    let resolved_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    // Workspace-level paths (relative to --root).
    let workspace_candidates = [
        resolved_root.join(".agents/plugins/catenary"),
        resolved_root.join("_agents/plugins/catenary"),
    ];

    // Global path.
    let global_candidate = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".gemini/config/plugins/catenary"));

    let (plugin_dir, scope) = workspace_candidates
        .iter()
        .find(|p| p.is_dir())
        .map(|p| (p.clone(), "workspace"))
        .or_else(|| {
            global_candidate
                .filter(|p| p.is_dir())
                .map(|p| (p, "global"))
        })
        .unzip();

    let Some(plugin_dir) = plugin_dir else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- not installed")
        ));
        return;
    };
    let scope = scope.unwrap_or("unknown");
    let scope_col = format!("{scope:<20}");

    let hooks_path = plugin_dir.join("hooks.json");
    match std::fs::read_to_string(&hooks_path) {
        Ok(installed) => {
            if normalize_json(&installed) == normalize_json(ANTIGRAVITY_HOOKS_EXPECTED) {
                let _ = out.writeln(format_args!(
                    "  {label}{scope_col}{}",
                    out.colors.green("✓ hooks match")
                ));
            } else {
                let _ = out.writeln(format_args!(
                    "  {label}{scope_col}{}",
                    out.colors.red("✗ stale hooks (reinstall plugin)"),
                ));
                if show_diff {
                    show_unified_diff(
                        out,
                        &pretty_json(&installed),
                        &pretty_json(ANTIGRAVITY_HOOKS_EXPECTED),
                        "installed",
                        "expected",
                    );
                }
            }
        }
        Err(_) => {
            let _ = out.writeln(format_args!(
                "  {label}{scope_col}{}",
                out.colors.yellow("? hooks.json not found"),
            ));
        }
    }
}

/// Check whether the running binary matches what `$PATH` would resolve.
fn check_path_binary(out: &mut Output) {
    let label = format!("{:<14}", "PATH");
    let spacer = " ".repeat(20);

    let Some(current_exe) = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
    else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.yellow("? cannot determine current executable"),
        ));
        return;
    };

    // Find catenary on PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    let Some(path_binary) = std::env::split_paths(&path_var)
        .map(|dir| dir.join("catenary"))
        .find(|p| p.is_file())
    else {
        let _ = out.writeln(format_args!(
            "  {label}{spacer}{}",
            out.colors.red("✗ catenary not found on PATH"),
        ));
        return;
    };

    let resolved_path = std::fs::canonicalize(&path_binary).unwrap_or(path_binary);

    if current_exe == resolved_path {
        let _ = out.writeln(format_args!(
            "  {label}{spacer}{}",
            out.colors.green(&format!("✓ {}", resolved_path.display())),
        ));
    } else {
        let _ = out.writeln(format_args!(
            "  {label}{spacer}{}",
            out.colors.red(&format!(
                "✗ {} differs from {}",
                resolved_path.display(),
                current_exe.display(),
            )),
        ));
    }
}

/// Check Claude Code agent instruction files (SKILL.md).
///
/// Validates plugin version against the current binary version and
/// checks SKILL.md frontmatter format per the Agent Skills spec.
fn check_claude_instructions(out: &mut Output, show_diff: bool) {
    let label = format!("{:<14}", "Claude Code");
    let Ok(home_str) = std::env::var("HOME") else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- cannot determine home directory"),
        ));
        return;
    };
    let home = PathBuf::from(home_str);

    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(plugins_json) = std::fs::read_to_string(&plugins_file) else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- not installed"),
        ));
        return;
    };

    let Ok(plugins) = serde_json::from_str::<serde_json::Value>(&plugins_json) else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.yellow("? cannot parse installed_plugins.json"),
        ));
        return;
    };

    let entries = match plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
    {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.dim("- not installed"),
            ));
            return;
        }
    };

    let entry = &entries[0];
    let installed_version = entry
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let expected_version = env!("CATENARY_VERSION");

    let Some(install_path_str) = entry.get("installPath").and_then(serde_json::Value::as_str)
    else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.yellow("? missing installPath"),
        ));
        return;
    };
    let install_path = PathBuf::from(install_path_str);

    // Version staleness check
    let is_stale = installed_version != expected_version;
    if is_stale {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.red(&format!(
                "✗ stale (v{installed_version} installed, v{expected_version} expected)"
            )),
        ));
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("  run: catenary install claude"),
        ));
    } else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors
                .green(&format!("✓ up to date (v{installed_version})")),
        ));
    }

    // SKILL.md content comparison against embedded version
    let skill_path = install_path.join("skills/catenary/SKILL.md");
    match std::fs::read_to_string(&skill_path) {
        Ok(content) if content != SKILL_MD_EXPECTED => {
            if !is_stale {
                // Version matches but content drifted (manual edit, corruption)
                let _ = out.writeln(format_args!(
                    "  {label}{}",
                    out.colors
                        .yellow("⚠ SKILL.md content differs from expected"),
                ));
            }
            if show_diff {
                show_unified_diff(out, &content, SKILL_MD_EXPECTED, "installed", "expected");
            }
        }
        Ok(_) => {} // Content matches — nothing extra to report
        Err(_) => {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.red("✗ SKILL.md not found in plugin"),
            ));
        }
    }
}

/// Check Gemini CLI agent instruction files (context file).
///
/// Validates extension version against the current binary version.
/// Linked extensions are always current by definition.
#[allow(
    clippy::too_many_lines,
    reason = "Sequential discovery + version check + file check"
)]
fn check_gemini_instructions(out: &mut Output, show_diff: bool) {
    let label = format!("{:<14}", "Gemini CLI");
    let Ok(home_str) = std::env::var("HOME") else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- cannot determine home directory"),
        ));
        return;
    };
    let home = PathBuf::from(home_str);

    let ext_dir = home.join(".gemini/extensions");
    let ext_path = ["Catenary", "catenary"]
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir());

    let Some(ext_path) = ext_path else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- not installed"),
        ));
        return;
    };

    // Determine install type and resolve path
    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let install_meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = install_meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown");
    let is_linked = install_type == "link";

    let resolved = if is_linked {
        install_meta
            .as_ref()
            .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
            .map_or_else(|| ext_path.clone(), PathBuf::from)
    } else {
        ext_path
    };

    // Read manifest for version and context file name
    let manifest_path = resolved.join("gemini-extension.json");
    let manifest = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let version = manifest
        .as_ref()
        .and_then(|v| v.get("version").and_then(serde_json::Value::as_str));

    let context_filename = manifest
        .as_ref()
        .and_then(|v| v.get("contextFileName").and_then(serde_json::Value::as_str))
        .unwrap_or("gemini-context.md");

    // Version staleness
    let expected_version = env!("CATENARY_VERSION");
    let is_stale = !is_linked && version.is_some_and(|v| v != expected_version);

    if is_linked {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.green("✓ linked (always current)"),
        ));
    } else if let Some(v) = version {
        if v == expected_version {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.green(&format!("✓ up to date (v{v})")),
            ));
        } else {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.red(&format!(
                    "✗ stale (v{v} installed, v{expected_version} expected)"
                )),
            ));
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.dim("  run: catenary install gemini"),
            ));
        }
    } else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.yellow("? cannot determine version"),
        ));
    }

    // Context file check
    let context_path = resolved.join(context_filename);
    match std::fs::read_to_string(&context_path) {
        Ok(content) if content.trim().is_empty() => {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.yellow(&format!("⚠ {context_filename} is empty")),
            ));
        }
        Ok(content) => {
            if is_stale && show_diff {
                show_unified_diff(
                    out,
                    &content,
                    GEMINI_CONTEXT_EXPECTED,
                    "installed",
                    "expected",
                );
            }
        }
        Err(_) => {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.red(&format!("✗ {context_filename} not found")),
            ));
        }
    }
}

/// Check Antigravity CLI agent instruction files (rules).
///
/// Compares installed rules file content against the embedded version.
/// Symlinked installs are always current by definition.
fn check_antigravity_instructions(out: &mut Output, show_diff: bool, project_root: &Path) {
    let label = format!("{:<14}", "Antigravity");

    let resolved_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    // Discovery: same paths as check_antigravity_hooks
    let workspace_candidates = [
        resolved_root.join(".agents/plugins/catenary"),
        resolved_root.join("_agents/plugins/catenary"),
    ];

    let global_candidate = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".gemini/config/plugins/catenary"));

    let plugin_dir = workspace_candidates
        .iter()
        .find(|p| p.is_dir())
        .cloned()
        .or_else(|| global_candidate.filter(|p| p.is_dir()));

    let Some(plugin_dir) = plugin_dir else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("- not installed"),
        ));
        return;
    };

    // Symlinked installs are always current
    if plugin_dir.is_symlink() {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.green("✓ symlinked (always current)"),
        ));
        return;
    }

    // Content comparison
    let rules_path = plugin_dir.join("rules/catenary.md");
    if let Ok(content) = std::fs::read_to_string(&rules_path) {
        if content == ANTIGRAVITY_RULES_EXPECTED {
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.green("✓ rules up to date"),
            ));
        } else {
            let _ = out.writeln(format_args!("  {label}{}", out.colors.red("✗ stale rules")));
            let _ = out.writeln(format_args!(
                "  {label}{}",
                out.colors.dim("  run: catenary install antigravity"),
            ));
            if show_diff {
                show_unified_diff(
                    out,
                    &content,
                    ANTIGRAVITY_RULES_EXPECTED,
                    "installed",
                    "expected",
                );
            }
        }
    } else {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.red("✗ rules/catenary.md not found"),
        ));
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim("  run: catenary install antigravity"),
        ));
    }
}

/// Validate SKILL.md frontmatter format per the Claude Code Agent Skills spec.
///
/// Returns a list of validation error messages. Empty list means valid.
/// Checks: valid YAML frontmatter delimiters, `name` field (must match
/// `catenary`, lowercase alphanumeric + hyphens, 1-64 chars), `description`
/// field (non-empty, max 1024 chars), and non-empty body after frontmatter.
#[cfg(test)]
fn validate_skill_frontmatter(content: &str) -> Vec<String> {
    let mut errors = Vec::new();

    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        errors.push("missing opening `---` delimiter".to_string());
        return errors;
    }

    let after_opening = trimmed[3..]
        .strip_prefix('\n')
        .unwrap_or_else(|| &trimmed[3..]);
    let Some(end_pos) = after_opening.find("\n---") else {
        errors.push("missing closing `---` delimiter".to_string());
        return errors;
    };

    let frontmatter = &after_opening[..end_pos];
    let body_start = end_pos + 4; // skip "\n---"
    let body = if body_start < after_opening.len() {
        &after_opening[body_start..]
    } else {
        ""
    };

    // Validate name
    match extract_frontmatter_value(frontmatter, "name") {
        None => errors.push("`name` field missing".to_string()),
        Some(name) => {
            if name != "catenary" {
                errors.push(format!("`name` is '{name}', expected 'catenary'"));
            }
            if !is_valid_skill_name(&name) {
                errors.push(format!(
                    "`name` '{name}': must be 1-64 lowercase alphanumeric/hyphen chars, \
                     no leading/trailing/consecutive hyphens"
                ));
            }
        }
    }

    // Validate description
    match extract_frontmatter_value(frontmatter, "description") {
        None => errors.push("`description` field missing".to_string()),
        Some(desc) if desc.len() > 1024 => {
            errors.push(format!("`description` is {} chars (max 1024)", desc.len()));
        }
        Some(_) => {}
    }

    // Validate body
    if body.trim().is_empty() {
        errors.push("no body content after frontmatter".to_string());
    }

    errors
}

/// Check whether a skill name conforms to the Claude Code Agent Skills spec.
///
/// Valid: 1-64 characters, lowercase alphanumeric + hyphens,
/// no leading/trailing/consecutive hyphens.
#[cfg(test)]
fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Extract a simple value from YAML frontmatter.
///
/// Handles inline values (`key: value`) and multi-line folded/literal
/// scalars (`key: >` / `key: |` followed by indented continuation lines).
#[cfg(test)]
fn extract_frontmatter_value(frontmatter: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let mut lines = frontmatter.lines().peekable();

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let rest = rest.trim();

            if rest.is_empty() || rest == ">" || rest == "|" {
                // Multi-line: collect indented continuation lines
                let mut parts = Vec::new();
                while let Some(&next) = lines.peek() {
                    if next.starts_with(' ') || next.starts_with('\t') {
                        parts.push(next.trim());
                        lines.next();
                    } else {
                        break;
                    }
                }
                return if parts.is_empty() {
                    None
                } else {
                    Some(parts.join(" "))
                };
            }

            return Some(rest.to_string());
        }
    }
    None
}

/// Check whether `~/.claude/settings.json` still references the legacy Python script.
///
/// If found, warns the user to remove it and migrate to `[commands]` config.
fn check_constrained_bash_claude(out: &mut Output) {
    check_legacy_script(out, "Claude Code", ".claude/settings.json");
}

/// Check whether `~/.gemini/settings.json` still references the legacy Python script.
///
/// If found, warns the user to remove it and migrate to `[commands]` config.
fn check_constrained_bash_gemini(out: &mut Output) {
    check_legacy_script(out, "Gemini CLI", ".gemini/settings.json");
}

/// Check a host CLI settings file for references to the legacy `constrained_bash.py`.
fn check_legacy_script(out: &mut Output, client: &str, settings_rel: &str) {
    let label = format!("{client:<14}");

    let Ok(home_str) = std::env::var("HOME") else {
        return;
    };
    let home = PathBuf::from(home_str);

    let settings_path = home.join(settings_rel);
    let Ok(settings_json) = std::fs::read_to_string(&settings_path) else {
        return;
    };

    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&settings_json) else {
        return;
    };

    if find_script_path_in_json(&settings, "constrained_bash.py").is_some() {
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.yellow("⚠  legacy constrained_bash.py detected"),
        ));
        let _ = out.writeln(format_args!(
            "  {label}{}",
            out.colors.dim(&format!("  {CONSTRAINED_BASH_MIGRATION}")),
        ));
    }
}

/// Report the status of the built-in command filter configuration.
fn check_command_filter_config(out: &mut Output, config: &crate::config::Config) {
    match &config.resolved_commands {
        Some(resolved) if resolved.client_enforcement_only => {
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors
                    .dim("client_enforcement_only — Catenary enforcement disabled"),
            ));
        }
        Some(resolved) if resolved.is_active() => {
            let total = resolved.allow.len() + resolved.pipeline.len();
            let build_suffix = if !resolved.default_build.is_empty() {
                let tools = resolved.default_build.join(", ");
                format!(
                    ", build tool{}: {tools}",
                    if resolved.default_build.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                )
            } else if !resolved.build.is_empty() {
                let mut tools: Vec<&str> = resolved
                    .build
                    .values()
                    .flat_map(|v| v.iter().map(String::as_str))
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                tools.sort_unstable();
                format!(
                    ", build tool{}: {}",
                    if tools.len() == 1 { "" } else { "s" },
                    tools.join(", ")
                )
            } else {
                String::new()
            };
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors.green(&format!(
                    "✓ {total} command{} allowed{build_suffix}",
                    if total == 1 { "" } else { "s" },
                )),
            ));
        }
        Some(_) | None => {
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors
                    .dim("no [commands] section — all shell commands allowed"),
            ));
        }
    }
}

/// Normalize a JSON string for comparison (parse and re-serialize).
///
/// Returns the compact re-serialized form, or the original string (trimmed)
/// if parsing fails.
fn normalize_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_else(|| s.trim().to_string())
}

/// Pretty-print a JSON string for use in human-readable diffs.
///
/// Returns the pretty-printed form, or the original string if parsing fails.
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string())
}

/// Print a unified diff between `old` and `new` using the `similar` crate.
fn show_unified_diff(out: &mut Output, old: &str, new: &str, old_label: &str, new_label: &str) {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    let _ = out.write_str(format_args!(
        "{}",
        diff.unified_diff()
            .context_radius(3)
            .header(old_label, new_label)
    ));
}

/// Walk all string values in `json` and return the whitespace-split token
/// that contains `needle`, searching depth-first.
///
/// Returns `None` if no string value in the tree mentions `needle`.
fn find_script_path_in_json(json: &serde_json::Value, needle: &str) -> Option<String> {
    match json {
        serde_json::Value::String(s) if s.contains(needle) => s
            .split_whitespace()
            .find(|token| token.contains(needle))
            .map(std::string::ToString::to_string),
        serde_json::Value::Object(map) => map
            .values()
            .find_map(|v| find_script_path_in_json(v, needle)),
        serde_json::Value::Array(arr) => {
            arr.iter().find_map(|v| find_script_path_in_json(v, needle))
        }
        _ => None,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::fs;

    // ── user_config_path_in tests ───────────────────────────────

    #[test]
    fn config_path_none_when_base_is_none() {
        assert!(user_config_path_in(None).is_none());
    }

    #[test]
    fn config_path_none_when_file_absent() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        assert!(user_config_path_in(Some(tmp.path().to_path_buf())).is_none());
    }

    #[test]
    fn config_path_some_when_file_exists() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let config_dir = tmp.path().join("catenary");
        fs::create_dir_all(&config_dir).expect("create config dir");
        let config_file = config_dir.join("config.toml");
        fs::write(&config_file, "# empty").expect("write config");

        let result = user_config_path_in(Some(tmp.path().to_path_buf()));
        assert_eq!(result.expect("should find config file"), config_file,);
    }

    // ── collect_suggestions tests ───────────────────────────────

    #[test]
    fn suggestions_no_config_file() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let config = crate::config::Config::default();
        let suggestions = collect_suggestions(&config, Some(tmp.path().to_path_buf()));

        assert!(
            suggestions
                .iter()
                .any(|s| s.contains("No config file found")),
            "should mention missing config file",
        );
        assert!(
            suggestions.iter().any(|s| s.contains("catenary config")),
            "should suggest `catenary config`",
        );
    }

    #[test]
    fn suggestions_config_exists_but_no_commands() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let config_dir = tmp.path().join("catenary");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(config_dir.join("config.toml"), "# no commands").expect("write config");

        let config = crate::config::Config::default();
        let suggestions = collect_suggestions(&config, Some(tmp.path().to_path_buf()));

        assert!(
            suggestions
                .iter()
                .any(|s| s.contains("No [commands] section")),
            "should mention missing [commands] section",
        );
        assert!(
            !suggestions
                .iter()
                .any(|s| s.contains("No config file found")),
            "should not mention missing config file when file exists",
        );
    }

    #[test]
    fn suggestions_empty_when_fully_configured() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let config_dir = tmp.path().join("catenary");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(
            config_dir.join("config.toml"),
            "[commands.deny]\ncat = \"test\"",
        )
        .expect("write config");

        let mut config = crate::config::Config::default();
        config.resolved_commands = Some(crate::config::ResolvedCommands::default());
        let suggestions = collect_suggestions(&config, Some(tmp.path().to_path_buf()));

        assert!(
            suggestions.is_empty(),
            "should have no suggestions when config file and commands exist",
        );
    }

    #[test]
    fn suggestions_no_config_file_includes_path() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let config = crate::config::Config::default();
        let suggestions = collect_suggestions(&config, Some(tmp.path().to_path_buf()));

        let expected_path = tmp
            .path()
            .join("catenary")
            .join("config.toml")
            .display()
            .to_string();
        assert!(
            suggestions.iter().any(|s| s.contains(&expected_path)),
            "suggestion should include the platform-resolved config path",
        );
    }

    #[test]
    fn suggestions_none_base_falls_back() {
        let config = crate::config::Config::default();
        let suggestions = collect_suggestions(&config, None);

        assert!(
            suggestions
                .iter()
                .any(|s| s.contains("~/.config/catenary/config.toml")),
            "should fall back to ~/.config path when config_dir is None",
        );
    }

    // ── probe_timeout tests ─────────────────────────────────────────

    #[test]
    fn probe_timeout_default_is_five_minutes() {
        // Assumes CATENARY_DOCTOR_TIMEOUT_SECS is not set in the test
        // environment. The default is 5 minutes (300 seconds).
        let timeout = probe_timeout();
        assert_eq!(
            timeout,
            Duration::from_mins(5),
            "default probe timeout should be 5 minutes",
        );
    }

    // ── extract_capabilities tests ──────────────────────────────────

    #[test]
    fn extract_capabilities_hover() {
        let caps = serde_json::json!({"hoverProvider": true});
        let result = extract_capabilities(&caps, false);
        assert!(
            result.contains(&"hover"),
            "should include hover capability, got: {result:?}",
        );
    }

    #[test]
    fn extract_capabilities_definition() {
        let caps = serde_json::json!({"definitionProvider": true});
        let result = extract_capabilities(&caps, false);
        assert!(
            result.contains(&"definition"),
            "should include definition capability, got: {result:?}",
        );
    }

    #[test]
    fn extract_capabilities_empty_when_none() {
        let caps = serde_json::json!({});
        let result = extract_capabilities(&caps, false);
        assert!(result.is_empty(), "empty caps should yield nothing");
    }

    #[test]
    fn extract_capabilities_ignores_null_values() {
        // LSP servers may explicitly set a capability to null.
        let caps = serde_json::json!({"hoverProvider": null});
        let result = extract_capabilities(&caps, false);
        assert!(
            !result.contains(&"hover"),
            "null provider should not be included",
        );
    }

    // ── binary_exists / resolve_binary tests ────────────────────────

    #[test]
    fn binary_exists_finds_known_binary() {
        // "sh" should exist on any Unix system.
        assert!(binary_exists("sh"));
    }

    #[test]
    fn binary_exists_rejects_nonexistent() {
        assert!(!binary_exists("catenary_nonexistent_binary_xyz"));
    }

    #[test]
    fn resolve_binary_finds_known_binary() {
        let result = resolve_binary("sh");
        assert!(result.is_some(), "should resolve 'sh'");
    }

    #[test]
    fn resolve_binary_returns_none_for_nonexistent() {
        assert!(resolve_binary("catenary_nonexistent_binary_xyz").is_none());
    }

    // ── normalize_json / pretty_json tests ──────────────────────────

    #[test]
    fn normalize_json_canonicalizes() {
        let input = r#"{ "b": 2, "a": 1 }"#;
        let result = normalize_json(input);
        // Should be parseable as valid JSON.
        assert!(
            serde_json::from_str::<serde_json::Value>(&result).is_ok(),
            "normalized JSON should be valid, got: {result}",
        );
    }

    #[test]
    fn pretty_json_formats_readably() {
        let input = r#"{"a":1,"b":2}"#;
        let result = pretty_json(input);
        assert!(
            result.contains('\n'),
            "pretty JSON should be multi-line, got: {result}",
        );
    }

    // ── validate_skill_frontmatter tests ───────────────────────────

    #[test]
    fn valid_skill_frontmatter_inline() {
        let content = "---\nname: catenary\ndescription: A tool\n---\n\nBody content here.\n";
        let errors = validate_skill_frontmatter(content);
        assert!(errors.is_empty(), "should be valid, got: {errors:?}");
    }

    #[test]
    fn valid_skill_frontmatter_multiline_description() {
        let content =
            "---\nname: catenary\ndescription: >\n  Multi-line\n  description.\n---\n\nBody.\n";
        let errors = validate_skill_frontmatter(content);
        assert!(
            errors.is_empty(),
            "should be valid with folded description, got: {errors:?}",
        );
    }

    #[test]
    fn skill_frontmatter_missing_opening_delimiter() {
        let content = "name: catenary\ndescription: A tool\n---\n\nBody.\n";
        let errors = validate_skill_frontmatter(content);
        assert!(
            errors.iter().any(|e| e.contains("opening")),
            "should report missing opening delimiter, got: {errors:?}",
        );
    }

    #[test]
    fn skill_frontmatter_missing_closing_delimiter() {
        let content = "---\nname: catenary\ndescription: A tool\n\nBody.\n";
        let errors = validate_skill_frontmatter(content);
        assert!(
            errors.iter().any(|e| e.contains("closing")),
            "should report missing closing delimiter, got: {errors:?}",
        );
    }

    #[test]
    fn skill_frontmatter_missing_name() {
        let content = "---\ndescription: A tool\n---\n\nBody.\n";
        let errors = validate_skill_frontmatter(content);
        assert!(
            errors.iter().any(|e| e.contains("`name`")),
            "should report missing name, got: {errors:?}",
        );
    }

    #[test]
    fn skill_frontmatter_wrong_name() {
        let content = "---\nname: wrong\ndescription: A tool\n---\n\nBody.\n";
        let errors = validate_skill_frontmatter(content);
        assert!(
            errors.iter().any(|e| e.contains("'wrong'")),
            "should report wrong name, got: {errors:?}",
        );
    }

    #[test]
    fn skill_frontmatter_missing_description() {
        let content = "---\nname: catenary\n---\n\nBody.\n";
        let errors = validate_skill_frontmatter(content);
        assert!(
            errors.iter().any(|e| e.contains("`description`")),
            "should report missing description, got: {errors:?}",
        );
    }

    #[test]
    fn skill_frontmatter_empty_body() {
        let content = "---\nname: catenary\ndescription: A tool\n---\n";
        let errors = validate_skill_frontmatter(content);
        assert!(
            errors.iter().any(|e| e.contains("body")),
            "should report empty body, got: {errors:?}",
        );
    }

    #[test]
    fn skill_frontmatter_long_description() {
        let long = "a".repeat(1025);
        let content = format!("---\nname: catenary\ndescription: {long}\n---\n\nBody.\n");
        let errors = validate_skill_frontmatter(&content);
        assert!(
            errors.iter().any(|e| e.contains("1024")),
            "should report long description, got: {errors:?}",
        );
    }

    // ── is_valid_skill_name tests ──────────────────────────────────

    #[test]
    fn valid_skill_names() {
        assert!(is_valid_skill_name("catenary"));
        assert!(is_valid_skill_name("my-tool"));
        assert!(is_valid_skill_name("a"));
        assert!(is_valid_skill_name("tool123"));
    }

    #[test]
    fn invalid_skill_names() {
        assert!(!is_valid_skill_name(""), "empty");
        assert!(!is_valid_skill_name("-leading"), "leading hyphen");
        assert!(!is_valid_skill_name("trailing-"), "trailing hyphen");
        assert!(
            !is_valid_skill_name("double--hyphen"),
            "consecutive hyphens",
        );
        assert!(!is_valid_skill_name("UPPERCASE"), "uppercase");
        assert!(!is_valid_skill_name("has spaces"), "spaces");
        assert!(!is_valid_skill_name(&"a".repeat(65)), "too long");
    }

    // ── extract_frontmatter_value tests ────────────────────────────

    #[test]
    fn extract_inline_value() {
        let fm = "name: catenary\ndescription: A tool";
        assert_eq!(
            extract_frontmatter_value(fm, "name"),
            Some("catenary".to_string()),
        );
    }

    #[test]
    fn extract_multiline_value() {
        let fm = "name: catenary\ndescription: >\n  Multi\n  line";
        assert_eq!(
            extract_frontmatter_value(fm, "description"),
            Some("Multi line".to_string()),
        );
    }

    #[test]
    fn extract_missing_key() {
        let fm = "name: catenary";
        assert_eq!(extract_frontmatter_value(fm, "description"), None);
    }

    #[test]
    fn extract_literal_block_scalar() {
        let fm = "name: catenary\ndescription: |\n  Line one.\n  Line two.";
        assert_eq!(
            extract_frontmatter_value(fm, "description"),
            Some("Line one. Line two.".to_string()),
        );
    }

    // ── embedded instruction file tests ────────────────────────────

    #[test]
    fn embedded_skill_md_valid() {
        let errors = validate_skill_frontmatter(SKILL_MD_EXPECTED);
        assert!(
            errors.is_empty(),
            "embedded SKILL.md should pass validation, got: {errors:?}",
        );
    }

    #[test]
    fn embedded_gemini_context_non_empty() {
        assert!(
            !GEMINI_CONTEXT_EXPECTED.trim().is_empty(),
            "embedded gemini-context.md should not be empty",
        );
    }

    #[test]
    fn embedded_antigravity_rules_non_empty() {
        assert!(
            !ANTIGRAVITY_RULES_EXPECTED.trim().is_empty(),
            "embedded antigravity rules should not be empty",
        );
    }
}
