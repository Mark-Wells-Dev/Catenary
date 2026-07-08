// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::print_stderr,
    reason = "a skipped conformance run announces itself on stderr so a skip is \
              never silently indistinguishable from a pass"
)]
//! Language-server conformance harness (tui-rework 07).
//!
//! For one named server, this drives the **real** Catenary lifecycle against a
//! tiny fixture project through the **shipped default config** — no
//! `CATENARY_SERVERS` mock, no `CATENARY_CONFIG` override — so the exact server
//! command, language binding, and `workspace/configuration` delivery a user
//! gets are what run here. It:
//!
//! 1. copies the fixture project into an isolated tempdir root,
//! 2. spawns the daemon with the shipped defaults ([`BridgeProcess::spawn_conformance`]),
//! 3. marks the fixture's representative file edited and runs a diagnostics
//!    batch **with settle**, bounded by a generous-but-finite wall clock,
//! 4. collects the receipt and shuts the daemon down.
//!
//! It then asserts three things (tui-rework 06 §"Blessing gate"):
//!
//! - **settle reaches idle inside a sane wall bound** — the
//!   [`CONFORMANCE_WALL_BOUND`] cap. A server that never yields (the pyright
//!   44-minute class, waitv2 findings 5/6; or a JVM server whose GC/JIT threads
//!   tick forever) blows the cap and fails loudly instead of hanging a user;
//! - **the intentional diagnostic actually publishes** — the fixture's file is
//!   diagnosed, not `[clean]` / `[unverified]` / `[no LSP coverage]`;
//! - **shutdown is clean** — the bridge exits on stdin-close without a kill.
//!
//! ## Gating
//!
//! - **Local / ordinary `make test` runs** exercise the maintainer's dogfooded
//!   fleet (rust-analyzer, lattice, taplo, yaml-ls, vscode-json, bash-ls) as the
//!   sentinel: one `#[test]` per server, each **skip-if-binary-missing** so a
//!   host lacking a server stays green. (Ordinary CI's `ci.yml` runs only
//!   `cargo test --lib --bins`, so these integration tests do not run there — the
//!   maintainer host and the conformance matrix are where they exercise.)
//! - **The conformance CI matrix** installs one pinned server per job and selects
//!   it with `CATENARY_CONFORMANCE=<server>`: [`conformance_selected_server`]
//!   then runs exactly that server and **requires** its binary (a missing binary
//!   is a failed install, not a skip).
//!
//! `isolate_env` discipline throughout: every daemon subprocess gets its own XDG
//! bases under a per-test tempdir; only `PATH` is restored so the real pinned
//! binary resolves.

mod common;

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use common::BridgeProcess;
use serde_json::json;

/// The generous-but-finite wall bound the settle must reach idle within.
///
/// It exists to catch the 44-minute / never-yield class, not to police a
/// working server: a tiny fixture cold-starts, indexes, and settles well inside
/// this even under parallel-test CPU contention. A server that never goes quiet
/// burns it down and fails the blessing.
const CONFORMANCE_WALL_BOUND: Duration = Duration::from_mins(3);

/// The generously-raised wall bound for cold-start-heavy servers (jdtls: JVM
/// spin-up + Eclipse workspace init).
///
/// It is raised, not tightened — still finite (catches the never-idle class) but
/// far above any healthy cold start even under parallel-test CPU contention. Per
/// the contention doctrine (tui-rework 07 landing gate): a generous bound is the
/// sanctioned lever; a sleep, retry loop, or tight bound is not.
const CONFORMANCE_WALL_BOUND_SLOW: Duration = Duration::from_mins(10);

/// Grace for a clean bridge shutdown after stdin close.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Env var the conformance CI matrix sets to select the one server a job runs.
const CONFORMANCE_ENV: &str = "CATENARY_CONFORMANCE";

/// A shipped-default routing override for a server that is defined in the shipped
/// config but is NOT its language's default binding.
///
/// The motivating case is `marksman`: `lattice` is the shipped markdown default
/// (decision 015), so a bare shipped-defaults spawn never routes to marksman.
/// When a [`Case`] carries a binding, the harness writes a `.catenary.toml` into
/// the isolated fixture root binding `language → server` — the exact one-line
/// opt-in a user of that server writes (docs/src/lsp/markdown.md) — so the harness
/// can reach it. The shipped server *definition* is unchanged (its command still
/// comes from `defaults/servers.toml`); only routing is overridden, and only in
/// the throwaway tempdir, so the shipped default every real root gets is untouched.
#[derive(Clone, Copy)]
struct CaseBinding {
    /// The language whose `servers` list is overridden (e.g. `markdown`).
    language: &'static str,
    /// The server to route it to (e.g. `marksman`).
    server: &'static str,
}

/// One conformance case: a shipped server, the fixture it runs against, and the
/// binary whose presence gates a local run.
struct Case {
    /// Canonical server name (`[lsp.server.*]` in `defaults/servers.toml`).
    server: &'static str,
    /// Fixture project directory under `tests/fixtures/conformance/`.
    fixture: &'static str,
    /// The representative file within the fixture, relative to its dir.
    file: &'static str,
    /// The binary the shipped command launches — the first token of the
    /// `[lsp.server.<server>].command`. Its presence on `PATH` gates a local run.
    probe: &'static str,
    /// A shipped-default routing override for a non-default server (see
    /// [`CaseBinding`]); `None` routes purely through the shipped defaults.
    binding: Option<CaseBinding>,
    /// `true` for a cold-start-heavy server (jdtls): use
    /// [`CONFORMANCE_WALL_BOUND_SLOW`] instead of [`CONFORMANCE_WALL_BOUND`].
    slow_start: bool,
}

/// Every conformance case. The dogfooded fleet (marked) has a sentinel `#[test]`
/// below; the rest are exercised by the CI matrix via `CATENARY_CONFORMANCE`.
///
/// `vscode-eslint`, `ansible-ls`, and `vim-ls` are intentionally absent: they
/// carry recipes but no `[lsp.language.*]` binding in `defaults/languages.toml`,
/// so no file-open can route to them — the harness cannot drive a server it
/// cannot reach. `vscode-html` is absent too: the HTML server publishes no
/// diagnostics for a plain document, so it has no intentional-diagnostic to
/// assert on. (All recorded in the tui-rework 07 report as real gaps.)
///
/// The tui-rework 10 tranche (rust-analyzer already present; clangd, jdtls,
/// ruby-lsp, marksman added) covers "the servers everyone actually wants" plus
/// the lattice dogfood, obtained in CI via `defaults/ci-provision.toml` rather
/// than an install recipe. marksman carries a [`CaseBinding`] because `lattice`
/// is the shipped markdown default (decision 015); jdtls is `slow_start`.
const CASES: &[Case] = &[
    // ── dogfooded fleet (local sentinels) ──────────────────────────────
    Case {
        server: "rust-analyzer",
        fixture: "rust",
        file: "src/main.rs",
        probe: "rustup",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "taplo",
        fixture: "toml",
        file: "broken.toml",
        probe: "taplo",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "lattice",
        fixture: "markdown",
        file: "broken.md",
        probe: "lattice",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "yaml-ls",
        fixture: "yaml",
        file: "broken.yaml",
        probe: "yaml-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "vscode-json",
        fixture: "json",
        file: "broken.json",
        probe: "vscode-json-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "bash-ls",
        fixture: "shellscript",
        file: "broken.sh",
        probe: "bash-language-server",
        binding: None,
        slow_start: false,
    },
    // ── npm / cargo / pip / go tranche (CI matrix) ─────────────────────
    Case {
        server: "pyright",
        fixture: "python",
        file: "broken.py",
        probe: "pyright-langserver",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "typescript-ls",
        fixture: "typescript",
        file: "broken.ts",
        probe: "typescript-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "vscode-css",
        fixture: "css",
        file: "broken.css",
        probe: "vscode-css-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "intelephense",
        fixture: "php",
        file: "broken.php",
        probe: "intelephense",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "svelte-ls",
        fixture: "svelte",
        file: "src/Broken.svelte",
        probe: "svelteserver",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "elm-ls",
        fixture: "elm",
        file: "src/Main.elm",
        probe: "elm-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "docker-ls",
        fixture: "dockerfile",
        file: "Dockerfile",
        probe: "docker-langserver",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "sql-ls",
        fixture: "sql",
        file: "broken.sql",
        probe: "sql-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "cmake-ls",
        fixture: "cmake",
        file: "CMakeLists.txt",
        probe: "cmake-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "gopls",
        fixture: "go",
        file: "main.go",
        probe: "gopls",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "lua-ls",
        fixture: "lua",
        file: "broken.lua",
        probe: "lua-language-server",
        binding: None,
        slow_start: false,
    },
    // ── tui-rework 10 coverage (CI matrix via ci-provision.toml) ───────
    Case {
        server: "clangd",
        fixture: "c",
        file: "broken.c",
        probe: "clangd",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "marksman",
        fixture: "markdown",
        file: "broken.md",
        probe: "marksman",
        // lattice is the shipped markdown default (decision 015); route the
        // fixture to marksman via the isolated-root opt-in without touching it.
        binding: Some(CaseBinding {
            language: "markdown",
            server: "marksman",
        }),
        slow_start: false,
    },
    Case {
        server: "jdtls",
        fixture: "java",
        file: "Broken.java",
        probe: "jdtls",
        binding: None,
        slow_start: true,
    },
    Case {
        server: "ruby-lsp",
        fixture: "ruby",
        file: "broken.rb",
        probe: "ruby-lsp",
        binding: None,
        slow_start: false,
    },
];

/// Finds `case` by server name.
fn lookup(server: &str) -> Option<&'static Case> {
    CASES.iter().find(|c| c.server == server)
}

/// Whether `binary` resolves on the inherited `PATH`.
fn on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(binary);
        candidate.is_file() || candidate.is_symlink()
    })
}

/// The checked-in fixture directory for `case`.
fn fixture_dir(case: &Case) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/conformance")
        .join(case.fixture)
}

/// Copies `src` into `dst` recursively (fixture → isolated tempdir root), so a
/// language server never writes its caches/target into the checked-in fixture.
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("mkdir {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read_dir {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).with_context(|| format!("copy {}", from.display()))?;
        }
    }
    Ok(())
}

/// Drives the editing lifecycle for `file` and consumes the diagnostics batch
/// **with settle**, bounded by a hard `wall` clock.
///
/// The editing preamble (start → accumulate → prepare-handoff) uses the ordinary
/// short-timeout IPC; the consuming `tool/editing-stop` — the step that settles
/// and pulls — is read to EOF under the wall bound. The write half is left open
/// (bug 24: a write-shutdown reads as client-disconnect on the daemon side and
/// races the response). Exceeding the wall bound is the never-idle failure.
fn run_settle_diagnostics(bridge: &BridgeProcess, file: &Path, wall: Duration) -> Result<String> {
    let socket = bridge.wait_for_ipc_socket()?;
    let file_str = file.to_str().context("fixture path is not UTF-8")?;

    common::ipc_request(
        &socket,
        &json!({ "method": "pre-tool/editing-start", "agent_id": "" }),
    )?;
    common::ipc_request(
        &socket,
        &json!({
            "method": "pre-tool/editing-state",
            "tool_name": "Edit",
            "file_path": file_str,
            "agent_id": "",
        }),
    )?;
    common::ipc_request(
        &socket,
        &json!({ "method": "pre-tool/editing-stop", "agent_id": "" }),
    )?;

    let raw = read_editing_stop_wall_bounded(&socket, wall)?;
    Ok(common::diagnostics_output(&raw))
}

/// Sends `tool/editing-stop` and reads the settle-and-pull response to EOF,
/// failing hard if the daemon has not closed the response within `wall`.
///
/// The 500 ms per-read timeout is only poll cadence: it lets the loop re-check
/// the wall clock every window while a blocked read is in flight. Unlike the
/// progress-aware IPC helpers this carries **no** no-progress escape — the whole
/// point of conformance is a finite ceiling on total settle time, so a server
/// that keeps its tree hot forever (never-idle) is caught, not excused.
fn read_editing_stop_wall_bounded(socket: &Path, wall: Duration) -> Result<String> {
    let mut stream = UnixStream::connect(socket).context("connect diagnostics socket")?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .context("set poll-cadence read timeout")?;
    writeln!(stream, "{}", json!({ "method": "tool/editing-stop" }))
        .context("write tool/editing-stop")?;
    // Do NOT shut down the write half (bug 24).

    let deadline = Instant::now() + wall;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e).context("read diagnostics socket"),
        }
        if Instant::now() >= deadline {
            bail!(
                "settle did not reach idle within the {wall:?} wall bound — the server \
                 never yielded (the 44-minute / never-idle class the harness exists to catch)"
            );
        }
    }
    String::from_utf8(buf).context("diagnostics response was not UTF-8")
}

/// Runs the full conformance lifecycle for `case`.
///
/// `require` distinguishes the two gating modes: `false` (local sentinel) skips
/// cleanly when the probe binary is absent; `true` (CI-matrix selection) treats
/// an absent binary as a failed install and errors.
fn run_conformance(case: &Case, require: bool) -> Result<()> {
    if !on_path(case.probe) {
        if require {
            bail!(
                "conformance for `{}` was selected via {CONFORMANCE_ENV} but its binary \
                 `{}` is not on PATH — the pinned install did not land",
                case.server,
                case.probe
            );
        }
        eprintln!(
            "conformance: skipping `{}` — binary `{}` not on PATH",
            case.server, case.probe
        );
        return Ok(());
    }

    let root = tempfile::tempdir().context("create fixture root")?;
    let src = fixture_dir(case);
    if !src.is_dir() {
        bail!("fixture `{}` missing at {}", case.fixture, src.display());
    }
    copy_dir(&src, root.path())?;

    // A non-default server (e.g. marksman, where lattice is the shipped markdown
    // default) needs a one-line routing opt-in — written into the isolated root
    // only, so the shipped default every real root gets is untouched. This is the
    // exact `.catenary.toml` a user of that server writes (docs/src/lsp/markdown.md).
    if let Some(binding) = case.binding {
        let config = format!(
            "# Conformance routing override (tui-rework 10): `{server}` is defined in\n\
             # the shipped config but is not `{language}`'s default binding. This is the\n\
             # one-line opt-in a user writes; it lives only in this throwaway fixture\n\
             # root, so the shipped default is untouched.\n\
             [lsp.language.{language}]\nservers = [\"{server}\"]\n",
            server = binding.server,
            language = binding.language,
        );
        std::fs::write(root.path().join(".catenary.toml"), config)
            .context("write conformance routing override")?;
    }

    let file = root.path().join(case.file);

    let file_name = Path::new(case.file)
        .file_name()
        .and_then(|n| n.to_str())
        .context("fixture file name")?;

    let mut bridge = BridgeProcess::spawn_conformance(root.path())?;
    bridge.initialize()?;

    // ONE cold settle-and-pull, wall-bounded — no retries, by landing-gate
    // ruling (tui-rework 07, 2026-07-07): a retry that absorbs a
    // cold-publish/contention miss launders the exact signal conformance
    // exists to produce, and (per the same investigation) a warm re-pull in
    // the SAME session cannot re-fire a fast push-only server's first
    // publish anyway. A server that intermittently loses its first publish
    // to the cold race fails here honestly — that race is a real Catenary
    // settle/pull gap (bug 74; the waitv2 finding-7 pull-diagnostics class),
    // and its known-deterministic casualties (`lattice`, `vscode-json`) are
    // `#[ignore]`d sentinels pointing at the bug, not absorbed.
    let wall = if case.slow_start {
        CONFORMANCE_WALL_BOUND_SLOW
    } else {
        CONFORMANCE_WALL_BOUND
    };
    let receipt = run_settle_diagnostics(&bridge, &file, wall)
        .with_context(|| format!("conformance settle for `{}`", case.server))?;

    // The intentional diagnostic MUST have published: the fixture file is
    // diagnosed, not clean / unverified / uncovered.
    assert!(
        !receipt.contains("[clean]"),
        "`{}` reported the fixture clean — its intentional diagnostic did not publish:\n{receipt}",
        case.server
    );
    assert!(
        !receipt.contains("[unverified"),
        "`{}` returned no result — the intentional diagnostic did not publish:\n{receipt}",
        case.server
    );
    assert!(
        !receipt.contains("[no LSP coverage]"),
        "no server covered the `{}` fixture — check the language binding:\n{receipt}",
        case.server
    );
    assert!(
        receipt.contains(file_name),
        "`{}` receipt does not diagnose the fixture file `{file_name}`:\n{receipt}",
        case.server
    );

    // Shutdown must be clean: the bridge exits on stdin-close without a kill.
    bridge
        .shutdown_clean(SHUTDOWN_GRACE)
        .with_context(|| format!("clean shutdown for `{}`", case.server))?;

    eprintln!("conformance: `{}` PASSED", case.server);
    Ok(())
}

/// Runs a sentinel case by server name — skips if its binary is absent.
fn sentinel(server: &str) -> Result<()> {
    let case = lookup(server).with_context(|| format!("no conformance case for `{server}`"))?;
    run_conformance(case, false)
}

// ── Dogfooded-fleet sentinels (skip-if-binary-missing) ────────────────
//
// These run in ordinary `make test` / `make check` and skip cleanly where a
// server binary is absent. rust-analyzer (native + flycheck), taplo, yaml-ls,
// and bash-ls (shellcheck linter feeder) deliver diagnostics that survive a cold
// diagnose and so are reliable make-check sentinels.

#[test]
fn conformance_rust_analyzer() -> Result<()> {
    sentinel("rust-analyzer")
}

#[test]
fn conformance_taplo() -> Result<()> {
    sentinel("taplo")
}

#[test]
fn conformance_yaml_ls() -> Result<()> {
    sentinel("yaml-ls")
}

#[test]
fn conformance_bash_ls() -> Result<()> {
    sentinel("bash-ls")
}

// ── Formerly-ignored cold-diagnose sentinels (bug 74, now fixed) ──────
//
// lattice and vscode-json each returned a deterministic `[clean]` on a cold
// daemon. The tui-rework 07 report filed both under one "cold-pull" heading, but
// they were two distinct gaps the conformance harness caught: lattice publishes
// once on its workspace scan (which `clear_stale_diagnostics` wipes before the
// batch reopens the unchanged file) and never advertises `diagnosticProvider`,
// so the settle-then-collect pipeline never pulled it — even though it answers
// `textDocument/diagnostic` on demand (fixed: best-effort pull on an empty push
// cache); vscode-json is pull-only and *was* pulled, but Catenary's empty
// `workspace/didChangeConfiguration {}` at init disabled its JSON validation, so
// the pull returned an empty report (fixed: an empty settings push is no longer
// sent, letting the server keep its defaults). Both now diagnose their fixture
// on a cold daemon, so they are ordinary skip-if-missing sentinels again.

#[test]
fn conformance_lattice() -> Result<()> {
    sentinel("lattice")
}

#[test]
fn conformance_vscode_json() -> Result<()> {
    sentinel("vscode-json")
}

// ── tui-rework 10 coverage sentinel (skip-if-binary-missing) ──────────
//
// clangd is a common, apt-provisioned server that publishes a hard parse error
// (undeclared identifier) on didOpen, so it is a reliable cold-diagnose sentinel
// where present. marksman, jdtls, and ruby-lsp are intentionally NOT sentinels:
// they are exercised by the CI matrix via `CATENARY_CONFORMANCE` (marksman needs
// its routing opt-in and jdtls a JVM cold start — neither belongs in the local
// dogfood fleet), keeping `make check` green on hosts without them.

#[test]
fn conformance_clangd() -> Result<()> {
    sentinel("clangd")
}

// ── CI-matrix entry point (exact-server selection) ────────────────────

/// Runs exactly the server named by `CATENARY_CONFORMANCE`, requiring its
/// binary. Inert (a no-op pass) when the var is unset, so it never runs on the
/// maintainer host outside a matrix job.
#[test]
fn conformance_selected_server() -> Result<()> {
    let Some(server) = std::env::var_os(CONFORMANCE_ENV) else {
        return Ok(());
    };
    let server = server
        .to_str()
        .context("CATENARY_CONFORMANCE is not UTF-8")?;
    let case = lookup(server).with_context(|| {
        format!("{CONFORMANCE_ENV}=`{server}` names no conformance case (see CASES)")
    })?;
    run_conformance(case, true)
}

/// Every case's fixture directory and representative file exist on disk — a
/// cheap guard so a mistyped `CASES` entry fails fast rather than at spawn time.
#[test]
fn every_case_fixture_exists() {
    for case in CASES {
        let dir = fixture_dir(case);
        assert!(
            dir.is_dir(),
            "`{}` fixture dir missing: {}",
            case.server,
            dir.display()
        );
        let file = dir.join(case.file);
        assert!(
            file.is_file(),
            "`{}` fixture file missing: {}",
            case.server,
            file.display()
        );
    }
}
