// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end coverage for the user context files (misc 224):
//! `~/.config/catenary/AGENTS.md` (leads) and `SUBAGENTS.md` (workers), each
//! with an optional client addendum keyed by the hook's `--format` token.
//!
//! The composition itself is unit-tested in `src/cli/user_context.rs` against a
//! fixture directory. What only a subprocess can prove is the wiring: that the
//! live hook resolves the files through `paths::config_dir()` (so
//! `CATENARY_CONFIG_DIR` isolation holds), that the payload rides each host's
//! own hook envelope, and that the role scoping is real — a subagent never
//! receives the lead's file.
//!
//! Every subprocess is isolated with `isolate_env` BEFORE any `CATENARY_*` var
//! is set, and the context files are written under `common::xdg_config_home`, so
//! the hook and the test agree on one tempdir config base and the operator's
//! real `~/.config/catenary` is never read.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{DaemonGuard, isolate_env, xdg_config_home};

/// The provenance-header label every emitted block opens with.
const PROVENANCE_LABEL: &str = "Catenary user context —";

/// Write a user context file to `<isolated XDG_CONFIG_HOME>/catenary/<name>`.
fn write_context_file(root: &str, name: &str, content: &str) -> Result<()> {
    let dir = xdg_config_home(root).join("catenary");
    std::fs::create_dir_all(&dir).context("create isolated catenary config dir")?;
    std::fs::write(dir.join(name), content).context("write user context file")
}

/// The spelling of a context file's path that its provenance header must carry.
///
/// Mirrors the production `~`-compression (`bridge::compress_home`, `pub(crate)`
/// and so unreachable from an integration test): `$HOME`-prefixed paths render
/// `~/…`, everything else absolute. `isolate_env` deliberately does not redirect
/// `$HOME`, so both sides resolve the same one — and the assertion holds even
/// where `TMPDIR` lives under the home directory.
fn provenance_target(root: &str, name: &str) -> String {
    let path = xdg_config_home(root).join("catenary").join(name);
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return path.display().to_string();
    }
    path.strip_prefix(&home).map_or_else(
        |_| path.display().to_string(),
        |rel| format!("~/{}", rel.display()),
    )
}

/// Drive a hook subprocess (`catenary hook <args…>`) with `payload` on stdin,
/// returning its stdout.
fn run_hook(root: &str, args: &[&str], payload: &Value) -> Result<String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.arg("hook");
    cmd.args(args);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn hook subprocess")?;
    {
        let mut stdin = child.stdin.take().context("hook stdin")?;
        write!(stdin, "{payload}").context("write hook payload")?;
    }
    let out = child.wait_with_output().context("wait for hook")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The `hookSpecificOutput.additionalContext` string carried by a hook's stdout.
fn additional_context(stdout: &str) -> Result<String> {
    let v: Value = serde_json::from_str(stdout.trim())
        .with_context(|| format!("hook stdout should be JSON, got: {stdout}"))?;
    v.get("hookSpecificOutput")
        .and_then(|o| o.get("additionalContext"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("no additionalContext in hook stdout: {stdout}"))
}

/// Run the `SubagentStart` hook for an isolated root and return its
/// `additionalContext`.
fn subagent_start_context(root: &str) -> Result<String> {
    let payload = json!({
        "hook_event_name": "SubagentStart",
        "session_id": "sess-user-context",
        "agent_id": "agent-1",
        "cwd": root,
    });
    additional_context(&run_hook(
        root,
        &["subagent-start", "--format=claude"],
        &payload,
    )?)
}

/// A subagent is served the SUBAGENTS pair — shared core first, client addendum
/// after — each block under a provenance header naming its own file. The lead's
/// `AGENTS.md` is present on disk and must NOT appear: the filename is the whole
/// of the role scoping, so lead-directed policy cannot bleed into a worker.
#[test]
fn subagent_start_serves_the_subagents_pair_shared_then_addendum() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    write_context_file(root, "SUBAGENTS.md", "WORKER-SHARED-CORE\n")?;
    write_context_file(root, "SUBAGENTS.claude.md", "WORKER-CLAUDE-ADDENDUM\n")?;
    write_context_file(root, "AGENTS.md", "LEAD-ONLY-POLICY\n")?;

    let ctx = subagent_start_context(root)?;

    let shared_at = ctx
        .find("WORKER-SHARED-CORE")
        .context("the shared SUBAGENTS.md must ride the subagent payload")?;
    let addendum_at = ctx
        .find("WORKER-CLAUDE-ADDENDUM")
        .context("the claude addendum must ride the subagent payload")?;
    assert!(
        shared_at < addendum_at,
        "shared core first, addendum after: {ctx}"
    );
    assert!(
        !ctx.contains("LEAD-ONLY-POLICY"),
        "a worker must never be served the lead's AGENTS.md: {ctx}"
    );

    // Provenance: one header per concatenated file, each naming its source.
    assert!(
        ctx.contains(&provenance_target(root, "SUBAGENTS.md")),
        "the shared block must name its file: {ctx}"
    );
    assert!(
        ctx.contains(&provenance_target(root, "SUBAGENTS.claude.md")),
        "the addendum block must name its file: {ctx}"
    );
    assert_eq!(
        ctx.matches(PROVENANCE_LABEL).count(),
        2,
        "exactly one provenance header per file: {ctx}"
    );

    // The teaching payload still opens the context — the user context is
    // appended to it, never a replacement for it.
    assert!(
        ctx.contains("The edit→diagnostics loop"),
        "the teaching payload must still ride: {ctx}"
    );
    Ok(())
}

/// A different client token does not pull another client's addendum: a
/// `--format=claude` dispatch reads `SUBAGENTS.claude.md` and leaves
/// `SUBAGENTS.antigravity.md` alone. The shared core rides either way.
#[test]
fn subagent_start_ignores_another_clients_addendum() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    write_context_file(root, "SUBAGENTS.md", "WORKER-SHARED-CORE\n")?;
    write_context_file(root, "SUBAGENTS.antigravity.md", "AGY-ONLY-ADDENDUM\n")?;

    let ctx = subagent_start_context(root)?;
    assert!(
        ctx.contains("WORKER-SHARED-CORE"),
        "the shared core rides for every client: {ctx}"
    );
    assert!(
        !ctx.contains("AGY-ONLY-ADDENDUM"),
        "a claude dispatch must not pull the antigravity addendum: {ctx}"
    );
    assert_eq!(
        ctx.matches(PROVENANCE_LABEL).count(),
        1,
        "only the shared file emits a block: {ctx}"
    );
    Ok(())
}

/// Absent files are a silent no-op: no block, no header, no warning. This is an
/// opt-in surface the user populates by hand.
#[test]
fn absent_context_files_emit_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    // No context files written at all — the isolated config dir is empty.
    let ctx = subagent_start_context(root)?;
    assert!(
        !ctx.contains(PROVENANCE_LABEL),
        "no context file → no user-context block: {ctx}"
    );
    assert!(
        ctx.contains("The edit→diagnostics loop"),
        "the teaching payload is unaffected: {ctx}"
    );
    Ok(())
}

/// A lead is served the AGENTS pair on Claude Code's `SessionStart` seam — and
/// never the worker's `SUBAGENTS.md`.
///
/// `SessionStart` carries the spawn-on-demand daemon fallback (ws49-04), so this
/// test brings a daemon up as a side effect; the [`DaemonGuard`] tears it down on
/// drop, including a panic unwind.
#[test]
fn session_start_serves_the_agents_pair_for_a_lead() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;
    let _guard = DaemonGuard::new(root);

    write_context_file(root, "AGENTS.md", "LEAD-SHARED-CORE\n")?;
    write_context_file(root, "AGENTS.claude.md", "LEAD-CLAUDE-ADDENDUM\n")?;
    write_context_file(root, "SUBAGENTS.md", "WORKER-ONLY-POLICY\n")?;

    let payload = json!({
        "hook_event_name": "SessionStart",
        "session_id": "sess-user-context-lead",
        "source": "startup",
        "cwd": root,
    });
    let ctx = additional_context(&run_hook(
        root,
        &["session-start", "--format=claude"],
        &payload,
    )?)?;

    let shared_at = ctx
        .find("LEAD-SHARED-CORE")
        .context("the shared AGENTS.md must ride the session-start payload")?;
    let addendum_at = ctx
        .find("LEAD-CLAUDE-ADDENDUM")
        .context("the claude addendum must ride the session-start payload")?;
    assert!(
        shared_at < addendum_at,
        "shared core first, addendum after: {ctx}"
    );
    assert!(
        !ctx.contains("WORKER-ONLY-POLICY"),
        "a lead must never be served the worker's SUBAGENTS.md: {ctx}"
    );
    assert!(
        ctx.contains(&provenance_target(root, "AGENTS.md"))
            && ctx.contains(&provenance_target(root, "AGENTS.claude.md")),
        "each block names the file it came from: {ctx}"
    );
    // Appended to the teaching payload, which still opens the context.
    let teaching_at = ctx
        .find("The edit→diagnostics loop")
        .context("the teaching payload must still ride the session-start context")?;
    assert!(
        teaching_at < shared_at,
        "the user context closes the payload: {ctx}"
    );
    Ok(())
}

/// The composed payload is opaque: markdown, frontmatter, and `{EDIT}`-style
/// tokens all reach the agent byte-for-byte. Catenary parses nothing.
#[test]
fn context_content_rides_verbatim() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    let raw = "---\ntrigger: always_on\n---\n\n# Worker policy\n\nUse {EDIT}, not `sed`.\n";
    write_context_file(root, "SUBAGENTS.md", raw)?;

    let ctx = subagent_start_context(root)?;
    assert!(
        ctx.contains(raw.trim_end_matches('\n')),
        "the file's bytes must ride verbatim: {ctx}"
    );
    Ok(())
}

/// The isolation the whole suite rests on: the hook reads its context files from
/// `CATENARY_CONFIG_DIR`, never the operator's real `~/.config/catenary`.
#[test]
fn context_files_resolve_through_the_isolated_config_dir() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().to_str().context("tempdir path")?;

    write_context_file(root, "SUBAGENTS.md", "ISOLATED-MARKER\n")?;
    let ctx = subagent_start_context(root)?;

    assert!(ctx.contains("ISOLATED-MARKER"), "{ctx}");
    assert!(
        xdg_config_home(root)
            .join("catenary")
            .join("SUBAGENTS.md")
            .is_file(),
        "the fixture must live under the isolated config base",
    );
    assert!(
        ctx.contains(&provenance_target(root, "SUBAGENTS.md")),
        "the provenance header must name the ISOLATED path: {ctx}"
    );
    Ok(())
}
