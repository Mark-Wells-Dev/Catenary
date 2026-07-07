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
loading, daemon assembly), tracing events are captured in a bounded
in-memory buffer (4096 events). Nothing is written to disk yet — the
sinks do not exist.

When daemon assembly constructs the sinks,
`LoggingServer::activate(sinks)` is called. This drains the bootstrap
buffer through the sinks in FIFO order and switches to direct dispatch.
From this point, every tracing event flows to the sinks immediately.
Activation depends only on sink readiness — there is no database
connection or migration.

If bootstrap events were dropped due to buffer overflow, activate emits
a `warn!()` describing the loss. That event flows through the now-active
sinks like any other.

### Sinks

Post-activation, the sinks receive every tracing event:

- **Notification queue** — severity-filtered (default `warn`),
  deduplicated. Accumulates user-facing notifications between drain
  points. See [notification lifecycle](#notification-lifecycle) below.

- **JSONL firehose** — appends every event as one JSON line to a
  sharded, append-only log under `cache_dir` (see
  [Data source](#data-source) below). Protocol events carry `kind` in
  `{lsp, mcp, hook}`; internal events use `kind = "internal"`. The
  `level` field always reflects the event's tracing severity. This is the
  full audit trail, read after the fact by `catenary query`.

- **Snapshot alert ring** — `warn!()` / `error!()` events are also folded
  into the alert ring of the daemon-owned `state.json` snapshot, so the
  TUI surfaces them without reading the firehose.

The firehose sink replaces the former `MessageLog` (protocol logging),
`ErrorLayer` (error reporting), and the SQLite `messages` table — all
removed during the logging consolidation and the observability rewrite.
The consolidation means every telemetry event follows the same pipeline
regardless of origin.

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
| `post-agent/require-release` | `Stop` / `AfterAgent` | Force `catenary diagnostics` if the agent stops with covered edits pending |
| `subagent-start/mount-worktree` | `SubagentStart` | Mount an `isolation:"worktree"` subagent's git worktree as its own `worktree:{session_id}:{path}` LSP root |
| `worktree-remove/unmount-worktree` | `WorktreeRemove` | Tear down a `worktree:*` root — fires only for non-git VCS / `--worktree` session exit (see worktree-root teardown below) |
| `worktree-create/log-payload` | `WorktreeCreate` | Best-effort observability sink — the hook forwards its full payload here so it lands in the firehose (`catenary query --kind hook`); worktree creation itself is a self-contained local operation (see worktree relocation below) |

### Teaching-payload injection

`SessionStart` and `SubagentStart` inject the **full prevention payload**
into the agent's context via `hookSpecificOutput.additionalContext` (a Claude
Code channel) — not a pointer to run `catenary primer`, but the primer's
content inlined. One module, `src/cli/teaching.rs`, is the single source: the
`catenary primer` command and both hooks render the same
`teaching::payload_body()`, so the on-demand command and the pushed hook
context can never drift.

The payload has three tiers (~600–800 tokens): the **live commands surface**
(the allow / pipeline / deny surface resolved from the config at emission time,
rendered by the same machinery `catenary commands` prints, closing with the
write-model line), the **invariants** (the edit→diagnostics loop, bare-only vs
pipe-friendly command classes, the glob quoting / pattern-path form), and
compact **flag synopses** for `grep`/`glob`, each ending in a
`full: catenary <cmd> --help` breadcrumb. It names no `catenary primer` /
`catenary commands` pointer — inlining is the point.

`SessionStart` re-injects across context discontinuities: it fires on
`startup` / `clear` / `compact` (re-stamping the payload the discontinuity may
have dropped) and skips only `resume` (which restores the prior transcript
verbatim). `SubagentStart` fires once per subagent spawn and appends a
per-agent debt line (a subagent's diagnostic debt is tracked per-agent); its
`additionalContext` lands in the subagent's own window under one shared label,
so the payload is self-contained and prefix-identifiable.

### Hook contracts by host

Different host CLIs have different hook surfaces. The hook definitions
live in host-specific JSON files:

| Host CLI | Hook file | Events |
|----------|-----------|--------|
| Claude Code | `plugins/catenary/hooks/hooks.json` | `SessionStart`, `PreToolUse`, `Stop`, `SubagentStop`, `SessionEnd`, `SubagentStart`, `WorktreeRemove`, `WorktreeCreate` |
| Antigravity CLI | `plugins/catenary-antigravity/hooks.json` | `PreInvocation`, `PreToolUse`, `Stop` |

The `PreToolUse` hook handles both editing state enforcement and command
filtering in a single invocation.

### Worktree relocation (out of tree)

Claude Code's `WorktreeCreate` hook lets a plugin own worktree creation: the
hook receives a JSON payload on stdin and must print the created worktree's
absolute path on stdout (a failure or empty path fails creation). Catenary uses
it (`catenary hook worktree-create`) to place every subagent worktree **outside
the source repo tree**, under `<cache_dir>/catenary/worktrees/<flattened-repo>-<id>`
(see `src/worktree_create.rs` and `paths::agent_worktree_dir`). This is the
structural fix for nested-worktree index pollution: a worktree nested *inside* a
tracked root is a second copy of the project that gitignore-blind server
discovery (rust-analyzer's cargo walk) indexes a second time; a worktree that
lives outside the repo can never be reached by that downward walk, with zero
per-server exclude configuration.

Relocation is transparent to the rest of the worktree lifecycle: a git worktree
records its upstream repo through a `.git` **file** (`gitdir:
<repo>/.git/worktrees/<name>`), not its filesystem location, so the
`SubagentStart` mount predicate (`worktree_to_auto_mount`, which resolves the
worktree's canonical project root from that pointer) and the deletion watch both
work unchanged for a cache-dir worktree. Cleanup is unchanged too: Claude Code
removes git worktrees itself with `git worktree remove`, which drops the
directory and its `.git/worktrees/<name>` metadata regardless of where the
directory lives. As a crash-safety backstop, each create first runs
`git worktree prune` semantics over the cache dir (`worktree_create::prune_orphans`),
sweeping any directory whose git linkage is already dead.

Because the hook **replaces** Claude Code's default worktree creation entirely,
two host behaviors are reimplemented in `worktree_create.rs`. First,
`.worktreeinclude`: the host normally copies untracked, git-ignored local config
(the `.env` class) into each new worktree per a `<repo>/.worktreeinclude` file
(`.gitignore` pattern syntax); `copy_worktree_includes` carries the matched
files into the relocated worktree, preserving relative paths and skipping any
path already checked out (so tracked files are never clobbered). Second, VCS
detection and per-VCS creation: before any VCS call, `detect_vcs` examines the
payload `cwd` (and its ancestors) for a marker, and each supported VCS gets its
worktree-shaped analog under the agents scheme —

- **git** — `git worktree add` (shared object store, separate working dir);
- **hg** — `hg share` (the true worktree analog: a shared store with a separate
  working dir), falling back to `hg clone` when the bundled `share` extension is
  unavailable;
- **svn** — `svn checkout` of the source working copy's `URL@revision` (a second,
  independent working copy). svn has no shared-store worktree concept, so local
  uncommitted changes in the source do **not** carry over — an inherent parity
  gap, surfaced honestly at creation (a user notification) rather than hidden.

`.jj` keeps the single honest refusal line naming the detected VCS (decision-030
gate still closed — no jujutsu binary on the host), and an unversioned directory
gets the marker-list error rather than a raw VCS error. The sidecar records the
backing VCS and its per-VCS base marker (git `HEAD`, svn `URL@revision`, hg base
changeset), which the disposal clean proof consumes (misc 148).

### Worktree-root teardown

A worktree subagent's root is mounted at `SubagentStart` and meant to be
torn down at `WorktreeRemove`. But `WorktreeRemove` **never fires for git
worktrees**: the host runs `git worktree remove` itself and invokes the
hook only for the non-git VCS / `--worktree` session-exit path. So for the
common git case the prompt teardown signal is the **directory deletion**
itself. For a **non-git (svn/hg) worktree** the host *does* fire
`WorktreeRemove` and expects the hook to delete the copy: the handler runs
the same guarded disposal routine (`worktree_dispose::dispose` with
`host_initiated`) that every other trigger uses — a clean copy is deleted,
a dirty one is refused with a logged divergence, and a path outside our
scheme or lacking a sidecar is never touched (misc 148). This is the live
leg of the arming that stays dormant for git. The daemon reaps the `worktree:{session_id}:{path}` root via a
bounded, non-recursive directory-deletion watch (`notify`/inotify) on each
mounted worktree dir — registered at mount, fired the instant the dir
disappears (see `src/worktree_watch.rs`). The hourly dir-gone GC
(`reap_missing_worktree_roots`) and the `SessionEnd` sweep remain as
crash-safe backstops: the watch is in-memory and dies with the daemon. The
reap (`remove_contributor` + root re-sync) is identical and idempotent
across all three paths, so a double-reap is a harmless no-op.

The `--format=claude` / `--format=antigravity` flag on each `catenary hook`
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
dashboard is a pure file reader: it reads the daemon-owned `state.json`
snapshot and never connects to the daemon process, the firehose, or a
database. It is a read-only observer — it cannot affect a running session.

For full protocol and trace history (a single session, a server, a
search), query the firehose with `catenary query` — see
[CLI & Dashboard](../cli.md#catenary-query).

### Data source

The TUI reads a single file: the daemon-owned `state.json` snapshot under
`runtime_dir`. A file watcher on the snapshot's directory triggers a
reload whenever the daemon rewrites it. The TUI never polls and never
opens the firehose — it wakes only when the snapshot changes. The
snapshot holds live state only (current sessions, servers, recent alerts
and activity); historical protocol traffic lives in the firehose and is
read with `catenary query`.

### Firehose record

Every event is appended to the firehose as one JSON line. Empty or absent
fields are omitted; the principal fields are:

| Field | Purpose |
|-------|---------|
| `ts` | Timestamp (RFC 3339, millisecond, UTC) |
| `kind` | Event kind: `lsp`, `mcp`, `hook`, or `internal` |
| `level` | Tracing severity (`debug`, `info`, `warn`, `error`) |
| `scope_id` | Self-describing shard id: session id, search UUID, `server@root`, or instance id |
| `parent_id` | Causation ID pairing a response with its request |
| `server` | LSP server name (e.g., `rust-analyzer`) |
| `scope_root` | Workspace root the event was routed for |
| `cwd` | Originating working directory (the `catenary query --cwd` filter dimension) |
| `method` | Protocol method (e.g., `textDocument/hover`) or, for internal events, the module target |
| `source` | Subsystem taxonomy (e.g., `lsp.lifecycle`) |
| `payload` | Nested protocol JSON (protocol events) |
| `message` / `language` / `fields` | Rendered message and remaining structured fields (internal events) |

Three boundary components own logging: `McpServer` (MCP), `LspClient`
(LSP), and `HookServer` (hooks). Application servers are black boxes —
the protocol messages that went in and came out are linked by
`parent_id` in the firehose records.

### Panes

The dashboard is a snapshot renderer: it shows the daemon's current state
directly, with no message-stream reconstruction (no request/response
pairing or scope collapse — that view is `catenary query`'s job). It is a
health/config surface, not a stream monitor, and answers a single binary
question: *is it working?* Its inputs are `state.json` and the health
model's findings; it never probes an LSP or opens the firehose. Four
panes share a 2×2 master-detail grid:

- **Root/server tree** (top-left) — grouped by root (the lifecycle/RAM
  unit), collapsible; root lines carry the schema-2 contributor labels
  and ephemeral idle countdowns, with per-root server rows
  (lifecycle / time-in-state / respawns / last-death) and dormant
  inventory behind a toggle. A healthy fleet collapses to one line per
  root — nothing green shouts.
- **Client/session tree** (bottom-left) — grouped by client, with
  install-health findings inline at the client node and live sessions
  underneath. Session status is capability-aware: each host renders only
  what its events feed (Claude Code adds subagent sub-rows; a host with
  no stop coverage degrades to an honest `last seen Nm` — no fabricated
  statuses).
- **Detail pane** (top-right) — the contextual view of the cursored
  node: a server's effective config by layer with provenance, a root's
  routing table, or a session's recent actions + live subagents.
- **Problems pane** (bottom-right) — the durable notification surface:
  every finding sorted `Fatal`/`Error`/`Warning` with its fix-it line,
  suggestions as a collapsed tail that never displaces a problem.
  Selecting a problem focuses the board on its owner. An empty pane is
  the working verdict.

Findings render twice — inline at their owning tree node and in the
problems pane — from one health model, so the two views can never
disagree. A header strip carries the one-line verdict, daemon identity,
version + skew, and snapshot staleness.

### Layout

Shared borders divide the grid; each pane's title renders inside its
body, not on the border. On narrow terminals the grid degrades to four
full-width stacked panes. Navigation is keyboard-first with mouse click
as an equal path: `j`/`k` to move, `Tab` to cycle panes, `Enter` to
expand a node or focus a problem's owner, `p` for problems-only, `d` to
toggle dormant inventory, `g`/`G` and `PageUp`/`PageDown` to jump, `y` to
yank the selected entry via OSC 52, `?` for the keybinds panel, `q` to
quit. All colors use the terminal's ANSI palette, so the TUI inherits the
user's theme; every severity also carries a glyph so it reads on a
monochrome terminal.

This snapshot dashboard replaced an earlier message-stream TUI: with a
shared daemon and many concurrent sessions, reconstructing every
session's protocol stream in the UI did not scale and coupled the TUI to
a growing database. Reading a small live snapshot instead keeps the
dashboard structurally unable to wedge the daemon.

For keybindings and usage, see the
[CLI & Dashboard](../cli.md#dashboard-tui) page.

## Tracing conventions

`LoggingServer` routes events based on structured fields. The
[Tracing Conventions](../tracing-conventions.md) page defines the
severity guidelines, reserved field names, and source taxonomy that
all code must follow. Key rules:

- `warn!()` and `error!()` reach the notification queue by default.
  Only use these for user-relevant, actionable conditions. The one
  exception is server-forwarded window messages (`source = lsp.logging`,
  from `window/logMessage` / `window/showMessage`): those are
  firehose-only and never enqueue, regardless of severity — the queue is
  reserved for Catenary's own events (misc 125).
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
