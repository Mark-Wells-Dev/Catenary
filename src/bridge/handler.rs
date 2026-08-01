// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Path utilities shared by bridge components.

use std::path::Path;

use super::filesystem_manager::FilesystemManager;

/// Expands a leading `~` or `~/` to the user's home directory.
///
/// Home resolves through [`crate::paths::home_dir`] (misc 229), so a test's
/// `CATENARY_HOME_DIR` moves this expansion with every other home-rooted path.
#[must_use]
pub fn expand_tilde(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = crate::paths::home_dir()
    {
        return format!("{}{}", home.display(), &path[1..]);
    }
    path.to_string()
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
