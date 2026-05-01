// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Doctor command: check language server health and hook configuration.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::cli::ColorConfig;
use crate::lsp;

/// Expected Claude Code hooks, embedded at compile time.
const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../../plugins/catenary/hooks/hooks.json");

/// Expected Gemini CLI hooks, embedded at compile time.
const GEMINI_HOOKS_EXPECTED: &str = include_str!("../../hooks/hooks.json");

/// Migration guidance for users who still have the legacy Python script configured.
const CONSTRAINED_BASH_MIGRATION: &str = "Command filtering is now built into `catenary hook pre-tool`. \
     Remove the constrained_bash.py hook from your settings and use \
     `[commands]` in your Catenary config instead. \
     Run `catenary config` to generate a recommended template.";

/// Run the doctor command: check all configured language servers.
///
/// # Errors
///
/// Returns an error if the configuration cannot be loaded.
#[allow(
    clippy::too_many_lines,
    reason = "Doctor command has sequential output logic"
)]
pub async fn run_doctor(project_root: &Path, nocolor: bool, show_diff: bool) -> Result<()> {
    let colors = ColorConfig::new(nocolor);

    // Print version header
    println!("Catenary {}", env!("CATENARY_VERSION"));
    println!();

    // Check config sources for old-format entries before loading
    doctor_check_config(&colors);

    // Load configuration — report errors inline instead of bailing
    let config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("{}", colors.red(&format!("✗ Config error: {e:#}")));
            println!();
            return Ok(());
        }
    };

    // Print config header
    let config_source = std::env::var("CATENARY_CONFIG")
        .ok()
        .unwrap_or_else(|| "default paths".to_string());
    println!("{} {}", colors.bold("Config:"), config_source);
    println!();

    // Validation errors
    let validation_errors = config.validate();
    for err in &validation_errors {
        println!("{}", colors.red(&format!("✗  {err}")));
    }

    // Unreferenced server warnings
    let referenced: HashSet<&str> = config
        .language
        .values()
        .flat_map(|lc| lc.servers.iter().map(|b| b.name.as_str()))
        .collect();
    let mut unreferenced: Vec<&str> = config
        .server
        .keys()
        .filter(|name| !referenced.contains(name.as_str()))
        .map(String::as_str)
        .collect();
    unreferenced.sort_unstable();
    for name in &unreferenced {
        println!(
            "{}",
            colors.yellow(&format!(
                "⚠  Server '{name}' is defined but not referenced by any [language.*] entry"
            )),
        );
    }

    // Duplicate extension warnings
    let dup_exts =
        crate::bridge::filesystem_manager::ClassificationTables::find_duplicate_extensions(&config);
    for (ext, first, second) in &dup_exts {
        println!(
            "{}",
            colors.yellow(&format!(
                "⚠  Extension '.{ext}' claimed by both [language.{first}] and \
                 [language.{second}] — first wins"
            )),
        );
    }

    if !validation_errors.is_empty() || !unreferenced.is_empty() || !dup_exts.is_empty() {
        println!();
    }

    // ── Project config section ──────────────────────────────────────
    doctor_check_project_config(&colors, project_root, &config);

    if config.language.is_empty() && config.server.is_empty() {
        println!("No language servers configured.");
        return Ok(());
    }

    // ── Servers section ──────────────────────────────────────────────
    // Spawn each unique server once, collect capabilities.
    let mut server_names: Vec<&str> = config.server.keys().map(String::as_str).collect();
    server_names.sort_unstable();

    let max_server_width = server_names.iter().map(|n| n.len()).max().unwrap_or(10);

    println!("{}:", colors.bold("Servers"));
    let mut server_capabilities: std::collections::HashMap<&str, Vec<&'static str>> =
        std::collections::HashMap::new();
    for name in &server_names {
        let server_def = &config.server[*name];
        let command = server_def.command.as_str();
        let name_display = format!("  {name:<max_server_width$}");

        if !binary_exists(command) {
            println!(
                "{name_display}  {}",
                colors.red(&format!("✗ {command}: command not found")),
            );
            continue;
        }

        let args_refs: Vec<&str> = server_def.args.iter().map(String::as_str).collect();
        let spawn_result = lsp::LspClient::spawn_quiet(
            command,
            &args_refs,
            name,
            name,
            crate::logging::LoggingServer::new(),
            server_def.env.as_ref(),
        );

        let mut client = match spawn_result {
            Ok(client) => client,
            Err(e) => {
                println!(
                    "{name_display}  {}",
                    colors.red(&format!("✗ spawn failed: {e}")),
                );
                continue;
            }
        };

        match client
            .initialize(&[], server_def.initialization_options.clone())
            .await
        {
            Ok(result) => {
                let tools =
                    extract_capabilities(&result["capabilities"], client.supports_type_hierarchy());
                let status = if server_def.file_patterns.is_empty() {
                    "✓ ready".to_string()
                } else {
                    format!(
                        "✓ ready  file_patterns: [{}]",
                        server_def
                            .file_patterns
                            .iter()
                            .map(|p| format!("\"{p}\""))
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                };
                println!("{name_display}  {}", colors.green(&status));
                server_capabilities.insert(name, tools);
            }
            Err(e) => {
                println!(
                    "{name_display}  {}",
                    colors.red(&format!("✗ initialize failed: {e}")),
                );
            }
        }

        let _ = client.shutdown().await;
    }

    // ── Languages section ────────────────────────────────────────────
    println!();
    println!("{}:", colors.bold("Languages"));

    // Build sorted list of (language, server_name) pairs
    let mut lang_entries: Vec<(&str, &str)> = Vec::new();
    for (lang, lc) in &config.language {
        if let Some(binding) = lc.servers.first() {
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
        println!("{lang_display}  → {target}");
        // Show capabilities from the server, indented
        if let Some(tools) = server_capabilities.get(target)
            && !tools.is_empty()
        {
            println!(
                "{}    {}",
                " ".repeat(max_lang_width + 2),
                colors.dim(&tools.join(" ")),
            );
        }
    }

    // Hooks health section
    println!();
    println!("{}:", colors.bold("Hooks"));
    check_claude_hooks(&colors, show_diff);
    check_gemini_hooks(&colors, show_diff);
    check_path_binary(&colors);

    // Legacy script migration warnings
    println!();
    println!("{}:", colors.bold("Command filter"));
    check_constrained_bash_claude(&colors);
    check_constrained_bash_gemini(&colors);
    check_command_filter_config(&colors, &config);

    // Actionable suggestions at the very bottom so they aren't buried
    let suggestions = collect_suggestions(&config, dirs::config_dir());
    if !suggestions.is_empty() {
        println!();
        println!("{}:", colors.bold("Suggestions"));
        for suggestion in &suggestions {
            println!("  {}", colors.dim(suggestion));
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
    server_name: &str,
    project_root: &Path,
    nocolor: bool,
) -> Result<()> {
    let colors = ColorConfig::new(nocolor);

    println!("Catenary {}", env!("CATENARY_VERSION"));
    println!();

    let config = match crate::config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("{}", colors.red(&format!("✗ Config error: {e:#}")));
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
        println!(
            "{}\n",
            colors.red(&format!("✗ Unknown server: '{server_name}'")),
        );
        let mut available: Vec<&str> = merged_config.server.keys().map(String::as_str).collect();
        available.sort_unstable();
        println!("Available servers:");
        for name in &available {
            println!("  {name}");
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
    println!("{}:", colors.bold("Command"));
    println!("  {command}{args_display}");
    println!();

    // ── 2. Binary check ────────────────────────────────────────────
    println!("{}:", colors.bold("Binary"));
    if let Some(path) = resolve_binary(command) {
        println!("  {} {}", colors.green("✓"), path.display());
    } else {
        println!(
            "  {}",
            colors.red(&format!("✗ {command}: command not found")),
        );
        return Ok(());
    }
    println!();

    // ── 3. Spawn ──────────────────────────────────────────────────
    println!("{}:", colors.bold("Spawn"));
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
            println!("  {} process started", colors.green("✓"));
            pair
        }
        Err(e) => {
            println!("  {}", colors.red(&format!("✗ spawn failed: {e}")));
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

    println!();

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

    println!("{}:", colors.bold("Initialize request"));
    if let Ok(pretty) = serde_json::to_string_pretty(&init_params) {
        for line in pretty.lines() {
            println!("  {line}");
        }
    }
    println!();

    println!("{}:", colors.bold("Initialize response"));
    match client
        .initialize(&resolved_roots, server_def.initialization_options.clone())
        .await
    {
        Ok(result) => {
            if let Ok(pretty) = serde_json::to_string_pretty(&result) {
                for line in pretty.lines() {
                    println!("  {line}");
                }
            }
            println!();

            // ── 5. Capabilities summary ─────────────────────────────
            let tools =
                extract_capabilities(&result["capabilities"], client.supports_type_hierarchy());
            println!("{}:", colors.bold("Capabilities"));
            if tools.is_empty() {
                println!("  {}", colors.dim("(none)"));
            } else {
                for tool in &tools {
                    println!("  {} {tool}", colors.green("✓"));
                }
            }
        }
        Err(e) => {
            println!("  {}", colors.red(&format!("✗ initialize failed: {e}")));
        }
    }

    // ── 6. Shutdown ────────────────────────────────────────────────
    let _ = client.shutdown().await;
    println!();

    // ── 7. Server stderr ───────────────────────────────────────────
    if let Some(task) = stderr_task {
        // Give the task a moment to finish collecting output
        let lines = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();

        if !lines.is_empty() {
            println!("{}:", colors.bold("Server stderr"));
            for line in &lines {
                println!("  {line}");
            }
            if lines.len() >= STDERR_MAX_LINES {
                println!(
                    "  {}",
                    colors.dim(&format!("(truncated at {STDERR_MAX_LINES} lines)"))
                );
            }
            println!();
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
fn doctor_check_config(colors: &ColorConfig) {
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
                    print_migration(colors, source, key, entry_table, true);
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
                        println!(
                            "{}",
                            colors.yellow(&format!(
                                "⚠  {}: [language.{key}] uses removed `inherit` field — \
                                 copy `servers` list from [language.{target}] into \
                                 [language.{key}] instead.",
                                source.display(),
                            )),
                        );
                    }

                    // Intermediate format: inline server definition fields
                    let has_server_fields = crate::config::SERVER_DEF_KEYS
                        .iter()
                        .any(|k| entry_table.contains_key(*k));
                    if has_server_fields {
                        found_issues = true;
                        print_migration(colors, source, key, entry_table, false);
                    }
                }
            }
        }

        // [commands] entries with old denylist-format fields
        if let Some(cmd_table) = raw.get("commands").and_then(toml::Value::as_table) {
            if cmd_table.contains_key("deny_when_first") {
                found_issues = true;
                println!(
                    "{}",
                    colors.yellow(&format!(
                        "⚠  {}: [commands] uses removed `deny_when_first` field — \
                         Catenary now uses an allowlist model. \
                         Run `catenary config` for the recommended template.",
                        source.display(),
                    )),
                );
            }

            if let Some(deny_table) = cmd_table.get("deny").and_then(toml::Value::as_table) {
                for (key, value) in deny_table {
                    if value.is_str() {
                        found_issues = true;
                        println!(
                            "{}",
                            colors.yellow(&format!(
                                "⚠  {}: [commands.deny.{key}] has a string value — \
                                 the old guidance-string format is removed. `deny` now \
                                 maps commands to arrays of denied subcommands \
                                 (e.g., `git = [\"grep\", \"ls-files\"]`).",
                                source.display(),
                            )),
                        );
                        break; // One message per file is enough.
                    }
                }
            }
        }
    }

    if found_issues {
        println!();
    }
}

/// Check a project root for `.catenary.toml` and validate its contents.
///
/// Reports unsupported sections, parse errors, and orphan server definitions.
/// Called with `--root` (defaults to cwd).
fn doctor_check_project_config(
    colors: &ColorConfig,
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

    println!(
        "{} {}",
        colors.bold("Project config:"),
        config_path.display(),
    );

    match crate::config::load_project_config(&resolved) {
        Ok(Some(pc)) => {
            // Count entries
            let lang_count = pc.language.len();
            let server_count = pc.server.len();
            println!(
                "  {}",
                colors.green(&format!(
                    "✓ {lang_count} language{}, {server_count} server{}",
                    if lang_count == 1 { "" } else { "s" },
                    if server_count == 1 { "" } else { "s" },
                )),
            );

            // Orphan server warnings
            for (server_name, server_def) in &pc.server {
                if server_def.command.is_empty() {
                    continue;
                }

                let referenced_by_project = pc
                    .language
                    .values()
                    .any(|lc| lc.servers.iter().any(|b| b.name == *server_name));

                let referenced_by_user = user_config
                    .language
                    .values()
                    .any(|lc| lc.servers.iter().any(|b| b.name == *server_name));

                if !referenced_by_project && !referenced_by_user {
                    println!(
                        "  {}",
                        colors.yellow(&format!(
                            "⚠  [server.{server_name}] has a `command` but no \
                             [language.*] references it"
                        )),
                    );
                }
            }

            // Server ref validation — project language refs must resolve
            // against the combined (user + project) server set.
            for (lang_key, lang_config) in &pc.language {
                for binding in &lang_config.servers {
                    if !pc.server.contains_key(&binding.name)
                        && !user_config.server.contains_key(&binding.name)
                    {
                        println!(
                            "  {}",
                            colors.red(&format!(
                                "✗  [language.{lang_key}] references server '{}', \
                                 but no [server.{}] is defined in project or user config",
                                binding.name, binding.name,
                            )),
                        );
                    }
                }
            }
        }
        Ok(None) => {} // No project config — already handled by the exists check above.
        Err(e) => {
            println!(
                "  {}",
                colors.red(&format!("✗ {}: {e:#}", config_path.display())),
            );
        }
    }

    println!();
}

/// Print migration guidance for a single old-format entry.
fn print_migration(
    colors: &ColorConfig,
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
    println!(
        "{}",
        colors.yellow(&format!(
            "⚠  {}: [{section}.{key}] uses old format — migrate to [language.*] + [server.*]:",
            source.display(),
        )),
    );

    // Determine server name from command, falling back to the key
    let server_name = entry
        .get("command")
        .and_then(toml::Value::as_str)
        .unwrap_or(key);

    // Build old-format display
    println!();
    println!("  Old:");
    println!("    [{section}.{key}]");
    for (k, v) in entry {
        println!("    {k} = {v}");
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

    println!();
    println!("  New:");
    println!("    [language.{key}]");
    println!("    servers = [\"{server_name}\"]");
    for (k, v) in &lang_fields {
        println!("    {k} = {v}");
    }
    println!();
    println!("    [server.{server_name}]");
    for (k, v) in &server_fields {
        println!("    {k} = {v}");
    }
    println!();
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
fn check_claude_hooks(colors: &ColorConfig, show_diff: bool) {
    let label = format!("{:<14}", "Claude Code");
    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory"),
        );
        return;
    };
    let home = PathBuf::from(home_str);

    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(plugins_json) = std::fs::read_to_string(&plugins_file) else {
        println!("  {label}{}", colors.dim("- not installed"));
        return;
    };

    let Ok(plugins) = serde_json::from_str::<serde_json::Value>(&plugins_json) else {
        println!(
            "  {label}{}",
            colors.yellow("? cannot parse installed_plugins.json"),
        );
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
            println!("  {label}{}", colors.dim("- not installed"));
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
        println!(
            "  {label}{version:<8}{}",
            colors.yellow("? missing installPath"),
        );
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
                println!("  {label}{ver_col}{}", colors.green("✓ hooks match"));
            } else {
                println!(
                    "  {label}{ver_col}{}",
                    colors.red("✗ stale hooks (reinstall: claude plugin uninstall catenary@catenary && claude plugin install catenary@catenary)"),
                );
                if show_diff {
                    show_unified_diff(
                        &pretty_json(&installed),
                        &pretty_json(CLAUDE_HOOKS_EXPECTED),
                        "installed",
                        "expected",
                    );
                }
            }
        }
        Err(_) => {
            println!(
                "  {label}{ver_col}{}",
                colors.red("✗ hooks.json not found in plugin cache"),
            );
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
fn check_gemini_hooks(colors: &ColorConfig, show_diff: bool) {
    let label = format!("{:<14}", "Gemini CLI");
    let Ok(home_str) = std::env::var("HOME") else {
        println!(
            "  {label}{}",
            colors.dim("- cannot determine home directory"),
        );
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
        println!("  {label}{}", colors.dim("- not installed"));
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
                println!("  {label}{ver_col}{}", colors.green("✓ hooks match"));
            } else {
                println!(
                    "  {label}{ver_col}{}",
                    colors.red("✗ stale hooks (update extension)"),
                );
                if show_diff {
                    show_unified_diff(
                        &pretty_json(&installed),
                        &pretty_json(GEMINI_HOOKS_EXPECTED),
                        "installed",
                        "expected",
                    );
                }
            }
        }
        Err(_) => {
            println!(
                "  {label}{ver_col}{}",
                colors.yellow("? hooks.json not found"),
            );
        }
    }
}

/// Check whether the running binary matches what `$PATH` would resolve.
fn check_path_binary(colors: &ColorConfig) {
    let label = format!("{:<14}", "PATH");
    let spacer = " ".repeat(20);

    let Some(current_exe) = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
    else {
        println!(
            "  {label}{}",
            colors.yellow("? cannot determine current executable"),
        );
        return;
    };

    // Find catenary on PATH
    let path_var = std::env::var("PATH").unwrap_or_default();
    let Some(path_binary) = std::env::split_paths(&path_var)
        .map(|dir| dir.join("catenary"))
        .find(|p| p.is_file())
    else {
        println!(
            "  {label}{spacer}{}",
            colors.red("✗ catenary not found on PATH"),
        );
        return;
    };

    let resolved_path = std::fs::canonicalize(&path_binary).unwrap_or(path_binary);

    if current_exe == resolved_path {
        println!(
            "  {label}{spacer}{}",
            colors.green(&format!("✓ {}", resolved_path.display())),
        );
    } else {
        println!(
            "  {label}{spacer}{}",
            colors.red(&format!(
                "✗ {} differs from {}",
                resolved_path.display(),
                current_exe.display(),
            )),
        );
    }
}

/// Check whether `~/.claude/settings.json` still references the legacy Python script.
///
/// If found, warns the user to remove it and migrate to `[commands]` config.
fn check_constrained_bash_claude(colors: &ColorConfig) {
    check_legacy_script(colors, "Claude Code", ".claude/settings.json");
}

/// Check whether `~/.gemini/settings.json` still references the legacy Python script.
///
/// If found, warns the user to remove it and migrate to `[commands]` config.
fn check_constrained_bash_gemini(colors: &ColorConfig) {
    check_legacy_script(colors, "Gemini CLI", ".gemini/settings.json");
}

/// Check a host CLI settings file for references to the legacy `constrained_bash.py`.
fn check_legacy_script(colors: &ColorConfig, client: &str, settings_rel: &str) {
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
        println!(
            "  {label}{}",
            colors.yellow("⚠  legacy constrained_bash.py detected"),
        );
        println!(
            "  {label}{}",
            colors.dim(&format!("  {CONSTRAINED_BASH_MIGRATION}")),
        );
    }
}

/// Report the status of the built-in command filter configuration.
fn check_command_filter_config(colors: &ColorConfig, config: &crate::config::Config) {
    match &config.resolved_commands {
        Some(resolved) if resolved.client_enforcement_only => {
            println!(
                "  {}",
                colors.dim("client_enforcement_only — Catenary enforcement disabled"),
            );
        }
        Some(resolved) if resolved.is_active() => {
            let total = resolved.allow.len() + resolved.pipeline.len();
            println!(
                "  {}",
                colors.green(&format!(
                    "✓ {total} command{} allowed{}",
                    if total == 1 { "" } else { "s" },
                    resolved.default_build.as_ref().map_or_else(
                        || {
                            if resolved.build.is_empty() {
                                String::new()
                            } else {
                                let mut tools: Vec<&str> = resolved
                                    .build
                                    .values()
                                    .map(String::as_str)
                                    .collect::<std::collections::HashSet<_>>()
                                    .into_iter()
                                    .collect();
                                tools.sort_unstable();
                                format!(
                                    ", build tool{}: {}",
                                    if tools.len() == 1 { "" } else { "s" },
                                    tools.join(", ")
                                )
                            }
                        },
                        |b| format!(", build tool: {b}"),
                    ),
                )),
            );
        }
        Some(_) | None => {
            println!(
                "  {}",
                colors.dim("no [commands] section — all shell commands allowed"),
            );
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
fn show_unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    print!(
        "{}",
        diff.unified_diff()
            .context_radius(3)
            .header(old_label, new_label)
    );
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
}
