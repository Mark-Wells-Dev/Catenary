# Architecture

Catenary is a multi-surface intelligence router. A single daemon manages
a pool of LSP servers and exposes them through four decoupled interfaces:
MCP (connection), hooks (enforcement), CLI (queries and editing
lifecycle), and TUI (observability). All interfaces share the same LSP
server pool. None depends on the others.

## Four surfaces

Every external interaction crosses one of four boundaries:

- **CLI** — agent ↔ Catenary. The agent invokes CLI commands via the
  host's shell tool: `catenary grep`, `catenary glob` for search;
  `catenary diagnostics` for the batched diagnostic report (editing is
  tracked implicitly — the first edit starts it, there is no start step).
  Mass edits go through native `sed -i` — the host writes and the hook
  resolves and tracks the write-set. Commands connect to the daemon over a
  Unix domain socket, send a request, and print the result to stdout.
- **MCP** — agent ↔ Catenary. A pure connection surface for session
  management and workspace root discovery. No application-level tools.
- **LSP** — Catenary ↔ language servers. Catenary spawns and manages
  language server processes, sending requests and receiving
  notifications over JSON-RPC stdio.
- **Hooks** — host CLI ↔ Catenary. The host CLI (Claude Code, Antigravity
  CLI) fires hooks at lifecycle boundaries (pre-tool, post-agent,
  session start/end). Hook processes connect to the daemon's
  IPC socket and exchange JSON messages.

## Multiplexing

A single Catenary session can manage multiple language servers across
multiple workspace roots. Files route to the right server(s) based on
language detection, configuration, and server capabilities. A Rust
file goes to rust-analyzer; a TypeScript file goes to
typescript-language-server. If a language has multiple configured
servers, Catenary dispatches to all of them and merges results.

## Hexagonal structure

Catenary follows a port/adapter pattern. Three boundary components own
all protocol logging:

- **`McpServer`** — MCP protocol adapter. Handles the MCP lifecycle
  (initialize, roots, ping) over JSON-RPC. No application-level tools —
  grep and glob are served via CLI commands over the IPC socket.
- **`LspClient`** — LSP protocol adapter. One instance per language
  server process. Manages the JSON-RPC connection, document state, and
  capability negotiation.
- **`HookServer`** — Hook protocol adapter. Listens on an IPC socket,
  dispatches hook requests, returns responses.

**`LoggingServer`** is the telemetry port. It is a `tracing` Layer
that dispatches every event to its sinks: the notification queue (for
user-facing `systemMessage` delivery) and the append-only JSONL telemetry
firehose (the full audit trail, read by `catenary query`). Warn/error
events additionally feed the alert ring of the daemon-owned `state.json`
snapshot. Every protocol message flows through it.

Application servers (`GrepServer`, `GlobServer`, `DiagnosticsServer`)
are the transformation layer. They receive application-level parameters
from the IPC router, do work using `LspClient`, and return results.
They do not log protocol messages — that is the boundary components'
job. An application server is a black box: the protocol messages that
went in and came out are linked by `parent_id` in the firehose records.

## Component diagram

```
                ┌─────────────────────────────────────────────────────┐
                │                    Catenary daemon                  │
                │                                                     │
Agent ◄──CLI──► │  IPC router ──► GrepServer / GlobServer /           │
  (grep, glob,  │                  DiagnosticsServer                  │
   diagnostics) │                       │                             │
                │                 LspClientManager                    │
                │                 ┌─────┴──────┐                     │
                │            LspClient    LspClient                  │
                │                 │            │                      │
                └─────────────────┼────────────┼──────────────────────┘
                                  │            │
                             LSP (stdio)  LSP (stdio)
                                  │            │
                           rust-analyzer  typescript-
                                          language-server

Agent ◄──MCP──► McpServer (session management, roots discovery, ping)

Host CLI ◄──IPC──► HookServer ──► HookRouter (editing enforcement,
  (hooks)                          command filtering, file tracking)

LoggingServer (tracing Layer) ─── dispatches all events to sinks:
  ├── notification queue     (user-facing systemMessage)
  └── JSONL firehose         (append-only audit trail, read by catenary query)
```

## Shared infrastructure

- **`Session`** — application container. Owns application servers, the
  client manager, filesystem manager, editing state, path validation,
  and logging. Protocol boundaries hold `Arc<Session>`.
- **`FilesystemManager`** — file classification and root resolution.
  Single authority for language detection, shebang parsing, and
  workspace root membership. Also owns the per-root mtime baselines
  behind walk-on-demand change detection — there is no background
  watcher; the walk a query already performs feeds a precise per-server
  `workspace/didChangeWatchedFiles` changed-set (see [Document Lifecycle
  & File Watching](documents.md)).
- **`LspClientManager`** — LSP server lifecycle. Spawns, caches, and
  shuts down `LspClient` instances. Manages instance keying (language,
  server name, scope), multi-server routing, document lifecycle, and
  workspace folder synchronization.
- **State & storage** — there is no primary database. Live state (the
  session and server boards, recent alerts and activity) lives in a
  daemon-owned `state.json` snapshot under `runtime_dir`, rewritten on
  change; the full protocol and trace history streams to an append-only,
  sharded JSONL firehose under `cache_dir`, read by `catenary query`. The
  durable Unix sockets live under `state_dir`. A legacy `catenary.db` from
  older versions is deleted on daemon startup. See `src/paths.rs`.

## Topic pages

- [Session Lifecycle](session-lifecycle.md) — startup, serving, root
  addition, and shutdown.
- [Configuration Model](configuration.md) — config sources, layering,
  language/server split, project config.
- [Routing & Dispatch](routing.md) — file classification, instance
  keying, multi-server dispatch.
- [LSP Client Layer](lsp-client.md) — connection, server, client,
  capabilities, settle/idle detection.
- [Document Lifecycle & File Watching](documents.md) — document sync,
  editing mode, file watcher notifications.
- [Logging, Hooks & TUI](logging-hooks-tui.md) — tracing pipeline,
  hook integration, monitor dashboard.
