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
of LSP servers and exposes them through four decoupled surfaces: the **CLI**
(queries, search, and the editing lifecycle — `catenary grep`/`glob`/`sed`/
`diagnostics`, invoked via the host shell tool), **hooks** (editing enforcement,
command filtering, file tracking), the **MCP server** (the protocol handshake
plus the workspace-roots channel and user-facing notifications — it advertises no
query tools; `handle_request` serves only `initialize`/`ping`), and the **TUI**
(observability). All surfaces share the same LSP server pool. None depends on the
others.

### Core concepts

- **Daemon:** A single Catenary process per host. Multiple agents connect via
  Unix domain socket. LSP servers are shared across all sessions. See
  `src/router.rs` (`SessionManager`).
- **Session:** A connected agent. Each session has a unique ID (opaque string)
  and one or more workspace roots. Live sessions appear on the `state.json`
  session board (rendered by the `catenary` TUI dashboard); their telemetry is
  queryable via `catenary query --session <id>`.
- **State & storage:** There is no primary SQLite database (a legacy
  `catenary.db` is drained on startup). State spans three XDG base dirs, chosen for
  their durability semantics (see `src/paths.rs`): the durable Unix **socket**
  under `state_dir`; the ephemeral `state.json` **session-board snapshot** and
  per-session diagnostics-overflow reports under `runtime_dir` (tmpfs); and the
  regenerable, per-root-sharded **JSONL telemetry firehose** under `cache_dir`.
- **CLI search commands:** `catenary grep` and `catenary glob`, invoked via the
  host's shell tool. Stateless queries — no session identity, no connection
  binding. Each command connects to the daemon over a Unix domain socket,
  delegates to one or more LSP servers, and prints results to stdout.
- **Hooks:** Catenary registers several lifecycle hooks per host (Claude Code,
  Gemini CLI, Antigravity CLI); the `PreToolUse` hook handles editing enforcement,
  command filtering, and file tracking. Hook definitions (the full per-host set)
  live in
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
  are also emitted to the JSONL telemetry firehose.
- **Logging:** `LoggingServer` is a `tracing_subscriber::Layer` that subscribes
  to every tracing event and dispatches structured events to two sinks: the
  notification queue (user-facing `systemMessage`) and the per-root-sharded JSONL
  telemetry firehose (`src/logging/jsonl_sink.rs`). See `src/logging/mod.rs` and
  `docs/src/tracing-conventions`.

### Key source files

- `src/paths.rs` — XDG base-dir resolvers (durable `state_dir` / ephemeral
  `runtime_dir` / regenerable `cache_dir`) and the firehose shard-key encoding.
- `src/logging/mod.rs` — `LoggingServer`: multi-sink tracing Layer, the sole
  telemetry port/adapter. Dispatches to the notification queue and the JSONL
  firehose (`src/logging/jsonl_sink.rs`) sinks.
- `src/router.rs` — the daemon: `SessionManager`/session lifecycle, the
  `RootTracker`, and hook dispatch. Per-session state lives in
  `src/bridge/session.rs`.
- `docs/src/` — full documentation source.

## Coding Standards

- **Edition:** Rust 2024.
- **Safety:** `unsafe` code is strictly forbidden (`forbid(unsafe_code)`).
- **Error Handling:** Use `anyhow` for application logic and `thiserror` for library errors.
- **Strict Denials:** Do NOT use `unwrap()`, `panic!()`, `todo!()`, `unimplemented!()`, `dbg!()`, `println!()`, or `eprintln!()`. Use proper error handling and the `tracing` crate for logging. `expect()` is denied in production code but allowed in `#[cfg(test)]` modules — prefer `expect("reason")` over `anyhow` workarounds in tests.
- **Tracing:** `warn!()` and `error!()` events reach the user-notification queue by default. Only use these levels for user-relevant, actionable conditions. Internal diagnostics belong at `info!()` or `debug!()`. See `docs/src/tracing-conventions` for severity guidelines, reserved structured fields, and the `source` taxonomy.
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
  - Integration test subprocesses (bridge, `catenary install`, etc.) must call `isolate_env(&mut cmd, root)` **before** setting any `CATENARY_*` env vars. `isolate_env` clears all inherited `CATENARY_*` vars, then points each XDG base dir at a **distinct** subdir of the tempdir root: `XDG_CONFIG_HOME` → `<root>/config`, `XDG_STATE_HOME` → `<root>/state`, `XDG_DATA_HOME` → `<root>/data`, `XDG_RUNTIME_DIR` → `<root>/runtime`. Keeping the bases distinct makes `isolate_env` a mislocation detector — code that writes under the wrong base no longer silently lands in one shared dir. Callers then set `CATENARY_SERVERS`, `CATENARY_ROOTS`, or `CATENARY_CONFIG` after the call — these overwrite the cleared values. Test-side code that resolves a daemon path (socket, snapshot, or firehose log under `paths::state_dir()`) or writes a config the subprocess reads must derive it through `common::xdg_state_home(root)` / `common::xdg_config_home(root)` so both sides agree. Without `isolate_env`, subprocesses inherit the user's shell environment, writing to `~/.config`, `~/.local/state`, or `~/.local/share` and causing races between parallel tests and across worktrees.

## Building

All build, test, lint, and release tasks run through the `Makefile` — e.g.
`make check` (format + clippy + deny + machete + test in one pass). The `Makefile`
targets and `.github/workflows/` (`ci.yml`, `cd.yml`) are the source of truth for
the build / CI / release surface; this document does not duplicate them.
