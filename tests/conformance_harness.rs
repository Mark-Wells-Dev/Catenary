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

/// A **user-config** routing opt-in for a server that the shipped defaults do
/// not route to by default.
///
/// Two shapes of server need it, both opted into the same way — a `[lsp.language.*]`
/// binding in the user config layer (`CATENARY_CONFIG`), which merges over the
/// shipped defaults with array-replace semantics so the language's `servers` list
/// becomes `[server]` while every shipped server *definition* (command/args) stays
/// intact:
///
/// - **A non-default binding for an existing shipped language** (`marksman`:
///   `lattice` is the shipped markdown default, decision 015). Only [`Self::server`]
///   is set; the language already classifies files.
/// - **A server with no shipped language at all** (`vim-ls`, `ansible-ls`: defined
///   in `defaults/servers.toml` but bound by no `[lsp.language.*]`, so the shipped
///   defaults cannot classify a file to them — the tui-rework 07 gap). Here
///   [`Self::classify`] also supplies the classification (extensions and/or
///   filenames) the user config must define to route, exactly as a user of that
///   server would.
///
/// Why the **user** layer and not a project `.catenary.toml`: a project config's
/// `[lsp.language.*]` `servers` list drives classification only, not server
/// dispatch, so it never reroutes (tui-rework 13 class E). The user layer does.
/// The config lives only in the throwaway tempdir, so the shipped default every
/// real root gets is untouched.
#[derive(Clone, Copy)]
struct CaseBinding {
    /// The language whose `servers` list is set (e.g. `markdown`, `vimscript`).
    language: &'static str,
    /// The server to route it to (e.g. `marksman`, `vim-ls`).
    server: &'static str,
    /// Classification for a language absent from the shipped defaults: the
    /// `extensions` and/or `filenames` the user config must also define so a file
    /// classifies to `language`. `None` when `language` is already a shipped
    /// language (e.g. `markdown`), which classifies files without help.
    classify: Option<CaseClassify>,
}

/// The classification a [`CaseBinding`] must define for a language the shipped
/// defaults do not carry (`vim-ls` → `.vim`, `ansible-ls` → `playbook.yml`).
#[derive(Clone, Copy)]
struct CaseClassify {
    /// File extensions (without dot) that classify as the language.
    extensions: &'static [&'static str],
    /// Exact filenames that classify as the language (precedence over extension).
    filenames: &'static [&'static str],
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
/// This list is exactly the set of recipes + provisions that are neither
/// `pending` nor `conformance = false` — the [`matrix_and_cases_have_no_drift`]
/// guard fails `make check` on any divergence, so a new recipe/provision cannot
/// land without either a case here or an explicit exemption.
///
/// The tui-rework 13 additions close the 07-recorded gaps: `vscode-html`
/// (embedded-CSS validation), `ansible-ls` and `vim-ls` (each with a user-config
/// [`CaseBinding`] that defines + routes a language the shipped defaults do not
/// carry). The servers that cannot satisfy the intentional-diagnostic contract
/// through the shipped lifecycle — `cmake-ls` (no diagnostics at all),
/// `typescript-ls` and `marksman` (debounced/scan-based push with no pull
/// fallback), `vscode-eslint` (needs eslint co-install + settings) — are
/// `conformance = false` in the recipe/provision data with an honest `note`, so
/// they are absent here BY the guard, not by omission.
///
/// The tui-rework 10 tranche (rust-analyzer, clangd, jdtls, ruby-lsp, lattice
/// dogfood) is obtained in CI via `defaults/ci-provision.toml`; jdtls is
/// `slow_start`.
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
    // ── tui-rework 13 coverage (matrix/CASES drift closed) ─────────────
    // vscode-html publishes through its default embedded-CSS validation; vim-ls
    // and ansible-ls have no shipped `[lsp.language.*]` binding (the 07 gap), so
    // each carries a user-config `CaseBinding` that both defines and routes its
    // language — the exact opt-in a user of that server writes. vim-ls's fixture
    // diagnostic depends on `vint` (provisioned in CI, absent on the maintainer
    // host, so its local sentinel skips clean).
    Case {
        server: "vscode-html",
        fixture: "html",
        file: "broken.html",
        probe: "vscode-html-language-server",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "vim-ls",
        fixture: "vimscript",
        file: "broken.vim",
        probe: "vim-language-server",
        binding: Some(CaseBinding {
            language: "vimscript",
            server: "vim-ls",
            classify: Some(CaseClassify {
                extensions: &["vim"],
                filenames: &[],
            }),
        }),
        slow_start: false,
    },
    Case {
        server: "ansible-ls",
        fixture: "ansible",
        file: "playbook.yml",
        probe: "ansible-language-server",
        binding: Some(CaseBinding {
            language: "ansible",
            server: "ansible-ls",
            classify: Some(CaseClassify {
                extensions: &[],
                filenames: &["playbook.yml"],
            }),
        }),
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

/// Renders `values` as a TOML array of double-quoted strings (`["a", "b"]`).
///
/// The values here are static fixture extensions/filenames (no quotes/backslashes),
/// so a plain join suffices for the throwaway conformance config.
fn toml_string_array(values: &[&str]) -> String {
    let inner = values
        .iter()
        .map(|v| format!("\"{v}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
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
    // default) needs a one-line routing opt-in. It is written as a **user** config
    // (`CATENARY_CONFIG`) layered over the shipped defaults, NOT a project
    // `.catenary.toml`: a project config's `[lsp.language.*]` `servers` list drives
    // classification only, not server dispatch, so it never reroutes the language
    // (tui-rework 13 class E — the marksman fixture routed to the shipped default
    // `lattice` locally, which merely *masked* the miss because lattice was present;
    // in CI, where only marksman is provisioned, the same non-reroute surfaced as
    // `[no LSP coverage]`). The user layer's array-replace merge swaps the binding
    // to `[server]` deterministically in both environments while leaving every
    // shipped server *definition* (its command/args) intact. The file lives only in
    // this throwaway tempdir, so the shipped default every real root gets is
    // untouched.
    let user_config = match case.binding {
        Some(binding) => {
            let mut config = format!(
                "# Conformance routing opt-in (tui-rework 13): `{server}` is defined in\n\
                 # the shipped config but the shipped defaults do not route to it. A\n\
                 # user writes this binding in their user config to opt in; the shipped\n\
                 # server definition (command/args) is unchanged.\n\
                 [lsp.language.{language}]\nservers = [\"{server}\"]\n",
                server = binding.server,
                language = binding.language,
            );
            // A language absent from the shipped defaults also needs its
            // classification defined, exactly as a user of that server would.
            if let Some(classify) = binding.classify {
                use std::fmt::Write as _;
                if !classify.extensions.is_empty() {
                    let exts = toml_string_array(classify.extensions);
                    let _ = writeln!(config, "extensions = {exts}");
                }
                if !classify.filenames.is_empty() {
                    let names = toml_string_array(classify.filenames);
                    let _ = writeln!(config, "filenames = {names}");
                }
            }
            let config_path = root.path().join("conformance-user-config.toml");
            std::fs::write(&config_path, config).context("write conformance routing override")?;
            Some(config_path)
        }
        None => None,
    };

    let file = root.path().join(case.file);

    let file_name = Path::new(case.file)
        .file_name()
        .and_then(|n| n.to_str())
        .context("fixture file name")?;

    let mut bridge =
        BridgeProcess::spawn_conformance_with_config(root.path(), user_config.as_deref())?;
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
    let mut receipt = run_settle_diagnostics(&bridge, &file, wall)
        .with_context(|| format!("conformance settle for `{}`", case.server))?;

    // Bounded re-diagnose through the product's OWN repeat-run contract
    // (tui-rework 13 class C). A push-only server whose first `publishDiagnostics`
    // fires on a debounce timer *after* it has gone scheduler-idle loses that
    // publish to the cold settle-then-collect window: settle sees an idle tree
    // and releases, collect finds a never-heard cache, and (no advertised pull to
    // fall back on) the file reads a false `[clean]` — the residual push-only tail
    // of bug 74 the best-effort pull could not cover. The product's documented
    // answer is that a repeat bare `catenary diagnostics` re-diagnoses the same
    // batch fresh (AGENTS.md); on the second round the server is warm and its
    // publish lands inside the window. So on a `[clean]` first round, run exactly
    // ONE more full product diagnose round and take its receipt.
    //
    // This is NOT a retry loop and carries no sleep or wall-clock timing (the
    // contention doctrine, tui-rework 07 landing gate): it is a single, bounded
    // exercise of a real product guarantee — the same second run a user gets — and
    // it does not launder the signal, because a genuinely-clean fixture stays
    // `[clean]` across both rounds and still fails the assertion. Each round is
    // itself wall-bounded, so a never-idle server is still caught.
    if receipt.contains("[clean]") {
        receipt = run_settle_diagnostics(&bridge, &file, wall)
            .with_context(|| format!("conformance re-diagnose for `{}`", case.server))?;
    }

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
// where present. jdtls and ruby-lsp are intentionally NOT sentinels: they are
// exercised by the CI matrix via `CATENARY_CONFORMANCE` (jdtls a JVM cold start,
// ruby-lsp a gem install — neither belongs in the local dogfood fleet), keeping
// `make check` green on hosts without them.

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

/// The structural matrix↔`CASES` drift guard (tui-rework 13 class D): every
/// server the CI matrix runs (`conformed_server_names` — a recipe or provision
/// that is neither `pending` nor `conformance = false`) has exactly one `CASES`
/// entry, and no conformance-exempt server does.
///
/// This makes drift fail at `make check` instead of in CI: a new recipe/provision
/// with no case, or a case for a server the matrix will not run, fails here — the
/// gap 07 left open (a server could dispatch a matrix job with no `Case`, or a
/// `Case` could name a server the matrix never installs). The guard reads the
/// SAME two data files `tools/conformance_matrix.py` reads and applies the SAME
/// filter, so the two agree by construction.
#[test]
fn matrix_and_cases_have_no_drift() {
    use std::collections::BTreeSet;

    use catenary_mcp::recipes::{
        conformance_exempt_names, conformed_server_names, default_provisioning, default_recipes,
        provisioning_pending,
    };

    let recipes = default_recipes().expect("default recipes parse");
    let provisions = default_provisioning().expect("default provisioning parse");

    let case_servers: BTreeSet<&str> = CASES.iter().map(|c| c.server).collect();
    let conformed: BTreeSet<String> = conformed_server_names(&recipes, &provisions)
        .into_iter()
        .collect();
    let exempt: BTreeSet<String> = conformance_exempt_names(&recipes, &provisions)
        .into_iter()
        .collect();

    // Every matrix server has a case.
    let missing_case: Vec<&String> = conformed
        .iter()
        .filter(|s| !case_servers.contains(s.as_str()))
        .collect();
    assert!(
        missing_case.is_empty(),
        "matrix↔CASES drift: these recipe/provision servers run in the conformance \
         matrix but have no `Case` (add a fixture + CASES entry, or mark them \
         `conformance = false` with a note): {missing_case:?}"
    );

    // No conformance-exempt server has a case (an exemption must be complete —
    // both the data flag and the absent case, so the two never disagree).
    let exempt_with_case: Vec<&String> = exempt
        .iter()
        .filter(|s| case_servers.contains(s.as_str()))
        .collect();
    assert!(
        exempt_with_case.is_empty(),
        "matrix↔CASES drift: these servers are `conformance = false` (excluded from \
         the matrix) but still carry a `Case` — remove the case or drop the exemption: \
         {exempt_with_case:?}"
    );

    // Every `Case` names a server the matrix installs (conformed) OR one whose
    // provision is `pending` — a staged case that stays ready for the day the
    // maintainer fills the unresolved pin (jdtls). A case for a server that is
    // neither conformed nor pending would never be exercised and is drift.
    let pending: BTreeSet<String> = provisioning_pending(&provisions).into_iter().collect();
    let orphan_case: Vec<&str> = case_servers
        .iter()
        .copied()
        .filter(|s| !conformed.contains(*s) && !pending.contains(*s))
        .collect();
    assert!(
        orphan_case.is_empty(),
        "matrix↔CASES drift: these `CASES` name servers with no non-exempt, \
         non-pending recipe/provision, so the matrix never installs them: {orphan_case:?}"
    );
}
