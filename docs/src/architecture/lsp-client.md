# LSP Client Layer

The LSP client layer is the most complex subsystem in Catenary. It
manages server processes, protocol state, capability negotiation, idle
detection, and readiness signaling across potentially many concurrent
language server instances. This page explains the internal structure
and the reasoning behind it.

## Three-layer architecture

The waitv2 rewrite established three layers with distinct
responsibilities:

```
┌─────────────────────────────────────────────────────┐
│                    LspClient                         │
│  High-level operations: hover, references, definition│
│  Document open/close state (per-client versioning)   │
│  Readiness waiting (wait_ready)                      │
│  Health probing                                      │
│                                                      │
│  ┌────────────────────────────────────────────────┐  │
│  │              Arc<LspServer>                     │  │
│  │  Capabilities (OnceLock<bool> per method)       │  │
│  │  InstanceKey (language, server, scope)          │  │
│  │  Diagnostics cache + generation counters        │  │
│  │  Progress tracking ($/progress)                 │  │
│  │  Lifecycle state machine (ServerLifecycle)       │  │
│  │  Per-root settings + scopeUri resolution        │  │
│  │  File watcher registrations                     │  │
│  │  Notification dispatch (on_notification)        │  │
│  │  Server request dispatch (on_request)           │  │
│  │                                                  │  │
│  │  ┌──────────────────────────────────────────┐   │  │
│  │  │           Connection                      │   │  │
│  │  │  Child process (stdin/stdout)             │   │  │
│  │  │  Reader loop (background tokio task)      │   │  │
│  │  │  Request/response correlation             │   │  │
│  │  │  JSON-RPC framing (Content-Length)        │   │  │
│  │  │  CPU-tick failure detection               │   │  │
│  │  │  Retry on ContentModified (-32801)        │   │  │
│  │  │  MCP cancellation → $/cancelRequest      │   │  │
│  │  └──────────────────────────────────────────┘   │  │
│  └────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

### Connection — raw JSON-RPC transport

`Connection` owns the child process and the reader loop. It sends and
receives raw `serde_json::Value` messages over Content-Length-framed
stdio. It knows about JSON-RPC request/response correlation (pending
request map with `oneshot` channels) but nothing about LSP semantics.

Key behaviors:

- **Reader loop.** A background tokio task reads stdout, parses
  Content-Length frames, and routes messages. Responses are matched to
  pending requests by ID and delivered through `oneshot` channels.
  Notifications and server-initiated requests are forwarded to
  `LspServer` via `on_notification` and `on_request`.

- **Failure detection.** `Connection::request` does not use a simple
  wall-clock timeout. Instead, it polls a `ProcessMonitor` (from the
  `catenary_proc` crate) every 200ms, tracking CPU ticks consumed by
  the server process. If the server burns 10 CPU-seconds without
  responding and is not reporting progress, the request is considered
  stuck. A 30-second wall-clock deadline exists as a fallback when
  process monitoring is unavailable.

- **Retry.** `ContentModified` (-32801) and `RequestCancelled` (-32800)
  errors trigger automatic retry (up to 3 attempts), with a wait for
  server state change between retries.

- **MCP cancellation.** When the MCP client cancels a tool call, the
  `CancellationToken` fires. `Connection::request` sends
  `$/cancelRequest` to the LSP server and returns a
  `RequestCancelled` error, which propagates back through the MCP
  response.

- **Process lifecycle.** `Connection::new` spawns the process with
  `set_parent_death_signal` (so the server dies if Catenary dies) and
  registers it for cleanup. `Drop` kills the child to prevent zombies.

### LspServer — protocol state

`LspServer` is the knowledge layer. It knows what the server can do
and how it is configured, but does not own I/O. It is created at spawn
time (before `initialize`) with empty `OnceLock` fields that are
populated once after the init handshake.

Shared via `Arc<LspServer>` between `LspClient` and `Connection`. The
reader loop holds a `Weak<LspServer>` so it can forward notifications
without preventing cleanup.

Key responsibilities:

- **Static capabilities.** `OnceLock<bool>` fields for each supported
  method: `supports_hover`, `supports_definition`,
  `supports_references`, `supports_document_symbols`, etc. Set once
  by `set_capabilities` from the `InitializeResult`. Return `false`
  before initialization completes — conservative by default.

- **Dynamic capabilities.** `supports_pull_diagnostics` uses
  `AtomicBool` instead of `OnceLock` because it can downgrade at
  runtime. If `textDocument/diagnostic` fails repeatedly on a server
  that advertised `diagnosticProvider`, the capability is permanently
  disabled via `downgrade_pull_diagnostics` and the server falls back
  to push diagnostics.

- **Dynamic registration.** Handles `client/registerCapability` and
  `client/unregisterCapability` for two registration types:
  `workspace/didChangeWatchedFiles` (file watcher patterns stored per
  registration ID) and `workspace/didChangeConfiguration` (tracked as
  a registration ID set).

- **Notification dispatch.** `on_notification` routes incoming server
  notifications:
  - `textDocument/publishDiagnostics` — diagnostics cache + generation
    counter + waiter notification.
  - `$/progress` — progress tracker + lifecycle state transitions
    (`Healthy` / `Busy(n)` based on begin/end counts).
  - `window/logMessage`, `window/showMessage` — debug logging.

- **Server request dispatch.** `on_request` handles server-initiated
  requests:
  - `workspace/configuration` — resolves settings with `scopeUri`
    awareness (per-root project config overlays, deep merge).
  - `client/registerCapability` / `client/unregisterCapability` —
    file watcher and configuration registration management.
  - `window/workDoneProgress/create` — acknowledged (no-op).

- **Identity.** Language ID, server name (both known at spawn time,
  immutable), and scope (`OnceLock`, set once after init). Together
  these form the `InstanceKey`.

- **Configuration.** User-level `settings` (immutable after
  construction) and `settings_per_root` (`Mutex<HashMap>`, updated
  mid-session when roots are added). `resolve_configuration` does
  longest-prefix match for `scopeUri` resolution, deep-merging
  project overlays over user defaults.

### LspClient — high-level operations

`LspClient` owns an `Arc<LspServer>` and accesses the `Connection`
through it (`server.request()` delegates to `connection().request()`).
It provides typed methods that compose connection sends with server
state:

```rust
pub async fn hover(&self, uri: &str, line: u32, character: u32) -> Result<Value>
pub async fn references(&self, uri: &str, ...) -> Result<Value>
pub async fn definition(&self, uri: &str, ...) -> Result<Value>
pub async fn document_symbols(&self, uri: &str) -> Result<Value>
// ... and many more
```

Each method calls `require_capability` first (checking the relevant
`LspServer` flag), then delegates to `Connection::request` with the
current `parent_id` for causation tracking.

Client-local state (not shared with the reader loop):

- **Document tracking.** `open_documents` — the per-connection held-open
  registry: URI → real document version, last-sent disk state (mtime +
  content hash, the change gate), saved flag, and batch owners. Each
  client maintains independent monotonic version sequences, so
  multi-server dispatch gives each server a clean sequence.
  `plan_document_sync` decides per call: first open sends `didOpen`, a
  moved file sends `didChange` (full text, version++), an unchanged file
  sends nothing (see [Document Lifecycle](documents.md)).

- **Position encoding.** Negotiated during `initialize` (defaults to
  UTF-16 per spec).

- **Causation tracking.** `parent_id` links all LSP messages to their
  originating CLI command for database correlation and TUI display.

- **Cancellation.** `cancel: CancellationToken` — set before each tool
  dispatch, propagated to `Connection::request`.

### Why three layers

The separation is not arbitrary. Three concrete benefits:

1. **`Connection` is testable without LSP knowledge.** It deals in raw
   JSON values and framing. The reader loop can be tested with any
   JSON-RPC messages, not just LSP.

2. **`LspServer` is inspectable without I/O.** `get_servers` reads
   capability flags and lifecycle state on the `Arc<LspServer>` without
   acquiring the `LspClient` mutex. The routing layer can filter
   servers by capability without blocking on in-flight requests.

3. **`LspClient` composes both.** Typed methods enforce capability
   checks before sending requests, and the `parent_id` / cancel token
   flow through naturally from the MCP layer.

## Two-step spawn lifecycle

`InstanceKey` cannot be constructed before `initialize` — the scope
depends on whether the server supports `workspaceFolders`, which is
only known from the `InitializeResult`. Spawning therefore splits
into two steps:

1. **`spawn_inner`** (on `LspClientManager`) creates the `LspServer`,
   spawns the `Connection` (child process + reader loop), constructs
   `LspClient`, and runs `initialize` with the workspace roots.

2. **Scope determination.** From the `initialize` response:
   - If project-scoped (Rule A — root has `[lsp.language.*]` in
     `.catenary.toml`): scope is forced to `Scope::Root(root)`
     regardless of capabilities.
   - If workspace-capable (`workspaceFolders` supported): scope is
     `Scope::Workspace`.
   - Otherwise: scope is `Scope::Root(root)` (legacy per-root).

   `set_scope` is called on `LspServer`, and the full `InstanceKey` is
   constructed and inserted into the client map.

   Files outside all workspace roots take a separate path:
   `spawn_single_file` sets `Scope::SingleFile` *before* `initialize`
   (the scope is known without the handshake) and initializes with a
   null workspace. It is gated on `single_file = true` in `[lsp.server.*]`,
   and a server that rejects null-workspace init is negative-cached. See
   the single-file tier in [Routing & Dispatch](routing.md).

The clients lock is held across the entire sequence (`spawn_inner`
acquires it before the double-spawn check and holds it through
insertion). This prevents races where two concurrent spawns for the
same language/server/root both succeed and insert — only the first
one wins.

```
spawn_inner:
  lock clients
  ├── double-spawn check (existing alive instance?)
  ├── LspServer::new()
  ├── Connection::new() → child process + reader loop
  ├── LspClient { server, connection }
  ├── client.initialize(roots)
  ├── determine scope from capabilities
  ├── server.set_scope(scope)
  ├── construct InstanceKey
  ├── clients.insert(key, client)
  └── unlock
```

## Server lifecycle

`ServerLifecycle` is a single enum that tracks the server from spawn
through shutdown:

| State | Meaning |
|---|---|
| `Initializing` | Spawned, init handshake not yet complete. |
| `Probing` | Init complete, server unproven. Tool requests proceed as self-tests. |
| `Healthy` | Proven working, idle, accepts all requests. |
| `Busy(n)` | Server declared active via `$/progress` begin. `n` = in-flight token count. |
| `Failed` | Health probe failed or init error. Terminal. |
| `Dead` | Connection lost / process died. Terminal. |

State transitions:

```
Initializing ──► Probing ──► Healthy ◄──► Busy(n)
                    │            │
                    ▼            ▼
                  Failed       Dead
```

**Probing** is the key innovation. After `initialize`, the server is
unproven — it may crash on real requests. Rather than running a
separate health check that blocks all tool calls, Probing allows tool
requests to proceed as self-tests. The first successful response
transitions `Probing` to `Healthy` (via
`try_transition_probing_to_healthy` in the `LspClient::request`
wrapper). If the server fails, the diagnostics path runs an explicit
health probe (`run_health_probe` sends `textDocument/documentSymbol`)
that transitions to either `Healthy` or `Failed`.

**Busy** carries a count. Multiple concurrent `$/progress` begin
tokens increment the count; each end decrements it. When the count
reaches zero, the server returns to `Healthy`.

A terminal-state transition emits a `warn!()` through `LoggingServer`: it
surfaces as a health finding on the TUI dashboard (a routed-but-broken server —
"Language server unavailable: rust (rust-analyzer)") and lands in the firehose,
without the agent having to report it.

## Capability model

### Static capabilities

Populated once from `InitializeResult` via `set_capabilities`. Each
capability is an `OnceLock<bool>` on `LspServer`:

```
InitializeResult.capabilities:
  hoverProvider           → supports_hover
  definitionProvider      → supports_definition
  referencesProvider      → supports_references
  documentSymbolProvider  → supports_document_symbols
  workspaceSymbolProvider → supports_workspace_symbols
  renameProvider          → supports_rename
  typeDefinitionProvider  → supports_type_definition
  implementationProvider  → supports_implementation
  callHierarchyProvider   → supports_call_hierarchy
  typeHierarchyProvider   → supports_type_hierarchy
  codeActionProvider      → supports_code_action
  diagnosticProvider      → supports_pull_diagnostics (AtomicBool)
  textDocumentSync        → supports_text_document_sync
```

The extraction uses a simple rule: `true` or a non-null options object
means supported; `false`, `null`, or absent means not. Before
`set_capabilities` is called, all flags return `false` —
conservatively correct, since `get_servers` filters by capability and
an uninitialized server should not be selected.

### Dynamic downgrade

`supports_pull_diagnostics` is the only capability that can change
after initialization. When `textDocument/diagnostic` fails on a server
that claimed `diagnosticProvider`, `downgrade_pull_diagnostics` flips
the `AtomicBool` to `false`. Subsequent `get_servers` calls with the
diagnostics capability check naturally exclude this server from pull
diagnostics. The server continues to produce push diagnostics via
`publishDiagnostics` notifications (if it supports
`textDocumentSync`).

## Idle detection and settle

After sending a stimulus to a language server (e.g., `didOpen` for the
diagnostics pipeline), Catenary must wait for the server to finish
processing before reading results. This is the settle model.

### IdleDetector

`IdleDetector` is a pure state machine. Given a process tree snapshot
(from `catenary_proc::TreeMonitor`), it determines whether the server
is idle. Two modes:

- **`after_activity`** (post-stimulus) — requires observing activity
  before accepting silence as idle. Two phases:
  1. Wait for cumulative CPU ticks to advance from a pre-stimulus
     baseline, or any nonzero per-process delta. Either proves the
     server was scheduled.
  2. Wait for all processes to show zero deltas with per-child gates
     (every process that appeared during processing must show activity
     at least once before its silence counts as idle).

- **`unconditional`** (pre-stimulus) — accepts silence immediately.
  Used to verify the server is quiet before sending a stimulus.

### Per-child gates

When a new process appears in the tree during processing (e.g.,
`cargo check` spawns `rustc`), it gets a gate that blocks idle
detection until it shows at least one nonzero delta. This prevents
false idle: a child process that was just spawned but hasn't been
scheduled yet appears quiet, but silence means nothing because it
hasn't had a chance to run. Dead processes bypass the gate — a zombie
that never showed activity is not evidence of pending work.

### await_idle

The production function wraps `IdleDetector` in a polling loop:

- Polls every 50ms via `spawn_blocking` (process tree reads are sync
  `/proc` filesystem operations).
- Tracks cumulative CPU time against a 60-second budget (6000
  centiseconds). If the server consumes 60 CPU-seconds without
  settling, the budget is exhausted and the caller proceeds with
  whatever diagnostics are available.
- Pauses during `Busy(n)` lifecycle state — progress tokens are
  explicit activity declarations, so tree walking is unnecessary.
- Detects root process death (empty snapshot, zombie root, missing
  PID) and transitions to `Dead`.
- Respects a `CancellationToken` for MCP-level cancellation.

Returns one of three outcomes: `Settled` (server is idle),
`BudgetExhausted` (timeout), or `RootDied` (process gone).

## Wait primitives

`LspClientManager` provides three wait primitives that compose
`LspClient::wait_ready`:

| Primitive | Behavior |
|---|---|
| `wait_ready_for_path(path)` | Waits for every server bound to the path's language. |
| `wait_ready_all()` | Waits for every active instance across all languages. |
| `ensure_and_wait_for_paths(paths)` | Spawns missing servers for discovered languages, then waits for all. |

`wait_ready` on `LspClient` watches the lifecycle enum — it wakes on
every lifecycle transition and returns `true` for `Healthy` or
`Probing` (both accept requests), `false` for `Failed` or `Dead`. No
budget, no tick counting, no process sampling at this level. Servers
that pass health are waited for patiently; `Connection::request`
handles individual stuck requests with its own failure detection.

The typical tool call sequence is: `wait_ready_for_path` then
filesystem change notifications then `get_servers` then dispatch. By
the time `get_servers` runs, capability state is populated and the
capability filter is reliable.

## LspClientManager

`LspClientManager` is the lifecycle authority. It owns the client map
(`HashMap<InstanceKey, Arc<Mutex<LspClient>>>`), spawns and shuts down
instances, manages document state, and provides the routing interface.

Key operations:

- **`spawn_all`** — initial startup. Walks workspace roots, classifies
  files, detects languages, spawns servers. Handles workspace folder
  exclusion for project-scoped roots.
- **`ensure_server`** — lazy spawn for a single language/server/root.
  Checks for project-scope first; delegates to `spawn_inner`.
- **`sync_roots`** — mid-session root changes. Diffs roots, notifies
  workspace-capable servers via `didChangeWorkspaceFolders`, spawns /
  shuts down per-root instances, loads project configs.
- **`get_servers`** — the routing entry point. See [Routing &
  Dispatch](routing.md#the-get_servers-interface).
- **`shutdown_all`** — sends `shutdown` + `exit` to every live
  instance.

## Related pages

- [Routing & Dispatch](routing.md) — how files resolve to server
  handles via `get_servers`.
- [Session Lifecycle](session-lifecycle.md) — when `spawn_all` runs
  and how roots are managed.
- [Configuration Model](configuration.md) — server definitions,
  language bindings, project config.
- [Document Lifecycle & File Watching](documents.md) — document sync
  pipeline and `didChangeWatchedFiles`.
