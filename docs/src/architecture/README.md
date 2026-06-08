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
  `catenary sed` for tracked mass edits; `catenary diagnostics` for the
  batched diagnostic report (editing is tracked implicitly — the first
  edit starts it, there is no start step). Commands connect to the daemon
  over a Unix domain socket, send a request, and print the result to
  stdout.
- **MCP** — agent ↔ Catenary. A pure connection surface for session
  management and workspace root discovery. No application-level tools.
- **LSP** — Catenary ↔ language servers. Catenary spawns and manages
  language server processes, sending requests and receiving
  notifications over JSON-RPC stdio.
- **Hooks** — host CLI ↔ Catenary. The host CLI (Claude Code, Gemini
  CLI) fires hooks at lifecycle boundaries (pre-tool, pre-agent,
  post-agent, session start/end). Hook processes connect to the daemon's
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
that dispatches events to two sinks: a notification queue (for
user-facing `systemMessage` delivery) and a message database (for
monitor visibility, debugging, and TUI broadcast). Every protocol
message flows through it.

Application servers (`GrepServer`, `GlobServer`, `DiagnosticsServer`)
are the transformation layer. They receive application-level parameters
from the IPC router, do work using `LspClient`, and return results.
They do not log protocol messages — that is the boundary components'
job. An application server is a black box: the protocol messages that
went in and came out are linked by `parent_id` at the database level.

## Component diagram

```
                ┌─────────────────────────────────────────────────────┐
                │                    Catenary daemon                  │
                │                                                     │
Agent ◄──CLI──► │  IPC router ──► GrepServer / GlobServer / sed       │
  (grep, glob,  │                  DiagnosticsServer                  │
   sed,         │                       │                             │
   diagnostics) │                 LspClientManager                    │
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
  ├── NotificationQueueSink  (user-facing systemMessage)
  └── MessageDbSink          (messages table + TUI broadcast)
```

## Shared infrastructure

- **`Session`** — application container. Owns application servers, the
  client manager, filesystem manager, editing state, path validation,
  and logging. Protocol boundaries hold `Arc<Session>`.
- **`FilesystemManager`** — file classification and root resolution.
  Single authority for language detection, shebang parsing, and
  workspace root membership. Also implements the snapshot-and-diff
  model for `workspace/didChangeWatchedFiles` notifications.
- **`LspClientManager`** — LSP server lifecycle. Spawns, caches, and
  shuts down `LspClient` instances. Manages instance keying (language,
  server name, scope), multi-server routing, document lifecycle, and
  workspace folder synchronization.
- **SQLite database** — all session state (sessions, protocol
  messages, trace events, workspace roots, language servers) is stored
  in `~/.local/state/catenary/catenary.db` with WAL mode.

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
