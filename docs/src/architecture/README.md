# Architecture

Catenary is a multiplexing bridge between MCP and LSP. AI agents talk
to Catenary through MCP tools; Catenary translates those into LSP
requests to one or more language servers, collects results, and returns
them through MCP.

## Three protocols

Every external interaction crosses one of three protocol boundaries:

- **MCP** — agent ↔ Catenary. The agent calls tools (`grep`, `glob`,
  `start_editing`, `done_editing`) and receives structured results.
- **LSP** — Catenary ↔ language servers. Catenary spawns and manages
  language server processes, sending requests and receiving
  notifications over JSON-RPC stdio.
- **Hooks** — host CLI ↔ Catenary. The host CLI (Claude Code, Gemini
  CLI) fires hooks at lifecycle boundaries (pre-tool, post-tool,
  pre-agent, post-agent, session start). Hook processes connect to the
  running session's IPC socket and exchange JSON messages.

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

- **`McpServer`** — MCP protocol adapter. Reads JSON-RPC from stdin,
  dispatches tool calls, writes responses to stdout.
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

Tool servers (`GrepServer`, `GlobServer`, `DiagnosticsServer`) are the
transformation layer. They receive application-level parameters, do
work using `LspClient`, and return results. They do not log protocol
messages — that is the boundary components' job. A tool server is a
black box: the protocol messages that went in and came out are linked
by `parent_id` at the database level.

## Component diagram

```
                ┌─────────────────────────────────────────────────────┐
                │                    Catenary                         │
                │                                                     │
Agent ◄──MCP──► │  McpServer ──► McpRouter ──► ToolServer            │
                │                               (grep, glob,         │
                │                                diagnostics)         │
                │                                    │                │
                │                              LspClientManager       │
                │                              ┌─────┴──────┐        │
                │                         LspClient    LspClient     │
                │                              │            │         │
                └──────────────────────────────┼────────────┼─────────┘
                                               │            │
                                          LSP (stdio)  LSP (stdio)
                                               │            │
                                        rust-analyzer  typescript-
                                                       language-server
                ┌──────────────────────────────────────────────────────┐
  Host CLI ◄──IPC──► HookServer ──► HookRouter ──► Session           │
  (hooks)       │                                                      │
                └──────────────────────────────────────────────────────┘

  LoggingServer (tracing Layer) ─── dispatches all events to sinks:
    ├── NotificationQueueSink  (user-facing systemMessage)
    └── MessageDbSink          (messages table + TUI broadcast)
```

## Shared infrastructure

- **`Session`** — application container. Owns tool servers, the client
  manager, filesystem manager, editing state, path validation, logging,
  and the tree-sitter index. Protocol boundaries hold `Arc<Session>`.
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
