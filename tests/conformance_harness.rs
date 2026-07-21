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
//!   fleet (rust-analyzer, lattice, taplo, yaml-language-server,
//!   vscode-json-language-server, bash-language-server) as the
//!   sentinel: one `#[test]` per server, each **skip-if-binary-missing** so a
//!   host lacking a server stays green. (Ordinary CI's `ci.yml` runs only
//!   `cargo test --lib --bins`, so these integration tests do not run there — the
//!   maintainer host and the conformance matrix are where they exercise.)
//! - **The conformance CI matrix** installs one pinned server per job and selects
//!   it with `CATENARY_CONFORMANCE=<server>`: [`conformance_selected_server`]
//!   then runs exactly that server and **requires** its binary (a missing binary
//!   is a failed install, not a skip).
//!
//! ## Two modes: rooted and single-file (brackets 06)
//!
//! Every case runs its diagnostics probe in BOTH modes. The **rooted** leg is
//! the blessing gate above, unchanged. The **single-file** leg then probes the
//! SAME server under the rootless tier brackets 01 landed — the genuine LSP
//! single-file wire shape (null `rootUri`, null `rootPath`, NO
//! `workspaceFolders` member), produced by the product's own initialize
//! builder, never a fork — and records whether the server published/answered
//! trustworthy diagnostics for an opened stray document. A rooted PASS with a
//! single-file miss is a **finding** (the manifest's `single_file` row stays
//! `enrichment-only`), never a suite failure: the leg asserts nothing and
//! renders evidence on stderr ([`single_file_evidence`]). Upgrading a row to
//! `serves-diagnostics` is a maintainer act on the manifest data, citing a
//! SERVES line from a real run — the misc-196 verify-then-declare bar.
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
use common::{BridgeProcess, mockls_lsp_arg};
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

/// The bounded evidence window for the single-file (null-root) leg
/// (brackets 06).
///
/// Deliberately tighter than [`CONFORMANCE_WALL_BOUND`]: a negative outcome
/// here is a FINDING (the row stays `enrichment-only`), never a suite
/// failure, so a shorter window can only under-claim — the conservative,
/// fail-closed direction — while a served answer stops the wait immediately.
/// One minute is ~5–45x the fleet's observed publish latencies (1.3–1.9 s for
/// the event fleet, 6–12 s for ansible-lint's async pass, misc 196), so a
/// server that cannot answer inside it has no timely stray-file diagnostics
/// to offer the editing loop anyway. Slow-start cases (jdtls) ride
/// [`CONFORMANCE_WALL_BOUND`] instead — JVM spin-up dominates their clock.
const SINGLE_FILE_EVIDENCE_BOUND: Duration = Duration::from_mins(1);

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
/// - **A server with no shipped language at all** (`ansible-language-server`:
///   defined in `defaults/servers.toml` but bound by no `[lsp.language.*]`, so the shipped
///   defaults cannot classify a file to it — the tui-rework 07 gap). Here
///   [`Self::classify`] also supplies the classification (extensions and/or
///   filenames) the user config must define to route, exactly as a user of that
///   server would.
///
/// Why the **user** layer and not a project `.catenary.toml`: both layers reach
/// dispatch since misc 155 (bug 81's fix), but the harness deliberately routes
/// through the user layer — that is the layer these cases pin (the project-layer
/// reroute has its own dispatch-level tests in `src/lsp/manager.rs`). The config
/// lives only in the throwaway tempdir, so the shipped default every real root
/// gets is untouched.
#[derive(Clone, Copy)]
struct CaseBinding {
    /// The language whose `servers` list is set (e.g. `markdown`, `ansible`).
    language: &'static str,
    /// The server to route it to (e.g. `marksman`, `ansible-language-server`).
    server: &'static str,
    /// Classification for a language absent from the shipped defaults: the
    /// `extensions` and/or `filenames` the user config must also define so a file
    /// classifies to `language`. `None` when `language` is already a shipped
    /// language (e.g. `markdown`), which classifies files without help.
    classify: Option<CaseClassify>,
}

/// The classification a [`CaseBinding`] must define for a language the shipped
/// defaults do not carry (`ansible-language-server` → `playbook.yml`).
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
    ///
    /// The key IS the executable Catenary spawns (misc 162), so it is also the
    /// binary whose presence on `PATH` gates a local run — there is no separate
    /// `probe` field.
    server: &'static str,
    /// Fixture project directory under `tests/fixtures/conformance/`.
    fixture: &'static str,
    /// The representative file within the fixture, relative to its dir.
    file: &'static str,
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
/// The tui-rework 13 additions close the 07-recorded gaps: `vscode-html-language-server`
/// (embedded-CSS validation) and `ansible-language-server` (a user-config [`CaseBinding`]
/// that defines + routes a language the shipped defaults do not carry).
/// The servers that cannot satisfy the intentional-diagnostic contract
/// through the shipped lifecycle — `cmake-language-server` (no diagnostics at all),
/// `marksman` (scan-based push with no pull fallback),
/// `vscode-eslint-language-server` (needs eslint co-install + settings) — are
/// `conformance = false` in the recipe/provision data with an honest `note`, so
/// they are absent here BY the guard, not by omission.
///
/// `typescript-language-server` was in that exempt set (a debounced push with no
/// pull fallback) until diagnostics-debt 05 un-exempted it behind the
/// declared-constant gate (`discipline = "debounce"`, `debounce_ms = 850`): the
/// ledger awaits the version echo bounded by the declared constant, so its
/// debounced publish is collected instead of lost to silence. It now has a case.
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
        binding: None,
        slow_start: false,
    },
    Case {
        server: "taplo",
        fixture: "toml",
        file: "broken.toml",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "lattice",
        fixture: "markdown",
        file: "broken.md",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "yaml-language-server",
        fixture: "yaml",
        file: "broken.yaml",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "vscode-json-language-server",
        fixture: "json",
        file: "broken.json",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "bash-language-server",
        fixture: "shellscript",
        file: "broken.sh",
        binding: None,
        slow_start: false,
    },
    // ── npm / cargo / pip / go tranche (CI matrix) ─────────────────────
    Case {
        server: "pyright-langserver",
        fixture: "python",
        file: "broken.py",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "vscode-css-language-server",
        fixture: "css",
        file: "broken.css",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "intelephense",
        fixture: "php",
        file: "broken.php",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "svelteserver",
        fixture: "svelte",
        file: "src/Broken.svelte",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "elm-language-server",
        fixture: "elm",
        file: "src/Main.elm",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "docker-langserver",
        fixture: "dockerfile",
        file: "Dockerfile",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "gopls",
        fixture: "go",
        file: "main.go",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "lua-language-server",
        fixture: "lua",
        file: "broken.lua",
        binding: None,
        slow_start: false,
    },
    // ── tui-rework 10 coverage (CI matrix via ci-provision.toml) ───────
    Case {
        server: "clangd",
        fixture: "c",
        file: "broken.c",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "jdtls",
        fixture: "java",
        file: "Broken.java",
        binding: None,
        slow_start: true,
    },
    Case {
        server: "ruby-lsp",
        fixture: "ruby",
        file: "broken.rb",
        binding: None,
        slow_start: false,
    },
    // ── tui-rework 13 coverage (matrix/CASES drift closed) ─────────────
    // vscode-html-language-server publishes through its default embedded-CSS
    // validation; ansible-language-server has no shipped `[lsp.language.*]`
    // binding (the 07 gap), so it carries a user-config `CaseBinding` that both
    // defines and routes its language — the exact opt-in a user of that server
    // writes. (vim-language-server was dropped 2026-07-08: upstream dead 3
    // years, its vint linter dead since 2020 — "if we install it for the user,
    // it has to work".)
    Case {
        server: "vscode-html-language-server",
        fixture: "html",
        file: "broken.html",
        binding: None,
        slow_start: false,
    },
    Case {
        server: "ansible-language-server",
        fixture: "ansible",
        file: "playbook.yml",
        binding: Some(CaseBinding {
            language: "ansible",
            server: "ansible-language-server",
            classify: Some(CaseClassify {
                extensions: &[],
                filenames: &["playbook.yml"],
            }),
        }),
        slow_start: false,
    },
    // ── diagnostics-debt 05: ts-ls un-exempted behind the declared-constant gate ──
    // typescript-language-server was `conformance = false` (tui-rework 13) because
    // its first diagnostic lands on an internal debounce AFTER the process goes
    // scheduler-idle, past the settle-then-collect window. Ledger 05's
    // declared-constant gate awaits the version echo bounded by the manifest's
    // `debounce_ms` (850 ms), so ts-ls now conforms. Routed purely through the
    // shipped defaults (`.ts` → typescript → typescript-language-server); the
    // fixture carries a `tsconfig.json` root marker so tsserver type-checks.
    Case {
        server: "typescript-language-server",
        fixture: "typescript",
        file: "broken.ts",
        binding: None,
        slow_start: false,
    },
    // ── misc 207: TOML succession (dual-bless-and-watch, PULL lane) ────
    // tombi reuses the `toml` fixture (same broken.toml that taplo's case
    // uses). The `CaseBinding` reroutes TOML to tombi specifically — without
    // it, an ambient taplo would satisfy the intentional-diagnostic assertion
    // on taplo's work, laundering the exact evidence this job exists to
    // produce.
    Case {
        server: "tombi",
        fixture: "toml",
        file: "broken.toml",
        binding: Some(CaseBinding {
            language: "toml",
            server: "tombi",
            classify: None,
        }),
        slow_start: false,
    },
];

/// Finds `case` by server name.
fn lookup(server: &str) -> Option<&'static Case> {
    CASES.iter().find(|c| c.server == server)
}

/// Whether the executable for server `name` is actually installed on the
/// inherited `PATH`.
///
/// Since misc 162 the server key IS the executable, so this is the local-run
/// gate. It defers to the product's own [`server_binary_installed`], which is
/// honest against the rust-analyzer rustup proxy shim: a bare proxy with no
/// component behind it reads as NOT installed, so the sentinel skips cleanly on
/// a rustup host without the component (rather than spawning the shim and
/// failing). In CI the workflow links the real component ahead of the shim, so
/// the resolved binary is the component and the gate passes.
fn on_path(name: &str) -> bool {
    catenary_cli::health::servers::server_binary_installed(name, name)
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

/// Drives a diagnostics serve for `file` **with settle**, bounded by a hard
/// `wall` clock.
///
/// Root-ownership stage 3 retired the two-phase editing handoff: the daemon serves
/// `tool/editing-stop` against the on-disk ledger (bare) or the named `files`
/// (scoped). Conformance names its fixture file (the scoped form, served
/// regardless of ledger state), then reads the settle-and-pull response to EOF
/// under the wall bound. The write half is left open (bug 24: a write-shutdown
/// reads as client-disconnect on the daemon side and races the response).
/// Exceeding the wall bound is the never-idle failure.
fn run_settle_diagnostics(bridge: &BridgeProcess, file: &Path, wall: Duration) -> Result<String> {
    let socket = bridge.wait_for_ipc_socket()?;
    let file_str = file.to_str().context("fixture path is not UTF-8")?;
    let raw = read_editing_stop_wall_bounded(&socket, file_str, wall)?;
    Ok(common::diagnostics_output(&raw))
}

/// Sends a scoped `tool/editing-stop` (naming `file`) and reads the
/// settle-and-pull response to EOF, failing hard if the daemon has not closed the
/// response within `wall`.
///
/// The 500 ms per-read timeout is only poll cadence: it lets the loop re-check
/// the wall clock every window while a blocked read is in flight. Unlike the
/// progress-aware IPC helpers this carries **no** no-progress escape — the whole
/// point of conformance is a finite ceiling on total settle time, so a server
/// that keeps its tree hot forever (never-idle) is caught, not excused.
fn read_editing_stop_wall_bounded(socket: &Path, file: &str, wall: Duration) -> Result<String> {
    let mut stream = UnixStream::connect(socket).context("connect diagnostics socket")?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .context("set poll-cadence read timeout")?;
    writeln!(
        stream,
        "{}",
        json!({ "method": "tool/editing-stop", "files": [file] })
    )
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
    // The server key IS the executable (misc 162), so its presence on PATH is
    // exactly the local-run gate — no separate probe binary.
    if !on_path(case.server) {
        if require {
            bail!(
                "conformance for `{}` was selected via {CONFORMANCE_ENV} but its binary \
                 is not on PATH — the pinned install did not land",
                case.server,
            );
        }
        eprintln!(
            "conformance: skipping `{}` — binary not on PATH",
            case.server
        );
        return Ok(());
    }

    // KeepOnPanic folds in `keep_for_triage` (CI's unconditional keep) and adds
    // the failure-only keep: on a panicking assertion — every conformance
    // assertion is an `assert!` — the fixture root (its copied fixture, and the
    // daemon evidence written under it) survives the unwind, and the kept path
    // is eprintln'd into nextest's captured failure output (misc 194).
    let root = common::KeepOnPanic::new(tempfile::tempdir().context("create fixture root")?);
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
    // and its known-deterministic casualties (`lattice`, `vscode-json-language-server`) are
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
        !receipt_names_round_fault(&receipt),
        "`{}` was attributed a round fault instead of publishing its intentional \
         diagnostic — a fault note names the fixture file without diagnosing it, \
         so it must fail the probe explicitly:\n{receipt}",
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

    // The second mode (brackets 06): the SAME server, probed under the
    // rootless single-file tier. Evidence only — a miss is a finding, never a
    // suite failure, so nothing here asserts.
    eprintln!("{}", single_file_evidence(case).render(case.server));
    Ok(())
}

/// Whether the receipt carries a verified-contract-violation fault note
/// (`<path> [<server> did not answer for this round — … re-run to retry]`).
///
/// Such a receipt names the fixture file but renders no `[clean]`, no
/// `[unverified`, and no `[no LSP coverage]`, so without this check it slips
/// every conformance assertion while delivering zero diagnostics — the probe
/// demands the intentional diagnostic itself, not a fault attribution. The
/// matched substring is the stable head of the wording pinned product-side by
/// `unverified_label` (`src/bridge/diagnostics_server.rs`) and its
/// `format_contract_violation_renders_the_designs_exact_wording` test.
fn receipt_names_round_fault(receipt: &str) -> bool {
    receipt.contains("did not answer for this round")
}

/// A synthetic fault-note receipt proves the blind spot and its closure: it
/// passes all four pre-existing conformance rejections (names the fixture,
/// no `[clean]` / `[unverified` / `[no LSP coverage]`) yet delivers no
/// diagnostic, and [`receipt_names_round_fault`] is what catches it. A dirty
/// receipt stays unflagged.
#[test]
fn fault_note_receipt_is_rejected() {
    let fault = "/root/broken.toml [taplo did not answer for this round \u{2014} its \
                 verified behavior requires a response; treating as a server fault, \
                 re-run to retry]\n";
    assert!(fault.contains("broken.toml"));
    assert!(!fault.contains("[clean]"));
    assert!(!fault.contains("[unverified"));
    assert!(!fault.contains("[no LSP coverage]"));
    assert!(
        receipt_names_round_fault(fault),
        "a fault note is not a diagnosis — the probe must reject it"
    );

    let dirty = "/root/broken.toml\n  1:1 error: invalid TOML [taplo]\n";
    assert!(
        !receipt_names_round_fault(dirty),
        "a receipt carrying real diagnostics must not be flagged"
    );
}

// ── Single-file (null-root) evidence leg (brackets 06) ────────────────
//
// Brackets 01 landed the rootless tier: a server may spawn in genuine LSP
// single-file mode — null `rootUri`, null `rootPath`, NO `workspaceFolders`
// member — and each blessed row carries a `single_file` capability
// (`unsupported` fail-closed / `enrichment-only` / `serves-diagnostics`).
// Every row shipped conservative `enrichment-only` because every prior probe
// ran ROOTED; the maintainer ruling — "the servers that get stray-file
// diagnostics are the ones that can serve them" — demands REAL observed
// null-root behavior before a row is upgraded. This leg produces that
// evidence beside every rooted blessing run.
//
// The probe drives the product's own client — `LspClient::spawn` +
// `initialize(&[], …)`, the exact calls the daemon's `spawn_single_file`
// makes — so the wire shape IS the landed builder
// (`src/lsp/params.rs::initialize` with no roots), never a forked second
// one, and the trust judgment is the product's own settlement consult
// (`settled_diagnostics`) plus its best-effort on-demand pull. The daemon
// IPC surface cannot express this probe: since misc 203 a scoped serve
// auto-mounts the named file's enclosing directory (`ExplicitTarget`), so
// every `tool/editing-stop` serve is ROOTED by construction.

/// What the single-file (null-root) probe observed for one server.
///
/// Rendered as one stderr evidence line ([`Self::render`]) — the suite's
/// reporting surface. Only [`Self::Serves`] is upgrade evidence; every other
/// variant is a finding that keeps the row at `enrichment-only`.
enum SingleFileEvidence {
    /// Trustworthy diagnostics arrived for the opened stray document: a
    /// publish that settles for the opened version (the product's own
    /// settlement judgment), or a non-empty answered pull.
    Serves {
        /// Which channel carried the evidence (`settled publish` / `answered
        /// pull`).
        channel: &'static str,
        /// How many diagnostics arrived.
        count: usize,
        /// Time from `didOpen` to the evidence.
        elapsed: Duration,
    },
    /// The server rejected the null-root `initialize` — the same class the
    /// daemon negative-caches (`spawn_single_file`'s "rejected single-file
    /// mode" arm).
    RejectedInit(String),
    /// The probe machinery could not complete (spawn failure, notification
    /// transport failure, tempdir trouble). Reported honestly, never a suite
    /// failure.
    ProbeIncomplete(String),
    /// The bound expired with no trustworthy diagnostics.
    NoDiagnostics {
        /// Final push-cache state for the URI: `None` = never heard,
        /// `Some(n)` = a publish arrived carrying `n` diagnostics but never
        /// settled as trustworthy (`Some(0)` = only empty publishes).
        heard: Option<usize>,
        /// Whether the best-effort pull answered — with an empty report.
        empty_pull_answer: bool,
        /// The evidence window that expired.
        bound: Duration,
    },
}

impl SingleFileEvidence {
    /// Renders the one-line stderr evidence record for `server`.
    fn render(&self, server: &str) -> String {
        match self {
            Self::Serves {
                channel,
                count,
                elapsed,
            } => {
                let secs = elapsed.as_secs_f64();
                format!(
                    "conformance single-file: `{server}` SERVES DIAGNOSTICS under null root — \
                     {count} diagnostic(s) via {channel} in {secs:.1}s; direct evidence for \
                     upgrading the manifest row to `serves-diagnostics` (misc-196 evidence bar)"
                )
            }
            Self::RejectedInit(err) => format!(
                "conformance single-file: `{server}` FINDING — rejected null-root initialize \
                 (the negative-cache class): {err}; row stays `enrichment-only`"
            ),
            Self::ProbeIncomplete(err) => format!(
                "conformance single-file: `{server}` FINDING — probe did not complete ({err}); \
                 row stays `enrichment-only`"
            ),
            Self::NoDiagnostics {
                heard,
                empty_pull_answer,
                bound,
            } => {
                let push = match heard {
                    None => "no publish arrived",
                    Some(0) => "only empty publishes arrived",
                    Some(_) => "a publish arrived but never settled for the opened version",
                };
                let pull = if *empty_pull_answer {
                    "; the on-demand pull answered an empty report"
                } else {
                    "; no on-demand pull answer carried diagnostics"
                };
                format!(
                    "conformance single-file: `{server}` FINDING — no trustworthy diagnostics \
                     within {bound:?} under null root ({push}{pull}); row stays `enrichment-only`"
                )
            }
        }
    }
}

/// Watches the opened stray document for trustworthy diagnostics under the
/// null-root session, bounded by `bound`.
///
/// The 100 ms cadence is poll only (the harness's wall-bound idiom): each
/// tick consults the product's own settlement judgment
/// ([`catenary_cli::lsp::LspClient::settled_diagnostics`] — the consult the
/// receipt renders from, so a stale or untrusted publish never counts), and
/// once per second the product's best-effort on-demand pull
/// ([`catenary_cli::lsp::LspClient::try_pull_diagnostics`]) runs, mirroring
/// retrieval's pull channel. The bound is a finite ceiling whose expiry is a
/// FINDING, not a failure.
async fn observe_null_root_diagnostics(
    client: &catenary_cli::lsp::LspClient,
    uri: &str,
    bound: Duration,
) -> SingleFileEvidence {
    let started = Instant::now();
    let deadline = started + bound;
    let mut empty_pull_answer = false;
    let mut next_pull = started + Duration::from_secs(1);
    loop {
        if let Some(diags) = client.settled_diagnostics(uri)
            && !diags.is_empty()
        {
            return SingleFileEvidence::Serves {
                channel: "settled publish",
                count: diags.len(),
                elapsed: started.elapsed(),
            };
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= next_pull {
            next_pull = now + Duration::from_secs(1);
            if let Some(diags) = client.try_pull_diagnostics(uri).await {
                if diags.is_empty() {
                    empty_pull_answer = true;
                } else {
                    return SingleFileEvidence::Serves {
                        channel: "answered pull",
                        count: diags.len(),
                        elapsed: started.elapsed(),
                    };
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    SingleFileEvidence::NoDiagnostics {
        heard: client.get_diagnostics(uri).map(|d| d.len()),
        empty_pull_answer,
        bound,
    }
}

/// Runs the single-file (null-root) probe for `case` and returns the observed
/// evidence.
///
/// Never panics and never fails the suite — every outcome, machinery trouble
/// included, renders as an evidence line. The fixture's representative file
/// is copied ALONE into a bare, markerless tempdir (the stray shape the tier
/// exists for: a lone script or config outside any project), so a server that
/// needs its project context to diagnose misses here honestly. The server
/// definition is the shipped `[lsp.server.*]` default — no user layer — and
/// the spawn is PATH-resolved exactly like the [`on_path`] gate that admitted
/// this run (the key IS the executable, misc 162). The spawned child inherits
/// this process's cwd, the same posture a production rootless spawn has with
/// the daemon's cwd: unrelated to the stray file, so evidence rides the
/// opened URI only.
fn single_file_evidence(case: &Case) -> SingleFileEvidence {
    use catenary_cli::logging::LoggingServer;
    use catenary_cli::lsp::{LspClient, Scope};

    // Shipped defaults only: the same definition the daemon's rootless spawn
    // reads on a stock install.
    let config = catenary_cli::config::Config::default_with_classification();
    let Some(def) = config.server.get(case.server) else {
        return SingleFileEvidence::ProbeIncomplete(format!(
            "no shipped [lsp.server.{}] definition",
            case.server
        ));
    };

    let dir = match tempfile::tempdir() {
        Ok(dir) => dir,
        Err(e) => {
            return SingleFileEvidence::ProbeIncomplete(format!("create stray tempdir: {e}"));
        }
    };
    let src = fixture_dir(case).join(case.file);
    let Some(file_name) = Path::new(case.file).file_name() else {
        return SingleFileEvidence::ProbeIncomplete("fixture file has no name".to_string());
    };
    let stray = dir.path().join(file_name);
    if let Err(e) = std::fs::copy(&src, &stray) {
        return SingleFileEvidence::ProbeIncomplete(format!("copy stray fixture: {e}"));
    }
    let text = match std::fs::read_to_string(&stray) {
        Ok(text) => text,
        Err(e) => return SingleFileEvidence::ProbeIncomplete(format!("read stray fixture: {e}")),
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return SingleFileEvidence::ProbeIncomplete(format!("build tokio runtime: {e}")),
    };
    let bound = if case.slow_start {
        CONFORMANCE_WALL_BOUND
    } else {
        SINGLE_FILE_EVIDENCE_BOUND
    };

    rt.block_on(async {
        let args: Vec<&str> = def.args.iter().map(String::as_str).collect();
        let mut client = match LspClient::spawn(
            def.program(case.server),
            &args,
            // The fixture dir name IS the language key throughout CASES
            // (`rust`, `shellscript`, `dockerfile`, …), so it is the
            // `languageId` the daemon would open this document with.
            case.fixture,
            case.server,
            LoggingServer::new(),
            def.settings.clone(),
            def.env.as_ref(),
            "",
        ) {
            Ok(client) => client,
            Err(e) => return SingleFileEvidence::ProbeIncomplete(format!("spawn failed: {e:#}")),
        };
        client.server().set_scope(Scope::SingleFile);

        // The landed null-root initialize (brackets 01): empty roots make the
        // product builder emit null `rootUri`, null `rootPath`, and no
        // `workspaceFolders` member — reused via the product client, never
        // forked. Rejection here is exactly the class the daemon
        // negative-caches.
        if let Err(e) = client
            .initialize(&[], def.initialization_options.clone())
            .await
        {
            return SingleFileEvidence::RejectedInit(format!("{e:#}"));
        }

        let uri = format!("file://{}", stray.display());
        if let Err(e) = client.did_open(&uri, case.fixture, 1, &text).await {
            return SingleFileEvidence::ProbeIncomplete(format!("didOpen failed: {e:#}"));
        }
        // The editing lifecycle always delivers the save event for a changed
        // file, so deliver it here too — save-triggered publishers (the diff
        // discipline) get their contractual trigger.
        if let Err(e) = client.did_save(&uri).await {
            return SingleFileEvidence::ProbeIncomplete(format!("didSave failed: {e:#}"));
        }

        let outcome = observe_null_root_diagnostics(&client, &uri, bound).await;
        // Best-effort teardown; Drop kills the process if shutdown fails.
        let _ = client.shutdown().await;
        outcome
    })
}

/// Runs a sentinel case by server name — skips if its binary is absent.
fn sentinel(server: &str) -> Result<()> {
    let case = lookup(server).with_context(|| format!("no conformance case for `{server}`"))?;
    run_conformance(case, false)
}

// ── Dogfooded-fleet sentinels (skip-if-binary-missing) ────────────────
//
// These run in ordinary `make test` / `make check` and skip cleanly where a
// server binary is absent. rust-analyzer (native + flycheck), taplo,
// yaml-language-server, and bash-language-server (shellcheck linter feeder)
// deliver diagnostics that survive a cold diagnose and so are reliable
// make-check sentinels.

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
    sentinel("yaml-language-server")
}

#[test]
fn conformance_bash_ls() -> Result<()> {
    sentinel("bash-language-server")
}

// ── Formerly-ignored cold-diagnose sentinels (bug 74, now fixed) ──────
//
// lattice and vscode-json-language-server each returned a deterministic
// `[clean]` on a cold daemon. The tui-rework 07 report filed both under one
// "cold-pull" heading, but they were two distinct gaps the conformance harness
// caught: lattice publishes once on its workspace scan (which the then
// per-round pre-batch cache clear wiped before the batch reopened the
// unchanged file — today the per-send clear in `open_document_on` only wipes
// entries for content actually being resent)
// and never advertises `diagnosticProvider`, so the settle-then-collect
// pipeline never pulled it — even though it answers `textDocument/diagnostic`
// on demand (fixed: best-effort pull on an empty push cache);
// vscode-json-language-server is pull-only and *was* pulled, but Catenary's empty
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
    sentinel("vscode-json-language-server")
}

// ── Declared-push behavioral leg (misc 187) ───────────────────────────

/// Pins the lattice `declares_push` profile declaration (misc 187) to the
/// live binary: a cold server publishes for EVERY `didOpen` — an explicit
/// (possibly empty) publish for a clean file, a non-empty publish for a
/// dirty one (the misc-153 / Lattice-16 / decision-022 contract). A lattice
/// that drifts fails this re-pin, not the user's receipts at runtime — the
/// asymmetry that makes carrying the declaration in code safe.
///
/// The observable is the product receipt, which under the declaration is
/// itself the proof: lattice advertises no pull channel and rejects the
/// best-effort probe (`-32601`, 7 days of firehose evidence), and the
/// declared-push evidence bar arms from turn zero of the fresh connection —
/// so `[clean]` can only be rendered over a heard publish (the explicit
/// `[]`), never over absence; silence resolves `[unverified — …]` and fails.
///
/// Contention doctrine: no sleeps, no tight bounds — both legs ride the
/// harness's wall-bounded settle machinery, and the evidence bar's own
/// dead-air budget is the only wait for the publish.
#[test]
fn conformance_lattice_declared_push() -> Result<()> {
    if !on_path("lattice") {
        eprintln!("conformance: skipping `lattice` declared-push — binary not on PATH");
        return Ok(());
    }

    // The checked-in markdown fixture supplies the dirty file; the clean file
    // is written beside it in the throwaway copy (nothing dangles).
    let case = lookup("lattice").context("no conformance case for `lattice`")?;
    let mut root = tempfile::tempdir().context("create fixture root")?;
    common::keep_for_triage(&mut root);
    copy_dir(&fixture_dir(case), root.path())?;
    let clean = root.path().join("clean.md");
    std::fs::write(
        &clean,
        "# Clean fixture\n\nNothing in this document dangles.\n",
    )
    .context("write clean fixture")?;
    let dirty = root.path().join(case.file);

    let mut bridge = BridgeProcess::spawn_conformance(root.path())?;
    bridge.initialize()?;

    // Cold server, didOpen a clean file: the explicit (possibly empty)
    // publish must arrive.
    let receipt = run_settle_diagnostics(&bridge, &clean, CONFORMANCE_WALL_BOUND)
        .context("declared-push clean leg")?;
    assert!(
        !receipt.contains("[unverified"),
        "no publish arrived for the clean didOpen — the declared-push contract \
         (publish on every didOpen, explicit [] for clean) has drifted:\n{receipt}"
    );
    assert!(
        receipt.contains("[clean]"),
        "the clean didOpen must resolve to a publish-backed [clean]:\n{receipt}"
    );

    // Same connection, didOpen a dirty file: a non-empty publish must arrive.
    let receipt = run_settle_diagnostics(&bridge, &dirty, CONFORMANCE_WALL_BOUND)
        .context("declared-push dirty leg")?;
    assert!(
        !receipt.contains("[clean]") && !receipt.contains("[unverified"),
        "the dirty didOpen must produce a non-empty publish:\n{receipt}"
    );
    assert!(
        receipt.contains("broken.md"),
        "the dirty receipt must diagnose the fixture file:\n{receipt}"
    );

    bridge
        .shutdown_clean(SHUTDOWN_GRACE)
        .context("clean shutdown for `lattice` declared-push")?;

    eprintln!("conformance: `lattice` declared-push PASSED");
    Ok(())
}

// ── Synthetic mockls-persona legs (diagnostics-debt 04c) ──────────────
//
// Each persona incarnates one publisher discipline from the DESIGN's taxonomy as
// a blessed manifest row (`defaults/mockls-personas.toml`, gated behind
// `feature = "mockls"`) paired with a default mockls behaviour bundle
// (`tools/mockls.rs::persona_bundle`) selected from the server key. These legs
// generalize misc 187's "mockls twins isolate the declaration as the sole
// variable" to the whole taxonomy: the manifest row DECLARES X, the binary
// (spawned under the persona key with NO flags) demonstrably DOES X — and the
// violator demonstrably does NOT.
//
// Unlike the real-server cases they need no binary on PATH: mockls is a workspace
// binary built by the same `--features mockls` test build that carries the
// persona rows, so these run everywhere `make check` runs. They spawn via a
// `CATENARY_SERVERS` mockls spec (the persona key doubles as the language + file
// extension), NOT the shipped-defaults `spawn_conformance` path the real cases
// use. Contention doctrine: `call_diagnostics` runs the pipeline to completion
// (settle + collect) before returning, so the receipt is authoritative with no
// sleep or poll.

/// Spawns a bridge whose sole server is mockls under the persona server `key`
/// (its default behaviour bundle, plus any extra `flags`), diagnoses a file with
/// the persona's extension, and returns the receipt.
fn persona_receipt(key: &str, flags: &str, content: &str) -> Result<String> {
    let dir = tempfile::tempdir().context("persona tempdir")?;
    let file = dir.path().join(format!("case.{key}"));
    std::fs::write(&file, content).context("write persona fixture")?;

    let lsp = mockls_lsp_arg(key, flags);
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("root")?)?;
    bridge.initialize()?;
    let receipt = bridge.call_diagnostics(file.to_str().context("file")?)?;
    bridge
        .shutdown_clean(SHUTDOWN_GRACE)
        .with_context(|| format!("clean shutdown for persona `{key}`"))?;
    Ok(receipt)
}

/// `mockls-pull` — pull discipline (the gopls shape). The manifest declares
/// `discipline = "pull"`; the binary's bundle (`--pull-diagnostics
/// --no-push-diagnostics`) makes the pull the sole channel, so a dirty file's
/// diagnostic is retrieved via the pull that settles the debt.
#[test]
fn conformance_mockls_pull() -> Result<()> {
    let receipt = persona_receipt("mockls-pull", "", "echo hello\n")?;
    assert!(
        receipt.contains("mock diagnostic"),
        "`mockls-pull` declares pull discipline — the pull must retrieve and \
         settle the diagnostic:\n{receipt}"
    );
    eprintln!("conformance: `mockls-pull` PASSED");
    Ok(())
}

/// `mockls-event` — event discipline, versioned publishes (the rust-analyzer
/// shape). The manifest declares `discipline = "event"`; the binary publishes on
/// didOpen and (with the leg's `--publish-version`) echoes the version, so the
/// versioned publish settles and the diagnostic reaches the receipt.
#[test]
fn conformance_mockls_event() -> Result<()> {
    let receipt = persona_receipt("mockls-event", "--publish-version", "echo hello\n")?;
    assert!(
        receipt.contains("mock diagnostic"),
        "`mockls-event` declares event discipline — a versioned publish must \
         settle the debt and carry the diagnostic:\n{receipt}"
    );
    eprintln!("conformance: `mockls-event` PASSED");
    Ok(())
}

/// `mockls-declared` — event discipline, unversioned + `declares_push` (the
/// lattice shape). The manifest declares the contractual publish-per-didOpen with
/// an explicit `[]` for clean; the binary's bundle (`--push-empty`) publishes
/// exactly that empty set, so a clean file resolves to a publish-backed `[clean]`
/// — never a probe-backed one, never `[unverified]`.
#[test]
fn conformance_mockls_declared() -> Result<()> {
    let receipt = persona_receipt("mockls-declared", "", "echo hello\n")?;
    assert!(
        !receipt.contains("[unverified"),
        "`mockls-declared` declares push (explicit [] for clean) — the empty \
         publish must arrive, never resolve to absence:\n{receipt}"
    );
    assert!(
        receipt.contains("[clean]"),
        "the declared-push clean file must resolve to a publish-backed \
         [clean]:\n{receipt}"
    );
    eprintln!("conformance: `mockls-declared` PASSED");
    Ok(())
}

/// `mockls-debounce` — debounce discipline (the ts-ls shape), the declared-constant
/// gate's synthetic harness (diagnostics-debt 05). The manifest declares
/// `discipline = "debounce"` with `debounce_ms = 300`; the binary's bundle fires a
/// versioned publish on the save event AFTER that declared window (a sleeping timer
/// the settle activity model cannot see), and answers no pull.
///
/// **Single-round await-echo-within-bound (the upgrade).** On a cold diagnose the
/// file is never-heard at settle. The debounce discipline arms the retrieval
/// evidence bar from turn zero — it does not wait for a demonstrated publish
/// (05's arming change) — and holds collection, bounded by the declared 300 ms
/// constant read off the pin. The version-echoing publish lands inside that bound,
/// so the round settles the moment the echo arrives: the receipt carries the
/// diagnostic, never a false `[clean]`, in ONE round. This directly demonstrates
/// the machinery ledger 04c's two-round seed shape stood in for.
///
/// Contention doctrine: no seed round, no `sleep` — `call_diagnostics` runs the
/// pipeline (settle + the bounded await) to completion, so the receipt is
/// authoritative. The declared window is the only wait, and it is finite.
#[test]
fn conformance_mockls_debounce() -> Result<()> {
    let dir = tempfile::tempdir().context("debounce tempdir")?;
    let file = dir.path().join("case.mockls-debounce");
    std::fs::write(&file, "echo hello\n").context("write debounce fixture")?;

    let lsp = mockls_lsp_arg("mockls-debounce", "");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("root")?)?;
    bridge.initialize()?;
    let file_str = file.to_str().context("file")?;

    // One round: never-heard at settle, the declared-constant gate awaits the
    // echo bounded by 300 ms, the version-echoing publish arrives inside the
    // bound, and the round settles on its arrival — no seed, no sleep.
    let receipt = bridge.call_diagnostics(file_str)?;
    bridge
        .shutdown_clean(SHUTDOWN_GRACE)
        .context("clean shutdown for persona `mockls-debounce`")?;

    assert!(
        receipt.contains("mock diagnostic"),
        "`mockls-debounce` declares a bounded debounce window — the version echo \
         must be awaited within the declared bound and collected in one round, \
         never lost to silence:\n{receipt}"
    );
    assert!(
        !receipt.contains("[clean]"),
        "the receipt must not render [clean] over a debounced publish awaited \
         within the bound:\n{receipt}"
    );
    eprintln!("conformance: `mockls-debounce` PASSED");
    Ok(())
}

/// `mockls-debounce` bound-expiry twin — the other half of the declared-constant
/// gate (diagnostics-debt 05). The SAME debounce persona, but the test pins an
/// explicit `--diagnostics-delay` past the declared 300 ms bound: the version echo
/// is scheduled to land AFTER the gate's window (declared window + dead-air slack)
/// closes. So the round must NOT hang waiting forever, and must NOT render a false
/// `[clean]` over the pending publish — bound expiry renders the fault-attribution
/// wording (the verified-contract-violation arm): the discipline said an answer was
/// owed this round and none came inside the bound.
///
/// Contention doctrine: the delay is generous-but-finite and the bound is finite,
/// so the round completes when the bound expires (well before the delayed publish),
/// never hangs. The subject is precisely the bound, so real time is sanctioned here.
#[test]
fn conformance_mockls_debounce_bound_expiry() -> Result<()> {
    let dir = tempfile::tempdir().context("debounce-expiry tempdir")?;
    let file = dir.path().join("case.mockls-debounce");
    std::fs::write(&file, "echo hello\n").context("write debounce-expiry fixture")?;

    // A publish scheduled 4 s out — comfortably past the ~1.8 s gate bound
    // (declared 300 ms window + dead-air slack), so the echo cannot land inside
    // the bound and the gate must expire to the fault, not wait for the publish.
    let lsp = mockls_lsp_arg("mockls-debounce", "--diagnostics-delay 4000");
    let mut bridge = BridgeProcess::spawn(&[&lsp], dir.path().to_str().context("root")?)?;
    bridge.initialize()?;
    let file_str = file.to_str().context("file")?;

    let receipt = bridge.call_diagnostics(file_str)?;
    bridge
        .shutdown_clean(SHUTDOWN_GRACE)
        .context("clean shutdown for persona `mockls-debounce` bound-expiry")?;

    // Bound expiry NEVER renders [clean] — the pending echo is not evidence.
    assert!(
        !receipt.contains("[clean]"),
        "bound expiry must never render [clean] over a pending debounced \
         publish:\n{receipt}"
    );
    // It renders the fault-attribution wording (the verified-contract-violation
    // arm): a debounce server whose echo missed the declared bound is a fault
    // attributed to the server, named + caused + actioned. The retired v1 shrug
    // stays unwritten.
    assert!(
        receipt.contains("did not answer for this round")
            && receipt.contains("treating as a server fault, re-run to retry"),
        "bound expiry must render the fault-attribution wording, naming the \
         server + evidenced cause + action:\n{receipt}"
    );
    assert!(
        !receipt.contains("publishes on its own schedule"),
        "the retired v1 shrug must never be written:\n{receipt}"
    );
    eprintln!("conformance: `mockls-debounce` bound-expiry PASSED");
    Ok(())
}

/// `mockls-scan` — scan discipline (marksman-class scan-once). The manifest
/// declares `discipline = "scan"`; the binary's bundle (`--workspace-diagnostics
/// --scan-roots`) answers one whole-workspace pull off the scanned model, so a
/// root-scoped diagnose reports the `DIRTY`-marked document. Silence would NOT be
/// clean (the fault floor's job), but here the scan reports the diagnostic, which
/// reaches the receipt. Uses the root-scoped diagnose (the whole-workspace pull's
/// entry point), not the per-file path.
#[test]
fn conformance_mockls_scan() -> Result<()> {
    let dir = tempfile::tempdir().context("scan tempdir")?;
    // The scan reports a document dirty only when its content carries `DIRTY`.
    std::fs::write(dir.path().join("case.mockls-scan"), "DIRTY marker line\n")
        .context("write scan fixture")?;

    let lsp = mockls_lsp_arg("mockls-scan", "");
    let root = dir.path().to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    let receipt = bridge.call_diagnostics_scoped(&[root])?;
    bridge
        .shutdown_clean(SHUTDOWN_GRACE)
        .context("clean shutdown for persona `mockls-scan`")?;

    assert!(
        receipt.contains("workspace diagnostic"),
        "`mockls-scan` declares scan discipline — the one workspace pull must \
         report the DIRTY document:\n{receipt}"
    );
    eprintln!("conformance: `mockls-scan` PASSED");
    Ok(())
}

/// `mockls-diff` — diff discipline (marksman diff-only). The manifest declares
/// `discipline = "diff"`; the binary's bundle (`--diagnostics-on-save
/// --advertise-save --reject-pull`) publishes only on the save event the editing
/// batch triggers — never on the ambient didOpen the eager health probe fires —
/// and answers no pull. The saved file's single publish is the sole channel and
/// reaches the receipt; a never-saved file would stay silent (silence is NOT
/// clean — the fault floor's job, ledger 05).
#[test]
fn conformance_mockls_diff() -> Result<()> {
    let receipt = persona_receipt("mockls-diff", "", "echo hello\n")?;
    assert!(
        receipt.contains("mock diagnostic"),
        "`mockls-diff` declares diff discipline — its save-triggered publish must \
         reach the receipt:\n{receipt}"
    );
    eprintln!("conformance: `mockls-diff` PASSED");
    Ok(())
}

/// `mockls-violator` — the violating twin (ledger 05's fault-attribution dummy).
/// The manifest row DECLARES the push contract (`declares_push = true`, like
/// `mockls-declared`), but the binary's bundle (`--no-push-diagnostics
/// --reject-pull`) WITHHOLDS — it never publishes and answers no pull, breaking
/// its own contract. Its discipline OWED an answer this round (a declared-push
/// server) and gave none, so bound expiry renders the verified-contract-violation
/// arm of the fault floor (diagnostics-debt 05): the DESIGN's exact wording,
/// naming the server + evidenced cause + action, and the round strikes the same
/// ledger a crashing server feeds. NEVER a false `[clean]`.
#[test]
fn conformance_mockls_violator() -> Result<()> {
    let receipt = persona_receipt("mockls-violator", "", "echo hello\n")?;
    // The fault-attribution wording (the DESIGN's exact phrasing): the discipline
    // required a response, none came, so it is treated as a server fault.
    assert!(
        receipt.contains("mockls-violator did not answer for this round")
            && receipt.contains("its verified behavior requires a response")
            && receipt.contains("treating as a server fault, re-run to retry"),
        "`mockls-violator` declares push but withholds — the violation must render \
         the fault-attribution wording (server + cause + action):\n{receipt}"
    );
    assert!(
        !receipt.contains("[clean]"),
        "a contract violation must never render [clean] over absence:\n{receipt}"
    );
    // The retired v1 shrug ("publishes on its own schedule") stays unwritten.
    assert!(
        !receipt.contains("publishes on its own schedule"),
        "the retired v1 shrug must never be written:\n{receipt}"
    );
    eprintln!("conformance: `mockls-violator` PASSED");
    Ok(())
}

/// `mockls-pending` — the created-token-pending shape (misc 200, the elm
/// cold-download trace). The manifest declares `discipline = "event"` plus the
/// verified `declares_progress` leg; the binary's bundle
/// (`--create-delayed-begin`) announces a work-done token via
/// `window/workDoneProgress/create` at init, goes quiet for the gap, then opens
/// and closes the `$/progress` bracket — DROPPING any didOpen/didSave that lands
/// inside the gap (elm's `NoWorkspaceContainsError`, never re-pushed).
///
/// **The red/green pin.** Pre-fix, Catenary acks the create and discards it, so
/// the lifecycle arms only on `begin`: both batch settle seams sample the quiet
/// tree, release inside the gap, and the didOpen/didSave land mid-init where the
/// persona drops them — the file resolves absent (no publish ever arrives),
/// never a real diagnostic. Post-fix, the created token holds settle in the
/// Pending state across the create→begin gap (the same Stuck ceiling a hung
/// Busy bracket rides), so the stimulus lands AFTER the bracket, the persona
/// publishes, and the diagnostic reaches the receipt. This leg asserts the
/// green: the mock diagnostic is collected, proving the batch held through the
/// gap.
///
/// Contention doctrine: `call_diagnostics` runs the pipeline (settle + collect)
/// to completion, so the receipt is authoritative; the gap is the only wait and
/// it is finite.
#[test]
fn conformance_mockls_pending() -> Result<()> {
    let receipt = persona_receipt("mockls-pending", "", "echo hello\n")?;
    assert!(
        receipt.contains("mock diagnostic"),
        "`mockls-pending` announces a work-done token before its download gap — \
         the created token must hold both settle seams across the create\u{2192}begin \
         gap so the stimulus lands post-init and the publish reaches the receipt \
         (post-fix green; pre-fix the settle released in the gap and the file read \
         absent):\n{receipt}"
    );
    assert!(
        !receipt.contains("[clean]"),
        "the held batch must collect the real diagnostic, never a settle-released \
         [clean] over a dropped mid-init stimulus:\n{receipt}"
    );
    eprintln!("conformance: `mockls-pending` PASSED");
    Ok(())
}

/// `mockls-scan` withholding twin — the SCAN floor arm (misc 196). The scan
/// persona owes its WHOLE-WORKSPACE answer: the `workspace/diagnostic` pull is its
/// contractual trigger. Here the persona bundle's workspace-diagnostic serving is
/// overridden by `--fail-on workspace/diagnostic`, so the ALIVE scan server refuses
/// the pull it owes. That refusal must be DETECTED as a verified-contract violation
/// — the fault-attribution wording, naming the server + cause + action — never a
/// `[clean]` collapse over the genuinely-unscanned root. Uses the root-scoped
/// diagnose (the whole-workspace pull's entry point), mirroring `conformance_mockls_scan`.
#[test]
fn conformance_mockls_scan_withholding_is_a_fault() -> Result<()> {
    let dir = tempfile::tempdir().context("scan-withhold tempdir")?;
    // A dirty document: were the pull served it would report `DIRTY`. The refusal
    // means the round has NO answer for it — which must not read `[clean]`.
    std::fs::write(dir.path().join("case.mockls-scan"), "DIRTY marker line\n")
        .context("write scan-withhold fixture")?;

    // The scan persona, but its owed workspace pull is refused by an alive server.
    let lsp = mockls_lsp_arg("mockls-scan", "--fail-on workspace/diagnostic");
    let root = dir.path().to_str().context("root")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    let receipt = bridge.call_diagnostics_scoped(&[root])?;
    bridge
        .shutdown_clean(SHUTDOWN_GRACE)
        .context("clean shutdown for persona `mockls-scan` withholding")?;

    // A refused workspace answer NEVER renders [clean] over the unscanned root.
    assert!(
        !receipt.contains("[clean]"),
        "a scan server refusing its owed workspace pull must never render \
         [clean]:\n{receipt}"
    );
    // It renders the fault-attribution wording (the verified-contract-violation
    // arm), naming the server + evidenced cause + action.
    assert!(
        receipt.contains("mockls-scan did not answer for this round")
            && receipt.contains("its verified behavior requires a response")
            && receipt.contains("treating as a server fault, re-run to retry"),
        "a scan server's refused workspace pull must render the fault-attribution \
         wording (server + cause + action):\n{receipt}"
    );
    assert!(
        !receipt.contains("publishes on its own schedule"),
        "the retired v1 shrug must never be written:\n{receipt}"
    );
    eprintln!("conformance: `mockls-scan` withholding PASSED");
    Ok(())
}

/// `mockls-diff` withholding twin — the DIFF floor arm (misc 196). The diff persona
/// owes a publish on any round that DELIVERED its trigger; our editing lifecycle
/// always sends `didSave` for a changed file, so the save is delivered. Here the
/// persona bundle's save-triggered publish is suppressed by `--no-push-diagnostics`,
/// so the ALIVE diff server receives its trigger and stays silent — a violation of
/// its contract. That must be DETECTED as a verified-contract violation, never a
/// `[clean]` collapse. (The complementary "no trigger ⇒ owes nothing" boundary is a
/// unit-level fact — a diff round that delivered no save is not a fault — pinned by
/// the profile's round-conditional design; this leg proves the delivered-trigger
/// violation end to end.)
#[test]
fn conformance_mockls_diff_withholding_is_a_fault() -> Result<()> {
    // The diff persona, but its save-triggered publish is withheld. The editing
    // lifecycle's didSave IS delivered (the file is written fresh), so the diff
    // server owes a publish and gives none.
    let receipt = persona_receipt("mockls-diff", "--no-push-diagnostics", "echo hello\n")?;
    assert!(
        !receipt.contains("[clean]"),
        "a diff server silent after its delivered save trigger must never render \
         [clean]:\n{receipt}"
    );
    assert!(
        receipt.contains("mockls-diff did not answer for this round")
            && receipt.contains("its verified behavior requires a response")
            && receipt.contains("treating as a server fault, re-run to retry"),
        "a diff server's silence after a delivered save must render the \
         fault-attribution wording (server + cause + action):\n{receipt}"
    );
    assert!(
        !receipt.contains("publishes on its own schedule"),
        "the retired v1 shrug must never be written:\n{receipt}"
    );
    eprintln!("conformance: `mockls-diff` withholding PASSED");
    Ok(())
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

/// The macOS `$TMPDIR` prefix-alias spelling, reproduced with a symlink on any
/// platform (the `ee12779` macOS CI red). The session speaks an ALIAS spelling
/// of the fixture root — `CATENARY_ROOTS` and the scoped serve's named file —
/// while the daemon's fs roots canonicalize to the real one. The serve's
/// on-demand spawn (`ensure_clients_for_paths`) received the raw spelling,
/// missed the canonical roots, and ensured nothing; the coverage lookup then
/// read a cold registry and answered `[no LSP coverage]`. The fixture file is
/// created AFTER the bridge boots so the boot-time `spawn_all` cannot mask the
/// miss (it did on fast hosts — the Linux legs stayed green on timing luck;
/// macOS's slower spawn lost the race). Real-server shaped deliberately: mockls
/// routes via `CATENARY_SERVERS` and never exercised the miss.
#[test]
fn conformance_clangd_aliased_root_spelling() -> Result<()> {
    let Some(case) = lookup("clangd") else {
        bail!("no conformance case for `clangd`");
    };
    if !on_path(case.server) {
        eprintln!(
            "conformance: skipping `{} (aliased root)` — binary not on PATH",
            case.server
        );
        return Ok(());
    }

    // The alias sits on an ANCESTOR (macOS aliases `/var`, not the tempdir
    // itself): the fixture root is a REAL directory reached through an aliased
    // prefix, so the root path is not a symlink — only its spelling is.
    let base = common::KeepOnPanic::new(tempfile::tempdir().context("create alias base")?);
    let real = base.path().join("real");
    std::fs::create_dir(&real).context("create real prefix")?;
    let alias = base.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).context("create alias symlink")?;
    let root = alias.join("fixture-root");
    std::fs::create_dir(&root).context("create fixture root via alias")?;

    // Boot against the EMPTY root: no .c file exists yet, so no boot-time
    // clangd spawn can win the race for the serve below.
    let mut bridge = BridgeProcess::spawn_conformance(&root)?;
    bridge.initialize()?;

    // Now the fixture arrives, spelled through the alias. The scoped serve's
    // ensure-spawn leg is the only thing that can bring clangd up for it.
    let src = fixture_dir(case);
    copy_dir(&src, &root)?;
    let file = root.join(case.file);

    let receipt = run_settle_diagnostics(&bridge, &file, CONFORMANCE_WALL_BOUND)
        .context("aliased-root settle")?;
    assert!(
        !receipt.contains("[no LSP coverage]"),
        "the aliased spelling lost coverage the canonical spelling has:\n{receipt}"
    );
    assert!(
        receipt.contains("broken.c"),
        "receipt does not diagnose the fixture file:\n{receipt}"
    );

    bridge.shutdown_clean(Duration::from_secs(10))?;
    Ok(())
}

// ── gopls pull-first gate (diagnostics-debt 05, re-enabling bug 87) ────
//
// Bug 87 forced gopls's `pullDiagnostics` OFF because its pull-mode empty
// placeholder pushes were read as authoritative heard-empty, defeating the pull
// that would fetch the real results. Ledger 03's version-echo settlement retired
// that defeat structurally (an unversioned placeholder echoes no version → settles
// nothing), so ledger 05 re-enables the pull: gopls's manifest discipline is now
// `pull` and `pullDiagnostics = false` is lifted. That re-enable ships ONLY behind
// this conformance gate proving pull-first collection against the pin.
//
// gopls now advertises `diagnosticProvider`, so Catenary's retrieval takes the
// pull path (`supports_pull_diagnostics()` → `pull_settling`); a green diagnose of
// the go fixture's intentional type error is therefore pull-first collection. This
// is a skip-if-binary-missing sentinel like clangd: where gopls is on PATH it runs
// (and gates the re-enable on this host), and a host lacking it stays green — the
// CI matrix's `CATENARY_CONFORMANCE=gopls` job requires the binary against the pin.
#[test]
fn conformance_gopls_pull_mode() -> Result<()> {
    sentinel("gopls")
}

// ── ts-ls declared-constant gate (diagnostics-debt 05, un-exemption) ───
//
// typescript-language-server was `conformance = false` (tui-rework 13): a
// push-only publisher whose first didOpen/didSave diagnostic lands on an internal
// debounce timer AFTER the process has gone scheduler-idle, past the
// settle-then-collect window — a deterministic false `[clean]`. Ledger 05's
// declared-constant gate closes it: with `discipline = "debounce"` and
// `debounce_ms = 850` on the pin, the retrieval evidence bar arms from turn zero
// and AWAITS the version echo bounded by the declared constant, so the debounced
// publish is collected rather than lost to silence. This sentinel is
// skip-if-binary-missing like gopls/clangd; where ts-ls is on PATH it gates the
// un-exemption on this host, and the CI matrix's `CATENARY_CONFORMANCE=typescript-language-server`
// job requires the binary against the pin.
#[test]
fn conformance_typescript_ls() -> Result<()> {
    sentinel("typescript-language-server")
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

    use catenary_cli::recipes::{
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

/// The macOS platform leg's honesty guard (misc 164):
/// `defaults/ci-provision-macos.toml` must PARTITION the Linux-conformed set.
/// Every server the Linux matrix conforms appears there exactly once — either
/// brew-provisioned (`kind = "homebrew"` + `formula`), a neutral-kind reference
/// (`linux-recipe` / `linux-provision`) that resolves against the Linux source,
/// or an explicit `skip = true` with an honest `note` — so a server the macOS
/// matrix does not prove is never silently absent, and no stanza names a server
/// the Linux matrix does not run. `tools/conformance_matrix.py --platform macos`
/// applies the identical check in the discover job, so the guard and the CI
/// matrix agree by construction (the drift-guard idiom above). Every
/// macOS-provisioned server has a `CASES` entry for free: this set is a subset
/// of the conformed set, which [`matrix_and_cases_have_no_drift`] already
/// covers.
#[test]
fn macos_provisioning_partitions_the_conformed_set() {
    use std::collections::BTreeSet;

    use catenary_cli::recipes::{conformed_server_names, default_provisioning, default_recipes};

    let doc: toml::Value = toml::from_str(include_str!("../defaults/ci-provision-macos.toml"))
        .expect("ci-provision-macos.toml parses");
    let stanzas = doc
        .get("provision")
        .and_then(toml::Value::as_table)
        .expect("a [provision.*] table");

    let recipes = default_recipes().expect("default recipes parse");
    let provisions = default_provisioning().expect("default provisioning parse");
    let conformed: BTreeSet<String> = conformed_server_names(&recipes, &provisions)
        .into_iter()
        .collect();
    let macos: BTreeSet<String> = stanzas.keys().cloned().collect();

    let silently_absent: Vec<&String> = conformed.difference(&macos).collect();
    assert!(
        silently_absent.is_empty(),
        "macOS platform drift: these Linux-conformed servers are silently absent from \
         defaults/ci-provision-macos.toml (add a homebrew stanza, a \
         linux-recipe/linux-provision reference, or an explicit `skip = true` with a \
         note): {silently_absent:?}"
    );
    let orphans: Vec<&String> = macos.difference(&conformed).collect();
    assert!(
        orphans.is_empty(),
        "macOS platform drift: these ci-provision-macos.toml stanzas name servers the \
         Linux matrix does not conform: {orphans:?}"
    );

    for (name, stanza) in stanzas {
        let stanza = stanza.as_table().expect("stanza is a table");
        let skip = stanza
            .get("skip")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        if skip {
            let note = stanza
                .get("note")
                .and_then(toml::Value::as_str)
                .unwrap_or("");
            assert!(
                !note.trim().is_empty(),
                "macOS skip for `{name}` carries no honest `note` — an exclusion must \
                 state why there is no viable provisioning path"
            );
            continue;
        }
        let kind = stanza.get("kind").and_then(toml::Value::as_str);
        assert!(
            matches!(kind, Some("homebrew" | "linux-recipe" | "linux-provision")),
            "macOS provision `{name}` has kind `{kind:?}` — must be one of \
             \"homebrew\" / \"linux-recipe\" / \"linux-provision\" (maintainer ruling, \
             misc 164)"
        );
        match kind {
            Some("homebrew") => {
                let formula = stanza
                    .get("formula")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("");
                assert!(
                    !formula.trim().is_empty(),
                    "macOS provision `{name}` names no `formula`"
                );
            }
            // A neutral-kind stanza REFERENCES this server's Linux source so a
            // Linux pin bump cannot diverge; the reference is the stanza key.
            Some("linux-recipe") => {
                assert!(
                    recipes.contains_key(name),
                    "macOS provision `{name}` is `linux-recipe` but names no recipe in \
                     defaults/recipes.toml"
                );
                // lsm 04: a `binary` recipe pins per-platform artifacts, which
                // are not platform-neutral — a Linux artifact can never conform
                // macOS, so the reference kind is refused (mirrors
                // tools/conformance_matrix.py `validate_macos`).
                assert_ne!(
                    recipes[name].ecosystem,
                    catenary_cli::recipes::Ecosystem::Binary,
                    "macOS provision `{name}` is `linux-recipe` but the Linux recipe is \
                     ecosystem `binary` — use a homebrew stanza or an explicit skip"
                );
            }
            // The only remaining valid kind (the assert above ruled out any
            // other), so no catch-all `panic!` arm is needed (`clippy::panic` is
            // denied in this test file).
            _ => assert!(
                provisions.contains_key(name),
                "macOS provision `{name}` is `linux-provision` but names no stanza in \
                 defaults/ci-provision.toml"
            ),
        }
    }
}
