// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end coverage for the ledger-keyed shell write gate (bug 118).
//!
//! The gate self-ARMED: a write-resolving command (a redirect/append, `cp`,
//! `tee`, `sed -i`, `git apply`) booked its own target into the durable debt
//! ledger at the hook-side lock seam, and then the DAEMON-side debt gate — which
//! reads that same ledger over IPC — denied the command on the very booking it
//! had just created. A paid ledger could never be kept paid: the next covered
//! write re-armed the gate against itself, forever.
//!
//! These drive the REAL `catenary hook pre-tool` binary as a subprocess against
//! a LIVE daemon (so the daemon debt gate actually fires — a daemon-down hook
//! books but never reaches the gate). The isolated env points every base dir at
//! the bridge's `state_home`, so the hook books into the SAME ledger the daemon
//! reads. A `.rs` edit books via the merged default config (`rust` →
//! `rust-analyzer`), so no real language server is needed — the daemon just has
//! to be up for the IPC round-trip.
//!
//! The three ruled invariants under test:
//!   1. **check-then-book** — a content-changing redirect to a covered file on a
//!      PAID ledger is ALLOWED (the exact bug-118 repro).
//!   2. **deny-books-nothing** — a command denied for any reason leaves NO
//!      booking behind (asserted against the ledger, not just the receipt).
//!   3. **the honest gate survives** — an executed write books; the NEXT write
//!      to the same file is gated until paid.

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use common::{BridgeProcess, isolate_env, xdg_config_home, xdg_state_home};

/// A user config activating the command allowlist so the write resolver runs.
/// `printf`/`cat` write through redirects (resolved and booked); `git` covers
/// the git-authorship split. The allowlist must be ACTIVE (non-empty `allow`)
/// or the resolver short-circuits and books nothing — no write to gate.
const WRITE_COMMANDS: &str = "\
[commands]
allow = [\"printf\", \"cat\", \"cp\", \"tee\", \"git\"]
pipeline = [\"grep\"]
";

/// Write the user config the isolated `catenary` hook reads (`XDG_CONFIG_HOME`).
fn write_user_config(state_home: &str, contents: &str) -> Result<()> {
    let dir = xdg_config_home(state_home).join("catenary");
    std::fs::create_dir_all(&dir).context("create config dir")?;
    std::fs::write(dir.join("config.toml"), contents).context("write config")?;
    Ok(())
}

/// The workspace root: a tempdir carrying a `.git` marker (so the lock root
/// resolves) and a `src/` dir for edit / write targets.
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

    /// Create (and return the path of) a `.rs` file under the repo — a type the
    /// default config books.
    fn rs_file(&self, rel: &str) -> Result<String> {
        let p = self.dir.path().join(rel);
        std::fs::write(&p, b"fn f() {}\n")?;
        p.to_str().map(str::to_string).context("file path utf-8")
    }
}

/// Drive `catenary hook pre-tool --format=claude` for an arbitrary payload
/// against `bridge`'s daemon, isolating the env to the bridge's `state_home` so
/// the hook books into the daemon's ledger and reaches the daemon socket.
fn run_hook(bridge: &BridgeProcess, payload: &Value) -> Result<String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_catenary"));
    isolate_env(&mut cmd, bridge.state_home());
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

/// Drive an `Edit` of `file` (books it at the lock seam).
fn run_edit_hook(bridge: &BridgeProcess, file: &str) -> Result<String> {
    run_hook(
        bridge,
        &json!({
            "cwd": null,
            "tool_name": "Edit",
            "tool_input": { "file_path": file },
        }),
    )
}

/// Drive a `Bash` `command` with `cwd` set to `root` — the shape the daemon
/// debt gate needs (it resolves the kitchen from `cwd`).
fn run_bash_hook(bridge: &BridgeProcess, root: &str, command: &str) -> Result<String> {
    run_hook(
        bridge,
        &json!({
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

/// The due files in `root`'s ledger, read from the bridge's isolated state dir.
fn due_files(bridge: &BridgeProcess, root: &str) -> Vec<std::path::PathBuf> {
    let locks_base = xdg_state_home(bridge.state_home()).join("locks");
    let canonical = std::path::Path::new(root)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(root));
    catenary_cli::lock::due_files_in(&locks_base, &canonical)
}

/// Perform the write the admitted tool would perform (misc 230).
///
/// The hook books a TRACKING entry carrying the target's pre-write fingerprint;
/// debt is asserted at consult only once the content has actually moved. These
/// tests drive the hook alone — the shell never runs — so a case that models an
/// EXECUTED write has to move the bytes itself, exactly as the shell would after
/// the hook allows.
fn execute_write(path: &str, line: &str) -> Result<()> {
    let mut bytes = std::fs::read(path).unwrap_or_default();
    bytes.extend_from_slice(line.as_bytes());
    std::fs::write(path, &bytes).context("execute the admitted write")
}

/// Pay a root's ledger for a set of files — the diagnostics delivery seam
/// unlinks their touch leaves. Simulates `catenary diagnostics <files>` without
/// needing a real language server.
fn pay(bridge: &BridgeProcess, root: &str, files: &[&str]) {
    let locks_base = xdg_state_home(bridge.state_home()).join("locks");
    let canonical = std::path::Path::new(root)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(root));
    let paths: Vec<std::path::PathBuf> = files.iter().map(std::path::PathBuf::from).collect();
    catenary_cli::lock::unlink_delivered_in(&locks_base, &canonical, &paths);
}

/// Spawn a daemon with no language servers but a live IPC socket, rooted at
/// `root`. The debt gate reads the durable ledger, not the LSP, so no server is
/// needed — only the IPC round-trip.
fn spawn_daemon(root: &str) -> Result<BridgeProcess> {
    let mut bridge = BridgeProcess::spawn(&[], root)?;
    bridge.initialize()?;
    bridge.wait_for_ipc_socket()?;
    Ok(bridge)
}

/// bug 118, invariant 1 (check-then-book): a covered edit books; the ledger is
/// PAID; then a content-changing redirect/append to that same covered file is
/// ALLOWED — the command must never be denied on the debt its own booking just
/// created. This is the exact attended repro (`diagnostics` → `[clean]`, then
/// `printf … >> src/lock.rs` denied forever).
#[test]
fn paid_ledger_then_append_is_allowed() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.rs_file("src/lock.rs")?;
    let bridge = spawn_daemon(root)?;
    write_user_config(bridge.state_home(), WRITE_COMMANDS)?;

    // A covered edit books the file, and the admitted edit then runs.
    let e = run_edit_hook(&bridge, &file)?;
    assert!(deny_reason(&e).is_none(), "the edit is admitted, got: {e}");
    execute_write(&file, "// edit\n")?;
    assert_eq!(due_files(&bridge, root).len(), 1, "the edit books one file");

    // Pay the ledger (diagnostics delivery unlinks the touch leaf).
    pay(&bridge, root, &[&file]);
    assert_eq!(
        due_files(&bridge, root).len(),
        0,
        "the ledger is paid — no debt"
    );

    // The bug: an append to the same covered file self-armed and was denied on
    // its own fresh booking. With check-then-book it must be ALLOWED.
    let append = run_bash_hook(&bridge, root, "printf '// probe\\n' >> src/lock.rs")?;
    assert!(
        deny_reason(&append).is_none(),
        "a paid-ledger append to a covered file must be allowed, got: {append}"
    );
    execute_write(&file, "// probe\n")?;

    // The write DID resolve and book (it runs after this allow), so its target
    // is now honestly due — the write model attributing the redirect like an
    // edit.
    let due = due_files(&bridge, root);
    assert_eq!(due.len(), 1, "the allowed append books its target");
    assert_eq!(
        due,
        vec![std::path::Path::new(&file).canonicalize()?],
        "the booked target is the appended file"
    );

    Ok(())
}

/// bug 118, invariant 3 (the honest gate survives): after a write EXECUTES and
/// books, the NEXT write to the SAME file is correctly gated until paid — and
/// paying unblocks it. The check-then-book fix must not weaken this: the second
/// write's target was already due (from the first write's booking), so it is NOT
/// in the self-booked cut and the gate fires honestly.
#[test]
fn executed_write_gates_next_write_until_paid() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let file = repo.rs_file("src/lock.rs")?;
    let bridge = spawn_daemon(root)?;
    write_user_config(bridge.state_home(), WRITE_COMMANDS)?;

    // First append: allowed (paid — actually never edited, so no debt), and it
    // books its target.
    let first = run_bash_hook(&bridge, root, "printf '// one\\n' >> src/lock.rs")?;
    assert!(
        deny_reason(&first).is_none(),
        "the first append on an unarmed ledger is allowed, got: {first}"
    );
    execute_write(&file, "// one\n")?;
    assert_eq!(
        due_files(&bridge, root).len(),
        1,
        "the executed write books its target"
    );

    // Second append to the SAME file: now honestly gated — the file is due from
    // the first write, which is NOT this command's own fresh booking.
    let second = run_bash_hook(&bridge, root, "printf '// two\\n' >> src/lock.rs")?;
    let reason = deny_reason(&second)
        .context("the second write to a due file must be gated (honest gate)")?;
    assert!(
        reason.contains("edited but haven't been diagnosed"),
        "the honest gate names the undiagnosed debt, got: {reason}"
    );
    assert!(
        reason.contains("src/lock.rs") || reason.contains(&file),
        "the honest gate names the due file, got: {reason}"
    );

    // Paying unblocks it.
    pay(&bridge, root, &[&file]);
    let after = run_bash_hook(&bridge, root, "printf '// three\\n' >> src/lock.rs")?;
    assert!(
        deny_reason(&after).is_none(),
        "paying the ledger unblocks the next write, got: {after}"
    );

    Ok(())
}

/// bug 118, invariant 2 (deny-books-nothing): a command denied for a REAL reason
/// (pre-existing, undiagnosed debt on ANOTHER file in the kitchen) must leave NO
/// booking behind — its own resolved write is unwound, so the ledger holds only
/// the pre-existing debt, never the phantom debt of a command that never ran.
#[test]
fn denied_write_leaves_no_booking() -> Result<()> {
    let repo = Repo::new()?;
    let root = repo.root_str()?;
    let edited = repo.rs_file("src/edited.rs")?;
    let redirect_target = repo.rs_file("src/target.rs")?;
    let bridge = spawn_daemon(root)?;
    write_user_config(bridge.state_home(), WRITE_COMMANDS)?;

    // A covered edit books `edited.rs` and is NOT paid — the kitchen carries
    // real, honest debt.
    let e = run_edit_hook(&bridge, &edited)?;
    assert!(deny_reason(&e).is_none(), "the edit is admitted, got: {e}");
    execute_write(&edited, "// edit\n")?;
    let before: Vec<_> = due_files(&bridge, root);
    assert_eq!(before.len(), 1, "the unpaid edit is the only debt");

    // A redirect to a DIFFERENT covered file resolves and books `target.rs`
    // hook-side, but the daemon debt gate denies on the pre-existing `edited.rs`
    // debt. Deny-books-nothing: `target.rs`'s fresh booking must be unwound.
    let redirect = run_bash_hook(&bridge, root, "printf '// x\\n' >> src/target.rs")?;
    let reason = deny_reason(&redirect)
        .context("the redirect must be denied on the pre-existing edited.rs debt")?;
    assert!(
        reason.contains("edited but haven't been diagnosed"),
        "the deny is the undiagnosed-edits gate, got: {reason}"
    );

    // The ledger holds ONLY the pre-existing debt — the denied redirect booked
    // nothing that survives.
    let after: Vec<_> = due_files(&bridge, root);
    assert_eq!(
        after, before,
        "a denied command leaves no booking behind — ledger unchanged"
    );
    let target_canonical = std::path::Path::new(&redirect_target).canonicalize()?;
    assert!(
        !after.contains(&target_canonical),
        "the denied redirect's target must not remain booked (no phantom debt)"
    );

    Ok(())
}

/// bug 118, invariant 1, under the macOS geometry: a first write to a target
/// that does NOT EXIST YET, reached through a symlinked root prefix, must still
/// be allowed — and must still arm the honest gate for the next one.
///
/// This is the seam misc 230 stressed and the macOS CI red exposed. The hook
/// books write targets *before* the shell runs (the fingerprint has to snapshot
/// the pre-write state), so the booked path is frequently one `canonicalize`
/// cannot resolve. Every spelling in the round trip therefore has to come from
/// the same lenient resolver:
///
///   - the ledger leaf `acquire` writes,
///   - the `already_due` membership test that computes the self-arm cut,
///   - the daemon-side `pre_existing_debt` subtraction.
///
/// With a raw fallback at any one of them the cut misses and the command is
/// denied on its own fresh booking — bug 118, resurrected for exactly the paths
/// a macOS agent uses, since `/tmp` and every tempdir there sit under a symlink.
#[test]
fn ghost_target_under_a_symlinked_root_does_not_self_arm() -> Result<()> {
    let repo = Repo::new()?;
    let real = repo.root_str()?;
    // The aliased spelling: a sibling symlink to the repo root, which is what a
    // host on macOS hands the hook for every path under `/tmp`.
    let link_holder = tempfile::tempdir()?;
    let alias = link_holder.path().join("alias");
    std::os::unix::fs::symlink(real, &alias)?;
    let alias_root = alias.to_str().context("alias utf-8")?;

    let bridge = spawn_daemon(alias_root)?;
    write_user_config(bridge.state_home(), WRITE_COMMANDS)?;

    // The target does not exist — a redirect that CREATES it. `canonicalize`
    // cannot resolve it, so this is the case the raw fallback broke.
    let ghost = alias.join("src/ghost.rs");
    assert!(
        !ghost.exists(),
        "the target must be a ghost at booking time"
    );

    let first = run_bash_hook(&bridge, alias_root, "printf 'x\\n' > src/ghost.rs")?;
    assert!(
        deny_reason(&first).is_none(),
        "a first write to a ghost target must never be denied on its own \
         booking (bug 118 check-then-book), got: {first}"
    );

    // The command runs and creates the file.
    let ghost_str = ghost.to_str().context("ghost utf-8")?;
    execute_write(ghost_str, "x\n")?;

    // The booking landed on the canonical ledger under the mirrored relative
    // path — not a flattened alias spelling — so the debt is real and payable.
    let due = due_files(&bridge, alias_root);
    assert_eq!(
        due,
        vec![ghost.canonicalize()?],
        "the created ghost is owed under its canonical spelling, got: {due:?}"
    );

    // …and the honest gate still fires for the NEXT write to the same file.
    let second = run_bash_hook(&bridge, alias_root, "printf 'y\\n' >> src/ghost.rs")?;
    let reason =
        deny_reason(&second).context("the second write to a now-due ghost target must be gated")?;
    assert!(
        reason.contains("edited but haven't been diagnosed"),
        "the honest gate names the undiagnosed debt, got: {reason}"
    );

    Ok(())
}
