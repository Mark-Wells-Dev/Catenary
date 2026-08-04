// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Integration tests for persisted pins (misc 175).
//!
//! `catenary pin` is durable operator intent: the root is recorded in the user
//! config's `[roots] pinned` list (a comment-preserving TOML edit) so it survives
//! a daemon restart, re-added at the next boot as a `hook` contributor. These
//! end-to-end tests drive the real IPC pin/unpin path (`tool/roots-add` /
//! `tool/roots-rm`, what the CLI sends) against a `mockls`-backed daemon and a
//! shared, externally-owned state dir, then bounce the daemon (`catenary stop` +
//! a fresh bridge in the same state) to exercise boot restore.
//!
//! The daemon's own `CATENARY_ROOTS` boot seed shares the file, since it is the
//! other contributor a boot installs: its survivability across a pin/unpin
//! re-sync (misc 192) and its source — an explicit variable and nothing else,
//! no cwd fallback (bug 145).
//!
//! The config the daemon writes and reads is `$CATENARY_CONFIG_DIR/catenary/
//! config.toml`, which `isolate_env` points at `<state>/config` — the same file
//! on both the write leg (daemon-side pin) and the read leg (boot restore). Tests
//! derive it through `common::xdg_config_home` so both sides agree (per AGENTS.md
//! `isolate_env` discipline). No `CATENARY_CONFIG` is set — that is an
//! explicit-file override, not the user config the pin list lives in.

#![cfg(unix)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use common::{BridgeProcess, ipc_request, mockls_lsp_arg, xdg_config_home, xdg_runtime_dir};

const MOCK_LANG: &str = "pIn17";

/// How long the boot-restore laziness tests keep sampling the snapshot after
/// their positive signal has landed.
///
/// These assert that something never happens, which no completion signal can
/// prove outright — so they anchor on the strongest positive signal available
/// (the restored pin on the roots board; in the warm-root sibling, a spawned
/// server for the boot root) and then observe for a bounded period, sampling at
/// `POLL_SPACING` throughout. Sized well past the ~250 ms at which the bug-155
/// macOS spawn landed: a violation the daemon commits at boot has long since
/// surfaced on the snapshot board by the end of this window.
const BOOT_OBSERVATION_WINDOW: Duration = Duration::from_secs(2);

/// The user config file the daemon reads and the pin write targets, under the
/// isolated state home.
fn user_config_path(state_home: &str) -> PathBuf {
    xdg_config_home(state_home)
        .join("catenary")
        .join("config.toml")
}

/// Writes `contents` to the isolated user config, creating the directory.
fn write_user_config(state_home: &str, contents: &str) -> Result<PathBuf> {
    let path = user_config_path(state_home);
    std::fs::create_dir_all(path.parent().context("config parent")?)?;
    std::fs::write(&path, contents)?;
    Ok(path)
}

/// Reads the isolated user config (empty string when absent).
fn read_user_config(state_home: &str) -> String {
    std::fs::read_to_string(user_config_path(state_home)).unwrap_or_default()
}

/// Whether the config's `[roots] pinned` list carries `target` — spelling-agnostic
/// (a `~`-rendered entry and its absolute form both count).
///
/// The written entry is home-compressed (`~/...` under `$HOME`), so a literal
/// substring check against the absolute `target` would miss it. This expands each
/// entry's leading `~` and canonicalizes, matching the daemon's own restore
/// comparison.
fn config_pins_target(state_home: &str, target: &Path) -> bool {
    let text = read_user_config(state_home);
    let Ok(doc) = toml::from_str::<toml::Value>(&text) else {
        return false;
    };
    let home = std::env::var("HOME").unwrap_or_default();
    doc.get("roots")
        .and_then(|r| r.get("pinned"))
        .and_then(toml::Value::as_array)
        .is_some_and(|arr| {
            arr.iter().filter_map(toml::Value::as_str).any(|entry| {
                let expanded = entry.strip_prefix("~/").map_or_else(
                    || PathBuf::from(entry),
                    |rest| PathBuf::from(&home).join(rest),
                );
                expanded.canonicalize().is_ok_and(|c| c == target)
            })
        })
}

/// Runs `catenary stop` against the isolated state dir and waits for the sockets
/// to disappear — a genuine daemon bounce.
fn stop_daemon(state_home: &str) -> Result<()> {
    let mut stop = Command::new(env!("CARGO_BIN_EXE_catenary"));
    common::isolate_env(&mut stop, state_home);
    stop.arg("stop");
    let out = stop.output().context("run catenary stop")?;
    assert!(
        out.status.success(),
        "catenary stop must exit 0, stderr:\n{}",
        String::from_utf8_lossy(&out.stderr),
    );
    let sock = common::xdg_state_home(state_home)
        .join("catenary")
        .join("catenary-mcp.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if sock.exists() {
        bail!("daemon socket still present after stop");
    }
    // Pulse 04: `catenary stop` now declares a standing stop intent, under
    // which a freshly spawned bridge waits connect-only and never respawns
    // the daemon. This helper's contract is a *bounce* — the next spawned
    // bridge must bring the daemon back with its own env (servers, roots) —
    // so clear the marker, making the death read as a crash.
    let marker = common::xdg_runtime_dir(state_home).join("daemon.intent");
    if marker.exists() {
        std::fs::remove_file(&marker).context("clear daemon.intent after stop")?;
    }
    Ok(())
}

/// Sends the `tool/roots-add` IPC the `catenary pin` CLI sends, returning the
/// daemon's status string.
fn pin(socket: &Path, path: &str) -> Result<String> {
    let resp = ipc_request(socket, &json!({ "method": "tool/roots-add", "path": path }))?;
    Ok(status_of(&resp))
}

/// Sends the `tool/roots-rm` IPC the `catenary unpin` CLI sends, returning the
/// daemon's status string.
fn unpin(socket: &Path, path: &str) -> Result<String> {
    let resp = ipc_request(socket, &json!({ "method": "tool/roots-rm", "path": path }))?;
    Ok(status_of(&resp))
}

/// Parses the `status` field out of an IPC response line.
fn status_of(resp: &str) -> String {
    serde_json::from_str::<serde_json::Value>(resp.trim())
        .ok()
        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(String::from))
        .unwrap_or_default()
}

/// Returns the sorted contributor sources `roots-ls` reports for `target`, or
/// `None` when `target` is not a tracked root at all.
///
/// The env-seed survival tests (misc 192) assert both that the seed root stays
/// tracked across a pin/unpin AND that it is attributed to the `seed:env`
/// contributor — an honest board presence, not a phantom membership.
fn roots_ls_sources(socket: &Path, target: &str) -> Result<Option<Vec<String>>> {
    let resp = ipc_request(socket, &json!({ "method": "tool/roots-ls" }))?;
    let roots: serde_json::Value = serde_json::from_str(resp.trim()).context("roots-ls json")?;
    Ok(roots["roots"].as_array().and_then(|arr| {
        arr.iter()
            .find(|e| e["path"].as_str() == Some(target))
            .and_then(|e| e["sources"].as_array())
            .map(|s| {
                s.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
    }))
}

/// Every root path `roots-ls` reports, sorted — the whole board, not one entry.
///
/// The boot-seed tests (bug 145) assert on the board's *size*: that a rootless
/// boot registers nothing at all is a claim no per-target lookup can make.
fn roots_ls_paths(socket: &Path) -> Result<Vec<String>> {
    let resp = ipc_request(socket, &json!({ "method": "tool/roots-ls" }))?;
    let roots: serde_json::Value = serde_json::from_str(resp.trim()).context("roots-ls json")?;
    let mut paths: Vec<String> = roots["roots"].as_array().map_or_else(Vec::new, |arr| {
        arr.iter()
            .filter_map(|e| e["path"].as_str().map(String::from))
            .collect()
    });
    paths.sort();
    Ok(paths)
}

/// Reads the daemon's `state.json` snapshot from the isolated runtime dir.
fn read_snapshot(state_home: &str) -> Option<serde_json::Value> {
    let state_json = xdg_runtime_dir(state_home)
        .join("catenary")
        .join("state.json");
    let text = std::fs::read_to_string(state_json).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether the daemon's servers board carries any server scoped to `root`.
///
/// The board's `scope_root` renders the daemon's canonical spelling, so callers
/// pass a canonical path (`common::canonical_tempdir`) — which is also what
/// makes a hit attributable to one specific tempdir rather than merely proving
/// that *something* spawned (bug 155).
fn servers_for_root(state_home: &str, root: &str) -> bool {
    read_snapshot(state_home).is_some_and(|snap| {
        snap["servers"].as_array().is_some_and(|servers| {
            servers
                .iter()
                .any(|s| s["scope_root"].as_str() == Some(root))
        })
    })
}

/// A pin survives a daemon restart: pinning writes the config, and a fresh daemon
/// re-adds the entry as a tracked (non-ephemeral) root at boot.
#[test]
fn pin_survives_daemon_restart() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    // A base root that holds the first daemon open, distinct from the pin target.
    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // The root to pin — canonical, so it matches the daemon's stored form.
    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");

    // ── First daemon: pin the target. ────────────────────────────────
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    assert_eq!(pin(&socket, target_str)?, "ok", "pin succeeds");
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;

    // The pin is now in the user config.
    assert!(
        config_pins_target(state_home, target.path()),
        "pin persisted to user config:\n{}",
        read_user_config(state_home)
    );

    // ── Bounce the daemon. ───────────────────────────────────────────
    drop(bridge);
    stop_daemon(state_home)?;

    // ── Second daemon: the pin is restored from config at boot. ──────
    let mut bridge2 = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge2.initialize()?;
    let socket2 = bridge2.wait_for_ipc_socket()?;
    bridge2.wait_for_root(target_str, Duration::from_secs(5))?;

    // roots-ls reports it as a pinned (not ephemeral) root — the whole point of
    // the sighting was that a restart downgraded it to `[ephemeral · expires
    // when idle]`.
    let ls = ipc_request(&socket2, &json!({ "method": "tool/roots-ls" }))?;
    let roots: serde_json::Value = serde_json::from_str(ls.trim()).context("roots-ls json")?;
    let entry = roots["roots"]
        .as_array()
        .and_then(|arr| arr.iter().find(|e| e["path"].as_str() == Some(target_str)))
        .context("restored root present in roots-ls")?;
    assert_eq!(
        entry["ephemeral"].as_bool(),
        Some(false),
        "restored pin is pinned, not ephemeral: {entry}"
    );

    drop(bridge2);
    stop_daemon(state_home)?;
    Ok(())
}

/// `unpin` removes the entry from the user config, so it does not return on the
/// next daemon start.
#[test]
fn unpin_removes_the_config_entry() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    assert_eq!(pin(&socket, target_str)?, "ok");
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;
    assert!(
        config_pins_target(state_home, target.path()),
        "pinned first"
    );

    assert_eq!(unpin(&socket, target_str)?, "ok", "unpin succeeds");
    assert!(
        !config_pins_target(state_home, target.path()),
        "unpin removed the config entry:\n{}",
        read_user_config(state_home)
    );

    // A repeat unpin is a benign no-op (nothing to remove).
    assert_eq!(
        unpin(&socket, target_str)?,
        "not_found",
        "repeat unpin is idempotent"
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// Comments and layout in a hand-authored config survive a pin/unpin round-trip
/// performed through the live daemon (the comment-preserving write path).
#[test]
fn hand_authored_comments_survive_pin_unpin_round_trip() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;

    // A hand-tuned config with comments the write must never clobber.
    let original = "\
# Catenary — hand tuned, do not clobber.
log_retention_days = 21   # three weeks

[roots]
# my durable projects
pinned = [
]

# rust toolchain
[lsp.server.rust-analyzer]
path = \"/usr/bin/rust-analyzer\"   # pinned binary
";
    write_user_config(state_home, original)?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    assert_eq!(pin(&socket, target_str)?, "ok");
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;

    let after_pin = read_user_config(state_home);
    for line in [
        "# Catenary — hand tuned, do not clobber.",
        "log_retention_days = 21   # three weeks",
        "# my durable projects",
        "# rust toolchain",
        "path = \"/usr/bin/rust-analyzer\"   # pinned binary",
    ] {
        assert!(
            after_pin.contains(line),
            "comment/line survives pin: {line}\n{after_pin}"
        );
    }
    assert!(
        config_pins_target(state_home, target.path()),
        "pin added: {after_pin}"
    );

    assert_eq!(unpin(&socket, target_str)?, "ok");
    let after_unpin = read_user_config(state_home);
    assert!(
        !config_pins_target(state_home, target.path()),
        "unpin removed the entry:\n{after_unpin}"
    );
    for line in [
        "# Catenary — hand tuned, do not clobber.",
        "# my durable projects",
        "# rust toolchain",
        "path = \"/usr/bin/rust-analyzer\"   # pinned binary",
    ] {
        assert!(
            after_unpin.contains(line),
            "comment/line survives unpin: {line}\n{after_unpin}"
        );
    }

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// A pinned path missing at boot is KEPT in the config (never pruned) and
/// surfaced by `catenary doctor` — operator intent is never silently discarded.
#[test]
fn missing_pin_at_boot_is_kept_and_flagged_by_doctor() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // A pinned entry pointing at a path that does not exist on disk.
    let ghost = state_dir.path().join("ghost-repo");
    let ghost_str = ghost.to_str().context("ghost path")?;
    let config = format!("[roots]\npinned = [\n  \"{ghost_str}\",\n]\n");
    write_user_config(state_home, &config)?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // roots-ls must NOT track the missing path (never spawned/tracked at boot),
    // and the base root confirms the daemon is up.
    let ls = ipc_request(&socket, &json!({ "method": "tool/roots-ls" }))?;
    assert!(
        !ls.contains(ghost_str),
        "missing pin is not tracked at boot: {ls}"
    );

    // The config still carries the entry — Catenary never rewrites it.
    let persisted = read_user_config(state_home);
    assert!(
        persisted.contains(ghost_str),
        "missing pin kept in config, not pruned:\n{persisted}"
    );

    // `catenary doctor` flags it (the config-pinned-root-missing finding).
    let mut doctor = Command::new(env!("CARGO_BIN_EXE_catenary"));
    common::isolate_env(&mut doctor, state_home);
    doctor.arg("doctor");
    let out = doctor.output().context("run catenary doctor")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(ghost_str) && stdout.to_lowercase().contains("missing"),
        "doctor surfaces the missing pin:\n{stdout}"
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// Boot restore spawns no language server: a restored pin is a tracker entry and
/// a roots-board line only, until its first touch (the lazy path is preserved).
#[test]
fn boot_restore_spawns_no_servers() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    // The pinned root carries a mock-language file, so a naive eager restore
    // WOULD spawn a server for it. It must not.
    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;
    std::fs::write(
        target.path().join(format!("code.{MOCK_LANG}")),
        "fn restored_symbol()\n",
    )?;

    // Pin it directly in the config so the restore path (not a runtime pin) is
    // what tracks it at boot.
    let config = format!("[roots]\npinned = [\n  \"{target_str}\",\n]\n");
    write_user_config(state_home, &config)?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge.initialize()?;
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;

    // The restored pin is tracked (roots board carries it) but no server spawned.
    //
    // Observe the WHOLE boot window, sampling throughout — never assert once at
    // the instant the root appears tracked and break (bug 155). Boot restore is
    // a sequence, not an event: the tracker entry (what `wait_for_root` and the
    // roots board see) lands first, the root push that makes the root visible to
    // the spawn machinery lands after it, and an erroneous spawn after that. An
    // assert-and-break at tracked-appears therefore samples the one moment the
    // violation is guaranteed absent, and a spawn landing a few hundred
    // milliseconds later is never looked at — which is exactly how this stayed
    // green on Linux while going red on 2 of 5 macOS runs.
    let observe_until = Instant::now() + BOOT_OBSERVATION_WINDOW;
    let mut ever_tracked = false;
    while Instant::now() < observe_until {
        if let Some(snap) = read_snapshot(state_home) {
            let roots = snap["roots"].as_array().cloned().unwrap_or_default();
            ever_tracked |= roots.iter().any(|r| r["path"].as_str() == Some(target_str));
            let servers = snap["servers"].as_array().cloned().unwrap_or_default();
            // Name both tempdirs in the failure (bug 155): the daemon
            // canonicalizes registered roots, so on macOS a canonical
            // `scope_root` alone cannot say whether the erroneous spawn was the
            // pinned target (an eager-restore violation) or the session base (a
            // first-touch-on-boot question) — two different bugs.
            assert!(
                servers.is_empty(),
                "boot restore spawned a server (should be lazy): {servers:?}\n\
                 pinned target root: {target_str}\n\
                 session base root:  {base_str}"
            );
        }
        std::thread::sleep(common::POLL_SPACING);
    }
    assert!(
        ever_tracked,
        "restored pin never appeared on the roots board: {}",
        read_snapshot(state_home).unwrap_or_default()
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// Boot restore stays lazy even when the daemon's own boot root is warm
/// (bug 155) — the deterministic half of the laziness contract.
///
/// The zero-cost-restore claim rested on a premise: "on a fresh daemon no
/// language is active, so `spawn_for_added_roots` — the warm-language spawn leg
/// — fires for nothing". That premise holds only while the boot root carries no
/// code, which the empty-tempdir sibling above arranges and which no real daemon
/// ever does. Give the boot root a file of the mock language and it collapses,
/// in **either** interleaving of the two fire-and-forget boot tasks:
///
/// - restore's root push lands first ⇒ the boot pre-warm reads the roots live
///   and walks the restored pin directly;
/// - the boot pre-warm lands first ⇒ it spawns for the boot root, which makes
///   the mock language *active*, and the restore's own push then runs
///   `spawn_for_added_roots` over the newly added pin and spawns for it too.
///
/// Both orderings lose, which is what makes this deterministic on every platform
/// where the macOS-only sibling caught the same defect once in five runs. The
/// pinned root is one no session ever touched — the contract says it stays a
/// tracker entry and a roots-board line until an actual query or edit lands on
/// it.
///
/// Red before the fix: two servers, one scoped to the restored pin. Green after:
/// exactly one, scoped to the boot root — the boot pre-warm still does its job.
#[test]
fn boot_restore_stays_lazy_beside_a_warm_boot_root() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    // The daemon's own boot root, WARM: it carries a mock-language file, so the
    // boot pre-warm legitimately spawns a server for it. Canonical, so its
    // `scope_root` on the servers board is comparable.
    let base = common::canonical_tempdir()?;
    let base_str = base.path().to_str().context("base path")?;
    std::fs::write(
        base.path().join(format!("boot.{MOCK_LANG}")),
        "fn boot_symbol()\n",
    )?;

    // The pinned root carries a mock-language file too, so an eager restore WOULD
    // spawn a server for it. It must not — nothing ever touches this root.
    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;
    std::fs::write(
        target.path().join(format!("code.{MOCK_LANG}")),
        "fn restored_symbol()\n",
    )?;

    // Pin it directly in the config so the restore path (not a runtime pin) is
    // what tracks it at boot.
    let config = format!("[roots]\npinned = [\n  \"{target_str}\",\n]\n");
    write_user_config(state_home, &config)?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "--scan-roots");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge.initialize()?;
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;

    // Positive completion signal: the boot pre-warm has run and produced the
    // server it SHOULD produce, for the warm boot root. Waiting on this (rather
    // than on the clock) is what makes the observation window below meaningful —
    // the spawn machinery has demonstrably executed by the time it opens.
    let deadline = Instant::now() + common::POLL_BACKSTOP;
    while !servers_for_root(state_home, base_str) {
        if Instant::now() >= deadline {
            bail!(
                "boot pre-warm never spawned a server for the warm boot root {base_str}: {}",
                read_snapshot(state_home).unwrap_or_default()
            );
        }
        std::thread::sleep(common::POLL_SPACING);
    }

    // …and the restored pin has none, throughout the rest of the boot window.
    let observe_until = Instant::now() + BOOT_OBSERVATION_WINDOW;
    while Instant::now() < observe_until {
        let snap = read_snapshot(state_home).unwrap_or_default();
        let servers = snap["servers"].as_array().cloned().unwrap_or_default();
        assert!(
            !servers_for_root(state_home, target_str),
            "boot restore spawned a server for the restored pin (should be lazy \
             until first touch): {servers:?}\n\
             pinned target root: {target_str}\n\
             warm boot root:     {base_str}"
        );
        std::thread::sleep(common::POLL_SPACING);
    }

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// A `CATENARY_ROOTS`-seeded root survives a later pin (misc 192).
///
/// Before the fix the env seed was set on the primary session but never
/// registered as a `RootTracker` contributor, so pinning a *different* root
/// triggered a `sync_roots` rebuilt from tracker contributors only — silently
/// evicting the seed from the served union (no retire log, no board change the
/// user asked for). Registering the seed as the `seed:env` contributor at boot
/// makes every re-sync rebuild the same union: the seed stays tracked across the
/// pin AND is attributed to `seed:env` on the roots board.
///
/// Red before the fix: the seed root is absent from `roots-ls` entirely (never a
/// contributor). Green after: it is present with `sources = ["seed:env"]` both
/// before and after the pin.
#[test]
fn env_seed_survives_a_pin() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    // The env seed — canonical, so it matches the daemon's stored form.
    let seed = common::canonical_tempdir()?;
    let seed_str = seed.path().to_str().context("seed path")?;

    // A distinct root to pin, which triggers the contributor-union re-sync.
    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", seed_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // The seed is a first-class tracked root from boot — attributed to seed:env.
    bridge.wait_for_root(seed_str, Duration::from_secs(5))?;
    assert_eq!(
        roots_ls_sources(&socket, seed_str)?,
        Some(vec!["seed:env".to_string()]),
        "env seed is tracked as the seed:env contributor at boot"
    );

    // Pin the distinct target — this is the re-sync that used to evict the seed.
    assert_eq!(pin(&socket, target_str)?, "ok", "pin succeeds");
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;

    // The seed is STILL tracked after the pin (the eviction is fixed), still
    // attributed to seed:env — not dropped, not silently reclassified.
    assert_eq!(
        roots_ls_sources(&socket, seed_str)?,
        Some(vec!["seed:env".to_string()]),
        "env seed survives the pin"
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// A `CATENARY_ROOTS`-seeded root survives an unpin too (misc 192).
///
/// The unpin path (`tool/roots-rm`) runs the same contributor-union re-sync as a
/// pin, so it is the mirror eviction risk. Pinning then unpinning a distinct
/// root must leave the seed tracked throughout.
#[test]
fn env_seed_survives_an_unpin() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let seed = common::canonical_tempdir()?;
    let seed_str = seed.path().to_str().context("seed path")?;

    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", seed_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;
    bridge.wait_for_root(seed_str, Duration::from_secs(5))?;

    // Pin then unpin the distinct target; the unpin re-sync must not evict the seed.
    assert_eq!(pin(&socket, target_str)?, "ok");
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;
    assert_eq!(unpin(&socket, target_str)?, "ok", "unpin succeeds");

    assert_eq!(
        roots_ls_sources(&socket, seed_str)?,
        Some(vec!["seed:env".to_string()]),
        "env seed survives the unpin"
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// A daemon booted with **no** `CATENARY_ROOTS` registers no boot roots at all
/// (bug 145).
///
/// The dropped fallback seeded `PathBuf::from(".")` — the daemon's cwd — as the
/// `seed:env` contributor whenever the variable was unset. The daemon is no
/// longer launched from the project it serves, so that cwd is whatever its
/// launcher held: `$HOME` under `systemd --user`, `/` under launchd. The first
/// live always-on boot rooted the operator's entire home directory, which made
/// every project a subdirectory of an existing root so no real root ever
/// mounted; the seed is deliberately un-removable (misc 192), so nothing short
/// of a restart corrected it.
///
/// The assertion is ordered, not raced: the daemon binds its sockets before boot
/// setup but only *serves* IPC once its accept loop runs, which is after
/// `with_session` has done its env-seed registration. A `roots-ls` that answers
/// at all therefore answers after any boot seed would already be on the board.
///
/// Red before the fix: the board carries the harness's cwd (this crate's root)
/// as `seed:env`. Green after: the board is empty.
#[test]
fn boot_without_catenary_roots_registers_no_roots() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    // `isolate_env` clears every inherited `CATENARY_*` var and this spawn adds
    // no `CATENARY_ROOTS` back — the unset case verbatim. The mock server
    // binding keeps the daemon off the shipped registry; it matches nothing.
    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    assert!(
        roots_ls_paths(&socket)?.is_empty(),
        "a daemon booted without CATENARY_ROOTS tracks no roots, got: {:?}",
        roots_ls_paths(&socket)?,
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// An **empty** `CATENARY_ROOTS` is the same rootless boot as an unset one
/// (bug 145).
///
/// The env read has always treated `""` as absent; the fallback then supplied
/// cwd for both. With the fallback gone the two must still agree — an operator
/// who clears the variable in a unit file gets a rootless daemon, not a
/// home-rooted one.
#[test]
fn boot_with_empty_catenary_roots_registers_no_roots() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", "");
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    assert!(
        roots_ls_paths(&socket)?.is_empty(),
        "an empty CATENARY_ROOTS tracks no roots, got: {:?}",
        roots_ls_paths(&socket)?,
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// An explicit `CATENARY_ROOTS` still seeds exactly its roots at boot, and
/// nothing else (bug 145).
///
/// The other half of the contract: dropping the cwd fallback changed the *unset*
/// case only. A set variable seeds the named roots as the first-class `seed:env`
/// contributor exactly as before — the survivability machinery (misc 192) is
/// untouched, and the board carries those roots and no phantom extra.
#[test]
fn explicit_catenary_roots_still_seeds_the_boot_roots() -> Result<()> {
    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    let _daemon_guard = common::DaemonGuard::new(state_home);

    // Canonical, so it matches the daemon's stored (canonicalized) spelling.
    let seed = common::canonical_tempdir()?;
    let seed_str = seed.path().to_str().context("seed path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", seed_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;
    bridge.wait_for_root(seed_str, Duration::from_secs(5))?;

    assert_eq!(
        roots_ls_paths(&socket)?,
        vec![seed_str.to_string()],
        "the explicit seed is the whole board — no cwd root rides along"
    );
    assert_eq!(
        roots_ls_sources(&socket, seed_str)?,
        Some(vec!["seed:env".to_string()]),
        "the explicit seed is still attributed to seed:env"
    );

    drop(bridge);
    stop_daemon(state_home)?;
    Ok(())
}

/// The REAL user config the harness itself resolves — the one bug 109 poisoned.
///
/// Resolved through the library's own [`catenary_cli::paths::config_dir`] so it
/// honors the harness process's `CATENARY_CONFIG_DIR` / `XDG_CONFIG_HOME` /
/// `$HOME` exactly as the daemon would with the *unisolated* environment. This
/// is the file a leaking test would write; the guard only ever READS it.
fn real_user_config_path() -> PathBuf {
    catenary_cli::paths::config_dir()
        .join("catenary")
        .join("config.toml")
}

/// Snapshot the real user config's bytes (or `None` when it does not exist), for
/// a before/after equality assertion. Never creates or writes the file.
fn snapshot_real_user_config() -> Option<Vec<u8>> {
    std::fs::read(real_user_config_path()).ok()
}

/// Poison guard (bug 109): a subprocess pin driven under `isolate_env` must land
/// **only** in the isolated tempdir config and leave the operator's REAL
/// `~/.config/catenary/config.toml` byte-identical.
///
/// This is the tripwire the sighting demanded: for eight months a
/// `tool/roots-add` reaching `persist_pin` wrote the user's real config because
/// the write target was env-resolved, not injected. Here the write path runs
/// end-to-end through a real daemon under `isolate_env`; we snapshot the real
/// file before and after and assert it never changed. It fails loudly if any
/// isolation leg regresses (the subprocess's `CATENARY_CONFIG_DIR`, or a future
/// escape through `$HOME`), and it can never itself write the real file — it only
/// reads it. The in-process router-test escape is sealed structurally by the
/// injected config path; this covers the subprocess seam the same guarantee.
#[test]
fn subprocess_pin_never_touches_the_real_user_config() -> Result<()> {
    // Snapshot the real config BEFORE. Absent is a valid snapshot (`None`); the
    // guard asserts absent-stays-absent just as it asserts bytes-stay-equal, so a
    // leak that *creates* the file is caught too.
    let before = snapshot_real_user_config();

    let state_dir = tempfile::tempdir()?;
    let state_home = state_dir.path().to_str().context("state dir")?;
    // Panic-safe daemon teardown (bug 131): shared-state spawns leave daemon
    // lifecycle to the test, so a failed assertion before `stop_daemon` would
    // leak the daemon without this guard.
    let _daemon_guard = common::DaemonGuard::new(state_home);

    let base = tempfile::tempdir()?;
    let base_str = base.path().to_str().context("base path")?;

    let target = common::canonical_tempdir()?;
    let target_str = target.path().to_str().context("target path")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_SERVERS", &lsp);
        cmd.env("CATENARY_ROOTS", base_str);
    })?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    // Drive the exact IPC the poison came from.
    assert_eq!(pin(&socket, target_str)?, "ok", "pin succeeds");
    bridge.wait_for_root(target_str, Duration::from_secs(5))?;

    // The pin DID land — in the ISOLATED config, proving the write path ran.
    assert!(
        config_pins_target(state_home, target.path()),
        "the pin must persist to the ISOLATED config:\n{}",
        read_user_config(state_home)
    );

    // …and unpin round-trips through the same seam.
    assert_eq!(unpin(&socket, target_str)?, "ok", "unpin succeeds");

    drop(bridge);
    stop_daemon(state_home)?;

    // The REAL user config is byte-identical (or still absent). A poison would
    // have appended a `~/.claude/tmp/.tmp…/…` entry or replaced the file wholesale.
    let after = snapshot_real_user_config();
    assert_eq!(
        before,
        after,
        "bug 109 poison guard: the REAL user config at {} changed across an \
         isolated pin/unpin — a test escaped isolation and wrote the operator's file",
        real_user_config_path().display(),
    );
    Ok(())
}
