# Catenary Agent Context

This file serves as the single point of truth for AI agents (Claude, Gemini, etc.) working on the Catenary project.

## Project Grounding

- **Project Goal:** Multi-surface intelligence router — LSP-powered code intelligence for AI agents.
- **Repository:** `TwoWells/Catenary` on GitHub.
- **Config:** `@./Cargo.toml`
- **Dependency Policy:** `@./deny.toml`
- **Documentation:** `docs/src/`

## How Catenary Works

Catenary is a multi-surface intelligence router. A single daemon manages a pool
of LSP servers and exposes them through four decoupled interfaces: MCP (queries),
hooks (enforcement), CLI (editing lifecycle), and TUI (observability). All
surfaces share the same LSP server pool. None depends on the others.

### Core concepts

- **Daemon:** A single Catenary process per host. Multiple agents connect via
  Unix domain socket. LSP servers are shared across all sessions. See
  `src/session.rs`.
- **Session:** A connected agent. Each session has a unique ID (opaque string)
  and one or more workspace roots. Sessions are discoverable via `catenary list`
  and monitorable via `catenary monitor <id>`.
- **Database:** All state (sessions, events, workspace roots) is stored in
  `~/.local/state/catenary/catenary.db` (SQLite with WAL mode). See `src/db.rs`.
- **CLI search commands:** `catenary grep` and `catenary glob`, invoked via the
  host's shell tool. Stateless queries — no session identity, no connection
  binding. Each command connects to the daemon over a Unix domain socket,
  delegates to one or more LSP servers, and prints results to stdout.
- **Hooks:** Catenary registers a single `PreToolUse` hook per host (Claude Code,
  Gemini CLI, Antigravity CLI). This hook handles editing enforcement, command
  filtering, and file tracking. Hook definitions live in
  `plugins/catenary/hooks/hooks.json` (Claude Code), `hooks/hooks.json`
  (Gemini CLI), and `plugins/catenary-antigravity/hooks.json` (Antigravity CLI).
- **CLI commands:** Editing lifecycle invoked via the host's shell tool:
  - Editing starts implicitly on the first edit — there is no explicit start
    step (`catenary editing start` remains as an idempotent no-op).
  - `catenary diagnostics` — exit editing mode, print LSP diagnostics for all
    modified files to stdout, and clear the tracked set.
  - `catenary roots add <path>` / `catenary roots rm <path>` — manage workspace
    roots.
- **Diagnostics:** `catenary diagnostics` triggers the diagnostics pipeline:
  batch all modified files, send to LSP servers, collect diagnostics, print to
  stdout. The agent sees diagnostics in the shell tool output. Diagnostic events
  are stored in the SQLite database for later querying via `catenary debug query`.
- **Logging:** `LoggingServer` is a `tracing_subscriber::Layer` that subscribes
  to every tracing event and dispatches to multiple sinks: notification queue
  (user-facing `systemMessage`), protocol DB (LSP/MCP/hook messages), and trace
  DB (non-protocol events). See `src/logging/mod.rs` and
  `docs/src/tracing-conventions.md`.

### Key source files

- `src/db.rs` — SQLite connection management, schema creation, and migrations.
- `src/logging/mod.rs` — `LoggingServer`: multi-sink tracing Layer, the sole
  telemetry port/adapter. Dispatches to notification queue, protocol DB, and
  trace DB sinks.
- `src/session.rs` — session lifecycle and event broadcasting.
- `docs/src/` — full documentation source.

## Coding Standards

- **Edition:** Rust 2024.
- **Safety:** `unsafe` code is strictly forbidden (`forbid(unsafe_code)`).
- **Error Handling:** Use `anyhow` for application logic and `thiserror` for library errors.
- **Strict Denials:** Do NOT use `unwrap()`, `panic!()`, `todo!()`, `unimplemented!()`, `dbg!()`, `println!()`, or `eprintln!()`. Use proper error handling and the `tracing` crate for logging. `expect()` is denied in production code but allowed in `#[cfg(test)]` modules — prefer `expect("reason")` over `anyhow` workarounds in tests.
- **Tracing:** `warn!()` and `error!()` events reach the user-notification queue by default. Only use these levels for user-relevant, actionable conditions. Internal diagnostics belong at `info!()` or `debug!()`. See `docs/src/tracing-conventions.md` for severity guidelines, reserved structured fields, and the `source` taxonomy.
- **Imports:** No wildcard imports (`use crate::*`).
- **Formatting:** Code must be formatted with `rustfmt`.
- **Linting:** Must pass `cargo clippy` with `pedantic`, `nursery`, and `cargo` groups enabled.
- **Dependencies:** Must pass `cargo machete` (no unused dependencies).

## Quality Standards

- **License Compliance:** All new dependencies MUST have permissive licenses (MIT, Apache-2.0, etc.) as specified in `@./deny.toml`. Catenary is dual-licensed under AGPL-3.0-or-later and a commercial license.
- **Documentation:** All public APIs must have documentation comments.
- **Testing:**
  - All new features must include tests.
  - Integration tests in `tests/` often require real LSP servers (e.g., `rust-analyzer`).
  - Integration test subprocesses (bridge, `catenary install`, etc.) must call `isolate_env(&mut cmd, root)` **before** setting any `CATENARY_*` env vars. `isolate_env` clears all inherited `CATENARY_*` vars, then points each XDG base dir at a **distinct** subdir of the tempdir root: `XDG_CONFIG_HOME` → `<root>/config`, `XDG_STATE_HOME` → `<root>/state`, `XDG_DATA_HOME` → `<root>/data`, `XDG_RUNTIME_DIR` → `<root>/runtime`. Keeping the bases distinct makes `isolate_env` a mislocation detector — code that writes under the wrong base no longer silently lands in one shared dir. Callers then set `CATENARY_SERVERS`, `CATENARY_ROOTS`, or `CATENARY_CONFIG` after the call — these overwrite the cleared values. Test-side code that resolves a daemon path (socket, DB, log under `db::state_dir()`) or writes a config the subprocess reads must derive it through `common::xdg_state_home(root)` / `common::xdg_config_home(root)` so both sides agree. Without `isolate_env`, subprocesses inherit the user's shell environment, writing to `~/.config`, `~/.local/state`, or `~/.local/share` and causing races between parallel tests and across worktrees.

## Development Commands

- **Check (full):** `make check` — format, lint, deny, machete, and test in one pass.
- **Test (all):** `make test`
- **Test (filtered):** `make test T=<filter>` — run only tests matching the filter (e.g., `make test T=json_diagnostics`).
- **Test (repeat):** `make test T=<filter> N=<count>` — stress-test by repeating N times (e.g., `make test T=flaky_test N=5`).
- **Mutation testing:** `make mutants` — pre-release only. Pass `T=<module>` to scope (e.g., `make mutants T=command_filter`).
- **Stop mutation testing:** `make mutants-stop` — kills cargo-mutants and all child processes. **Never use `pkill cargo-mutants`** — it kills only the parent, orphaning test binaries that run without timeout or memory limits. An orphaned mutant test binary caused a 41.8GB OOM that crashed the GPU driver.

## Release Workflow

Versioning and releases are managed via the `Makefile`.

- **Patch Release:** `make release-patch` (e.g., 0.1.0 -> 0.1.1)
- **Minor Release:** `make release-minor` (e.g., 0.1.0 -> 0.2.0)
- **Major Release:** `make release-major` (e.g., 0.1.0 -> 1.0.0)
- **Custom Version:** `make release V=x.y.z`

These commands automatically:

1. Verify the working tree is clean and on `main`.
2. Run `cargo update` to ensure `Cargo.lock` is fresh.
3. Bump versions in `Cargo.toml` and `.claude-plugin/marketplace.json`.
4. Run all tests and linting checks.
5. Commit the changes and create a git tag.

To complete the release, push the changes and tags:
`git push && git push --tags`

Pushing the tag triggers the CD workflow (binary builds + crates.io
publish) and a docs rebuild. The docs workflow builds stable docs
from the latest `v*` tag, so any docs changes on `main` only reach
`/stable/` after a tagged release. Dev docs at `/dev/` update on
every push to `main`.

### Pre-release checklist

Before running `make release-*`:

1. Ensure `git push` has been run so local `main` matches `origin/main`.
2. Run `make mutants` and address any surviving mutants (for major releases).

If checks or the commit fail, the Makefile automatically rolls back
the version bump — it is safe to re-run `make release-*` after fixing
the issue.
