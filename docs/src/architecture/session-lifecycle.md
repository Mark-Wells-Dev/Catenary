# Session Lifecycle

This page traces what happens from `catenary` invocation through
shutdown.

## Startup sequence

When the binary starts without a subcommand and stdin/stdout are not a
terminal, it runs as an MCP server. The startup sequence is:

1. **`LoggingServer` construction.** Created in buffering mode. All
   `tracing` events during early startup are captured in a bounded
   in-memory buffer (4096 events). Nothing is written to disk yet.

2. **Config loading.** `Config::load()` reads sources in order:
   embedded default language definitions, user config
   (`~/.config/catenary/config.toml`), and optional explicit file
   (`CATENARY_CONFIG` env var). Later sources override earlier ones.
   Environment variable overrides (`CATENARY_SERVERS`,
   `CATENARY_ROOTS`) are applied last.

3. **Root resolution.** Workspace roots come from `CATENARY_ROOTS`
   (path-separated) or default to the current directory. Roots are
   canonicalized to absolute paths.

4. **Session creation.** A `Session` is inserted into the SQLite
   database (`~/.local/state/catenary/catenary.db`) with a generated
   ID, the process PID, and the workspace display name. The sessions
   directory (`~/.local/state/catenary/sessions/<id>/`) is created for
   the IPC socket.

5. **`Session` assembly.** The application container is constructed:
   - Logging sinks are created (notification queue, message DB) and
     `LoggingServer::activate()` is called. This drains the
     bootstrap buffer through the sinks and switches to direct
     dispatch. From this point, all `tracing` events flow to the
     database.
   - `FilesystemManager` is constructed with classification tables
     derived from config, roots are set, and `seed()` is called
     (snapshots the initial filesystem state for later diffing).
   - `TsIndex` is built from workspace roots (tree-sitter symbol
     index, used by grep for structural search).
   - `LspClientManager` is constructed with the config, logging, and
     filesystem manager.
   - Tool servers (`GrepServer`, `GlobServer`, `DiagnosticsServer`)
     and supporting infrastructure (`EditingManager`, `PathValidator`)
     are created.

6. **`spawn_all`.** The client manager walks workspace roots, classifies
   files via `FilesystemManager`, detects which configured languages
   have matching files, and spawns LSP servers:
   - Project configs (`.catenary.toml`) are loaded for each root.
   - Per-root classification tables are set.
   - For each detected language, each configured server binding is
     spawned. The first root triggers the initial spawn; the server's
     capability response determines scope.
   - Workspace-capable servers get a single `Scope::Workspace`
     instance with all roots. Legacy servers get a separate
     `Scope::Root` instance per root.
   - Project-scoped roots (those with a `.catenary.toml` that
     overrides the language's server config) get their own
     `Scope::Root` instance and are excluded from the workspace
     instance via `didChangeWorkspaceFolders`.

7. **Hook server start.** `HookServer` is created and bound to the
   IPC socket (`sessions/<id>/notify.sock` on Unix, named pipe on
   Windows). This enables host CLI hooks to communicate with the
   session.

8. **MCP server start.** `McpServer` is created with the `McpRouter`
   (which implements `ToolHandler`) and begins reading JSON-RPC
   messages from stdin. The `on_client_info` callback records the MCP
   client's name and version in the session. The `on_roots_changed`
   callback triggers `Session::sync_roots` when the MCP client
   updates its root list.

## Root discovery

Workspace roots are known at startup from `CATENARY_ROOTS` or the
current directory. The MCP `initialize` handshake may also provide
roots via `roots/list`. Each root is checked for a `.catenary.toml`
project config, which can override language and server definitions for
that root's scope.

Per-root classification tables are derived from both the user config
and any project config. These tables map file extensions, filenames,
and shebangs to language IDs, and are used by `FilesystemManager` for
language detection.

## Serving

Once initialized, Catenary enters the MCP dispatch loop. Each tool
call follows this sequence:

1. **File change notification.** Before any LSP interaction, Catenary
   diffs the filesystem against the last snapshot
   (`FilesystemManager::diff()`) and sends
   `workspace/didChangeWatchedFiles` notifications to servers with
   matching glob registrations.

2. **Tool dispatch.** `McpRouter` routes the tool name to the
   appropriate `ToolServer`:
   - `grep` → `GrepServer` — parallel ripgrep + tree-sitter symbol
     index search, LSP enrichment (hover, definitions, references).
   - `glob` → `GlobServer` — file listing with structural symbol
     outlines from LSP `documentSymbol`.
   - `start_editing` → enters editing mode (defers diagnostics).
   - `done_editing` → exits editing mode, runs batched diagnostics
     across all modified files.

3. **LSP interaction.** Tool servers use `LspClientManager` to find
   the right server(s) for each file, wait for readiness, open
   documents, send LSP requests, and collect responses. Multi-server
   languages use priority-chain dispatch for request/response methods
   (first non-empty result wins) and diagnostic concatenation (all
   enabled servers contribute).

4. **Result return.** The tool server returns a structured result
   through MCP.

### Editing mode

`start_editing` and `done_editing` bracket a batch of file edits. The
host CLI's Edit/Write tools modify files directly; Catenary's
`PostToolUse` hook accumulates the paths of changed files. When
`done_editing` is called, `DiagnosticsServer` opens all modified files
on their respective language servers, waits for each server to settle,
retrieves diagnostics, and returns a consolidated report.

During editing mode, the `PreToolUse` hook enforces boundaries: only
edit-related tools (Edit, Write, and filesystem Bash commands) are
allowed without calling `done_editing` first.

## Mid-session root addition

When the host CLI adds a workspace directory (`/add-dir` in Claude
Code), the MCP client sends a `roots/list` update. Catenary processes
it through `Session::sync_roots`:

1. `FilesystemManager` roots are updated and the filesystem is
   re-seeded.
2. Project configs are loaded for new roots; classification tables
   are updated.
3. Workspace-capable servers receive `didChangeWorkspaceFolders`
   notifications (additions for non-project-scoped roots, removals for
   roots that disappeared).
4. Per-root settings from project configs are sent via
   `didChangeConfiguration`.
5. Legacy servers get new `Scope::Root` instances spawned for added
   roots and existing instances shut down for removed roots.
6. `spawn_all` runs again to detect languages in new roots.

## Shutdown

Shutdown is triggered by stdin EOF (MCP client disconnects), Ctrl+C,
or SIGTERM:

1. The MCP dispatch loop exits.
2. The hook server's IPC listener is aborted.
3. `Session::shutdown()` sends LSP `shutdown` requests to all active
   servers, waits for responses, then sends `exit` notifications.
4. The session is marked dead in the database (`alive = 0`,
   `ended_at` is set).
5. The session directory and IPC socket are cleaned up on `Drop`.

## TUI monitoring

`catenary monitor <id>` connects to a session's database (not to the
running process). It reads protocol messages from the `messages` table
using WAL-based change notification and applies the display pipeline:

1. **Pair merge** — joins request/response messages that share a
   `request_id`.
2. **Run collapse** — groups consecutive messages in the same category
   into a single summary line.
3. **Scope collapse** — groups LSP messages behind their parent MCP
   tool call using `parent_id`.

The TUI renders a BSP layout with a session tree, events panels,
scrollbar, selection, filter, and responsive degradation. It operates
read-only against the database — monitoring cannot affect the running
session.
