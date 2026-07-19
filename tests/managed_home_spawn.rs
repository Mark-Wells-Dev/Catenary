// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Spawn resolution prefers the managed server home (ls-manager 02).
//!
//! Each test stages TWO candidate executables for the same blessed server key
//! (`mockls-event`, whose mockls-persona manifest row pins the version
//! `mockls-persona`): one in the isolated managed home
//! (`<data_dir>/catenary/servers/<name>/<version>/bin/<name>`) and one on a
//! private `PATH` directory. Both are `#!/bin/sh` wrappers that touch a
//! distinct marker file and then `exec` the real mockls binary — so the marker
//! is an argv pin on the spawn: whichever wrapper's marker appears IS the
//! binary the daemon executed, and the server still comes up healthy either
//! way.
//!
//! The trust boundary rides along: `[servers] prefer_managed` is honored from
//! the user config layer (`CATENARY_CONFIG`) and ignored in a project
//! `.catenary.toml` — a public repo must never steer which binary a server
//! spawn executes on a private machine.

mod common;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::json;

use common::{BridgeProcess, POLL_BACKSTOP, POLL_SPACING};

/// The server under test: a blessed mockls persona, so the daemon's seed
/// manifest (built with `--features mockls`) carries a pinned row for it.
const SERVER: &str = "mockls-event";

/// The persona row's pinned version (`defaults/mockls-personas.toml`) — the
/// exact managed-home version dir spawn resolution must consult.
const PINNED_VERSION: &str = "mockls-persona";

/// One test's staged world: an isolated state home whose managed home and
/// `PATH` directory each carry a marker-writing wrapper around real mockls.
struct SpawnFixture {
    /// Panic-safe daemon teardown (bug 131): `spawn_in_state` leaves daemon
    /// lifecycle to the test. Declared before `state` so it drops first —
    /// the guard reads the snapshot before the tempdir is wiped.
    _daemon_guard: common::DaemonGuard,
    /// Owns every path below; the externally-owned state home for
    /// [`BridgeProcess::spawn_in_state`].
    state: tempfile::TempDir,
    /// The workspace root (contains one `test.mockls-event` file).
    root: PathBuf,
    /// The user config file (`CATENARY_CONFIG`).
    config: PathBuf,
    /// The private `PATH` directory carrying the PATH-leg wrapper.
    path_dir: PathBuf,
    /// Touched iff the managed-home wrapper spawned.
    managed_marker: PathBuf,
    /// Touched iff the PATH wrapper spawned.
    path_marker: PathBuf,
}

/// Writes an executable `#!/bin/sh` wrapper at `path` that records its spawn
/// by touching `marker`, then `exec`s the real mockls with the daemon's args.
fn write_exec_wrapper(path: &Path, marker: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mockls = env!("CARGO_BIN_EXE_mockls");
    let script = format!(
        "#!/bin/sh\n: > '{}'\nexec '{}' \"$@\"\n",
        marker.display(),
        mockls,
    );
    std::fs::create_dir_all(path.parent().context("wrapper parent")?)?;
    std::fs::write(path, script)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Builds the staged world. `user_config_extra` is prepended to the user
/// config (e.g. a `[servers]` section); `stage_managed` controls whether the
/// managed home carries an install for the pinned version.
fn fixture(user_config_extra: &str, stage_managed: bool) -> Result<SpawnFixture> {
    let state = tempfile::tempdir()?;
    let root = state.path().join("root");
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join(format!("test.{SERVER}")), "echo hello\n")?;

    let managed_marker = state.path().join("managed-spawned");
    let path_marker = state.path().join("path-spawned");

    if stage_managed {
        // The ruled invariant: executables at `<name>/<version>/bin/`, under
        // the same data dir `isolate_env` points the subprocess at.
        let managed_bin = common::xdg_data_home(state.path())
            .join("catenary")
            .join("servers")
            .join(SERVER)
            .join(PINNED_VERSION)
            .join("bin")
            .join(SERVER);
        write_exec_wrapper(&managed_bin, &managed_marker)?;
    }

    let path_dir = state.path().join("pathbin");
    write_exec_wrapper(&path_dir.join(SERVER), &path_marker)?;

    // No `path` override: the server key IS the executable (misc 162), so
    // resolution is free to prefer the managed home.
    let config = state.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "{user_config_extra}[lsp.server.{SERVER}]\n\
             args = [\"{SERVER}\", \"--log-pid-suffix\"]\n\n\
             [lsp.language.{SERVER}]\n\
             servers = [\"{SERVER}\"]\n"
        ),
    )?;

    Ok(SpawnFixture {
        _daemon_guard: common::DaemonGuard::new(state.path()),
        state,
        root,
        config,
        path_dir,
        managed_marker,
        path_marker,
    })
}

/// Spawns the bridge against the fixture's state home, then forces the LSP
/// spawn with one grep over the covered root.
fn spawn_and_touch(fx: &SpawnFixture) -> Result<BridgeProcess> {
    let state_home = fx.state.path().to_str().context("state home utf8")?;
    let config = fx.config.clone();
    let root = fx.root.clone();
    let path_dir = fx.path_dir.clone();
    let mut bridge = BridgeProcess::spawn_in_state(state_home, |cmd| {
        cmd.env("CATENARY_CONFIG", &config);
        cmd.env("CATENARY_ROOTS", &root);
        // `isolate_env` cleared PATH; the private dir is the only PATH leg.
        cmd.env("PATH", &path_dir);
    })?;
    bridge.initialize()?;

    let output = bridge.call_tool_text(
        "grep",
        &json!({
            "pattern": "hello",
            "directory": fx.root.to_string_lossy(),
        }),
    )?;
    assert!(
        output.contains("hello"),
        "grep over the covered root finds the fixture line: {output}"
    );
    Ok(bridge)
}

/// Waits for `marker` to appear (signal-polled; the backstop only trips on a
/// genuine hang).
fn wait_for_marker(marker: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + POLL_BACKSTOP;
    while !marker.exists() {
        if std::time::Instant::now() >= deadline {
            bail!("spawn marker {} never appeared", marker.display());
        }
        std::thread::sleep(POLL_SPACING);
    }
    Ok(())
}

/// Pinned row + managed install present → the managed binary spawns; the PATH
/// candidate (also present) is never executed.
#[test]
fn pinned_managed_install_wins_the_spawn() -> Result<()> {
    let fx = fixture("", true)?;
    let _bridge = spawn_and_touch(&fx)?;

    wait_for_marker(&fx.managed_marker)?;
    assert!(
        !fx.path_marker.exists(),
        "the PATH wrapper must not spawn when the managed install wins"
    );
    Ok(())
}

/// `[servers] prefer_managed = false` in the USER config → PATH resolution;
/// the managed install (present) is ignored.
#[test]
fn prefer_managed_false_resolves_on_path() -> Result<()> {
    let fx = fixture("[servers]\nprefer_managed = false\n\n", true)?;
    let _bridge = spawn_and_touch(&fx)?;

    wait_for_marker(&fx.path_marker)?;
    assert!(
        !fx.managed_marker.exists(),
        "the managed home must be ignored when the user opts out"
    );
    Ok(())
}

/// Managed home empty for the row → PATH fallback, no error, no noise: the
/// grep still answers and the PATH binary serves.
#[test]
fn empty_managed_home_falls_back_to_path() -> Result<()> {
    let fx = fixture("", false)?;
    let _bridge = spawn_and_touch(&fx)?;

    wait_for_marker(&fx.path_marker)?;
    assert!(!fx.managed_marker.exists());
    Ok(())
}

/// `prefer_managed` in a project `.catenary.toml` is ignored (the trust
/// boundary, mirroring `[roots]`/`[permissions]`): with the user config
/// silent, the default (prefer the managed home) holds and the managed
/// install spawns despite the project's opt-out.
#[test]
fn project_config_cannot_flip_prefer_managed() -> Result<()> {
    let fx = fixture("", true)?;
    std::fs::write(
        fx.root.join(".catenary.toml"),
        "[servers]\nprefer_managed = false\n",
    )?;
    let _bridge = spawn_and_touch(&fx)?;

    wait_for_marker(&fx.managed_marker)?;
    assert!(
        !fx.path_marker.exists(),
        "a project config must never steer spawn resolution"
    );
    Ok(())
}
