// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The Catenary-managed server home (ls-manager 01): install destinations the
//! system can't break.
//!
//! The guided-install recipes used to delegate to ecosystem installers into
//! ecosystem-owned prefixes (`cargo install` → `~/.cargo/bin`, `npm -g` → the
//! system `node_modules`), where a `pacman -Syu` or `npm update -g` silently
//! voids the version-pinned warranty. The managed home is Catenary-owned:
//! `<data_dir>/catenary/servers/<name>/<version>/…`, regenerable artifacts in
//! the regenerable tier ([`crate::paths::data_dir`]) — losing it costs a
//! reinstall, never state.
//!
//! **The ruled invariant: every version dir is self-contained and exposes
//! executables at `<name>/<version>/bin/`.** Path derivation lives here, in
//! [`ManagedHome`], and nowhere else: recipes stay declarative and never name
//! destinations (the recipe schema rejects an unknown key, so a recipe naming
//! one is a schema error), and the per-ecosystem containment code in
//! [`crate::install`] asks this type for the version dir when it translates
//! "install `<recipe>` at `<version>`" into the contained invocation.
//!
//! Spawn resolution — which binary a server *spawn* uses — is deliberately not
//! here (ls-manager 02). After this ticket installs land in the managed home
//! while resolution still finds servers on `$PATH`; that intermediate state is
//! expected.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

/// The Catenary-managed server home: the single owner of every
/// install-destination path.
#[derive(Debug, Clone)]
pub struct ManagedHome {
    /// The `servers/` root every version dir nests under.
    root: PathBuf,
}

impl ManagedHome {
    /// The production home: `<data_dir>/catenary/servers/`.
    #[must_use]
    pub fn resolve() -> Self {
        Self::at(crate::paths::data_dir().join("catenary").join("servers"))
    }

    /// A home rooted at an explicit directory.
    ///
    /// The injection seam: tests point this at a tempdir subdir so containment
    /// ("nothing lands outside the version dir") is asserted against an
    /// isolated root, in the `isolate_env` mislocation-detector spirit.
    #[must_use]
    pub const fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The `servers/` root the version dirs nest under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The self-contained install destination for `server` at `version`:
    /// `<root>/<server>/<version>/`.
    ///
    /// Executables are exposed at `bin/` inside it — the ruled invariant every
    /// per-ecosystem containment leg lands on.
    ///
    /// # Errors
    ///
    /// Returns an error when either segment is not a plain path component
    /// (empty, `.`/`..`, or containing a separator). Server names and versions
    /// come from Catenary's own blessed data — trusted — but the join stays
    /// defensive so no segment can ever escape the home.
    pub fn version_dir(&self, server: &str, version: &str) -> Result<PathBuf> {
        validate_segment("server name", server)?;
        validate_segment("version", version)?;
        Ok(self.root.join(server).join(version))
    }

    /// The executable directory of a version dir:
    /// `<root>/<server>/<version>/bin/`.
    ///
    /// # Errors
    ///
    /// Same segment validation as [`Self::version_dir`].
    pub fn bin_dir(&self, server: &str, version: &str) -> Result<PathBuf> {
        Ok(self.version_dir(server, version)?.join("bin"))
    }
}

/// Refuse a path segment that is not one plain component.
///
/// Segments come from blessed recipe data (trusted), so a rejection here is a
/// data bug surfacing loudly — never a silently mislocated install.
fn validate_segment(what: &str, segment: &str) -> Result<()> {
    if segment.trim().is_empty() {
        bail!("managed-home {what} is empty");
    }
    if segment == "." || segment == ".." {
        bail!("managed-home {what} `{segment}` would traverse out of the home");
    }
    if segment.contains(['/', '\\', '\0']) {
        bail!("managed-home {what} `{segment}` contains a path separator");
    }
    Ok(())
}

/// Expose `executable` — a file already unpacked *inside* `version_dir` — at
/// `<version_dir>/bin/<bin_name>`.
///
/// The direct-fetch containment leg: a directly fetched artifact unpacks in
/// place under the version dir, and its executable is linked into `bin/` so the
/// dir satisfies the ruled invariant. The link is **relative**, keeping the
/// version dir self-contained (relocatable as one unit); on Unix the target's
/// execute bits are set. An existing entry at the `bin/` name is replaced, so a
/// reinstall converges.
///
/// # Errors
///
/// Returns an error when `bin_name` is not a plain path component, when
/// `executable` does not live inside `version_dir` (the managed home never
/// links out of a version dir), or on a filesystem failure.
pub fn expose_executable(version_dir: &Path, executable: &Path, bin_name: &str) -> Result<PathBuf> {
    validate_segment("bin name", bin_name)?;
    let relative = executable.strip_prefix(version_dir).map_err(|_| {
        anyhow!(
            "executable {} lives outside the version dir {} — a version dir is self-contained",
            executable.display(),
            version_dir.display()
        )
    })?;
    let bin_dir = version_dir.join("bin");
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(executable)
            .with_context(|| format!("reading {}", executable.display()))?;
        let mut perms = metadata.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(executable, perms)
            .with_context(|| format!("marking {} executable", executable.display()))?;
    }
    let link = bin_dir.join(bin_name);
    if std::fs::symlink_metadata(&link).is_ok() {
        std::fs::remove_file(&link)
            .with_context(|| format!("replacing existing {}", link.display()))?;
    }
    // `bin/<name>` → `../<relative>`: one level up from `bin/` is the version
    // dir the target path is relative to.
    let target = Path::new("..").join(relative);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link)
        .with_context(|| format!("linking {} -> {}", link.display(), target.display()))?;
    #[cfg(not(unix))]
    std::fs::copy(executable, &link)
        .with_context(|| format!("copying {} -> {}", executable.display(), link.display()))?;
    Ok(link)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn production_home_is_the_servers_subtree_of_the_data_dir() {
        let home = ManagedHome::resolve();
        assert!(
            home.root().starts_with(crate::paths::data_dir()),
            "the managed home lives under the data dir (regenerable tier)",
        );
        assert!(
            home.root().ends_with("catenary/servers"),
            "the managed home is `<data>/catenary/servers`, got {}",
            home.root().display(),
        );
    }

    #[test]
    fn version_dir_is_name_then_version() {
        let home = ManagedHome::at(PathBuf::from("/mh"));
        let dir = home
            .version_dir("bash-language-server", "5.6.0")
            .expect("plain segments derive");
        assert_eq!(dir, PathBuf::from("/mh/bash-language-server/5.6.0"));
        assert_eq!(
            home.bin_dir("bash-language-server", "5.6.0")
                .expect("bin dir derives"),
            PathBuf::from("/mh/bash-language-server/5.6.0/bin"),
            "executables are exposed at `<name>/<version>/bin/` — the ruled invariant",
        );
    }

    #[test]
    fn version_dir_refuses_traversal_and_separator_segments() {
        let home = ManagedHome::at(PathBuf::from("/mh"));
        for bad in ["", " ", ".", "..", "a/b", "a\\b", "a\0b"] {
            assert!(
                home.version_dir(bad, "1.0.0").is_err(),
                "server segment {bad:?} must be refused",
            );
            assert!(
                home.version_dir("srv", bad).is_err(),
                "version segment {bad:?} must be refused",
            );
        }
        // A go-style `v`-prefixed version and a dotted name are plain segments.
        assert!(home.version_dir("gopls", "v0.22.0").is_ok());
    }

    #[test]
    fn expose_executable_links_relative_into_bin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let version_dir = tmp.path().join("srv").join("1.0.0");
        let unpacked = version_dir.join("unpacked").join("srv-binary");
        std::fs::create_dir_all(unpacked.parent().expect("parent")).expect("mkdir");
        std::fs::write(&unpacked, b"#!/bin/sh\n").expect("write artifact");

        let link = expose_executable(&version_dir, &unpacked, "srv").expect("expose");
        assert_eq!(link, version_dir.join("bin").join("srv"));
        // The link resolves to the in-place artifact and is relative, so the
        // version dir stays self-contained.
        assert_eq!(
            std::fs::canonicalize(&link).expect("resolve link"),
            std::fs::canonicalize(&unpacked).expect("resolve artifact"),
        );
        #[cfg(unix)]
        {
            let target = std::fs::read_link(&link).expect("read link");
            assert!(
                target.is_relative(),
                "the bin link is relative (relocatable), got {}",
                target.display(),
            );
            let mode = std::fs::metadata(&unpacked)
                .expect("artifact metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the artifact is executable");
        }
        // Re-exposing converges instead of failing on the existing link.
        expose_executable(&version_dir, &unpacked, "srv").expect("re-expose converges");
    }

    #[test]
    fn expose_executable_refuses_an_artifact_outside_the_version_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let version_dir = tmp.path().join("srv").join("1.0.0");
        std::fs::create_dir_all(&version_dir).expect("mkdir");
        let outside = tmp.path().join("elsewhere");
        std::fs::write(&outside, b"nope").expect("write");
        assert!(
            expose_executable(&version_dir, &outside, "srv").is_err(),
            "a version dir never links out of itself",
        );
    }
}
