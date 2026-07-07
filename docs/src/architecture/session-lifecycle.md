# Session Lifecycle

This page traces what happens from `catenary` invocation through
shutdown.

## Startup sequence

A single daemon serves the whole host. The bridge proxy — `catenary`
launched over stdio by an MCP client — starts the daemon on first
connection and then just forwards bytes; later agents reuse the same
daemon. Daemon startup, in order:

1. **Socket bind.** The daemon binds two deterministic Unix sockets under
   `state_dir`: `catenary/catenary-mcp.sock` (MCP traffic from the bridge
   proxy) and `catenary/catenary.sock` (hook events and CLI commands).
   Binding first proves this is the sole daemon and lets bridge proxies
   queue connections during the rest of init. Any leftover legacy
   `catenary.db` is deleted here.

2. **`LoggingServer` activation.** Constructed earlier in buffering mode
   (early `tracing` events are captured in a bounded 4096-event buffer);
   once the sinks exist, `activate()` drains the buffer and switches to
   direct dispatch. The sinks are the notification queue and the JSONL
   firehose.

3. **Config loading.** `Config::load()` reads sources in order: embedded
   default language definitions, user config
   (`~/.config/catenary/config.toml`), and an optional explicit file
   (`CATENARY_CONFIG` env var). Later sources override earlier ones;
   environment overrides (`CATENARY_SERVERS`, `CATENARY_ROOTS`) are
   applied last.

4. **Root resolution.** Workspace roots come from `CATENARY_ROOTS`
   (path-separated) or default to the current directory. Roots are
   canonicalized to absolute paths.

5. **Primary session assembly.** The daemon builds one shared,
   infrastructure-only `Session`. It owns the resources every agent
   shares: the `LspClientManager` (the LSP server pool), the tool servers
   (`GrepServer`, `GlobServer`, `DiagnosticsServer`), the `SymbolIndex`
   (populated lazily from LSP `documentSymbol` responses), and the JSONL
   firehose sink. `FilesystemManager` is constructed with classification
   tables from config and the roots are set — there is no filesystem
   snapshot at startup; change detection walks on demand, and each root's
   first walk is its own baseline. This session has no connection
   affinity.

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

7. **Accept loop.** `SessionManager` begins accepting connections on both
   sockets. The daemon never reads stdin — the bridge proxy owns the
   stdio pipe to the host CLI and forwards MCP traffic over the socket.
   `McpServer` handles the MCP lifecycle (`initialize`, roots, `ping`)
   per connection and exposes no application-level tools; an
   `on_roots_changed` callback triggers root re-sync when an MCP client
   updates its root list.

## Sessions

In the daemon model a *session* is a connected agent, not a process. A
session is identified by its bridge MCP connection and the `session_id`
its hooks carry, and appears on the `state.json` session board with its
client, roots, and status. All sessions share the daemon's LSP server
pool and tool servers — there is no per-session language server. What is
per-session is lightweight: the editing state (`EditingManager`,
the accumulated set of edited files), created on the session's first
hook dispatch. A session ends when its connection disconnects, dropping
it from the board; the daemon keeps running for the other sessions, and
exits only when the last one disconnects.

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

Once initialized, the daemon serves requests from two sockets: CLI
commands and hook events over the IPC socket (grep, glob, diagnostics,
roots, editing enforcement) and MCP traffic over the MCP socket (the
bridge proxy forwards the handshake and root updates). Each
CLI command follows this sequence:

1. **File change notification.** The walk a command already performs
   (grep's search walk, glob's directory scan, the diagnostics
   stat-walk) doubles as change detection: observed `(path, mtime)`
   pairs are diffed against each root's baseline (`diff_and_update`), and
   only the files that actually changed are sent — as a precise,
   per-server changed-set (`nudge_changed_set`) — via
   `workspace/didChangeWatchedFiles` to servers whose registered watchers
   match.

2. **IPC dispatch.** The IPC router dispatches the request to the
   appropriate application server:
   - `grep` → `GrepServer` — parallel ripgrep + LSP symbol index
     search, LSP enrichment.
   - `glob` → `GlobServer` — file listing with structural symbol
     outlines from LSP `documentSymbol`.
   - `diagnostics` → ends the editing batch, runs batched diagnostics
     across all modified files (editing starts implicitly on the first
     covered edit — there is no separate start command).

3. **LSP interaction.** Application servers use `LspClientManager` to
   find the right server(s) for each file, wait for readiness, open
   documents, send LSP requests, and collect responses. Multi-server
   languages use priority-chain dispatch for request/response methods
   (first non-empty result wins) and diagnostic concatenation (all
   enabled servers contribute).

4. **Result return.** The application server returns a result string
   printed to the CLI command's stdout.

### Editing mode

Editing mode brackets a batch of file edits. It starts implicitly on the
first edit to a server-covered file — there is no separate start command
— and ends when the agent runs `catenary diagnostics`. The host CLI's
Edit/Write tools modify files directly — as do native shell writes like
`sed -i`, whose write-set the `PreToolUse` hook resolves; the hook tracks
which files are modified. When `catenary
diagnostics` runs, `DiagnosticsServer` opens all modified files on their
respective language servers, waits for each server to settle, retrieves
diagnostics, and prints a consolidated report to stdout.

While covered edits are pending, the `PreToolUse` hook enforces
boundaries: only edit-related tools (Edit, Write, filesystem Bash
commands, and canonical Catenary commands) are allowed without running
`catenary diagnostics` first.

## Mid-session root addition

When a workspace root is added (`catenary pin <path>` via the
host's shell tool, or via MCP `roots/list` update), Catenary processes
it through `Session::sync_roots`:

1. `FilesystemManager` roots are updated; a newly added root has no
   baseline yet, so its first walk becomes the baseline (no re-seed).
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

The daemon exits when the last client disconnects, on `catenary stop`,
or on SIGINT/SIGTERM:

1. The accept loop stops and both socket listeners are torn down.
2. `Session::shutdown()` sends LSP `shutdown` requests to all active
   servers, waits for responses, then sends `exit` notifications.
3. The JSONL firehose is flushed — the queue drains and the writer
   thread joins.
4. Dropping the `SessionManager` removes both daemon sockets
   (`catenary.sock` and `catenary-mcp.sock`), so a stale bridge cannot
   reconnect to a dead daemon.

(An individual session ending is not a daemon shutdown — it just
disconnects and drops off the `state.json` board while the daemon keeps
serving the rest.)

### Confirming a stop

Run in an interactive terminal with sessions still connected, `catenary
stop` prints the session board first — each connected session's host,
workspace root(s), and how long it has been connected, read from the
`state.json` snapshot — and asks for confirmation before disconnecting
anyone. Declining (the default) exits `0` with the daemon left running.
`--force` skips the prompt (scripts, and the documented upgrade flow), and a
non-interactive stdin skips it too. After the stop, a warning still names how
many sessions lost tooling — each needs a `/mcp` reconnect, since a host
restart alone won't respawn the daemon.

## TUI monitoring

Running `catenary` with no subcommand in an interactive terminal launches
the read-only TUI dashboard. It is a pure file reader: it file-watches
the daemon-owned `state.json` snapshot under `runtime_dir` and reloads on
change — it never connects to the daemon, the firehose, or a database, so
it cannot affect (or wedge) a running session.

The snapshot holds live state only, so the dashboard renders a
health/config surface rather than a message stream: a 2×2 master-detail
grid with the root/server tree and client/session tree on the left, a
contextual detail pane top-right, and the problems pane bottom-right. On
narrow terminals the four panes stack full-width.

For full protocol and trace history — request/response pairing, the LSP
traffic behind a single command — query the firehose with `catenary
query` (e.g. `catenary query --session <id>`).
