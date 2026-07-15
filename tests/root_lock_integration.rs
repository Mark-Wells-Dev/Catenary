// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::print_stderr,
    reason = "a git-less reconcile-bracket skip announces itself on stderr so a \
              skip is never silently indistinguishable from a pass"
)]
//! End-to-end coverage for the durable root lock (root-ownership stage 2).
//!
//! The lock is acquired at the `PreToolUse` edit seam by the `catenary hook
//! pre-tool` binary itself — **hook-process-local filesystem operations that
//! work with the daemon down**. These tests drive the real hook binary as a
//! subprocess with an isolated env (`isolate_env`, so every lock dir lands under
//! `CATENARY_STATE_DIR` inside the tempdir) and NO daemon running, so the
//! daemon-free lock path is the one exercised:
//!
//! - a first cook's edit is admitted and the lock dir + owner file appear;
//! - a second cook (a different identity) editing the same root is denied with
//!   the ruled briefing, while the first cook's re-edit stays admitted;
//! - read tools and edits in genuinely-foreign territory take no lock.
//!
//! Booking is static-data-driven (the merged default config binds `rust` to
//! `rust-analyzer`), so a `.rs` edit books with no daemon and no binary — the
//! cleared `PATH` never matters.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{isolate_env, xdg_config_home, xdg_state_home};

/// A user config activating the command allowlist so the stateful-tier gate can
/// classify `git`/`make`/`chmod` (root-ownership stage 5). Reads/read-only git
/// (`cat`, `git log`, `git status`) stay Read tier; `make`/`git commit`/`chmod`
/// are Stateful.
const TIER_COMMANDS: &str = "\
[commands]
build = \"make\"
allow = [\"git\", \"cat\", \"chmod\", \"make\"]
pipeline = [\"grep\"]
";

/// Write a user config the isolated `catenary` hook reads (`XDG_CONFIG_HOME`).
fn write_user_config(root: &str, contents: &str) -> Result<()> {
    let dir = xdg_config_home(root).join("catenary");
    std::fs::create_dir_all(&dir).context("create config dir")?;
    std::fs::write(dir.join("config.toml"), contents).context("write config")?;
    Ok(())
}

/// Drive `catenary hook pre-tool --format=claude` for a `Bash` command under
/// the given `session`, with `cwd` set to the root and NO daemon running.
fn run_bash_hook(root: &str, session: &str, command: &str) -> Result<String> {
    let payload = json!({
        "session_id": session,
        "cwd": root,
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });
    run_hook(root, &payload)
}

/// Drive `catenary hook post-tool-use --format=claude` for a `Bash` command —
/// the reconcile bracket's post-command leg (root-ownership stage 5).
///
/// Restores the inherited `PATH` (which `isolate_env` clears) so the bracket's
/// `git status --porcelain` oracle can find the real `git`. Returns the hook's
/// stdout (always empty — `PostToolUse` carries no decision).
fn run_post_tool_hook(root: &str, session: &str, command: &str) -> Result<String> {
    let payload = json!({
        "session_id": session,
        "cwd": root,
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, root);
    // The bracket spawns real `git`; restore the inherited PATH so it resolves.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.args(["hook", "post-tool-use", "--format=claude"]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn `hook post-tool-use`")?;
    {
        let mut stdin = child.stdin.take().context("hook stdin")?;
        writeln!(stdin, "{payload}").context("write hook payload")?;
    }
    let out = child.wait_with_output().context("wait for hook")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a real `git` command in `dir` (a helper for the reconcile-bracket
/// end-to-end test). Non-interactive; asserts a clean exit.
fn git(dir: &std::path::Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run git {args:?}"))?;
    anyhow::ensure!(status.success(), "git {args:?} failed");
    Ok(())
}

/// Drive `catenary hook pre-tool --format=claude` for an `Edit` of `file` under
/// the given `session`, with NO daemon running, returning the hook's stdout.
///
/// The identity tuple the lock titles the owner file with is
/// `claude+<session>+` (empty agent = the main agent).
fn run_edit_hook(root: &str, session: &str, file: &str) -> Result<String> {
    let payload = json!({
        "session_id": session,
        "tool_name": "Edit",
        "tool_input": { "file_path": file },
    });
    run_hook(root, &payload)
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

/// A repository fixture: a tempdir carrying a `.git` marker (so the lock's root
/// resolution admits it) and a `src/` dir for edit targets.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join(".git"))?;
        std::fs::create_dir_all(dir.path().join("src"))?;
        Ok(Self { dir })
    }

    fn root_str(&self) -> Result<&str> {
        self.dir.path().to_str().context("repo path utf-8")
    }

    fn file(&self, rel: &str) -> Result<String> {
        let p = self.dir.path().join(rel);
        std::fs::write(&p, b"fn f() {}\n")?;
        p.to_str().map(str::to_string).context("file path utf-8")
    }
}

/// Two-identity collision: the second cook's `.rs` edit is denied with the ruled
/// briefing; the first cook is unaffected — all with NO daemon (the lock is a
/// hook-plane fact).
#[test]
fn second_cook_denied_with_briefing_daemon_down() -> Result<()> {
    // The env-isolation root doubles as the repo (its `.git` marker makes the
    // lock root resolvable) so `xdg_state_home(root)/locks` is where the lock
    // dir lands.
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.file("src/main.rs")?;

    // First cook edits — admitted (allow is silent).
    let a = run_edit_hook(root, "session-a", &file)?;
    assert!(
        deny_reason(&a).is_none(),
        "first cook's edit must be admitted, got: {a}"
    );

    // The lock dir and its owner file appear under the isolated state dir.
    let locks = xdg_state_home(root).join("locks");
    assert!(
        locks.is_dir(),
        "the lock base dir must exist under the isolated state dir"
    );

    // Second cook (a different session) edits the SAME root — denied with the
    // ruled briefing.
    let b = run_edit_hook(root, "session-b", &file)?;
    let reason = deny_reason(&b).context("second cook must be denied")?;
    assert!(
        reason.contains("root locked:"),
        "briefing header expected, got: {reason}"
    );
    assert!(
        reason.contains(root),
        "the briefing names the locked root path, got: {reason}"
    );
    assert!(
        reason.contains("catenary claim"),
        "the briefing points at `catenary claim`, got: {reason}"
    );
    assert!(
        reason.contains("awaiting diagnosis"),
        "the briefing reports the due count, got: {reason}"
    );

    // First cook is unaffected — a re-edit is still admitted.
    let a2 = run_edit_hook(root, "session-a", &file)?;
    assert!(
        deny_reason(&a2).is_none(),
        "first cook's re-edit must stay admitted, got: {a2}"
    );

    Ok(())
}

/// Drive `catenary hook pre-tool --format=claude` for a `catenary claim <root>`
/// Bash command under the given `session`, with NO daemon running — the hook
/// performs the owner-file rename itself (degrade-open, the lock is a hook-plane
/// fact). Returns the hook's stdout (empty on the allow path).
fn run_claim_hook(root: &str, session: &str, claim_root: &str) -> Result<String> {
    let payload = json!({
        "session_id": session,
        "tool_name": "Bash",
        "tool_input": { "command": format!("catenary claim {claim_root}") },
    });
    run_hook(root, &payload)
}

/// Claim end-to-end (degrade-open, daemon down): a denied second cook takes over
/// with `catenary claim`, then edits freely while the original owner is now
/// denied. The identity-bearing rename runs at the hook (the one seam identity
/// appears); with the daemon down the hook does it locally, proving the lock is a
/// hook-plane fact.
#[test]
fn claim_transfers_ownership_daemon_down() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.file("src/main.rs")?;

    // Session A locks the root.
    let a = run_edit_hook(root, "session-a", &file)?;
    assert!(deny_reason(&a).is_none(), "first cook admitted, got: {a}");

    // Session B is denied.
    let b = run_edit_hook(root, "session-b", &file)?;
    assert!(
        deny_reason(&b).is_some(),
        "second cook must be denied before claiming, got: {b}"
    );

    // Session B claims the root (the claim command's PreToolUse hook performs the
    // rename hook-local; the command itself is allowed, so stdout is empty).
    let claim = run_claim_hook(root, "session-b", root)?;
    assert!(
        deny_reason(&claim).is_none(),
        "the claim command must be allowed, got: {claim}"
    );

    // Session B now edits freely — the owner file was re-titled to it.
    let b2 = run_edit_hook(root, "session-b", &file)?;
    assert!(
        deny_reason(&b2).is_none(),
        "the claimant must edit freely after claiming, got: {b2}"
    );

    // Session A — the prior owner — is now the one denied.
    let a2 = run_edit_hook(root, "session-a", &file)?;
    assert!(
        deny_reason(&a2).is_some(),
        "the prior owner must be denied after the takeover, got: {a2}"
    );

    Ok(())
}

/// A read tool takes no lock — the window stays open for read-only agents even
/// while another agent holds the edit lock.
#[test]
fn read_tool_is_never_locked() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.file("src/main.rs")?;

    // Session A holds the edit lock.
    let a = run_edit_hook(root, "session-a", &file)?;
    assert!(deny_reason(&a).is_none(), "first cook admitted, got: {a}");

    // Session B READS the same file — never denied (reads take no lock).
    let read_payload = json!({
        "session_id": "session-b",
        "tool_name": "Read",
        "tool_input": { "file_path": file },
    });
    let b = run_hook(root, &read_payload)?;
    assert!(
        deny_reason(&b).is_none(),
        "a read from another session must never be locked, got: {b}"
    );

    Ok(())
}

/// Drive `catenary hook pre-tool --format=claude` for a `catenary diagnostics`
/// Bash command (bare or scoped) under the given `session`, with `cwd` set to the
/// root and NO daemon running. Returns the hook's stdout (empty on the allow
/// path). Exercises the hook-side diagnostics OWNER gate (root-ownership stage 3,
/// deliverable 4): the hook reads the lock owner file — a filesystem fact — so it
/// gates with the daemon down.
fn run_diagnostics_hook(root: &str, session: &str, command: &str) -> Result<String> {
    let payload = json!({
        "session_id": session,
        "cwd": root,
        "tool_name": "Bash",
        "tool_input": { "command": command },
    });
    run_hook(root, &payload)
}

/// The hook-side diagnostics owner gate (root-ownership stage 3, deliverable 4):
/// only the lock holder may pull a locked root's ledger via BARE `catenary
/// diagnostics`. A non-owner is denied naming the owed root and taught `catenary
/// claim`; the owner's bare pull is allowed; a scoped pull (naming a path) serves
/// for anyone regardless of ownership. All daemon-down — the gate reads the owner
/// file, a filesystem fact.
#[test]
fn bare_diagnostics_owner_gate_daemon_down() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.file("src/main.rs")?;

    // Session A locks the root with a covered edit.
    let a = run_edit_hook(root, "session-a", &file)?;
    assert!(deny_reason(&a).is_none(), "first cook admitted, got: {a}");

    // Session B — a NON-owner — runs bare `catenary diagnostics` from the root.
    // Denied: pulling A's ledger would serve work B did not author.
    let b_bare = run_diagnostics_hook(root, "session-b", "catenary diagnostics")?;
    let reason = deny_reason(&b_bare).context("a non-owner's bare diagnostics must be denied")?;
    assert!(
        reason.contains("root locked:") && reason.contains(root),
        "the deny names the owed root, got: {reason}"
    );
    assert!(
        reason.contains("catenary claim"),
        "the deny teaches the takeover path, got: {reason}"
    );

    // The OWNER's bare pull is allowed (silent).
    let a_bare = run_diagnostics_hook(root, "session-a", "catenary diagnostics")?;
    assert!(
        deny_reason(&a_bare).is_none(),
        "the owner's bare diagnostics must be allowed, got: {a_bare}"
    );

    // A SCOPED pull (naming a path) serves for anyone regardless of ownership —
    // the pull-anything arm (a diagnose of a named file is a read).
    let b_scoped =
        run_diagnostics_hook(root, "session-b", &format!("catenary diagnostics {file}"))?;
    assert!(
        deny_reason(&b_scoped).is_none(),
        "a scoped diagnostics serves regardless of ownership, got: {b_scoped}"
    );

    Ok(())
}

/// Bare `catenary diagnostics` against an UNLOCKED root is never owner-gated: with
/// no lock holder to protect, any session may pull. The gate fires only on a root
/// another agent holds.
#[test]
fn bare_diagnostics_on_unlocked_root_is_ungated() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;

    // No prior edit → no lock. A bare diagnostics is allowed.
    let out = run_diagnostics_hook(root, "session-a", "catenary diagnostics")?;
    assert!(
        deny_reason(&out).is_none(),
        "bare diagnostics on an unlocked root must be allowed, got: {out}"
    );

    Ok(())
}

/// An uncovered-extension edit (no configured server for `.txt`) books nothing
/// and takes no lock — booking honesty: the lock population matches the edit
/// gate's covered-file population.
#[test]
fn uncovered_edit_takes_no_lock() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.file("notes.txt")?;

    let out = run_edit_hook(root, "session-a", &file)?;
    assert!(
        deny_reason(&out).is_none(),
        "an uncovered edit is always admitted, got: {out}"
    );

    // No lock dir was created for a standalone uncovered edit.
    let locks = xdg_state_home(root).join("locks");
    let empty =
        !locks.exists() || std::fs::read_dir(&locks).map_or(true, |mut it| it.next().is_none());
    assert!(
        empty,
        "an uncovered standalone edit must take no lock (booking honesty)"
    );

    Ok(())
}

// ── Command tiers: stateful operations are kitchen operations (stage 5) ──────

/// A second identity's stateful-tier command (`git commit`, `make`) in a locked
/// kitchen is denied with the SAME briefing the edit seam produces; a waiter's
/// read-tier command (`git log`, `git status`, `cat`) passes anywhere, lock or
/// no lock. All daemon-down — the gate reads the owner file, a filesystem fact.
#[test]
fn stateful_tier_denied_for_second_identity_reads_pass() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.file("src/main.rs")?;
    write_user_config(root, TIER_COMMANDS)?;

    // Session A locks the root with a covered edit.
    let a = run_edit_hook(root, "session-a", &file)?;
    assert!(deny_reason(&a).is_none(), "first cook admitted, got: {a}");

    // Session B — a NON-owner — runs a STATEFUL command in A's kitchen. Denied
    // with the ruled briefing.
    for stateful in ["git commit -m x", "make check", "chmod +x src/main.rs"] {
        let b = run_bash_hook(root, "session-b", stateful)?;
        let reason = deny_reason(&b)
            .with_context(|| format!("a second identity's `{stateful}` must be denied"))?;
        assert!(
            reason.contains("root locked:") && reason.contains(root),
            "the `{stateful}` deny names the locked root, got: {reason}"
        );
        assert!(
            reason.contains("catenary claim"),
            "the `{stateful}` deny teaches the takeover path, got: {reason}"
        );
    }

    // Session B — a NON-owner — runs READ-tier commands in A's kitchen. Allowed:
    // a waiter looks through the window unbounded.
    for read in ["git log --oneline", "git status", "cat src/main.rs"] {
        let b = run_bash_hook(root, "session-b", read)?;
        assert!(
            deny_reason(&b).is_none(),
            "a waiter's `{read}` must pass in a locked kitchen, got: {b}"
        );
    }

    // Session A — the OWNER — runs its own stateful command freely.
    let a_stateful = run_bash_hook(root, "session-a", "git commit -m x")?;
    assert!(
        deny_reason(&a_stateful).is_none(),
        "the owner's own stateful command must pass, got: {a_stateful}"
    );

    Ok(())
}

/// A stateful-tier command against an UNLOCKED root imposes nothing — a lone cook
/// works free. The asymmetry: only a root LOCKED BY ANOTHER identity denies.
#[test]
fn stateful_tier_on_unlocked_root_is_free() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    write_user_config(root, TIER_COMMANDS)?;

    // No prior edit → no lock. A stateful command is allowed.
    for stateful in ["git commit -m x", "make check", "chmod +x src/main.rs"] {
        let out = run_bash_hook(root, "session-a", stateful)?;
        assert!(
            deny_reason(&out).is_none(),
            "`{stateful}` on an unlocked root must be free, got: {out}"
        );
    }

    Ok(())
}

// ── The reconcile bracket end-to-end (stage 5) ──────────────────────────────

/// The number of files due in `root`'s ledger, read from the isolated state dir
/// (the `.lock` touch-tree under `<state>/locks/<encoded-root>/dir/`).
fn due_count(root: &str) -> usize {
    let locks_base = xdg_state_home(root).join("locks");
    let canonical = std::path::Path::new(root)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(root));
    catenary_cli::lock::due_files_in(&locks_base, &canonical).len()
}

/// End-to-end debt round-trip through the reconcile bracket, driven by REAL git:
/// edit → booked; `git stash` → unbooked (no debt); `git stash pop` → re-booked.
/// The bracket runs at the `post-tool-use` hook with `git status --porcelain` as
/// the changed-ness oracle — attribution is clean because it wraps the cook's own
/// command. Skips gracefully if `git` is unavailable on PATH.
#[test]
fn reconcile_bracket_round_trip_edit_stash_pop() -> Result<()> {
    if which_git().is_none() {
        eprintln!("reconcile_bracket_round_trip: skipping — git not on PATH");
        return Ok(());
    }
    let repo = tempfile::tempdir()?;
    let dir = repo.path();
    let root = dir.to_str().context("repo path utf-8")?;

    // A real git repo with a committed `src/main.rs`.
    git(dir, &["init", "-q"])?;
    git(dir, &["config", "user.email", "t@t"])?;
    git(dir, &["config", "user.name", "t"])?;
    std::fs::create_dir_all(dir.join("src"))?;
    let file = dir.join("src/main.rs");
    std::fs::write(&file, "fn main() {}\n")?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-qm", "init"])?;
    let file_str = file.to_str().context("file path utf-8")?;

    // The cook edits the file (a real content change) and the edit hook books it.
    std::fs::write(&file, "fn main() { let x = 1; }\n")?;
    let e = run_edit_hook(root, "cook", file_str)?;
    assert!(deny_reason(&e).is_none(), "the edit is admitted, got: {e}");
    assert_eq!(due_count(root), 1, "the edit books one file");

    // `git stash` moves the modification out of the working tree; the bracket's
    // post-command reconcile finds it clean and UNBOOKS it.
    git(dir, &["stash", "-q"])?;
    run_post_tool_hook(root, "cook", "git stash")?;
    assert_eq!(
        due_count(root),
        0,
        "the stash unbooks: bare diagnostics would answer no-debt"
    );

    // `git stash pop` restores the modification; the bracket BOOKS the covered
    // file again ("the pop should present as writes that restore it").
    git(dir, &["stash", "pop", "-q"])?;
    run_post_tool_hook(root, "cook", "git stash pop")?;
    assert_eq!(
        due_count(root),
        1,
        "the pop re-books the restored file — diagnose serves it"
    );

    Ok(())
}

/// `git checkout -- <file>` unbooks exactly the reverted file, driven by REAL
/// git: a two-file edit books both; reverting one via checkout unbooks only it.
#[test]
fn reconcile_bracket_checkout_unbooks_reverted_file() -> Result<()> {
    if which_git().is_none() {
        eprintln!("reconcile_bracket_checkout: skipping — git not on PATH");
        return Ok(());
    }
    let repo = tempfile::tempdir()?;
    let dir = repo.path();
    let root = dir.to_str().context("repo path utf-8")?;

    git(dir, &["init", "-q"])?;
    git(dir, &["config", "user.email", "t@t"])?;
    git(dir, &["config", "user.name", "t"])?;
    std::fs::create_dir_all(dir.join("src"))?;
    let reverted = dir.join("src/reverted.rs");
    let kept = dir.join("src/kept.rs");
    std::fs::write(&reverted, "fn a() {}\n")?;
    std::fs::write(&kept, "fn b() {}\n")?;
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-qm", "init"])?;

    // Both files edited and booked.
    std::fs::write(&reverted, "fn a() { 1; }\n")?;
    std::fs::write(&kept, "fn b() { 2; }\n")?;
    run_edit_hook(root, "cook", reverted.to_str().context("path")?)?;
    run_edit_hook(root, "cook", kept.to_str().context("path")?)?;
    assert_eq!(due_count(root), 2, "both edits booked");

    // Revert only `reverted.rs` via checkout; the bracket unbooks exactly it.
    git(dir, &["checkout", "--", "src/reverted.rs"])?;
    run_post_tool_hook(root, "cook", "git checkout -- src/reverted.rs")?;
    assert_eq!(
        due_count(root),
        1,
        "checkout unbooks exactly the reverted file; the other stays due"
    );

    // The still-due file is `kept.rs`.
    let locks_base = xdg_state_home(root).join("locks");
    let canonical = dir.canonicalize()?;
    let due = catenary_cli::lock::due_files_in(&locks_base, &canonical);
    assert_eq!(due, vec![kept.canonicalize()?], "kept.rs remains booked");

    Ok(())
}

/// Whether `git` resolves on the current PATH (the reconcile bracket needs it).
fn which_git() -> Option<()> {
    Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(std::process::ExitStatus::success)
        .map(|_| ())
}
