// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Subprocess coverage for the Antigravity rules-file rewrite (bug 149).
//!
//! `catenary hook pre-invocation --format=antigravity` regenerates the
//! *installed* rules file on every model call. Its target used to resolve
//! through `dirs::home_dir()`, which `isolate_env` does not redirect — so
//! driving the agy format in a subprocess rewrote the operator's REAL
//! `~/.gemini/config/plugins/catenary/rules/catenary.md`. That hazard is exactly
//! why misc 224 kept its agy leg at unit level; these are the tests it withheld.
//!
//! The target now resolves through `paths::home_dir()`, whose `CATENARY_HOME_DIR`
//! override `isolate_env` points at `<root>/home`. Every assertion here reads the
//! ISOLATED path only — the operator's real home is never read, let alone
//! written. Landing a stamped rewrite at the isolated path *is* the proof that
//! resolution went through the override: before the fix that file was never
//! touched, no matter what the hook did.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use common::{catenary_home, isolate_env};

/// Content standing in for a stale installed rules file: not the live surface,
/// and not runtime-stamped, so a rewrite is unambiguous.
const STALE: &str = "---\ntrigger: always_on\n---\n\nSTALE INSTALLED CONTENT\n";

/// The installed Antigravity rules file under the ISOLATED home — the path
/// `paths::home_dir()` resolves to once `CATENARY_HOME_DIR` is set.
fn isolated_rules_file(root: &str) -> std::path::PathBuf {
    isolated_plugin_dir(root).join("rules").join("catenary.md")
}

/// The installed Antigravity plugin dir under the ISOLATED home.
fn isolated_plugin_dir(root: &str) -> std::path::PathBuf {
    catenary_home(root).join(".gemini/config/plugins/catenary")
}

/// Drive one `PreInvocation` model call for the Antigravity host, returning its
/// stdout.
///
/// The rewrite runs before stdin is even read, so no daemon is needed: `{}`
/// carries no `conversationId`, the first-sighting lookup fails closed, and the
/// hook answers the empty `{}` envelope.
fn run_pre_invocation(root: &str) -> Result<String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.args(["hook", "pre-invocation", "--format=antigravity"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn pre-invocation hook")?;
    {
        let mut stdin = child.stdin.take().context("hook stdin")?;
        write!(stdin, "{{}}").context("write hook payload")?;
    }
    let out = child.wait_with_output().context("wait for hook")?;
    anyhow::ensure!(
        out.status.success(),
        "the hook must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The rewrite lands under the ISOLATED home, not the operator's real one.
///
/// A copy install (a plain directory, no symlink, no `.git` ancestor) staged
/// under `<root>/home` is regenerated to the live surface — frontmatter first,
/// then the runtime generation stamp. Before bug 149's fix this file was
/// unreachable from the hook: resolution went to the real `$HOME` and the
/// operator's own rules file was the one rewritten.
#[test]
fn rules_rewrite_lands_under_the_isolated_home() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let rules = isolated_rules_file(root);
    std::fs::create_dir_all(rules.parent().context("rules parent")?)
        .context("stage the isolated copy install")?;
    std::fs::write(&rules, STALE).context("write the stale installed file")?;

    let stdout = run_pre_invocation(root)?;
    assert_eq!(
        stdout.trim(),
        "{}",
        "no daemon, no conversationId — the hook injects nothing: {stdout}"
    );

    let after = std::fs::read_to_string(&rules).context("read the rewritten rules file")?;
    assert_ne!(
        after, STALE,
        "the isolated installed file must be regenerated, got: {after}"
    );
    assert!(
        after.starts_with("---\ntrigger: always_on"),
        "the always_on frontmatter stays the very first bytes: {after}"
    );
    assert!(
        catenary_cli::cli::teaching::is_runtime_stamped(&after),
        "the rewrite carries the runtime generation stamp: {after}"
    );

    // The atomic rename leaves no temp file behind in the isolated install.
    let leftovers = std::fs::read_dir(rules.parent().context("rules parent")?)
        .context("read the isolated rules dir")?
        .filter_map(std::result::Result::ok)
        .filter(|e| e.file_name().to_string_lossy().contains("catenary-context"))
        .count();
    assert_eq!(leftovers, 0, "no temp file left after the atomic rename");
    Ok(())
}

/// With no install under the ISOLATED home the rewrite is a silent no-op — it
/// does not fall back to the real home, and it fabricates no install.
///
/// The pair with the test above is what pins the override as the deciding
/// input: staged under `<root>/home` the file is rewritten, absent from
/// `<root>/home` nothing is written at all.
#[test]
fn no_isolated_install_means_no_rewrite() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let stdout = run_pre_invocation(root)?;
    assert_eq!(stdout.trim(), "{}", "the hook still answers cleanly");

    assert!(
        !isolated_plugin_dir(root).exists(),
        "an absent install is a silent no-op — the hook must not create one",
    );
    Ok(())
}

/// A symlinked plugin dir under the ISOLATED home is a developer install and is
/// left alone — the link guard runs against the resolved target, so it still
/// protects a dev checkout now that resolution goes through the override.
#[test]
#[cfg(unix)]
fn a_symlinked_isolated_install_is_left_alone() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    // The "developer checkout" the link points into — a plain tempdir subtree,
    // so the `.git`-ancestry backstop is not what does the skipping here.
    let checkout = dir.path().join("dev-checkout/plugins/catenary");
    std::fs::create_dir_all(checkout.join("rules")).context("stage the dev checkout")?;
    std::fs::write(checkout.join("rules/catenary.md"), STALE).context("write the linked file")?;

    let plugin_dir = isolated_plugin_dir(root);
    std::fs::create_dir_all(plugin_dir.parent().context("plugins parent")?)
        .context("stage the isolated plugins dir")?;
    std::os::unix::fs::symlink(&checkout, &plugin_dir).context("link the plugin dir")?;

    run_pre_invocation(root)?;

    assert_eq!(
        std::fs::read_to_string(checkout.join("rules/catenary.md"))
            .context("read the linked file")?,
        STALE,
        "a link install must never be rewritten — that would dirty a dev worktree",
    );
    Ok(())
}
