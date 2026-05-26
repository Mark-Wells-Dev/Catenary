// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Install command: install or update the Catenary plugin for a host CLI.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::Output;

/// Source for installation: local dev path or remote repo identifier.
#[derive(Debug, Clone)]
enum InstallSource {
    /// Local filesystem path (dev install via link/symlink).
    Local(PathBuf),
    /// Remote repository identifier (release install).
    Remote(String),
}

/// Parse a source string into an [`InstallSource`].
///
/// A path starting with `/`, `./`, or `~` is treated as local.
/// Everything else is a repo identifier.
fn parse_source(source: &str) -> InstallSource {
    if source.starts_with('/') || source.starts_with("./") || source.starts_with('~') {
        let expanded = source.strip_prefix('~').map_or_else(
            || PathBuf::from(source),
            |rest| {
                dirs::home_dir().map_or_else(
                    || PathBuf::from(source),
                    |h| h.join(rest.strip_prefix('/').unwrap_or(rest)),
                )
            },
        );
        InstallSource::Local(expanded)
    } else {
        InstallSource::Remote(source.to_string())
    }
}

/// Default repo identifier when no source is specified.
const DEFAULT_REPO: &str = "TwoWells/Catenary";

// ── Host detection ─────────────────────────────────────────────────

/// Check whether a binary can be found on `$PATH`.
fn binary_exists(command: &str) -> bool {
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(command))
        .any(|p| p.is_file())
}

/// Detected host with its install status.
struct HostStatus {
    name: &'static str,
    detected: bool,
    status: String,
}

/// Detect all hosts and their install status.
fn detect_hosts() -> Vec<HostStatus> {
    vec![detect_claude(), detect_gemini(), detect_antigravity()]
}

/// Detect Claude Code install status.
fn detect_claude() -> HostStatus {
    let detected = binary_exists("claude");
    let status = if detected {
        claude_install_status()
    } else {
        "not detected".to_string()
    };
    HostStatus {
        name: "claude",
        detected,
        status,
    }
}

/// Get Claude Code plugin install status from `installed_plugins.json`.
fn claude_install_status() -> String {
    let Some(home) = dirs::home_dir() else {
        return "unknown".to_string();
    };
    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(json_str) = std::fs::read_to_string(&plugins_file) else {
        return "not installed".to_string();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        return "not installed".to_string();
    };

    let entries = json
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array);

    match entries {
        Some(arr) if !arr.is_empty() => {
            let version = arr[0]
                .get("version")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            format!("installed (v{version})")
        }
        _ => "not installed".to_string(),
    }
}

/// Detect Gemini CLI install status.
fn detect_gemini() -> HostStatus {
    let detected = binary_exists("gemini");
    let status = if detected {
        gemini_install_status()
    } else {
        "not detected".to_string()
    };
    HostStatus {
        name: "gemini",
        detected,
        status,
    }
}

/// Get Gemini CLI extension install status.
fn gemini_install_status() -> String {
    let Some(home) = dirs::home_dir() else {
        return "unknown".to_string();
    };
    let ext_dir = home.join(".gemini/extensions");
    let ext_path = ["Catenary", "catenary"]
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir());

    let Some(ext_path) = ext_path else {
        return "not installed".to_string();
    };

    // Check install type
    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let install_meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = install_meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown");

    // Resolve manifest path for version
    let resolved = if install_type == "link" {
        install_meta
            .as_ref()
            .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
            .map_or_else(|| ext_path.clone(), PathBuf::from)
    } else {
        ext_path
    };

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

    version.map_or_else(
        || type_label.to_string(),
        |v| format!("{type_label} (v{v})"),
    )
}

/// Detect Antigravity CLI install status.
fn detect_antigravity() -> HostStatus {
    let agy_exists = binary_exists("agy");
    let global_dir_exists =
        dirs::home_dir().is_some_and(|h| h.join(".gemini/config/plugins/catenary").is_dir());
    let detected = agy_exists || global_dir_exists;

    let status = if !detected {
        "not detected".to_string()
    } else if global_dir_exists {
        "installed (global)".to_string()
    } else {
        "not installed".to_string()
    };

    HostStatus {
        name: "antigravity",
        detected,
        status,
    }
}

// ── List subcommand ────────────────────────────────────────────────

/// Run the bare `catenary install` command: list detected hosts.
///
/// # Errors
///
/// Returns an I/O error if output fails.
pub fn run_install_list(out: &mut Output) -> Result<()> {
    let _ = out.writeln(format_args!("Detected hosts:"));

    let hosts = detect_hosts();
    for host in &hosts {
        let name_col = format!("  {:<14}", host.name);
        let status = if host.detected {
            out.colors.green(&host.status)
        } else {
            out.colors.dim(&host.status)
        };
        let _ = out.writeln(format_args!("{name_col}{status}"));
    }

    Ok(())
}

// ── Claude Code install ────────────────────────────────────────────

/// Run `catenary install claude`.
///
/// # Errors
///
/// Returns an error if the Claude Code binary is not found or
/// plugin commands fail.
pub fn run_install_claude(out: &mut Output, source: Option<&str>, dry_run: bool) -> Result<()> {
    let _ = out.writeln(format_args!("Claude Code:"));

    if !binary_exists("claude") {
        let _ = out.writeln(format_args!(
            "  {}",
            out.colors.red("✗ `claude` not found on PATH"),
        ));
        return Ok(());
    }

    let parsed_source = source.map(parse_source);

    // Determine marketplace source to use
    let marketplace_source = match &parsed_source {
        Some(InstallSource::Local(path)) => path.display().to_string(),
        Some(InstallSource::Remote(repo)) => repo.clone(),
        None => read_claude_marketplace_source().unwrap_or_else(|| DEFAULT_REPO.to_string()),
    };

    // Step 1: Ensure marketplace is registered
    let needs_marketplace_update =
        parsed_source.is_some() || read_claude_marketplace_source().is_none();

    if !claude_ensure_marketplace(out, &marketplace_source, needs_marketplace_update, dry_run)? {
        return Ok(());
    }

    // Step 2: Uninstall existing plugin (if any) and reinstall
    claude_ensure_plugin(out, dry_run)
}

/// Ensure the Claude Code marketplace is registered.
///
/// Returns `Ok(true)` if the caller should proceed to plugin install,
/// `Ok(false)` if a non-recoverable error occurred.
fn claude_ensure_marketplace(
    out: &mut Output,
    source: &str,
    needs_update: bool,
    dry_run: bool,
) -> Result<bool> {
    if !needs_update {
        let _ = out.writeln(format_args!(
            "  {} marketplace registered ({source})",
            out.colors.green("✓"),
        ));
        return Ok(true);
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} marketplace add {source}",
            out.colors.dim("(dry-run)"),
        ));
        return Ok(true);
    }

    let status = Command::new("claude")
        .args(["plugin", "marketplace", "add", source])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .context("failed to run `claude plugin marketplace add`")?;

    if status.success() {
        let _ = out.writeln(format_args!(
            "  {} marketplace registered ({source})",
            out.colors.green("✓"),
        ));
        Ok(true)
    } else {
        let _ = out.writeln(format_args!(
            "  {} marketplace add failed",
            out.colors.red("✗"),
        ));
        Ok(false)
    }
}

/// Ensure the Claude Code plugin is installed (uninstall + reinstall cycle).
fn claude_ensure_plugin(out: &mut Output, dry_run: bool) -> Result<()> {
    let is_installed = claude_plugin_is_installed();

    if is_installed {
        if dry_run {
            let _ = out.writeln(format_args!(
                "  {} plugin uninstall catenary && plugin install catenary@catenary",
                out.colors.dim("(dry-run)"),
            ));
            return Ok(());
        }

        let uninstall = Command::new("claude")
            .args(["plugin", "uninstall", "catenary"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
            .context("failed to run `claude plugin uninstall`")?;

        if !uninstall.success() {
            let _ = out.writeln(format_args!(
                "  {} plugin uninstall failed",
                out.colors.red("✗"),
            ));
            return Ok(());
        }
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} plugin install catenary@catenary",
            out.colors.dim("(dry-run)"),
        ));
        return Ok(());
    }

    let install = Command::new("claude")
        .args(["plugin", "install", "catenary@catenary"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .context("failed to run `claude plugin install`")?;

    if install.success() {
        let verb = if is_installed {
            "reinstalled"
        } else {
            "installed"
        };
        let _ = out.writeln(format_args!("  {} plugin {verb}", out.colors.green("✓")));
    } else {
        let _ = out.writeln(format_args!(
            "  {} plugin install failed",
            out.colors.red("✗"),
        ));
    }

    Ok(())
}

/// Check if the catenary plugin is currently installed in Claude Code.
fn claude_plugin_is_installed() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(json_str) = std::fs::read_to_string(&plugins_file) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        return false;
    };

    json.get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|arr| !arr.is_empty())
}

/// Read the current Claude Code marketplace source for catenary.
fn read_claude_marketplace_source() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude/plugins/known_marketplaces.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&contents).ok()?;
    json.get("catenary")
        .and_then(|c| c.get("source"))
        .and_then(|s| s.get("source"))
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string)
}

// ── Gemini CLI install ─────────────────────────────────────────────

/// Run `catenary install gemini`.
///
/// # Errors
///
/// Returns an error if the Gemini CLI binary is not found or
/// extension commands fail.
pub fn run_install_gemini(out: &mut Output, source: Option<&str>, dry_run: bool) -> Result<()> {
    let _ = out.writeln(format_args!("Gemini CLI:"));

    if !binary_exists("gemini") {
        let _ = out.writeln(format_args!(
            "  {}",
            out.colors.red("✗ `gemini` not found on PATH"),
        ));
        return Ok(());
    }

    match source.map(parse_source) {
        Some(InstallSource::Local(path)) => {
            install_gemini_local(out, &path, dry_run)?;
        }
        Some(InstallSource::Remote(repo)) => {
            install_gemini_remote(out, &repo, dry_run)?;
        }
        None => {
            install_gemini_refresh(out, dry_run)?;
        }
    }

    Ok(())
}

/// Install Gemini extension from a local path (link).
fn install_gemini_local(out: &mut Output, path: &Path, dry_run: bool) -> Result<()> {
    let current = gemini_current_install();

    // If already linked to same path, no-op
    if let Some((install_type, source_path)) = &current {
        if install_type == "link" && source_path.as_deref() == Some(&*path.to_string_lossy()) {
            let _ = out.writeln(format_args!(
                "  {} linked → {}",
                out.colors.green("✓"),
                path.display(),
            ));
            return Ok(());
        }

        // Different source — uninstall first
        if dry_run {
            let _ = out.writeln(format_args!(
                "  {} extensions uninstall catenary",
                out.colors.dim("(dry-run)"),
            ));
        } else {
            let _ = Command::new("gemini")
                .args(["extensions", "uninstall", "catenary"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .status();
        }
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} extensions link {}",
            out.colors.dim("(dry-run)"),
            path.display(),
        ));
    } else {
        let status = Command::new("gemini")
            .args(["extensions", "link", &path.to_string_lossy()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
            .context("failed to run `gemini extensions link`")?;

        if status.success() {
            let _ = out.writeln(format_args!(
                "  {} linked → {}",
                out.colors.green("✓"),
                path.display(),
            ));
        } else {
            let _ = out.writeln(format_args!(
                "  {} extensions link failed",
                out.colors.red("✗"),
            ));
        }
    }

    Ok(())
}

/// Install Gemini extension from a remote repo.
fn install_gemini_remote(out: &mut Output, repo: &str, dry_run: bool) -> Result<()> {
    let current = gemini_current_install();

    if let Some((install_type, _)) = &current {
        if install_type == "link" {
            // Linked — need to uninstall and install from remote
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions uninstall catenary && extensions install {repo}",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let _ = Command::new("gemini")
                    .args(["extensions", "uninstall", "catenary"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status();

                let status = Command::new("gemini")
                    .args(["extensions", "install", repo])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions install`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension installed from {repo}",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions install failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        } else {
            // Already installed from remote — update
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions update catenary",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let status = Command::new("gemini")
                    .args(["extensions", "update", "catenary"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions update`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension updated",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions update failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        }
    } else if dry_run {
        let _ = out.writeln(format_args!(
            "  {} extensions install {repo}",
            out.colors.dim("(dry-run)"),
        ));
    } else {
        let status = Command::new("gemini")
            .args(["extensions", "install", repo])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
            .context("failed to run `gemini extensions install`")?;

        if status.success() {
            let _ = out.writeln(format_args!(
                "  {} extension installed from {repo}",
                out.colors.green("✓"),
            ));
        } else {
            let _ = out.writeln(format_args!(
                "  {} extensions install failed",
                out.colors.red("✗"),
            ));
        }
    }

    Ok(())
}

/// Refresh existing Gemini extension install (no source argument).
fn install_gemini_refresh(out: &mut Output, dry_run: bool) -> Result<()> {
    let current = gemini_current_install();

    match current {
        Some((install_type, source_path)) if install_type == "link" => {
            // Linked — always current, no-op
            let display = source_path.as_deref().unwrap_or("(unknown)");
            let _ = out.writeln(format_args!(
                "  {} linked → {display} (always current)",
                out.colors.green("✓"),
            ));
        }
        Some(_) => {
            // Installed from remote — update
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions update catenary",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let status = Command::new("gemini")
                    .args(["extensions", "update", "catenary"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions update`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension updated",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions update failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        }
        None => {
            // Not installed — install from default repo
            let repo = DEFAULT_REPO;
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions install {repo}",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let status = Command::new("gemini")
                    .args(["extensions", "install", repo])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions install`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension installed from {repo}",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions install failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Get current Gemini extension install info: (type, `source_path`).
fn gemini_current_install() -> Option<(String, Option<String>)> {
    let home = dirs::home_dir()?;
    let ext_dir = home.join(".gemini/extensions");
    let ext_path = ["Catenary", "catenary"]
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir())?;

    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown")
        .to_string();

    let source = meta
        .as_ref()
        .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
        .map(std::string::ToString::to_string);

    Some((install_type, source))
}

// ── Antigravity CLI install ────────────────────────────────────────

/// Embedded Antigravity plugin files.
const AGY_PLUGIN_JSON: &str = include_str!("../../plugins/catenary-antigravity/plugin.json");
const AGY_MCP_CONFIG: &str = include_str!("../../plugins/catenary-antigravity/mcp_config.json");
const AGY_HOOKS: &str = include_str!("../../plugins/catenary-antigravity/hooks.json");
const AGY_RULES: &str = include_str!("../../plugins/catenary-antigravity/rules/catenary.md");

/// Antigravity plugin file set for installation.
const AGY_FILES: &[(&str, &str)] = &[
    ("plugin.json", AGY_PLUGIN_JSON),
    ("mcp_config.json", AGY_MCP_CONFIG),
    ("hooks.json", AGY_HOOKS),
    ("rules/catenary.md", AGY_RULES),
];

/// Run `catenary install antigravity`.
///
/// # Errors
///
/// Returns an error if file operations fail.
pub fn run_install_antigravity(
    out: &mut Output,
    source: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let _ = out.writeln(format_args!("Antigravity CLI:"));

    let parsed_source = source.map(parse_source);

    let home = dirs::home_dir().context("cannot determine home directory")?;
    let target_dir = home.join(".gemini/config/plugins/catenary");

    match parsed_source {
        Some(InstallSource::Local(path)) => {
            install_antigravity_local(out, &path, &target_dir, dry_run)?;
        }
        Some(InstallSource::Remote(_)) | None => {
            install_antigravity_bundled(out, &target_dir, dry_run)?;
        }
    }

    Ok(())
}

/// Install Antigravity plugin via symlink to local path.
fn install_antigravity_local(
    out: &mut Output,
    source: &Path,
    target: &Path,
    dry_run: bool,
) -> Result<()> {
    // Source should point to the antigravity plugin directory
    let plugin_dir = source.join("plugins/catenary-antigravity");
    let source_dir = if plugin_dir.is_dir() {
        plugin_dir
    } else {
        source.to_path_buf()
    };

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} symlink {} → {}",
            out.colors.dim("(dry-run)"),
            target.display(),
            source_dir.display(),
        ));
        return Ok(());
    }

    // Remove existing directory or symlink
    if target.is_symlink() || target.is_dir() {
        if target.is_symlink() {
            std::fs::remove_file(target)
                .with_context(|| format!("remove existing symlink at {}", target.display()))?;
        } else {
            std::fs::remove_dir_all(target)
                .with_context(|| format!("remove existing directory at {}", target.display()))?;
        }
    }

    // Ensure parent directory exists
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory {}", parent.display()))?;
    }

    // Create symlink
    #[cfg(unix)]
    std::os::unix::fs::symlink(&source_dir, target)
        .with_context(|| format!("symlink {} → {}", target.display(), source_dir.display()))?;

    #[cfg(not(unix))]
    {
        anyhow::bail!("symlink install not supported on this platform");
    }

    let _ = out.writeln(format_args!(
        "  {} symlinked → {}",
        out.colors.green("✓"),
        source_dir.display(),
    ));

    Ok(())
}

/// Install Antigravity plugin by copying embedded files.
fn install_antigravity_bundled(out: &mut Output, target: &Path, dry_run: bool) -> Result<()> {
    // Check if target is a symlink — that means dev install, don't overwrite
    if target.is_symlink() {
        let link_target = std::fs::read_link(target).unwrap_or_default();
        let _ = out.writeln(format_args!(
            "  {} symlinked → {} (use explicit source to switch)",
            out.colors.green("✓"),
            link_target.display(),
        ));
        return Ok(());
    }

    // Check staleness
    let mut stale_count = 0;
    let mut up_to_date = true;

    for (rel_path, expected_content) in AGY_FILES {
        let file_path = target.join(rel_path);
        match std::fs::read_to_string(&file_path) {
            Ok(current) if current == *expected_content => {}
            _ => {
                stale_count += 1;
                up_to_date = false;
            }
        }
    }

    if up_to_date {
        let _ = out.writeln(format_args!(
            "  {} plugin files up to date",
            out.colors.green("✓"),
        ));
        return Ok(());
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} write {stale_count} file(s) to {}",
            out.colors.dim("(dry-run)"),
            target.display(),
        ));
        return Ok(());
    }

    // Write files
    for (rel_path, content) in AGY_FILES {
        let file_path = target.join(rel_path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        std::fs::write(&file_path, content)
            .with_context(|| format!("write {}", file_path.display()))?;
    }

    let _ = out.writeln(format_args!(
        "  {} wrote {stale_count} file(s) to {}",
        out.colors.green("✓"),
        target.display(),
    ));

    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Source parsing ──────────────────────────────────────────────

    #[test]
    fn parse_source_absolute_path() {
        let src = parse_source("/home/user/Projects/Catenary");
        let InstallSource::Local(p) = src else {
            unreachable!("expected Local source");
        };
        assert_eq!(p, PathBuf::from("/home/user/Projects/Catenary"));
    }

    #[test]
    fn parse_source_relative_path() {
        let src = parse_source("./Catenary");
        let InstallSource::Local(p) = src else {
            unreachable!("expected Local source");
        };
        assert_eq!(p, PathBuf::from("./Catenary"));
    }

    #[test]
    fn parse_source_repo_identifier() {
        let src = parse_source("TwoWells/Catenary");
        let InstallSource::Remote(r) = src else {
            unreachable!("expected Remote source");
        };
        assert_eq!(r, "TwoWells/Catenary");
    }

    // ── Antigravity bundled files ──────────────────────────────────

    #[test]
    fn embedded_antigravity_files_not_empty() {
        for (name, content) in AGY_FILES {
            assert!(
                !content.is_empty(),
                "embedded file {name} should not be empty",
            );
        }
    }

    #[test]
    fn antigravity_bundled_creates_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_bundled(&mut out, &target, false).expect("install should succeed");

        for (rel_path, expected) in AGY_FILES {
            let file = target.join(rel_path);
            let content = std::fs::read_to_string(&file).expect("installed file should exist");
            assert_eq!(content, *expected, "{rel_path} content should match");
        }

        let output = out.into_string();
        assert!(output.contains("wrote"), "output: {output}");
    }

    #[test]
    fn antigravity_bundled_idempotent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_bundled(&mut out, &target, false).expect("first install");

        let mut out2 = Output::buffer(80);
        install_antigravity_bundled(&mut out2, &target, false).expect("second install");
        let output = out2.into_string();
        assert!(output.contains("up to date"), "output: {output}");
    }

    #[test]
    fn antigravity_bundled_dry_run_no_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_bundled(&mut out, &target, true).expect("dry run");

        assert!(!target.exists(), "dry run should not create directory");
        let output = out.into_string();
        assert!(output.contains("(dry-run)"), "output: {output}");
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_local_creates_symlink() {
        let source_dir = tempfile::tempdir().expect("create source dir");
        let target_dir = tempfile::tempdir().expect("create target dir");
        let target = target_dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_local(&mut out, source_dir.path(), &target, false)
            .expect("symlink install should succeed");

        assert!(target.is_symlink(), "target should be a symlink");
        let output = out.into_string();
        assert!(output.contains("symlinked"), "output: {output}");
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_local_replaces_directory_with_symlink() {
        let source_dir = tempfile::tempdir().expect("create source dir");
        let target_dir = tempfile::tempdir().expect("create target dir");
        let target = target_dir.path().join("catenary");

        // Create existing directory
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(target.join("old_file.txt"), "old").expect("write old file");

        let mut out = Output::buffer(80);
        install_antigravity_local(&mut out, source_dir.path(), &target, false)
            .expect("symlink install should replace dir");

        assert!(target.is_symlink(), "target should be a symlink");
    }

    // ── List subcommand ────────────────────────────────────────────

    #[test]
    fn install_list_output() {
        let mut out = Output::buffer(80);
        run_install_list(&mut out).expect("list should succeed");
        let output = out.into_string();
        assert!(output.contains("Detected hosts:"), "output: {output}");
    }
}
