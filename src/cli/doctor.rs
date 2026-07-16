// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Doctor command: a one-shot renderer over the [`crate::health`] model.
//!
//! Every check doctor performs — config migration, validation, unknown keys,
//! unreferenced/duplicate/project config, server probes, the routing table, and
//! hooks/instructions/filter staleness — lives in [`crate::health`] as typed
//! findings. This module gathers doctor's own probe feed (daemon-down capable),
//! asks the model for findings, and renders them. The finding *set* is the
//! contract (pinned by the model's tests); the prose here may reflow freely.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::cli::Output;
use crate::health::servers::{HealthFeed, ProbeFeed, ServerStatus};
use crate::health::{Finding, Severity};
use crate::lsp;

/// Maximum number of stderr lines to capture in verbose doctor mode.
const STDERR_MAX_LINES: usize = 50;

/// Render one finding: the glyph + message, its fix-it (indented, dim), and —
/// only under `--diff` — its stale-content diff.
fn render_finding(out: &mut Output, finding: &Finding, show_diff: bool) {
    let body = match finding.severity {
        Severity::Fatal => out
            .colors
            .bold(&out.colors.red(&format!("✗  {}", finding.message))),
        Severity::Error => out.colors.red(&format!("✗  {}", finding.message)),
        Severity::Warning => out.colors.yellow(&format!("⚠  {}", finding.message)),
        Severity::Suggestion => out.colors.cyan(&format!("○  {}", finding.message)),
        Severity::Ok => out.colors.green(&format!("✓  {}", finding.message)),
        Severity::Info => out.colors.dim(&finding.message),
    };
    let _ = out.writeln(format_args!("  {body}"));

    if let Some(fix_it) = &finding.fix_it {
        for line in fix_it.lines() {
            let styled = out.colors.dim(line);
            let _ = out.writeln(format_args!("     {styled}"));
        }
    }

    // Routing provenance under the fix-it line — "why is this being probed?"
    // (tui-rework 09, item 4).
    if let Some(provenance) = &finding.provenance {
        let styled = out.colors.dim(provenance);
        let _ = out.writeln(format_args!("     {styled}"));
    }

    if show_diff && let Some(diff) = &finding.diff {
        show_unified_diff(
            out,
            &diff.installed,
            &diff.expected,
            "installed",
            "expected",
        );
    }
}

/// Render a slice of findings, returning whether any were rendered (for spacing).
fn render_findings(out: &mut Output, findings: &[Finding], show_diff: bool) -> bool {
    for finding in findings {
        render_finding(out, finding, show_diff);
    }
    !findings.is_empty()
}

/// Run the doctor command: check all configured language servers.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded.
#[allow(
    clippy::too_many_lines,
    reason = "Doctor command has sequential output sections"
)]
pub async fn run_doctor(out: &mut Output, project_root: &Path, show_diff: bool) -> Result<()> {
    let _ = out.writeln(format_args!("Catenary {}", env!("CATENARY_VERSION")));
    let _ = out.writeln(format_args!(""));

    // Version skew (binary vs the running daemon's recorded version) — surfaced
    // up top so a stale daemon is the first thing seen. Daemon down → no
    // finding.
    let daemon_version = read_daemon_version();
    if let Some(finding) = crate::health::skew::skew_finding(
        crate::health::skew::BINARY_VERSION,
        daemon_version.as_deref(),
    ) {
        render_finding(out, &finding, show_diff);
        let _ = out.writeln(format_args!(""));
    }

    // Bridge↔daemon protocol-version mismatch (ws41-02) — the daemon records an
    // observed mismatch onto its snapshot; read it back into a persistent
    // finding that names the older side and its cure. Daemon down or versions
    // agree → no record → no finding.
    if let Some(finding) = read_bridge_mismatch()
        .and_then(|m| crate::health::skew::bridge_mismatch_finding(m.bridge.as_deref(), &m.daemon))
    {
        render_finding(out, &finding, show_diff);
        let _ = out.writeln(format_args!(""));
    }

    // Config migration walk — runs before the load so its rename guidance can
    // print above a config-load error. A non-empty set also lets the load-error
    // path drop the self-referential "run catenary doctor" pointer.
    let migration =
        crate::health::config_checks::migration_findings(&crate::config::config_sources());
    if render_findings(out, &migration, show_diff) {
        let _ = out.writeln(format_args!(""));
    }

    let config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            let rendered = format!("{e:#}");
            let rendered = if migration.is_empty() {
                rendered
            } else {
                crate::health::config_checks::rewrite_guard_pointer(&rendered)
            };
            let _ = out.writeln(format_args!(
                "{}",
                out.colors.red(&format!("✗ Config error: {rendered}"))
            ));
            let _ = out.writeln(format_args!(""));
            return Ok(());
        }
    };

    let config_source = std::env::var("CATENARY_CONFIG")
        .ok()
        .unwrap_or_else(|| "default paths".to_string());
    let _ = out.writeln(format_args!(
        "{} {}",
        out.colors.bold("Config:"),
        config_source
    ));
    let _ = out.writeln(format_args!(""));

    // Config-block findings: validation, unknown keys, unreferenced servers,
    // duplicate extensions.
    let sources = crate::config::config_sources();
    let mut config_findings = crate::health::config_checks::validation_findings(&config);
    // Section-scoped quarantine (bug 110): a section the load defaulted out
    // (e.g. an invalid [commands]) surfaces here with its full error list — the
    // doctor read of the loud degrade grep/glob/hook only summarize.
    config_findings.extend(crate::health::config_checks::quarantine_findings(&config));
    config_findings.extend(crate::health::config_checks::unknown_key_findings(&sources));
    config_findings.extend(crate::health::config_checks::unreferenced_server_findings(
        &config,
    ));
    config_findings.extend(crate::health::config_checks::duplicate_extension_findings(
        &config,
    ));
    config_findings.extend(crate::health::config_checks::leftover_launcher_args_findings(&config));
    config_findings.extend(crate::health::config_checks::pinned_root_findings(&config));
    if render_findings(out, &config_findings, show_diff) {
        let _ = out.writeln(format_args!(""));
    }

    // Project config section.
    if let Some(path) = crate::health::config_checks::project_config_path(project_root) {
        let _ = out.writeln(format_args!(
            "{} {}",
            out.colors.bold("Project config:"),
            path.display(),
        ));
        let project_findings =
            crate::health::config_checks::project_config_findings(project_root, &config);
        render_findings(out, &project_findings, show_diff);
        let _ = out.writeln(format_args!(""));
    }

    if config.language.is_empty() && config.server.is_empty() {
        let _ = out.writeln(format_args!("No language servers configured."));
        return Ok(());
    }

    // ── Servers section ──────────────────────────────────────────────
    // Gather the probe feed (concurrent one-shot probes), then render the
    // server findings the model derives from it — routed breaks are errors,
    // dormant breaks are inventory.
    let feed = gather_probe_feed(&config, daemon_version).await;
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Servers")));
    let server_findings = crate::health::servers::server_findings(&config, &feed);
    render_findings(out, &server_findings, show_diff);

    // Enrichment-only disclosure (diagnostics-debt 04b): each configured, routed
    // server that is unverified (absent from the blessed manifest) earns a
    // warn-tier finding naming it enrichment-only, with the manifest as the
    // pointer. A blessed server produces nothing here.
    let enrichment_findings =
        crate::health::servers::enrichment_only_findings(&config, feed.active_languages());
    render_findings(out, &enrichment_findings, show_diff);

    // Live strike-ledger state (misc 167): a struck-out server on the running
    // daemon's board stays down until a restart/remount, even when doctor's
    // own fresh probe succeeds — surface that split honestly. Read from the
    // same snapshot the activity ledger came from; daemon down ⇒ no board ⇒
    // no findings.
    let strike_findings = read_snapshot()
        .map(|s| crate::health::servers::strike_findings(&s.servers))
        .unwrap_or_default();
    render_findings(out, &strike_findings, show_diff);

    // ── Languages section (the routing table) ────────────────────────
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Languages")));
    render_languages(out, &config, &feed);

    // ── Hooks section ────────────────────────────────────────────────
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Hooks")));
    let mut hook_findings = crate::health::install_checks::claude_hooks_findings();
    hook_findings.extend(crate::health::install_checks::antigravity_hooks_findings(
        project_root,
    ));
    hook_findings.extend(crate::health::install_checks::path_binary_findings());
    render_findings(out, &hook_findings, show_diff);

    // ── Agent instructions section ───────────────────────────────────
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Agent instructions")));
    let mut instruction_findings = crate::health::install_checks::claude_instructions_findings();
    instruction_findings
        .extend(crate::health::install_checks::antigravity_instructions_findings(project_root));
    render_findings(out, &instruction_findings, show_diff);

    // ── Command filter section ───────────────────────────────────────
    let _ = out.writeln(format_args!(""));
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Command filter")));
    let mut filter_findings = crate::health::install_checks::legacy_script_findings();
    filter_findings.extend(crate::health::install_checks::command_filter_findings(
        &config,
    ));
    render_findings(out, &filter_findings, show_diff);

    // Actionable suggestions at the very bottom so they aren't buried.
    let suggestions = collect_suggestions(&config, Some(crate::paths::config_dir()));
    if !suggestions.is_empty() {
        let _ = out.writeln(format_args!(""));
        let _ = out.writeln(format_args!("{}:", out.colors.bold("Suggestions")));
        for suggestion in &suggestions {
            let _ = out.writeln(format_args!("  {}", out.colors.dim(suggestion)));
        }
    }

    Ok(())
}

/// Gather doctor's one-shot probe feed: concurrent `initialize` probes for every
/// configured server, plus the **activity-live** languages and their provenance
/// read from the daemon's `state.json` activity ledger, and the observed daemon
/// version.
///
/// Gating on activity rather than presence is the doctor half of "one model, two
/// renderers" (tui-rework 09, item 5): with the daemon down there is no activity
/// ledger, so no language is live and a broken *default* server reads as quiet
/// dormant inventory rather than a phantom Fatal — a dormant fixture directory
/// no session touched never screams "install this".
async fn gather_probe_feed(
    config: &crate::config::Config,
    daemon_version: Option<String>,
) -> ProbeFeed {
    let mut join_set = tokio::task::JoinSet::new();
    for (name, def) in &config.server {
        join_set.spawn(crate::health::servers::probe_server(
            name.clone(),
            def.program(name).to_string(),
            def.args.clone(),
            def.initialization_options.clone(),
            def.env.clone(),
        ));
    }

    let mut statuses: HashMap<String, ServerStatus> = HashMap::new();
    while let Some(joined) = join_set.join_next().await {
        if let Ok((name, status)) = joined {
            statuses.insert(name, status);
        }
    }

    let activity = read_snapshot()
        .map(|s| s.activity_languages)
        .unwrap_or_default();
    let (active_languages, provenance) = crate::health::servers::activity_inputs(&activity);

    ProbeFeed::new(statuses, active_languages, daemon_version).with_provenance(provenance)
}

/// Render the language→server routing table with each server's capabilities.
fn render_languages(out: &mut Output, config: &crate::config::Config, feed: &dyn HealthFeed) {
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
        if let Some(ServerStatus::Ready { capabilities, .. }) = feed.server_status(target)
            && !capabilities.is_empty()
        {
            let _ = out.writeln(format_args!(
                "{}    {}",
                " ".repeat(max_lang_width + 2),
                out.colors.dim(&capabilities.join(" ")),
            ));
        }
    }
}

/// Read + parse the running daemon's `state.json` snapshot, if present.
///
/// Read-only: the same source the TUI's snapshot feed uses. A missing or
/// unparseable snapshot (daemon down) yields `None`, so doctor sees no daemon
/// version and no activity ledger — version skew does not fire and no language
/// is activity-live.
fn read_snapshot() -> Option<crate::state_snapshot::Snapshot> {
    let path = crate::paths::runtime_dir()
        .join("catenary")
        .join("state.json");
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Read the running daemon's version from the `state.json` snapshot, if present.
///
/// A missing/unparseable snapshot or an empty version yields `None` (daemon down
/// or unknown), so version skew simply does not fire.
fn read_daemon_version() -> Option<String> {
    let version = read_snapshot()?.daemon.version;
    (!version.is_empty()).then_some(version)
}

/// The bridge/daemon versions of an observed mismatch, as recorded on the
/// snapshot (ws41-02). `bridge` is `None` for a pre-handshake bridge.
struct RecordedBridgeMismatch {
    bridge: Option<String>,
    daemon: String,
}

/// Read the daemon's recorded bridge↔daemon version mismatch, if any.
///
/// The daemon writes this onto its snapshot the moment a bridge's hello
/// disagrees and clears it once they agree, so a missing snapshot, an absent
/// record (agreement), or a daemon predating the field all yield `None`.
fn read_bridge_mismatch() -> Option<RecordedBridgeMismatch> {
    let m = read_snapshot()?.daemon.bridge_mismatch?;
    Some(RecordedBridgeMismatch {
        bridge: m.bridge_version,
        daemon: m.daemon_version,
    })
}

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

    // Merge project config if present.
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

    // Look up server.
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
    // The server key IS the executable (misc 162): resolve the `path` override
    // if set, else spawn the key `server_name` on PATH.
    let command = server_def.program(server_name);
    let args_display = if server_def.args.is_empty() {
        String::new()
    } else {
        format!(" {}", server_def.args.join(" "))
    };
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Command")));
    let _ = out.writeln(format_args!("  {command}{args_display}"));
    let _ = out.writeln(format_args!(""));

    // ── 1b. Root markers ────────────────────────────────────────────
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
    // `server_binary_installed` is honest against the rust-analyzer rustup proxy
    // shim (misc 162): a bare proxy with no component behind it reads as NOT
    // installed, not a phantom `✓`.
    let _ = out.writeln(format_args!("{}:", out.colors.bold("Binary")));
    match crate::health::servers::resolve_binary(command) {
        Some(path) if crate::health::servers::server_binary_installed(server_name, command) => {
            let _ = out.writeln(format_args!(
                "  {} {}",
                out.colors.green("✓"),
                path.display()
            ));
        }
        Some(path) => {
            // The proxy shim exists, but the component behind it does not.
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors.red(&format!(
                    "✗ {command}: the rustup proxy at {} has no component behind it — \
                     run `rustup component add {command}`",
                    path.display(),
                )),
            ));
            return Ok(());
        }
        None => {
            let _ = out.writeln(format_args!(
                "  {}",
                out.colors.red(&format!("✗ {command}: command not found")),
            ));
            return Ok(());
        }
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

    // Start stderr reader task.
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
        server_name,
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
            let tools = crate::health::servers::extract_capabilities(
                &result["capabilities"],
                client.supports_type_hierarchy(),
            );
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

/// Return the user config file path if it exists on disk.
///
/// Uses `config_base` as the parent directory (e.g. `~/.config`).
fn user_config_path_in(config_base: Option<PathBuf>) -> Option<PathBuf> {
    let path = config_base?.join("catenary").join("config.toml");
    path.exists().then_some(path)
}

/// Collect actionable suggestions based on current config state.
///
/// `config_base` is the platform config directory (from [`crate::paths::config_dir`]).
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::fs;

    // ── user_config_path_in ─────────────────────────────────────────

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
        assert_eq!(result.expect("should find config file"), config_file);
    }

    // ── collect_suggestions ─────────────────────────────────────────

    #[test]
    fn suggestions_no_config_file() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let config = crate::config::Config::default();
        let suggestions = collect_suggestions(&config, Some(tmp.path().to_path_buf()));

        assert!(
            suggestions
                .iter()
                .any(|s| s.contains("No config file found"))
        );
        assert!(suggestions.iter().any(|s| s.contains("catenary config")));
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
                .any(|s| s.contains("No [commands] section"))
        );
        assert!(
            !suggestions
                .iter()
                .any(|s| s.contains("No config file found"))
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

        assert!(suggestions.is_empty());
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
        assert!(suggestions.iter().any(|s| s.contains(&expected_path)));
    }

    #[test]
    fn suggestions_none_base_falls_back() {
        let config = crate::config::Config::default();
        let suggestions = collect_suggestions(&config, None);
        assert!(
            suggestions
                .iter()
                .any(|s| s.contains("~/.config/catenary/config.toml"))
        );
    }

    // ── render_finding ──────────────────────────────────────────────

    #[test]
    fn render_finding_includes_message_and_fix_it() {
        use crate::health::{Finding, FindingCode, Severity};
        let finding = Finding::new(FindingCode::ServerRoutedBroken, Severity::Error, "boom")
            .with_fix_it("do the thing");
        let mut out = Output::buffer(80);
        render_finding(&mut out, &finding, false);
        let text = out.into_string();
        assert!(text.contains("boom"), "message rendered: {text}");
        assert!(text.contains("do the thing"), "fix-it rendered: {text}");
    }
}
