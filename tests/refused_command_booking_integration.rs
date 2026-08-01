// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Bug 150: a command the FILTER refuses must book nothing into the debt ledger.
//!
//! The reported sighting was a refused command that nonetheless registered its
//! write targets, arming the diagnostics debt gate for files no write ever
//! touched, with the reporter's diagnosis "write-targets are recorded before the
//! refusal check runs". This is the repro sweep for that mechanism across the
//! whole filter-refusal surface — and it does NOT reproduce: every shape below
//! denies, and every one leaves the ledger empty. The ordering is already
//! check-then-book, in two layers: the filter itself walks the allowlist
//! (`check_script`) BEFORE the resolve-or-deny write pass, so a denial returns
//! with no write-set at all; and `run_pre_tool` prints every filter denial and
//! returns BEFORE `enforce_editing_state`, the only path to the booking seam
//! (`root_lock_gate`). Bug 118 ruled the two post-booking denials (hook lock
//! collision, daemon debt gate) unwind; this pins the pre-booking half.
//!
//! Kept as the standing guard for that half: each shape pairs a resolvable write
//! to a covered file with one refusal class — off-allowlist element, denied
//! subcommand, denied flag, a catenary bare-only violation, an opaque write, a
//! denied pipeline stage, a substitution-hosted denial, the subagent branch
//! guard — and asserts the ledger stays empty. The positive control (the same
//! write with no denied element) books, so an empty ledger can never be read as
//! a blind harness.
//!
//! No daemon: the hook-side lock seam is the only ledger booking site and it is
//! hook-process-local (a filesystem fact under `CATENARY_STATE_DIR`), so a
//! phantom booking is visible with the daemon down. The ledger is what a bare
//! `catenary diagnostics` enumerates (root-ownership stage 3 — "the single
//! source of truth, no in-memory mirror"), so it is the batch the report names.

mod common;

use std::io::Write as _;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{isolate_env, xdg_config_home, xdg_state_home};

/// A user config activating the allowlist (so the write resolver runs) with a
/// denied subcommand (`git grep`) and a denied flag (`make -C`) — the adjacent
/// refusal paths the ticket names.
const FILTER_COMMANDS: &str = "\
[commands]
build = \"make\"
allow = [\"printf\", \"cat\", \"cp\", \"tee\", \"git\", \"make\"]
pipeline = [\"grep\"]

[commands.deny]
git = [\"grep\"]

[commands.deny_flags]
make = [\"-C\"]
";

/// Write the user config the isolated `catenary` hook reads (`XDG_CONFIG_HOME`).
fn write_user_config(root: &str, contents: &str) -> Result<()> {
    let dir = xdg_config_home(root).join("catenary");
    std::fs::create_dir_all(&dir).context("create config dir")?;
    std::fs::write(dir.join("config.toml"), contents).context("write config")?;
    Ok(())
}

/// Drive `catenary hook pre-tool --format=claude` for an arbitrary payload with
/// NO daemon running, returning the hook's stdout.
fn run_hook(root: &str, payload: &Value) -> Result<String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    cmd.args(["hook", "pre-tool", "--format=claude"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn `hook pre-tool`")?;
    {
        let mut stdin = child.stdin.take().context("hook stdin")?;
        writeln!(stdin, "{payload}").context("write hook payload")?;
    }
    let out = child.wait_with_output().context("wait for hook")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Drive a `Bash` `command` with `cwd` set to `root`.
fn run_bash_hook(root: &str, command: &str) -> Result<String> {
    run_hook(
        root,
        &json!({
            "session_id": "s-150",
            "cwd": root,
            "tool_name": "Bash",
            "tool_input": { "command": command },
        }),
    )
}

/// Drive a `Bash` `command` as a SUBAGENT (a non-empty `agent_id` is what arms
/// the misc-221 branch guard, anchored at the hook `cwd`).
fn run_subagent_bash_hook(root: &str, command: &str) -> Result<String> {
    run_hook(
        root,
        &json!({
            "session_id": "s-150",
            "agent_id": "worker-1",
            "cwd": root,
            "tool_name": "Bash",
            "tool_input": { "command": command },
        }),
    )
}

/// Parse a Claude `PreToolUse` deny reason, or `None` when the hook allowed
/// (allow is silent → empty stdout).
fn deny_reason(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let out = v.get("hookSpecificOutput")?;
    if out.get("permissionDecision")?.as_str()? != "deny" {
        return None;
    }
    Some(out.get("permissionDecisionReason")?.as_str()?.to_string())
}

/// The due files in `root`'s ledger, read from the isolated state dir.
fn due_files(root: &str) -> Vec<std::path::PathBuf> {
    let locks_base = xdg_state_home(root).join("locks");
    let canonical = std::path::Path::new(root)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(root));
    catenary_cli::lock::due_files_in(&locks_base, &canonical)
}

/// A repository fixture: a tempdir carrying a `.git` marker (so the lock root
/// resolves) and a `src/` dir holding one existing `.rs` file.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Result<Self> {
        let dir = common::canonical_tempdir()?;
        std::fs::create_dir_all(dir.path().join(".git"))?;
        std::fs::create_dir_all(dir.path().join("src"))?;
        std::fs::write(dir.path().join("src/target.rs"), b"fn f() {}\n")?;
        std::fs::write(dir.path().join("src/source.rs"), b"fn g() {}\n")?;
        Ok(Self { dir })
    }

    fn root_str(&self) -> Result<&str> {
        self.dir.path().to_str().context("repo path utf-8")
    }
}

/// Every refusal shape bug 150 names, each carrying a resolvable write to a
/// covered `.rs` file. A refused command runs nothing, so its write targets must
/// never reach the ledger — the gate's honesty contract (debt = files an edit
/// actually touched).
///
/// A shape that stops being DENIED fails here too: an allowed command books, so
/// a silently-weakened denial would otherwise read as a passing "no booking".
#[test]
fn refused_command_books_nothing() -> Result<()> {
    // (label, command, the denial this shape must trip) — the write leg is
    // resolvable and covered in every one. Pinning the reason keeps each shape
    // exercising the refusal class it was written for: a shape that starts
    // denying for some other reason no longer proves anything about its own.
    let shapes: &[(&str, &str, &str)] = &[
        (
            "off-allowlist element",
            "printf 'x\\n' > src/target.rs && rg foo",
            "`rg` isn't allowed",
        ),
        (
            "denied subcommand",
            "printf 'x\\n' > src/target.rs && git grep foo",
            "denied subcommand",
        ),
        (
            "denied flag",
            "printf 'x\\n' > src/target.rs && make -C /tmp all",
            "denied flag",
        ),
        (
            "catenary bare-only violation",
            "printf 'x\\n' > src/target.rs && catenary diagnostics",
            "SOLE command",
        ),
        (
            "opaque write in a later leg",
            "printf 'x\\n' > src/target.rs && cat src/source.rs > \"$OUT\"",
            "isn't set in this command",
        ),
        (
            "denied pipeline stage",
            "printf 'x\\n' | tee src/target.rs | rg foo",
            "`rg` isn't allowed",
        ),
        (
            "cp write, denied element",
            "cp src/source.rs src/target.rs && rg foo",
            "`rg` isn't allowed",
        ),
        (
            "not-yet-existing target (ghost)",
            "printf 'x\\n' > src/ghost.rs && rg foo",
            "`rg` isn't allowed",
        ),
        (
            "denied element FIRST",
            "rg foo && printf 'x\\n' > src/target.rs",
            "`rg` isn't allowed",
        ),
        (
            "git worktree teaching deny",
            "printf 'x\\n' > src/target.rs && git worktree add /tmp/wt",
            "`git worktree` isn't allowed",
        ),
        (
            "retired `catenary sed` teaching deny",
            "printf 'x\\n' > src/target.rs && catenary sed 's/a/b/' src/source.rs",
            "`catenary sed` is retired",
        ),
        (
            "denial inside a command substitution",
            "printf \"$(rg foo)\" > src/target.rs",
            "`rg` isn't allowed",
        ),
        (
            "catenary redirect + denied foreign leg",
            "catenary grep foo > src/target.rs && rg bar",
            "`rg` isn't allowed",
        ),
    ];

    let mut offenders: Vec<String> = Vec::new();
    for (label, command, expected) in shapes {
        // A fresh repo (and fresh isolated state) per shape, so one shape's
        // booking can never be read as another's.
        let repo = Repo::new()?;
        let root = repo.root_str()?;
        write_user_config(root, FILTER_COMMANDS)?;

        let out = run_bash_hook(root, command)?;
        let Some(reason) = deny_reason(&out) else {
            offenders.push(format!("{label}: NOT DENIED — `{command}` → {out:?}"));
            continue;
        };
        if !reason.contains(expected) {
            offenders.push(format!(
                "{label}: denied for the WRONG reason (want {expected:?}) — {reason:?}"
            ));
        }
        let due = due_files(root);
        if !due.is_empty() {
            offenders.push(format!("{label}: booked {due:?} — `{command}`"));
        }
    }

    assert!(
        offenders.is_empty(),
        "a refused command must book nothing (bug 150):\n{}",
        offenders.join("\n")
    );
    Ok(())
}

/// The positive control for [`refused_command_books_nothing`]: the same write
/// with no denied element is ALLOWED and books its covered target.
///
/// Without this, an empty ledger over there could mean either "the refusal
/// booked nothing" (the invariant) or "this harness cannot see a booking at all"
/// (a blind test) — the config failing to load, the type going uncovered, the
/// resolver short-circuiting. This pins the harness's eyesight.
#[test]
fn allowed_write_books_its_target() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    write_user_config(root, FILTER_COMMANDS)?;

    let out = run_bash_hook(root, "printf 'x\\n' > src/target.rs")?;
    assert!(
        deny_reason(&out).is_none(),
        "the control write must be allowed, got: {out}"
    );
    let due = due_files(root);
    assert_eq!(
        due.len(),
        1,
        "an allowed write books its covered target, got: {due:?}"
    );
    Ok(())
}

/// The subagent branch guard (misc 221) books nothing either — the one refusal
/// class reached only with a non-empty `agent_id`, so it carries its own payload
/// rather than riding [`refused_command_books_nothing`]'s table.
#[test]
fn refused_branch_op_books_nothing() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    write_user_config(root, FILTER_COMMANDS)?;
    let outside = common::canonical_tempdir()?;
    let outside_path = outside.path().to_str().context("outside path utf-8")?;

    let command =
        format!("printf 'x\\n' > src/target.rs && git -C {outside_path} checkout -b stray");
    let out = run_subagent_bash_hook(root, &command)?;
    let reason = deny_reason(&out).context("the branch guard must deny a subagent's outside op")?;
    assert!(
        reason.contains("Branch work belongs in your anchored worktree"),
        "the branch guard must be the denial that fired, got: {reason}"
    );
    let due = due_files(root);
    assert!(
        due.is_empty(),
        "the refused branch op must book nothing, got: {due:?}"
    );
    Ok(())
}
