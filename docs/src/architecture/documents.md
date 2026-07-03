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

Used by the `catenary diagnostics` pipeline. `diagnostic_servers` on
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
— for example, when `catenary roots add` shifts which server handles a file, or
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

Editing mode brackets a batch of file edits. It starts **implicitly** on
the first edit to a server-covered file — there is no separate `editing
start` step to race against parallel tool calls — and ends when the agent
runs `catenary diagnostics`. During editing mode:

- **No LSP traffic for intermediate edits.** The agent edits freely
  with the host CLI's native Edit/Write tools (or native `sed -i`, whose
  writes the hook resolves and tracks). No `didOpen`, no `didChange`, no
  diagnostic retrieval per edit.
- **Path accumulation.** The `PreToolUse` hook detects edit-tool
  calls and accumulates the modified file paths in `EditingManager`.
  Paths are deduplicated — editing the same file twice records it
  once. A path is tracked only if a configured server would cover it
  (coverage-gated): doc-only or no-server edits accumulate nothing and
  never enter editing mode, so no diagnostics would be produced for them.
- **Boundary enforcement.** While a **non-empty covered tracked set**
  is pending, the `PreToolUse` hook blocks tool calls that would leave
  the edit batch. Read/Write, `ToolSearch`, filesystem-only Bash (`rm`,
  `cp`, `mv`), and canonical Catenary commands (`grep`/`glob`,
  the lifecycle commands) stay allowed; everything else is denied with a
  message that lists the tracked files and tells the agent to run
  `catenary diagnostics`. The block gates on the tracked set, not an
  editing-mode bit — an empty set flows free, so friction tracks value.
- **Batched diagnostics.** When the agent runs `catenary diagnostics`,
  the `DiagnosticsServer` runs a single consolidated diagnostic pipeline
  across all modified files. Naming paths (`catenary diagnostics <paths>`)
  scopes the pipeline to exactly those files and pays only their share of
  the gate — a partial pull leaves the tracked set (and the boundary
  block) armed for the files not yet diagnosed.

### The `catenary diagnostics` pipeline

`catenary diagnostics` triggers a multi-phase pipeline on
`DiagnosticsServer`:

1. **File change notifications.** The diagnostics stat-walk doubles as
   change detection: changed files are diffed against each root's mtime
   baseline (`diff_and_update`) and delivered to servers as a per-server
   changed-set (`nudge_changed_set`) first, so servers know about any new
   or deleted files before the diagnostic cycle.

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

5. **Baseline already current.** No separate cache-refresh pass is
   needed: the change-detection walk in step 1 already recorded these
   files' mtimes in each root's baseline, so the next walk will not
   re-report them as changed (see [interaction with editing
   mode](#interaction-with-editing-mode) below).

Cross-file diagnostics are correct because each server sees the
complete final state before producing diagnostics. A renamed type
in `lib.rs` and its updated imports in `main.rs` are both open on
the server simultaneously — the server produces diagnostics that
reflect the fully consistent state.

### State ownership

`EditingManager` holds the in-memory editing state: a map from
`agent_id` to accumulated file paths. Both the `HookRouter` (which
has the real `agent_id` from the host CLI) and the IPC router (which
handles CLI commands) access it through `Session`.

Editing state transitions are owned by the `PreToolUse` hook (because it
has the `agent_id`): the first covered edit implicitly enters editing
mode and accumulates the path. `catenary diagnostics` is a CLI command
invoked via the host's shell tool; the IPC router handles its diagnostic
pipeline (because it produces the stdout output) and clears the tracked
set. `SessionStart` clears any stale editing state from a previous agent
context. (`catenary editing start` survives only as an idempotent no-op
for a stray invocation — it is not part of the agent-facing workflow.)

## File watching

File watching is separate from document lifecycle.
`workspace/didChangeWatchedFiles` notifies servers about filesystem
changes that happen outside the document sync pipeline — new files
created by the agent, files deleted, files modified by Bash commands
or external tools.

### Why not a traditional file watcher

Most LSP clients use a background file watcher (inotify on Linux,
FSEvents on macOS) that fires events continuously over the whole
workspace. Catenary doesn't — it detects changes by walking at query
time, the same walk `grep`/`glob`/`diagnostics` already perform:

- **Zero idle overhead.** No background watcher over workspace files, no
  recursive inotify watches, no file-descriptor budget to manage.
  Between tool calls, Catenary does nothing.
- **No sub-`O(tree)` consumer to accelerate.** `grep` is `O(tree)` and
  asks a fresh question each time, so an incremental change feed would
  not speed it up. Walking on demand yields change detection for free,
  off work the query already does.
- **No watch limits.** Large monorepos can exceed the default inotify
  watch limit; Catenary never hits it because it registers no recursive
  workspace watches. Directory walking uses the `ignore` crate (already a
  `FilesystemManager` dependency).

The tradeoff: changes are not detected instantly. They are detected at
the next tool boundary, which is when they matter — that is when the
agent is about to interact with servers.

(Catenary does use the `notify` crate for one narrow, unrelated purpose:
a bounded, non-recursive directory-deletion watch on subagent worktree
roots, for teardown — not workspace-file watching. See [Logging, Hooks &
TUI](logging-hooks-tui.md#worktree-root-teardown).)

### Walk-on-demand change detection

`FilesystemManager` detects changes by walking on demand and diffing
against a per-root mtime baseline:

1. **First walk is the baseline.** There is no separate seed step. The
   first walk of a root records `(path, mtime)` for every file (stat-only,
   no content read; `.gitignore` respected via the `ignore` crate) and
   establishes that root's baseline.

2. **`diff_and_update` on each walk.** A later walk compares the current
   `(path, mtime)` against the baseline, produces a change set —
   `Created`, `Changed`, or `Deleted` — then merges the new mtimes back
   into the baseline. (The first walk's whole observation is reported as
   `Changed` — the cold snapshot.)

3. **Per-server routing.** Each covering server's registered watchers
   are snapshotted; observations are filtered to the union of their
   globs, then fanned out so each server receives only the changes that
   match its own globs *and* watch-kind mask (create / change / delete).
   Servers register interest via `client/registerCapability` for
   `workspace/didChangeWatchedFiles`.

4. **Precise notification.** Each server gets a single
   `workspace/didChangeWatchedFiles` carrying only its matching changes,
   as `(uri, changeType)` pairs whose `changeType` is the true semantic
   kind (Created → 1, Changed → 2, Deleted → 3). It is never a broad
   unconditional poke.

### Registration management

Glob registrations live on `LspServer`. They are populated via
`client/registerCapability` (the server declares what file patterns it
wants to watch) and cleared via `client/unregisterCapability`.
Registration IDs are tracked so specific registrations can be removed
without affecting others. Each watcher compiles its glob with LSP 3.17
semantics and carries a watch-kind mask; the matcher gates on both.

The registrations are snapshotted before matching, so the registration
lock is not held during the (potentially slow) glob matching and
notification loop.

### Interaction with editing mode

The change-detection walk at the start of `catenary diagnostics` also
updates each root's mtime baseline, recording the edited files at their
final mtime. A later walk therefore does not re-report them as
`Changed`, and servers do not receive redundant `didChangeWatchedFiles`
events for content they have already seen (via `didOpen`/`didChange`
during the diagnostic pipeline). Deletions are folded into the baseline
the same way, so they are not reported twice.

### When notifications fire

Change detection rides each query's walk, so a notification can fire on:

- **`grep`** — the search walk collects mtimes and feeds the changed-set
  before results are returned.
- **`glob`** — the scoped directory scan (`nudge_scoped`) does the same
  for the listed paths.
- **`catenary diagnostics`** — the stat-walk runs first, so servers know
  about any creates or deletes before receiving `didOpen` for modified
  files.

The notification is a no-op when the walk finds no changes — the common
case when the agent has not touched the filesystem since the last query.

## Related pages

- [Routing & Dispatch](routing.md) — how files resolve to server
  handles, priority chain vs. diagnostic concatenation.
- [LSP Client Layer](lsp-client.md) — connection management,
  capabilities, settle/idle detection used by the diagnostic pipeline.
- [Session Lifecycle](session-lifecycle.md) — daemon startup and the
  serving loop, including change detection on each query walk.
- [Configuration Model](configuration.md) — `diagnostics` flags on
  language bindings and servers.
- [Logging, Hooks & TUI](logging-hooks-tui.md) — hook integration
  for editing mode enforcement.
