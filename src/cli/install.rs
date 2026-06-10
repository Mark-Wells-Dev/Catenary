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
    vec![
        detect_claude(),
        detect_gemini(),
        detect_antigravity(),
        detect_opencode(),
    ]
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

/// Detect OpenCode install status.
fn detect_opencode() -> HostStatus {
    let binary = binary_exists("opencode");
    let plugin = dirs::home_dir().map(|h| h.join(".config/opencode/plugin/catenary.js"));
    let linked = plugin.as_ref().is_some_and(|p| p.is_symlink());
    // `is_file()` follows symlinks, so check `linked` first to distinguish.
    let bundled = plugin
        .as_ref()
        .is_some_and(|p| !p.is_symlink() && p.is_file());
    let detected = binary || linked || bundled;

    let status = if !detected {
        "not detected".to_string()
    } else if linked {
        "linked (global)".to_string()
    } else if bundled {
        "installed (global)".to_string()
    } else {
        "not installed".to_string()
    };

    HostStatus {
        name: "opencode",
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

// ── OpenCode install ───────────────────────────────────────────────

/// Embedded OpenCode plugin files.
const OC_PLUGIN_JS: &str = include_str!("../../plugins/catenary-opencode/catenary.js");
const OC_RULES: &str = include_str!("../../plugins/catenary-opencode/catenary.md");

/// Resolved install targets for OpenCode. Unlike the command-hook hosts,
/// OpenCode's plugin and rules land in different places, and the MCP heartbeat
/// + rules reference merge into a user-owned `opencode.json`.
struct OpenCodeTargets {
    /// Auto-discovered plugin file (`plugin/catenary.js`).
    plugin: PathBuf,
    /// Catenary-owned agent rules (`catenary.md`). Referenced from
    /// `opencode.json`'s `instructions` array rather than written into a
    /// user-authored `AGENTS.md` — matching how every other host ships its own
    /// rules file.
    rules: PathBuf,
    /// User-owned config the MCP heartbeat + rules reference merge into
    /// (`opencode.json`).
    config: PathBuf,
    /// The rules path written into `opencode.json`'s `instructions` array.
    /// OpenCode resolves instruction paths relative to the config file's
    /// directory, so this is relative to `config`'s parent (no home-path leak,
    /// portable across machines).
    instructions: &'static str,
}

/// Resolve OpenCode install targets for the global (`~/.config/opencode/`) or
/// workspace (`<cwd>/.opencode/`) location.
fn opencode_targets(workspace: bool) -> Result<OpenCodeTargets> {
    if workspace {
        let root = std::env::current_dir().context("cannot determine current directory")?;
        Ok(OpenCodeTargets {
            plugin: root.join(".opencode/plugin/catenary.js"),
            rules: root.join(".opencode/catenary.md"),
            config: root.join("opencode.json"),
            // Config dir is `<root>`; rules live under `.opencode/`.
            instructions: ".opencode/catenary.md",
        })
    } else {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        let base = home.join(".config/opencode");
        Ok(OpenCodeTargets {
            plugin: base.join("plugin/catenary.js"),
            rules: base.join("catenary.md"),
            config: base.join("opencode.json"),
            // Config dir is `~/.config/opencode`; rules sit beside the config.
            instructions: "catenary.md",
        })
    }
}

/// Run `catenary install opencode [--workspace]`.
///
/// Writes (or symlinks) the auto-discovered plugin file, writes the
/// Catenary-owned rules file, and merges the load-bearing MCP heartbeat plus
/// the rules `instructions` reference into `opencode.json` — never touching a
/// user-authored `AGENTS.md`.
///
/// # Errors
///
/// Returns an error if file operations fail or `opencode.json` is malformed.
pub fn run_install_opencode(
    out: &mut Output,
    source: Option<&str>,
    workspace: bool,
    dry_run: bool,
) -> Result<()> {
    let _ = out.writeln(format_args!("OpenCode:"));

    let targets = opencode_targets(workspace)?;

    match source.map(parse_source) {
        Some(InstallSource::Local(path)) => {
            install_opencode_local(out, &path, &targets, dry_run)?;
        }
        Some(InstallSource::Remote(_)) | None => {
            install_opencode_bundled(out, &targets, dry_run)?;
        }
    }

    // Rules and the config merge are independent of the plugin source mode. The
    // rules file is Catenary-owned (written like the plugin); the config merge
    // is the one required write to the user's `opencode.json` in every mode —
    // it carries both the MCP heartbeat and the rules `instructions` pointer.
    install_opencode_rules(out, &targets.rules, dry_run)?;
    merge_opencode_config(out, &targets.config, targets.instructions, dry_run)
}

/// Install the OpenCode plugin by symlinking to a local path (dev mode).
fn install_opencode_local(
    out: &mut Output,
    source: &Path,
    targets: &OpenCodeTargets,
    dry_run: bool,
) -> Result<()> {
    // Resolve the source plugin file: a repo root, the plugin dir, or the file.
    let in_repo = source.join("plugins/catenary-opencode/catenary.js");
    let in_dir = source.join("catenary.js");
    let source_file = if in_repo.is_file() {
        in_repo
    } else if in_dir.is_file() {
        in_dir
    } else {
        source.to_path_buf()
    };

    let target = &targets.plugin;

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} symlink {} → {}",
            out.colors.dim("(dry-run)"),
            target.display(),
            source_file.display(),
        ));
        return Ok(());
    }

    // Remove any existing plugin file or symlink.
    if target.is_symlink() || target.exists() {
        std::fs::remove_file(target)
            .with_context(|| format!("remove existing plugin at {}", target.display()))?;
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create plugin directory {}", parent.display()))?;
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&source_file, target)
        .with_context(|| format!("symlink {} → {}", target.display(), source_file.display()))?;

    #[cfg(not(unix))]
    {
        anyhow::bail!("symlink install not supported on this platform");
    }

    let _ = out.writeln(format_args!(
        "  {} plugin symlinked → {}",
        out.colors.green("✓"),
        source_file.display(),
    ));

    Ok(())
}

/// Install the OpenCode plugin by writing the embedded file (bundled mode).
fn install_opencode_bundled(
    out: &mut Output,
    targets: &OpenCodeTargets,
    dry_run: bool,
) -> Result<()> {
    let target = &targets.plugin;

    // A symlink means a dev install — don't overwrite it.
    if target.is_symlink() {
        let link_target = std::fs::read_link(target).unwrap_or_default();
        let _ = out.writeln(format_args!(
            "  {} plugin symlinked → {} (use explicit source to switch)",
            out.colors.green("✓"),
            link_target.display(),
        ));
        return Ok(());
    }

    let up_to_date = std::fs::read_to_string(target).is_ok_and(|c| c == OC_PLUGIN_JS);
    if up_to_date {
        let _ = out.writeln(format_args!(
            "  {} plugin up to date",
            out.colors.green("✓"),
        ));
        return Ok(());
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} write plugin to {}",
            out.colors.dim("(dry-run)"),
            target.display(),
        ));
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create plugin directory {}", parent.display()))?;
    }
    std::fs::write(target, OC_PLUGIN_JS).with_context(|| format!("write {}", target.display()))?;

    let _ = out.writeln(format_args!(
        "  {} wrote plugin to {}",
        out.colors.green("✓"),
        target.display(),
    ));

    Ok(())
}

/// Write the Catenary-owned rules file (`catenary.md`). Unlike a user-authored
/// `AGENTS.md`, this file belongs to Catenary, so it is written and
/// staleness-checked like the plugin — a symlink (dev install) is left alone.
/// It is surfaced to the agent via `opencode.json`'s `instructions` array
/// (see [`merge_opencode_config`]), so the user's own rules files are untouched.
fn install_opencode_rules(out: &mut Output, rules: &Path, dry_run: bool) -> Result<()> {
    // A symlink means a dev install — don't overwrite it.
    if rules.is_symlink() {
        let link_target = std::fs::read_link(rules).unwrap_or_default();
        let _ = out.writeln(format_args!(
            "  {} rules symlinked → {}",
            out.colors.green("✓"),
            link_target.display(),
        ));
        return Ok(());
    }

    let up_to_date = std::fs::read_to_string(rules).is_ok_and(|c| c == OC_RULES);
    if up_to_date {
        let _ = out.writeln(format_args!("  {} rules up to date", out.colors.green("✓")));
        return Ok(());
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} write rules to {}",
            out.colors.dim("(dry-run)"),
            rules.display(),
        ));
        return Ok(());
    }

    if let Some(parent) = rules.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create rules directory {}", parent.display()))?;
    }
    std::fs::write(rules, OC_RULES).with_context(|| format!("write {}", rules.display()))?;

    let _ = out.writeln(format_args!(
        "  {} wrote rules to {}",
        out.colors.green("✓"),
        rules.display(),
    ));

    Ok(())
}

/// Desired `mcp.catenary` entry for `opencode.json`. The persistent MCP
/// connection is the long-lived client that keeps the daemon + warm LSP pool
/// alive for the session (the daemon exits on last client disconnect).
fn opencode_mcp_entry() -> serde_json::Value {
    serde_json::json!({
        "type": "local",
        "command": ["catenary"],
        "enabled": true,
    })
}

/// Merge the MCP heartbeat and the rules `instructions` reference into
/// `opencode.json` without clobbering other keys or instructions. Creates the
/// file (with `$schema`) if absent.
fn merge_opencode_config(
    out: &mut Output,
    config: &Path,
    instructions: &str,
    dry_run: bool,
) -> Result<()> {
    let desired_mcp = opencode_mcp_entry();

    let mut root = match std::fs::read_to_string(config) {
        Ok(s) => serde_json::from_str::<serde_json::Value>(&s)
            .with_context(|| format!("parse {}", config.display()))?,
        Err(_) => serde_json::json!({ "$schema": "https://opencode.ai/config.json" }),
    };

    let obj = root
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object", config.display()))?;

    let mcp_present = obj
        .get("mcp")
        .and_then(|m| m.get("catenary"))
        .is_some_and(|c| *c == desired_mcp);
    let instructions_present = obj
        .get("instructions")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(instructions)));

    if mcp_present && instructions_present {
        let _ = out.writeln(format_args!(
            "  {} config up to date in {}",
            out.colors.green("✓"),
            config.display(),
        ));
        return Ok(());
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} merge mcp.catenary + instructions into {}",
            out.colors.dim("(dry-run)"),
            config.display(),
        ));
        return Ok(());
    }

    // MCP heartbeat — set/replace `mcp.catenary`.
    {
        let mcp = obj
            .entry("mcp")
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        let mcp_obj = mcp
            .as_object_mut()
            .with_context(|| format!("`mcp` in {} is not a JSON object", config.display()))?;
        mcp_obj.insert("catenary".to_string(), desired_mcp);
    }

    // Rules reference — append to `instructions`, preserving existing entries.
    {
        let instr = obj
            .entry("instructions")
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        let arr = instr.as_array_mut().with_context(|| {
            format!("`instructions` in {} is not a JSON array", config.display())
        })?;
        if !arr.iter().any(|v| v.as_str() == Some(instructions)) {
            arr.push(serde_json::Value::String(instructions.to_string()));
        }
    }

    if let Some(parent) = config.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
    }
    let serialized = serde_json::to_string_pretty(&root)
        .with_context(|| format!("serialize {}", config.display()))?;
    std::fs::write(config, format!("{serialized}\n"))
        .with_context(|| format!("write {}", config.display()))?;

    let _ = out.writeln(format_args!(
        "  {} merged MCP heartbeat + rules into {}",
        out.colors.green("✓"),
        config.display(),
    ));

    Ok(())
}

// ── Post-update refresh ───────────────────────────────────────────

/// Refresh all hosts that have an existing Catenary installation.
///
/// Called by `catenary update` after binary replacement. Runs
/// `install <host>` with no source argument for each host that
/// already has Catenary installed, preserving the existing
/// local/release registration.
///
/// # Errors
///
/// Returns an error if any install step fails.
pub fn refresh_installed_hosts(out: &mut Output) -> Result<()> {
    let claude = binary_exists("claude") && claude_plugin_is_installed();
    let gemini = binary_exists("gemini") && gemini_current_install().is_some();
    let antigravity = dirs::home_dir()
        .map(|h| h.join(".gemini/config/plugins/catenary"))
        .is_some_and(|p| p.is_dir() || p.is_symlink());
    let opencode = dirs::home_dir()
        .map(|h| h.join(".config/opencode/plugin/catenary.js"))
        .is_some_and(|p| p.is_file() || p.is_symlink());

    if claude {
        run_install_claude(out, None, false)?;
    }
    if gemini {
        run_install_gemini(out, None, false)?;
    }
    if antigravity {
        run_install_antigravity(out, None, false)?;
    }
    if opencode {
        run_install_opencode(out, None, false, false)?;
    }

    if !claude && !gemini && !antigravity && !opencode {
        let _ = out.writeln(format_args!(
            "  {} no hosts have Catenary installed",
            out.colors.dim("—"),
        ));
    }

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

    // ── OpenCode bundled files ─────────────────────────────────────

    fn oc_targets(base: &Path) -> OpenCodeTargets {
        OpenCodeTargets {
            plugin: base.join("plugin/catenary.js"),
            rules: base.join("catenary.md"),
            config: base.join("opencode.json"),
            instructions: "catenary.md",
        }
    }

    #[test]
    fn embedded_opencode_files_not_empty() {
        assert!(!OC_PLUGIN_JS.is_empty(), "plugin js should not be empty");
        assert!(!OC_RULES.is_empty(), "rules should not be empty");
    }

    #[test]
    fn opencode_bundled_creates_plugin() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, false).expect("install should succeed");

        let content = std::fs::read_to_string(&targets.plugin).expect("plugin should exist");
        assert_eq!(content, OC_PLUGIN_JS);
        assert!(out.into_string().contains("wrote plugin"));
    }

    #[test]
    fn opencode_bundled_idempotent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, false).expect("first install");

        let mut out2 = Output::buffer(80);
        install_opencode_bundled(&mut out2, &targets, false).expect("second install");
        assert!(out2.into_string().contains("up to date"));
    }

    #[test]
    fn opencode_bundled_staleness_rewrites() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());
        std::fs::create_dir_all(targets.plugin.parent().expect("parent"))
            .expect("create plugin dir");
        std::fs::write(&targets.plugin, "// stale").expect("write stale plugin");

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, false).expect("rewrite stale");

        let content = std::fs::read_to_string(&targets.plugin).expect("plugin should exist");
        assert_eq!(content, OC_PLUGIN_JS);
        assert!(out.into_string().contains("wrote plugin"));
    }

    #[test]
    fn opencode_bundled_dry_run_no_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, true).expect("dry run");

        assert!(!targets.plugin.exists(), "dry run should not write plugin");
        assert!(out.into_string().contains("(dry-run)"));
    }

    #[cfg(unix)]
    #[test]
    fn opencode_local_creates_symlink() {
        let source_dir = tempfile::tempdir().expect("create source dir");
        let source_file = source_dir.path().join("catenary.js");
        std::fs::write(&source_file, OC_PLUGIN_JS).expect("write source plugin");

        let target_dir = tempfile::tempdir().expect("create target dir");
        let targets = oc_targets(target_dir.path());

        let mut out = Output::buffer(80);
        install_opencode_local(&mut out, source_dir.path(), &targets, false)
            .expect("symlink install should succeed");

        assert!(targets.plugin.is_symlink(), "plugin should be a symlink");
        assert!(out.into_string().contains("symlinked"));
    }

    #[test]
    fn opencode_rules_written() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let rules = dir.path().join(".opencode/catenary.md");

        let mut out = Output::buffer(80);
        install_opencode_rules(&mut out, &rules, false).expect("write rules");

        let content = std::fs::read_to_string(&rules).expect("rules should exist");
        assert_eq!(content, OC_RULES);
        assert!(out.into_string().contains("wrote rules"));
    }

    #[test]
    fn opencode_rules_idempotent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let rules = dir.path().join("catenary.md");

        let mut out = Output::buffer(80);
        install_opencode_rules(&mut out, &rules, false).expect("first write");

        let mut out2 = Output::buffer(80);
        install_opencode_rules(&mut out2, &rules, false).expect("second write");
        assert!(out2.into_string().contains("up to date"));
    }

    #[test]
    fn opencode_rules_staleness_rewrites() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let rules = dir.path().join("catenary.md");
        // A Catenary-owned file with stale content is overwritten — it is ours,
        // referenced via `instructions`, not a user-authored rules file.
        std::fs::write(&rules, "# stale catenary rules\n").expect("write stale rules");

        let mut out = Output::buffer(80);
        install_opencode_rules(&mut out, &rules, false).expect("rewrite stale");

        let content = std::fs::read_to_string(&rules).expect("rules should exist");
        assert_eq!(content, OC_RULES);
        assert!(out.into_string().contains("wrote rules"));
    }

    #[test]
    fn opencode_rules_dry_run_no_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let rules = dir.path().join("catenary.md");

        let mut out = Output::buffer(80);
        install_opencode_rules(&mut out, &rules, true).expect("dry run");

        assert!(!rules.exists(), "dry run should not write rules");
        assert!(out.into_string().contains("(dry-run)"));
    }

    // ── OpenCode config merge (MCP heartbeat + rules instructions) ──

    #[test]
    fn merge_opencode_config_creates_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = dir.path().join("opencode.json");

        let mut out = Output::buffer(80);
        merge_opencode_config(&mut out, &config, "catenary.md", false).expect("create config");

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("config exists"))
                .expect("valid json");
        assert_eq!(json["mcp"]["catenary"], opencode_mcp_entry());
        assert_eq!(json["instructions"][0], "catenary.md");
        assert_eq!(json["$schema"], "https://opencode.ai/config.json");
    }

    #[test]
    fn merge_opencode_config_preserves_existing_keys_and_instructions() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = dir.path().join("opencode.json");
        std::fs::write(
            &config,
            r#"{"theme":"dark","instructions":["CONTRIBUTING.md"],"mcp":{"other":{"type":"local","command":["other"]}}}"#,
        )
        .expect("write existing config");

        let mut out = Output::buffer(80);
        merge_opencode_config(&mut out, &config, "catenary.md", false).expect("merge");

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("config exists"))
                .expect("valid json");
        assert_eq!(json["theme"], "dark", "top-level key preserved");
        assert_eq!(
            json["mcp"]["other"]["command"][0], "other",
            "sibling mcp server preserved",
        );
        assert_eq!(json["mcp"]["catenary"], opencode_mcp_entry());
        let instructions = json["instructions"].as_array().expect("instructions array");
        assert!(
            instructions.iter().any(|v| v == "CONTRIBUTING.md"),
            "existing instruction preserved",
        );
        assert!(
            instructions.iter().any(|v| v == "catenary.md"),
            "catenary rules appended",
        );
    }

    #[test]
    fn merge_opencode_config_idempotent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = dir.path().join("opencode.json");

        let mut out = Output::buffer(80);
        merge_opencode_config(&mut out, &config, "catenary.md", false).expect("first merge");

        let mut out2 = Output::buffer(80);
        merge_opencode_config(&mut out2, &config, "catenary.md", false).expect("second merge");
        assert!(out2.into_string().contains("up to date"));

        // The instruction must not be duplicated on re-run.
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("config exists"))
                .expect("valid json");
        let count = json["instructions"]
            .as_array()
            .expect("instructions array")
            .iter()
            .filter(|v| *v == "catenary.md")
            .count();
        assert_eq!(count, 1, "instruction should appear exactly once");
    }

    #[test]
    fn merge_opencode_config_dry_run_no_file() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let config = dir.path().join("opencode.json");

        let mut out = Output::buffer(80);
        merge_opencode_config(&mut out, &config, "catenary.md", true).expect("dry run");

        assert!(!config.exists(), "dry run should not write config");
        assert!(out.into_string().contains("(dry-run)"));
    }

    // ── List subcommand ────────────────────────────────────────────

    #[test]
    fn install_list_output() {
        let mut out = Output::buffer(80);
        run_install_list(&mut out).expect("list should succeed");
        let output = out.into_string();
        assert!(output.contains("Detected hosts:"), "output: {output}");
    }

    // ── Per-host primer pointer ─────────────────────────────────────

    #[test]
    fn install_writes_pointer_per_host() {
        // The primer (`catenary primer`) is the single source of agent-facing
        // guidance (Decision 12 / ticket 10). Each host's always-on
        // instruction surface carries a thin pointer to it, not a copy — so a
        // new or changed host is a one-line pointer, never a re-authoring.
        // These are the files the marketplace/extension serve (Claude, Gemini)
        // or the install embeds verbatim (Antigravity, `AGY_RULES`).
        const CLAUDE_SKILL: &str = include_str!("../../plugins/catenary/skills/primer/SKILL.md");
        const GEMINI_CONTEXT: &str = include_str!("../../gemini-context.md");

        for (host, surface) in [
            ("claude", CLAUDE_SKILL),
            ("gemini", GEMINI_CONTEXT),
            ("antigravity", AGY_RULES),
            ("opencode", OC_RULES),
        ] {
            assert!(
                surface.contains("catenary primer"),
                "{host} instruction surface should point at `catenary primer`",
            );
        }
    }
}
