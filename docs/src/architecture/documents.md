# Document Lifecycle & File Watching

When an agent calls a Catenary tool that touches a file — grep needs
hover information, glob needs a symbol outline, diagnostics need error
lists — LSP requires that file to be explicitly "opened" on the
server before any request can be sent. Catenary manages this lifecycle
entirely: the agent never sends `didOpen` or `didClose` directly.

This page covers two interconnected subsystems: document lifecycle
(how files move through open/close states on language servers) and
file watching (how servers learn about filesystem changes that happen
outside the document sync pipeline).

## Two open paths

Document opens follow the same split as
[dispatch](routing.md#dispatch-model): request/response methods and
diagnostics have different needs, so they have different open paths.

### `open_document_on` — targeted open

Used by request/response dispatch. The caller gets an ordered list
of clients from [`get_servers`](routing.md#the-get_servers-interface)
and opens the file on each as it iterates the priority chain:

```
tool call → get_servers(path, capability) → [client_a, client_b, ...]
  for each client:
    open_document_on(path, client) → didOpen or didChange
    send request
    if non-empty result: return (done)
```

The caller controls which servers see the file. A server that
fails the capability check or `file_patterns` filter in `get_servers`
never gets an open.

### `diagnostic_servers` — broadcast open

Used by the `done_editing` pipeline. `diagnostic_servers` on
`LspClientManager` returns every server where `diagnostics_enabled`
is true for the file's language binding. It applies both the
capability gate (`supports_diagnostics`) and the config-level filter
(language-level AND per-binding `diagnostics` flags). Every qualifying
server receives the file via `open_document_on` and produces
diagnostics independently.

## Per-client version tracking

Each server gets an independent monotonic version sequence. The first
`open_document` call for a URI returns `(first_open: true, version: 1)`
and sends `textDocument/didOpen`. Subsequent calls increment the
version and return `(false, version)` — the caller sends
`textDocument/didChange` with the full file content.

This per-client tracking means multi-server dispatch gives each server
a clean sequence starting at 1, regardless of how many other servers
have the same file open. LSP requires monotonically increasing
versions per server — sharing a global counter across servers would
create gaps that some servers reject.

## Stateless document lifecycle

Outside editing mode, Catenary uses a stateless document lifecycle:
**open → request → close** per tool call. No document state
accumulates across calls. After each tool dispatch, any file that was
opened for that request is closed.

This is a deliberate design choice from the waitv2 rewrite. Stateless
lifecycle eliminates migration concerns when routing changes mid-session
— for example, when `/add-dir` shifts which server handles a file, or
when a project-scoped server is spawned that shadows a workspace
instance. There is no accumulated document state to reconcile when
ownership changes.

The cost is that every tool call re-reads the file and sends the full
content. In practice this is cheap: files are already in the OS page
cache from the agent's own reads, and the `didOpen`/`didClose`
round-trip is a pair of notifications (no server response to wait for).

## Editing mode

Editing mode is Catenary's primary user-facing innovation for
diagnostic batching. It exists to solve a specific problem: AI agents
make many rapid edits, and per-edit diagnostics are noisy, slow, and
often stale by the time they arrive.

### The problem

Without editing mode, each file edit triggers a diagnostic cycle:
open the file on all diagnostic-enabled servers, wait for each to
settle, collect diagnostics, return them to the agent. For a typical
refactoring that touches 10 files, that is 10 separate diagnostic
cycles — each with its own settle wait. Worse, intermediate
diagnostics are misleading: renaming a type in `lib.rs` produces
errors in every file that imports it, but those errors will be fixed
by the next edit.

### The solution

`start_editing` and `done_editing` bracket a batch of file edits.
During editing mode:

- **No LSP traffic for intermediate edits.** The agent edits freely
  with the host CLI's native Edit/Write tools. No `didOpen`,
  no `didChange`, no diagnostic retrieval per edit.
- **Path accumulation.** The `PostToolUse` hook detects edit-tool
  calls and accumulates the modified file paths in `EditingManager`.
  Paths are deduplicated — editing the same file twice records it
  once.
- **Boundary enforcement.** The `PreToolUse` hook enforces editing
  mode boundaries. Only edit-related tools (Edit, Write, Read, and
  filesystem Bash commands like `rm`, `cp`, `mv`) are allowed during
  editing. Attempting to call grep, glob, or any other tool produces
  a denial message telling the agent to call `done_editing` first.
  Conversely, attempting to use Edit/Write on workspace files without
  `start_editing` first produces a denial.
- **Batched diagnostics.** When the agent calls `done_editing`, the
  `DiagnosticsServer` runs a single consolidated diagnostic pipeline
  across all modified files.

### The `done_editing` pipeline

`done_editing` triggers a multi-phase pipeline on `DiagnosticsServer`:

1. **File change notifications.** `notify_file_changes()` runs first,
   so servers know about any filesystem changes (new files, deletes)
   before the diagnostic cycle.

2. **Resolve and group.** Modified files are canonicalized, validated
   against workspace roots, and grouped by diagnostic-enabled server.
   Files outside workspace roots or with no server coverage are
   categorized as N/A.

3. **Per-server batch lifecycle.** For each server, the pipeline runs:
   - **Open all files** — `open_document_on` for every file in the
     server's group. The server sees the complete final state of all
     files simultaneously.
   - **Settle** — wait for the server to finish processing via the
     [idle detection model](lsp-client.md#idle-detection-and-settle).
   - **Health probe** — if the server is still in `Probing` state, run
     an explicit health check.
   - **`didSave` all** — triggers flycheck on servers that only
     produce diagnostics on save (e.g., rust-analyzer runs
     `cargo check` on `didSave`).
   - **Settle again** — wait for flycheck to complete.
   - **Retrieve diagnostics** — read per-file diagnostics from the
     server's cache.
   - **Close all** — `didClose` for every opened file.

4. **Format output.** Results are categorized: files with diagnostics
   get per-line error/warning output, clean files are grouped on one
   line, N/A files are grouped separately.

5. **`mark_current`** — refreshes the filesystem cache for all
   processed files, preventing the next `diff()` from reporting them
   as changed (see [interaction with editing mode](#interaction-with-editing-mode)
   below).

Cross-file diagnostics are correct because each server sees the
complete final state before producing diagnostics. A renamed type
in `lib.rs` and its updated imports in `main.rs` are both open on
the server simultaneously — the server produces diagnostics that
reflect the fully consistent state.

### State ownership

`EditingManager` holds the in-memory editing state: a map from
`agent_id` to accumulated file paths. Both the `HookRouter` (which
has the real `agent_id` from the host CLI) and the `McpRouter` (which
produces the tool result) access it through `Session`.

The MCP `start_editing` and `done_editing` tools are triggers — the
`PreToolUse` hook owns the state transition for `start_editing`
(because it has the `agent_id`), and the `McpRouter` handles the
diagnostic pipeline for `done_editing` (because it produces the tool
result). `SessionStart` clears any stale editing state from a
previous agent context.

## File watching

File watching is separate from document lifecycle.
`workspace/didChangeWatchedFiles` notifies servers about filesystem
changes that happen outside the document sync pipeline — new files
created by the agent, files deleted, files modified by Bash commands
or external tools.

### Why not a traditional file watcher

Most LSP clients use a background file watcher (inotify on Linux,
FSEvents on macOS) that fires events continuously. Catenary doesn't:

- **Zero idle overhead.** No background watcher thread, no inotify
  watches, no file descriptor budget to manage. Between tool calls,
  Catenary does nothing.
- **No platform-specific dependencies.** No `notify` crate, no
  `inotify`, no `FSEvents`, no `ReadDirectoryChangesW`. Directory
  walking uses the `ignore` crate, which is already a dependency for
  `FilesystemManager`.
- **No inotify watch limits.** Large monorepos can exceed the default
  8192 inotify watch limit. Catenary never hits this because it
  doesn't register watches.

The tradeoff: changes are not detected instantly. They are detected at
the next tool boundary, which is when they matter — that is when the
agent is about to interact with servers.

### Snapshot-and-diff model

`FilesystemManager` implements a snapshot-and-diff model:

1. **`seed()`** at session start — walks all workspace roots and
   records `(path, mtime)` for every file and directory. Stat-only,
   no content read. Respects `.gitignore` via the `ignore` crate.

2. **`diff()`** at tool boundaries — walks all roots again, compares
   current `(path, mtime)` to the cached snapshot, and produces a
   change set: `Created`, `Changed`, or `Deleted` entries. Updates the
   cache to reflect current disk state.

3. **Glob matching.** Changes are matched against per-server glob
   registrations. Servers register interest in file patterns via
   `client/registerCapability` for
   `workspace/didChangeWatchedFiles`. Each registration carries glob
   patterns and watch kinds (create, change, delete). Only changes
   that match a registration's patterns and watch kinds are delivered.

4. **Batched notification.** Matching changes are sent as a single
   `workspace/didChangeWatchedFiles` notification per server. The
   notification carries an array of `(uri, changeType)` pairs.

### Registration management

Glob registrations live on `LspServer`. They are populated via
`client/registerCapability` (the server declares what file patterns
it wants to watch) and cleared via `client/unregisterCapability`.
Registration IDs are tracked so that specific registrations can be
unregistered without affecting others.

The `file_watcher_snapshot` method on `LspServer` returns a clone
of the current registrations for matching. The snapshot approach
avoids holding the registration lock during the (potentially slow)
glob matching and notification loop.

### Interaction with editing mode

`done_editing` calls `FilesystemManager::mark_current()` after
processing diagnostics. This re-stats every processed file and
updates its mtime in the cache. Without this step, the next
`diff()` at the next tool boundary would report every edited file
as `Changed`, and servers would receive redundant
`didChangeWatchedFiles` events for files they have already seen
the final content of (via `didOpen`/`didChange` during the
diagnostic pipeline).

For files deleted during editing, `mark_current` removes them from
the cache — the next `diff()` will not report them as deleted again.

### When notifications fire

`notify_file_changes()` runs at two points:

- **Before every non-editing tool call.** In `McpRouter::call_tool`,
  file change notification runs before grep and glob dispatch. This
  ensures servers have up-to-date awareness of the filesystem state
  before the agent queries them.
- **At the start of `done_editing`.** The diagnostic pipeline calls
  `notify_file_changes()` first, so servers know about any file
  creates or deletes before receiving `didOpen` for modified files.

The notification is a no-op when `diff()` returns an empty change
set, which is the common case when the agent has not modified the
filesystem since the last tool call.

## Related pages

- [Routing & Dispatch](routing.md) — how files resolve to server
  handles, priority chain vs. diagnostic concatenation.
- [LSP Client Layer](lsp-client.md) — connection management,
  capabilities, settle/idle detection used by the diagnostic pipeline.
- [Session Lifecycle](session-lifecycle.md) — when `seed()` runs,
  editing mode in the serving loop.
- [Configuration Model](configuration.md) — `diagnostics` flags on
  language bindings and servers.
- [Logging, Hooks & TUI](logging-hooks-tui.md) — hook integration
  for editing mode enforcement.
