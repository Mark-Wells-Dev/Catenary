// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Path validation for LSP-aware operations.
//!
//! Workspace-root checks here are **not** a security layer — access control
//! is delegated to the host CLI's permission system (hooks, permission
//! dialogs). Instead, these checks gate LSP queries: if a file is outside
//! the roots we told the language server about, asking it for diagnostics
//! or symbols is pointless. Config file protection is the one true security
//! invariant — Catenary must not allow agents to modify its own config.

use anyhow::{Result, anyhow};
use std::path::{Path, PathBuf};
use tracing::debug;

/// Checks whether file paths fall within the workspace roots known to LSP
/// servers, and protects Catenary configuration files from modification.
///
/// The root check is an LSP-awareness gate, not a security boundary —
/// host CLI permission layers handle access control.
pub struct PathValidator {
    /// Canonical workspace root paths.
    roots: Vec<PathBuf>,
    /// Canonical paths of Catenary config files that must not be written.
    protected_configs: Vec<PathBuf>,
}

impl PathValidator {
    /// Creates a new `PathValidator` from workspace roots.
    ///
    /// Automatically discovers Catenary config file paths to protect:
    /// - `~/.config/catenary/config.toml` (user config)
    /// - `.catenary.toml` files found by searching upward from each root
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let protected_configs = Self::discover_config_paths(&roots);
        debug!(
            "PathValidator initialized with {} root(s), {} protected config(s)",
            roots.len(),
            protected_configs.len()
        );
        Self {
            roots,
            protected_configs,
        }
    }

    /// Returns the current workspace roots.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Updates the workspace roots and re-discovers protected config paths.
    pub fn update_roots(&mut self, roots: Vec<PathBuf>) {
        self.protected_configs = Self::discover_config_paths(&roots);
        debug!(
            "PathValidator updated: {} root(s), {} protected config(s)",
            roots.len(),
            self.protected_configs.len()
        );
        self.roots = roots;
    }

    /// Validates that a path is within LSP-known workspace roots.
    ///
    /// Canonicalizes the path (resolving symlinks) and checks that the
    /// canonical path is within at least one workspace root. This is not
    /// a security check — it prevents wasted LSP round-trips for files
    /// the language server has no knowledge of.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist or cannot be canonicalized.
    /// - The canonical path is outside all workspace roots.
    pub fn validate_read(&self, path: &Path) -> Result<PathBuf> {
        let canonical = path
            .canonicalize()
            .map_err(|e| anyhow!("Path does not exist: {}: {e}", path.display()))?;

        if !self.is_within_roots(&canonical) {
            return Err(anyhow!(
                "Path is outside workspace roots: {}",
                path.display()
            ));
        }

        Ok(canonical)
    }

    /// Validates a path for write-side operations.
    ///
    /// Performs the same LSP-awareness root check as [`Self::validate_read`], plus:
    /// - Rejects Catenary configuration files (`.catenary.toml`,
    ///   `~/.config/catenary/config.toml`) — the one true security invariant.
    ///
    /// For new files that don't exist yet, validates the parent directory
    /// is within workspace roots instead.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path (or its parent for new files) is outside workspace roots.
    /// - The path resolves to a Catenary configuration file.
    pub fn validate_write(&self, path: &Path) -> Result<PathBuf> {
        // For new files, the path itself won't exist yet. Check parent instead.
        let canonical = if path.exists() {
            let canonical = path
                .canonicalize()
                .map_err(|e| anyhow!("Cannot resolve path: {}: {e}", path.display()))?;

            if !self.is_within_roots(&canonical) {
                return Err(anyhow!(
                    "Path is outside workspace roots: {}",
                    path.display()
                ));
            }

            canonical
        } else {
            // New file: validate parent directory exists and is within roots
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("Cannot determine parent directory: {}", path.display()))?;

            // The parent might also not exist yet (create_dir_all will handle it),
            // so walk up to find the first existing ancestor.
            let existing_ancestor = Self::find_existing_ancestor(parent)?;
            let canonical_ancestor = existing_ancestor.canonicalize().map_err(|e| {
                anyhow!(
                    "Cannot resolve ancestor path: {}: {e}",
                    existing_ancestor.display()
                )
            })?;

            if !self.is_within_roots(&canonical_ancestor) {
                return Err(anyhow!(
                    "Path is outside workspace roots: {}",
                    path.display()
                ));
            }

            // Return the intended path (not canonical, since it doesn't exist yet).
            // Build from the canonical ancestor + remaining components.
            let remaining = path.strip_prefix(&existing_ancestor).unwrap_or_else(|_| {
                // If strip_prefix fails, use just the filename
                path.file_name().map_or_else(|| Path::new(""), Path::new)
            });
            canonical_ancestor.join(remaining)
        };

        if self.is_config_file(&canonical) {
            return Err(anyhow!(
                "Cannot modify Catenary configuration file: {}",
                path.display()
            ));
        }

        Ok(canonical)
    }

    /// Checks if a canonical path is within any LSP-known workspace root.
    fn is_within_roots(&self, canonical: &Path) -> bool {
        self.roots.iter().any(|root| canonical.starts_with(root))
    }

    /// Checks if a canonical path matches any protected config file.
    fn is_config_file(&self, canonical: &Path) -> bool {
        self.protected_configs
            .iter()
            .any(|config| canonical == config)
    }

    /// Discovers Catenary config file paths to protect.
    fn discover_config_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        // User config: ~/.config/catenary/config.toml
        {
            let user_config = crate::paths::config_dir()
                .join("catenary")
                .join("config.toml");
            if let Ok(canonical) = user_config.canonicalize() {
                paths.push(canonical);
            }
        }

        // Project-local config: .catenary.toml (search upward from each root)
        for root in roots {
            let mut current = Some(root.as_path());
            while let Some(dir) = current {
                let config_path = dir.join(".catenary.toml");
                if let Ok(canonical) = config_path.canonicalize() {
                    if !paths.contains(&canonical) {
                        paths.push(canonical);
                    }
                    break;
                }
                current = dir.parent();
            }
        }

        paths
    }

    /// Walks up the directory tree to find the first existing ancestor.
    fn find_existing_ancestor(path: &Path) -> Result<PathBuf> {
        let mut current = path;
        loop {
            if current.exists() {
                return Ok(current.to_path_buf());
            }
            current = current
                .parent()
                .ok_or_else(|| anyhow!("No existing ancestor found for: {}", path.display()))?;
        }
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
    use tempfile::TempDir;

    fn setup_workspace() -> Result<(TempDir, PathValidator)> {
        let dir = TempDir::new().map_err(|e| anyhow!("{e}"))?;
        let root = dir.path().canonicalize()?;

        // Create some test files
        fs::write(root.join("test.rs"), "fn main() {}")?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/lib.rs"), "// lib")?;

        let validator = PathValidator::new(vec![root]);
        Ok((dir, validator))
    }

    #[test]
    fn test_read_within_root_succeeds() -> Result<()> {
        let (dir, validator) = setup_workspace()?;
        let result = validator.validate_read(&dir.path().join("test.rs"));
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_read_subdirectory_succeeds() -> Result<()> {
        let (dir, validator) = setup_workspace()?;
        let result = validator.validate_read(&dir.path().join("src/lib.rs"));
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_read_outside_root_fails() -> Result<()> {
        let (_dir, validator) = setup_workspace()?;
        // The out-of-root path must EXIST (so it canonicalizes and reaches the
        // roots check) yet be outside every workspace root — asserting on the
        // "outside workspace roots" branch, not the "does not exist" one. A real
        // file in a separate tempdir satisfies both on every platform; a fixed
        // system path like `/etc/hostname` does not (absent on macOS, where
        // `canonicalize` fails with ENOENT and the error reads "does not exist").
        let outside_dir = TempDir::new().map_err(|e| anyhow!("{e}"))?;
        let outside_file = outside_dir.path().join("outside.txt");
        fs::write(&outside_file, "outside")?;

        let result = validator.validate_read(&outside_file);
        assert!(result.is_err());
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("outside workspace roots"),
            "Error should mention workspace roots: {err}"
        );
        Ok(())
    }

    #[test]
    fn test_read_nonexistent_fails() -> Result<()> {
        let (dir, validator) = setup_workspace()?;
        let result = validator.validate_read(&dir.path().join("nonexistent.rs"));
        assert!(result.is_err());
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("does not exist"),
            "Error should mention file not existing: {err}"
        );
        Ok(())
    }

    #[test]
    fn test_read_path_traversal_outside_root_fails() -> Result<()> {
        let (_dir, validator) = setup_workspace()?;
        // Even with ../ that technically resolves to something that exists,
        // if it's outside the root, it should fail.
        let result = validator.validate_read(Path::new("/tmp/../etc/hostname"));
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_write_within_root_succeeds() -> Result<()> {
        let (dir, validator) = setup_workspace()?;
        let result = validator.validate_write(&dir.path().join("test.rs"));
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_write_outside_root_fails() -> Result<()> {
        let (_dir, validator) = setup_workspace()?;
        let result = validator.validate_write(Path::new("/tmp/outside.rs"));
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_write_new_file_within_root_succeeds() -> Result<()> {
        let (dir, validator) = setup_workspace()?;
        let result = validator.validate_write(&dir.path().join("new_file.rs"));
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_write_new_file_in_new_subdir_within_root() -> Result<()> {
        let (dir, validator) = setup_workspace()?;
        let result = validator.validate_write(&dir.path().join("new_dir/new_file.rs"));
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_write_config_file_rejected() -> Result<()> {
        let (dir, _) = setup_workspace()?;
        // Create a .catenary.toml in the root
        let config_path = dir.path().join(".catenary.toml");
        fs::write(&config_path, "log_retention_days = 7")?;

        // Recreate validator to pick up the config
        let root = dir.path().canonicalize()?;
        let validator = PathValidator::new(vec![root]);

        let result = validator.validate_write(&config_path);
        assert!(result.is_err());
        let err = result.expect_err("expected error").to_string();
        assert!(
            err.contains("configuration file"),
            "Error should mention config file: {err}"
        );
        Ok(())
    }

    #[test]
    fn test_read_config_file_allowed() -> Result<()> {
        let (dir, _) = setup_workspace()?;
        let config_path = dir.path().join(".catenary.toml");
        fs::write(&config_path, "log_retention_days = 7")?;

        let root = dir.path().canonicalize()?;
        let validator = PathValidator::new(vec![root]);

        // Reading config files is fine
        let result = validator.validate_read(&config_path);
        assert!(result.is_ok());
        Ok(())
    }

    #[test]
    fn test_multiple_roots() -> Result<()> {
        let dir1 = TempDir::new()?;
        let dir2 = TempDir::new()?;
        let root1 = dir1.path().canonicalize()?;
        let root2 = dir2.path().canonicalize()?;

        fs::write(root1.join("a.rs"), "// a")?;
        fs::write(root2.join("b.rs"), "// b")?;

        let validator = PathValidator::new(vec![root1, root2]);

        assert!(validator.validate_read(&dir1.path().join("a.rs")).is_ok());
        assert!(validator.validate_read(&dir2.path().join("b.rs")).is_ok());
        Ok(())
    }

    #[test]
    fn test_update_roots() -> Result<()> {
        let dir1 = TempDir::new()?;
        let dir2 = TempDir::new()?;
        let root1 = dir1.path().canonicalize()?;
        let root2 = dir2.path().canonicalize()?;

        fs::write(root1.join("a.rs"), "// a")?;
        fs::write(root2.join("b.rs"), "// b")?;

        let mut validator = PathValidator::new(vec![root1]);

        // b.rs is outside current roots
        assert!(validator.validate_read(&dir2.path().join("b.rs")).is_err());

        // Update roots to include dir2
        validator.update_roots(vec![dir1.path().canonicalize()?, root2]);

        // Now b.rs is within roots
        assert!(validator.validate_read(&dir2.path().join("b.rs")).is_ok());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_within_root_succeeds() -> Result<()> {
        use std::os::unix::fs as unix_fs;

        let (dir, validator) = setup_workspace()?;
        let root = dir.path().canonicalize()?;

        // Create a symlink within the workspace
        let link_path = root.join("link.rs");
        unix_fs::symlink(root.join("test.rs"), &link_path)?;

        let result = validator.validate_read(&link_path);
        assert!(result.is_ok());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_outside_root_fails() -> Result<()> {
        use std::os::unix::fs as unix_fs;

        let (dir, validator) = setup_workspace()?;
        let root = dir.path().canonicalize()?;

        // Create a file outside the workspace
        let outside_dir = TempDir::new()?;
        let outside_file = outside_dir.path().join("secret.txt");
        fs::write(&outside_file, "secret")?;

        // Create a symlink inside workspace pointing outside
        let link_path = root.join("sneaky_link.txt");
        unix_fs::symlink(&outside_file, &link_path)?;

        // canonicalize() will resolve the symlink to the outside path
        let result = validator.validate_read(&link_path);
        assert!(result.is_err());
        Ok(())
    }
}
