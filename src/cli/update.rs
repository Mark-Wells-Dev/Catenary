// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Update command: self-update binary from GitHub releases.

use std::io::Write;

use anyhow::{Context, Result, bail};

use crate::cli::Output;

/// GitHub repository for release downloads.
const GITHUB_REPO: &str = "TwoWells/Catenary";

/// Current binary version, set at build time by `build.rs`.
const CURRENT_VERSION: &str = env!("CATENARY_VERSION");

/// Information about a GitHub release.
struct ReleaseInfo {
    /// Cleaned version string (no leading `v`).
    version: String,
    /// Download URL for the platform-appropriate binary asset.
    asset_url: Option<String>,
}

/// Returns the expected asset name for the current platform.
fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("catenary-linux-amd64"),
        ("macos", "x86_64") => Some("catenary-macos-amd64"),
        ("macos", "aarch64") => Some("catenary-macos-arm64"),
        ("windows", "x86_64") => Some("catenary-windows-amd64.exe"),
        _ => None,
    }
}

/// Parses a version string into `(major, minor, patch)`.
///
/// Handles plain semver (`1.6.1`) and git-describe suffixes
/// (`1.6.1-3-gabc1234-dirty`) by taking only the part before
/// the first hyphen.
fn parse_version(v: &str) -> (u32, u32, u32) {
    let base = v.split('-').next().unwrap_or(v);
    let parts: Vec<&str> = base.split('.').collect();
    let major = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

/// Returns `true` if `latest` is strictly newer than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    parse_version(latest) > parse_version(current)
}

/// Fetches the latest release info from the GitHub API.
fn fetch_latest_release() -> Result<ReleaseInfo> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");

    let resp = ureq::get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "catenary-update")
        .call()
        .context("GitHub API request failed")?;

    let body: serde_json::Value = resp
        .into_json()
        .context("failed to parse GitHub API response")?;

    // GitHub returns {"message": "..."} on errors (rate limit, not found).
    if let Some(msg) = body.get("message").and_then(serde_json::Value::as_str) {
        bail!("GitHub API error: {msg}");
    }

    let tag = body
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .context("no tag_name in release response")?;

    let version = tag.strip_prefix('v').unwrap_or(tag).to_string();

    let want = asset_name();
    let asset_url = want.and_then(|name| {
        body.get("assets")
            .and_then(serde_json::Value::as_array)
            .and_then(|assets| {
                assets
                    .iter()
                    .find(|a| a.get("name").and_then(serde_json::Value::as_str) == Some(name))
            })
            .and_then(|a| a.get("browser_download_url"))
            .and_then(serde_json::Value::as_str)
            .map(String::from)
    });

    Ok(ReleaseInfo { version, asset_url })
}

/// Downloads a file from `url` to `dest`.
fn download(url: &str, dest: &std::path::Path) -> Result<()> {
    let resp = ureq::get(url)
        .set("User-Agent", "catenary-update")
        .call()
        .context("download request failed")?;

    let mut file =
        std::fs::File::create(dest).context("failed to create temporary download file")?;

    std::io::copy(&mut resp.into_reader(), &mut file)
        .context("failed to write downloaded binary")?;

    file.flush().context("failed to flush download file")?;

    Ok(())
}

/// Replaces the current binary with the downloaded one.
///
/// Uses atomic rename on Unix. The old binary is replaced in-place —
/// the current process continues running from memory.
#[cfg(unix)]
fn replace_binary(new_binary: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let current_exe = std::env::current_exe().context("cannot determine current binary path")?;

    // Preserve permissions from the current binary, or default to 755.
    let perms = std::fs::metadata(&current_exe).map_or_else(
        |_| std::fs::Permissions::from_mode(0o755),
        |m| m.permissions(),
    );

    std::fs::set_permissions(new_binary, perms).context("failed to set executable permissions")?;

    std::fs::rename(new_binary, &current_exe).context("failed to replace binary (rename)")?;

    Ok(())
}

#[cfg(not(unix))]
fn replace_binary(_new_binary: &std::path::Path) -> Result<()> {
    bail!("self-update is not supported on this platform; use: cargo install catenary-mcp");
}

/// Checks whether a Catenary daemon is currently running by attempting
/// to connect to the hook socket. A stale socket file (from a crashed
/// daemon) returns `ConnectionRefused`, so this only returns `true`
/// when a live daemon is listening.
#[cfg(unix)]
fn daemon_is_running() -> bool {
    let hook_path = crate::router::hook_socket_path();
    std::os::unix::net::UnixStream::connect(&hook_path).is_ok()
}

#[cfg(not(unix))]
fn daemon_is_running() -> bool {
    false
}

/// Run `catenary update`.
///
/// Checks GitHub releases for a newer version, downloads and replaces
/// the binary, refreshes installed host configs, and warns if a daemon
/// is running.
///
/// # Errors
///
/// Returns an error if the network is unreachable, the download fails,
/// or binary replacement fails.
pub fn run_update(out: &mut Output, check: bool, force: bool) -> Result<()> {
    let current = CURRENT_VERSION;

    let release = match fetch_latest_release() {
        Ok(r) => r,
        Err(e) => {
            let _ = out.writeln(format_args!(
                "{} failed to check for updates: {e}",
                out.colors.red("✗"),
            ));
            let _ = out.writeln(format_args!("  try: cargo install catenary-mcp"));
            return Err(e);
        }
    };

    let newer = is_newer(&release.version, current);

    // --check: just report status
    if check {
        if newer {
            let _ = out.writeln(format_args!(
                "update available: v{current} → v{}",
                release.version,
            ));
        } else {
            let _ = out.writeln(format_args!("already up to date (v{current})"));
        }
        return Ok(());
    }

    // No update needed (unless --force)
    if !newer && !force {
        let _ = out.writeln(format_args!("already up to date (v{current})"));
        return Ok(());
    }

    // Resolve asset URL
    let Some(url) = &release.asset_url else {
        let name = asset_name().unwrap_or("unknown");
        bail!(
            "no binary asset '{name}' found in release v{}",
            release.version,
        );
    };

    // Download to a temp file in the same directory as the current binary
    // (same filesystem ensures atomic rename).
    let current_exe = std::env::current_exe().context("cannot determine current binary path")?;
    let dir = current_exe
        .parent()
        .context("current binary has no parent directory")?;
    let tmp = dir.join(".catenary-update.tmp");

    let _ = out.write_str(format_args!("downloading v{}...", release.version));

    if let Err(e) = download(url, &tmp) {
        // Clean up partial download
        let _ = std::fs::remove_file(&tmp);
        let _ = out.writeln(format_args!(" {}", out.colors.red("failed")));
        let _ = out.writeln(format_args!("  try: cargo install catenary-mcp"));
        return Err(e);
    }

    let _ = out.writeln(format_args!(" done"));

    // Replace binary
    if let Err(e) = replace_binary(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if newer {
        let _ = out.writeln(format_args!(
            "{} updated: v{current} → v{}",
            out.colors.green("✓"),
            release.version,
        ));
    } else {
        let _ = out.writeln(format_args!(
            "{} reinstalled v{}",
            out.colors.green("✓"),
            release.version,
        ));
    }

    // Refresh installed host configs
    crate::cli::install::refresh_installed_hosts(out)?;

    // Daemon awareness
    if daemon_is_running() {
        let _ = out.writeln(format_args!(
            "\n{}",
            out.colors
                .yellow("old daemon running, exit all sessions to use the new version."),
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

    // ── Version parsing ────────────────────────────────────────────

    #[test]
    fn parse_version_plain() {
        assert_eq!(parse_version("1.6.1"), (1, 6, 1));
    }

    #[test]
    fn parse_version_git_describe() {
        assert_eq!(parse_version("1.6.1-3-gabc1234"), (1, 6, 1));
    }

    #[test]
    fn parse_version_dirty() {
        assert_eq!(parse_version("1.6.1-3-gabc1234-dirty"), (1, 6, 1));
    }

    #[test]
    fn parse_version_major_only() {
        assert_eq!(parse_version("2"), (2, 0, 0));
    }

    // ── Version comparison ─────────────────────────────────────────

    #[test]
    fn newer_patch_bump() {
        assert!(is_newer("1.6.2", "1.6.1"));
    }

    #[test]
    fn newer_minor_bump() {
        assert!(is_newer("1.7.0", "1.6.9"));
    }

    #[test]
    fn newer_major_bump() {
        assert!(is_newer("2.0.0", "1.99.99"));
    }

    #[test]
    fn not_newer_same() {
        assert!(!is_newer("1.6.1", "1.6.1"));
    }

    #[test]
    fn not_newer_older() {
        assert!(!is_newer("1.6.0", "1.6.1"));
    }

    #[test]
    fn not_newer_dev_build() {
        // Dev build 1.6.1-3-gabc past v1.6.1 release — no update
        assert!(!is_newer("1.6.1", "1.6.1-3-gabc1234"));
    }

    #[test]
    fn newer_than_dev_build() {
        // v1.6.2 release is newer than 1.6.1-3-gabc dev build
        assert!(is_newer("1.6.2", "1.6.1-3-gabc1234"));
    }

    // ── Asset name ─────────────────────────────────────────────────

    #[test]
    fn asset_name_resolves() {
        // Should resolve on common CI/dev platforms
        let name = asset_name();
        if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            assert_eq!(name, Some("catenary-linux-amd64"));
        } else if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
            assert_eq!(name, Some("catenary-macos-amd64"));
        } else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            assert_eq!(name, Some("catenary-macos-arm64"));
        }
    }
}
