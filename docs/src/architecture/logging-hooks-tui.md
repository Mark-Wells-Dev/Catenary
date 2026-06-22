# Logging, Hooks & TUI

Catenary produces a large volume of protocol traffic — hundreds of LSP
messages per tool call, MCP request/response pairs, hook invocations.
This page explains how that traffic is captured, how it reaches the
right audience, and how the TUI makes it human-readable.

## Three-audience surface model

Every piece of telemetry in Catenary is destined for one of three
audiences. This separation is the organizing principle for the entire
observability stack.

| Audience | Channel | What goes here |
|---|---|---|
| Agent | Tool result content | Data + caller-input errors the agent can fix |
| User (real-time) | Host CLI `systemMessage` | Degradation notices, server status, anything user-actionable |
| User (investigating) | Logs, TUI | Full audit trail |

Why the separation matters: agent context is expensive (tokens), and
Catenary's internal problems — server crashes, config errors, routing
failures — are not the agent's problem. Surfacing them in tool results
wastes context on information the agent cannot act on. User-facing
channels exist for everything else.

The agent sees only data it asked for (hover results, diagnostics,
grep matches) and errors it can fix (file not found, ambiguous path).
The user sees operational status in real time through `systemMessage`.
The full protocol trace is always available through the TUI and
database for debugging.

## `LoggingServer` — the sole telemetry port

`LoggingServer` is a `tracing_subscriber::Layer`. Every
`tracing::info!()`, `warn!()`, `error!()` call in the codebase flows
through it. It is Catenary's only telemetry port — there is no separate
error reporting path, no separate protocol logging path, no side channel
for notifications. Everything goes through tracing, and `LoggingServer`
dispatches to sinks.

### Two-phase construction

`LoggingServer` starts in buffering mode. During early startup (config
loading, database migration), tracing events are captured in a bounded
in-memory buffer (4096 events). Nothing is written to disk yet — the
database connection does not exist.

When `Session` assembly creates the database connection and sinks,
`LoggingServer::activate()` is called. This drains the bootstrap buffer
through the sinks in FIFO order and switches to direct dispatch. From
this point, every tracing event flows to the database immediately.

If bootstrap events were dropped due to buffer overflow, activate emits
a `warn!()` describing the loss. That event flows through the now-active
sinks like any other.

### Sinks

Post-activation, two sinks receive every tracing event:

- **Notification queue** — severity-filtered (default `warn`),
  deduplicated. Accumulates user-facing notifications between drain
  points. See [notification lifecycle](#notification-lifecycle) below.

- **Message DB** — writes all events to the `messages` table. Protocol
  events (`kind` in `{lsp, mcp, hook}`) set `type` from the kind field;
  internal events set `type = "internal"`. The `level` column always
  reflects the event's tracing severity. Broadcasts inserted ROWIDs so
  `SqliteMessageTail` can stay live without polling. This is the data
  that powers the TUI.

The message DB sink replaces the former `MessageLog` (protocol logging)
and `ErrorLayer` (error reporting) — both were deleted during the
logging consolidation. The consolidation means every telemetry event
follows the same pipeline regardless of origin.

### Hot path

Post-activation, the dispatch hot path is lock-free: a single
`OnceLock::get` (atomic load) reads the sinks slice and dispatches
directly. No Mutex, no `Vec` clone, no refcount bumps per event. Each
sink call is wrapped in `catch_unwind` — a panicking sink does not
prevent other sinks from receiving the event or crash the caller.

### Correlation IDs

`LoggingServer::next_id()` mints monotonic in-process correlation IDs
(`AtomicI64`, session-scoped, starts at 0). Protocol boundary components
use these for two purposes:

- **`request_id`** — pairs a request with its response. The TUI joins
  on this field to create merged display entries with timing.
- **`parent_id`** — links LSP messages to the CLI command that caused
  them. The TUI uses this for scope collapse — hundreds of LSP messages
  from a single grep call group behind one summary line.

IDs are in-process monotonic values, not database ROWIDs. This avoids
round-trip latency and lets correlation work even before the database
write completes.

## Notification lifecycle

Notifications are user-facing messages delivered through the host CLI's
`systemMessage` field. They exist because Catenary bugs and server
problems should not consume agent context — but the user should still
know about them.

### From tracing call to queue

1. Any `warn!()` or `error!()` call in the codebase becomes a potential
   notification.
2. `NotificationQueueSink` filters by severity threshold (configurable
   via `[notifications] threshold`, default `warn`). Events below
   threshold are silently dropped.
3. The event's `NotificationKey` — `(source, server, language,
   message_stem)` — is checked against the session-scoped `seen` set.
   Duplicates are dropped. The message stem is normalized: lowercased,
   digits stripped, whitespace collapsed. This means "server crashed 3
   times" and "server crashed 5 times" collapse into a single entry.
4. The notification is pushed onto a bounded queue (cap: 100). On
   overflow, the oldest entry is evicted.

### Drain at stationary points

Notifications are not delivered immediately. They are drained at
**stationary points** — moments when the agent is not mid-tool-call
and the host CLI can display a message:

| Hook | Drains? | Why |
|------|---------|-----|
| `SessionStart` | Yes | Fresh session — show startup warnings |
| `Stop` / `AfterAgent` (allow) | Yes | Agent is done — safe to surface notices |
| `Stop` / `AfterAgent` (block) | No | Agent must fix something — preserve queue |
| `PreToolUse` | No | Mid-flight — don't interrupt |

Why drain at stationary points: host CLIs may not display
`systemMessage` during tool execution. Delivering at stationary points
ensures visibility.

### Output composition

`SystemMessageBuilder` composes two content surfaces into the
`systemMessage` string:

- **Direct** — synchronous handler messages (e.g., config validation
  warnings at session start). Rendered first.
- **Background** — accumulated notifications from the queue drain.
  Rendered after a visual header (`─── background ───`).

When both are present, they are separated by a blank line and the
header. When only one is present, the other is omitted. When neither
has content, no `systemMessage` field is emitted.

Sink panics (isolated by `catch_unwind` in the Layer) are surfaced as
`[err] sink panic: <message>` in the background section, so the user
sees them exactly once through the same channel.

For full configuration details, see the
[Notifications](../notifications.md) page.

## Hook system

Catenary integrates with host CLIs via hooks — shell commands that
execute at lifecycle boundaries (before/after tool use, session
start/stop). Hook processes are dumb transports: they read the hook
payload from the host CLI, connect to the running session's IPC socket,
forward the request, and format the response for the host.

All hook logic runs server-side. The hook process (`catenary hook
<subcommand>`) is a thin CLI client.

### Architecture

Two components split protocol concerns from application logic:

- **`HookServer`** — protocol boundary. Listens on an IPC endpoint
  (Unix domain socket on Unix, named pipe on Windows), parses JSON
  messages, logs request/response pairs for monitor visibility, and
  delegates to `HookRouter`. Analogous to `McpServer` for MCP and
  `Connection`/`LspServer` for LSP.

- **`HookRouter`** — application dispatch. Routes parsed `HookRequest`
  values to the appropriate handler. Owns editing state enforcement,
  file accumulation, root refresh signaling, and notification drain.
  Analogous to the IPC router for CLI command dispatch.

### Hook methods

Hook methods, each corresponding to a host CLI lifecycle event:

| Method | Host event | Purpose |
|--------|-----------|---------|
| `session-start/clear-editing` | `SessionStart` | Clear stale editing state from a previous agent context |
| `pre-tool/editing-state` | `PreToolUse` / `BeforeTool` | Editing state enforcement — deny or allow a tool call |
| `pre-tool/check-command` | `PreToolUse` / `BeforeTool` | Command filter — terse denial message (specific reason + fix + `catenary commands` pointer) |
| `post-agent/require-release` | `Stop` / `AfterAgent` | Force `catenary diagnostics` if the agent stops with covered edits pending |

### Hook contracts by host

Different host CLIs have different hook surfaces. The hook definitions
live in host-specific JSON files:

| Host CLI | Hook file | Events |
|----------|-----------|--------|
| Claude Code | `plugins/catenary/hooks/hooks.json` | `SessionStart`, `PreToolUse`, `Stop`, `SubagentStop`, `SessionEnd`, `SubagentStart`, `WorktreeRemove` |
| Gemini CLI | `hooks/hooks.json` | `SessionStart`, `BeforeTool`, `AfterAgent`, `SessionEnd` |

All hooks fire unconditionally (empty matcher). The `PreToolUse` /
`BeforeTool` hook handles both editing state enforcement and command
filtering in a single invocation.

The `--format=claude` / `--format=gemini` flag on each `catenary hook`
command selects the output format for the host's expected JSON structure.

### Diagnostic delivery path

Diagnostics flow through `catenary diagnostics` stdout output. The
`PreToolUse` hook tracks which files the agent modifies during editing
mode; `catenary diagnostics` collects those paths and runs the batched
diagnostic pipeline.

The current path:

1. `PreToolUse` hooks track modified file paths in `EditingManager`
   (via `HookRouter`) during editing mode.
2. The agent runs `catenary diagnostics` (CLI command via the host's
   shell tool).
3. `DiagnosticsServer` runs the batched diagnostic pipeline across all
   accumulated files.
4. Results are printed to stdout — the agent sees them in the shell
   tool output.

This keeps diagnostics in the agent channel (where the agent can act on
them) and hook responses in the user channel (where operational
information belongs). See [Document Lifecycle & File
Watching](documents.md#editing-mode) for the full diagnostic pipeline.

## TUI — the `catenary` dashboard

Running `catenary` with no subcommand in an interactive terminal launches
the TUI dashboard. (When stdin and stdout are pipes — an MCP client
launched it — the same binary serves MCP instead, no flag needed.) The
dashboard connects to the SQLite database (not to the running daemon
process) and renders the protocol message stream across all sessions. It
is a read-only observer — it cannot affect a running session.

For a single session's events as plain text instead of the full TUI, use
`catenary debug monitor <id>` — see
[CLI & Dashboard](../cli.md#catenary-debug).

### Data source

The TUI reads from the `messages` table using WAL-based change
notification. A file watcher on the WAL file triggers re-reads when
new rows are inserted. The TUI never polls — it wakes only when there
is new data. Historical sessions can be browsed after the fact because
all messages persist in the database.

### Message envelope

Every protocol message is stored as an envelope with these fields:

| Field | Purpose |
|-------|---------|
| `type` | Protocol boundary: `mcp`, `lsp`, `hook`, or `internal` |
| `method` | Protocol method name (e.g., `textDocument/hover`, `tools/call`) |
| `server` | LSP server name (e.g., `rust-analyzer`) |
| `client` | Host CLI identifier (e.g., `claude-code`) |
| `request_id` | Correlation ID for request/response pairing |
| `parent_id` | Causation ID linking LSP messages to their originating CLI command |
| `level` | Tracing severity (`debug`, `info`, `warn`, `error`) |
| `payload` | Raw JSON content |

Three boundary components own logging: `McpServer` (MCP), `LspClient`
(LSP), and `HookServer` (hooks). Application servers are black boxes —
the protocol messages that went in and came out are linked by
`parent_id` at the database level.

### Display pipeline

The TUI transforms raw messages into display-ready entries through
three passes:

```
Raw messages → Pair merge → Scope collapse → Run collapse → Display
```

1. **Pair merge.** Request/response messages that share a `request_id`
   merge into a single `Paired` display entry with timing information
   (response time = response timestamp - request timestamp).

2. **Scope collapse.** Entries sharing a `parent_id` group into a
   `Scope` display entry. The parent is typically an MCP `tools/call`
   pair; the children are the LSP messages that the tool call produced.
   A single grep call that generates hundreds of LSP messages collapses
   to one summary line showing result counts and timing. Scopes that
   are interrupted by unrelated root-level events (e.g., progress
   tokens) are split into segments with position tracking
   (`First`/`Middle`/`Last`).

3. **Run collapse.** Consecutive messages in the same category collapse
   into a single `Collapsed` entry with a count. Categories are
   determined by `lsp_category()` / `mcp_category()` /
   `hook_category()` functions that group by protocol method.

### Summary lines

The TUI inverts the display hierarchy. Summary lines surface the
innermost useful content — error messages, result counts, diagnostic
severity — rather than protocol scaffolding (direction arrows, method
names, JSON-RPC structure).

Icons replace direction arrows. An icon carries at-a-glance semantic
status (success, protocol error, cancellation, progress) while
direction is implied by protocol role. Three icon presets are available:
Unicode (default), Nerd Font, and emoji.

### Layout

The dashboard places a sidebar on the left and the message stream on the
right (the default *quadrant* layout):

- **Workspaces** (left, top) — connected sessions grouped by workspace
  root, each with its language servers nested beneath. Server rows show
  lifecycle state, `$/progress` percentages, and `window/logMessage`
  content. Selecting a session or server filters the stream; combining
  selections intersects.
- **Keybinds** (left, bottom) — a help panel, collapsed by default to a
  single "Keybinds — `?` to expand" line.
- **Traffic** (right) — the unified, chronological message stream, with
  search (`/`, then `n`/`N`), filtering, visual selection, and yank via
  OSC 52. Expanding a message or scope reveals the full JSON payload.

This unified stream + sidebar replaced the earlier per-session BSP panel
layout: with a shared daemon and many concurrent sessions, per-session
panels did not scale. The layout degrades responsively — on short
terminals the left column collapses to a tab stack, and on narrow
terminals every panel (Traffic included) stacks into one full-width tab
strip. Mouse support covers scrolling, selection, and dragging the
sidebar/stream divider. All colors use the terminal's ANSI palette, so
the TUI inherits the user's theme.

For keybindings and usage, see the
[CLI & Dashboard](../cli.md#dashboard-tui) page.

## Tracing conventions

`LoggingServer` routes events based on structured fields. The
[Tracing Conventions](../tracing-conventions.md) page defines the
severity guidelines, reserved field names, and source taxonomy that
all code must follow. Key rules:

- `warn!()` and `error!()` reach the notification queue by default.
  Only use these for user-relevant, actionable conditions.
- The `kind` field (`"lsp"`, `"mcp"`, `"hook"`) routes protocol events
  to the message DB. Internal events (no `kind` field) also go to the
  message DB with `type = "internal"`.
- Notification dedup fields (`source`, `server`, `language`) should be
  included on `warn!()` / `error!()` events so notifications with the
  same identity collapse.

## Related pages

- [Session Lifecycle](session-lifecycle.md) — when `LoggingServer` is
  constructed and activated, when hooks bind to the IPC socket.
- [Configuration Model](configuration.md) — `[notifications]` threshold
  configuration.
- [Document Lifecycle & File Watching](documents.md) — editing mode
  enforcement and the `catenary diagnostics` pipeline.
- [Routing & Dispatch](routing.md) — dispatch errors surface via
  `warn!()` through the tracing pipeline.
- [LSP Client Layer](lsp-client.md) — server lifecycle state
  transitions that produce notifications.
