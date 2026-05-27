// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Path utilities shared by bridge components.

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};

use super::filesystem_manager::FilesystemManager;

/// Expands a leading `~` or `~/` to the user's home directory.
#[must_use]
pub fn expand_tilde(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{}", &path[1..]);
    }
    path.to_string()
}

/// Resolves a file path, converting relative paths to absolute using the current working directory.
///
/// Expands a leading `~` to `$HOME` before resolution.
pub(super) fn resolve_path(file: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(file);
    let path = PathBuf::from(&expanded);
    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?;
        Ok(cwd.join(path))
    }
}

/// Makes a file path relative to the owning root, for display.
///
/// Uses [`FilesystemManager::resolve_root`] for longest-prefix matching
/// instead of ad-hoc iteration.
pub(super) fn display_path(file: &str, fs: &FilesystemManager) -> String {
    let path = Path::new(file);
    fs.resolve_root(path).map_or_else(
        || file.to_string(),
        |root| {
            path.strip_prefix(&root).map_or_else(
                |_| file.to_string(),
                |rel| rel.to_string_lossy().to_string(),
            )
        },
    )
}
