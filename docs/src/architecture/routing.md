# Routing & Dispatch

Every file that enters Catenary — through grep, glob, diagnostics, or
any other tool — needs to resolve to one or more language server
handles. This page explains how that resolution works: from file path
to language, from language to server bindings, from bindings to live
server instances.

The motivating case is PKGBUILD files. A PKGBUILD is shellscript, but
it benefits from two servers: `termux-language-server` for
package-specific hover and diagnostics, and `bash-language-server` for
shell fundamentals (definitions, references, symbols). The entire
routing system exists to make this kind of multi-server dispatch
correct and predictable.

## File classification

Classification — "what language is this file?" — is config-driven.
Three dimensions, checked in precedence order (highest first):

1. **Shebang** — the file's `#!` line declares its interpreter.
2. **Filename** — exact filename match (e.g., `PKGBUILD`, `Makefile`).
3. **Extension** — file extension match (e.g., `.rs`, `.ts`).

Each tier short-circuits: if a shebang match is found, filename and
extension checks are skipped. The merged config (defaults + user +
project) is the sole source of classification data — no hardcoded
fallback tables exist. See the
[Configuration Model](configuration.md#classification) page for full
detail on how classification tables are built and layered.

For the PKGBUILD example: the file has no extension and no shebang,
but the default config has `filenames = ["PKGBUILD"]` on
`[lsp.language.shellscript]`. Filename match → `shellscript`.

## Three-tier routing model

Once a file has a language, Catenary resolves which server instance(s)
handle it. The model has three tiers, tried in order for a file at
path P:

### Tier 1 — Project-scoped

If P's workspace root has a `.catenary.toml` with a `[lsp.language.X]`
entry for P's language, the instance is bound to that root. Separate
process, isolated config. The instance always uses `Scope::Root(root)`
regardless of whether the server supports `workspaceFolders`.

This is Rule A from the configuration model: the presence of
`[lsp.language.X]` in a project config is the signal for isolation. Users
opt in explicitly by writing the entry. See
[Tier promotion](configuration.md#tier-promotion) for the full
resolution matrix.

### Tier 2 — User-scoped

P is inside an active workspace root with no project config override
for its language. Two sub-cases based on server capabilities:

- **Workspace-capable servers** (those that support
  `workspaceFolders`) share one instance across all roots. The
  instance uses `Scope::Workspace` and receives
  `didChangeWorkspaceFolders` notifications as roots are added or
  removed.

- **Legacy servers** (no `workspaceFolders` support) get a separate
  instance per root, each using `Scope::Root(root)`. They cannot be
  told about multiple roots, so each process sees only its own.

### Tier 3 — Single-file

P is outside all active workspace roots. A server marked
`single_file = true` in `[lsp.server.*]` is spawned with a null workspace
(`rootUri: null`, `workspaceFolders: null`) to serve the file with a
`Scope::SingleFile` instance. If the server rejects null-workspace
initialization, the `(language, server)` pair is negative-cached so it
is not retried. Servers without `single_file = true` are skipped — a
file outside all roots resolves to nothing for them.

### Roots are explicit (pinned), with ephemeral activity mounts

Roots added via `--root`, `catenary roots add`, or the MCP workspace-roots
channel are **pinned**: active for the session's lifetime. Catenary does not
auto-discover pinned roots from file paths — implicit pinned discovery would
make the routing model hard to predict, especially in multi-root sessions where
adjacent directories might contain unrelated projects.

A `catenary grep`, `glob`, or `diagnostics` touching a path **outside every
mounted root** is the one exception: Catenary detects the enclosing project root
(walking `.git` up from the path) and mounts it as an **ephemeral** root so the
query is enriched/diagnosed from a real server. An ephemeral mount is scoped to
the single enclosing root (never a sibling, no companion templating) and
**expires after a few minutes of inactivity** — every qualifying activity
refreshes its idle clock, and `catenary roots add` on it upgrades it to pinned.
`catenary roots ls` and the `state.json` root board distinguish the two classes.
A file with no detectable enclosing project root still has no owning root; it
routes only to single-file-capable servers (tier 3).

## Instance keying

Every live server instance is identified by an `InstanceKey` — a
three-part identity:

```
InstanceKey { language_id, server, scope }
```

All three components are necessary. Without any one of them,
collisions occur:

- **Without `language_id`:** `clangd` serving C and C++ would
  collapse to one entry. But the two languages may have different
  dispatch priorities (C++ might have a second server that C doesn't).
- **Without `server`:** `termux-language-server` and
  `bash-language-server` for shellscript would collide.
- **Without `scope`:** A project-scoped rust-analyzer for root A and
  a workspace-scoped rust-analyzer would share a key, but they are
  distinct processes with distinct configs.

The `Scope` enum has three variants:

| Variant | Meaning |
|---|---|
| `Workspace` | Shared across roots. One instance per (language, server) pair. |
| `Root(PathBuf)` | Bound to a specific root. Used for legacy servers and project-scoped instances. |
| `SingleFile` | Tier 3. A `single_file = true` server spawned with a null workspace for a file outside all roots. |

### Instance lookup

`find_instance` resolves a `(language, server, root)` triple to a live
client by trying `Scope::Root(root)` first, then `Scope::Workspace`.
Root-first ordering is essential: when a project-scoped instance and a
workspace instance both exist for the same language and server, the
project-scoped instance must win for files in its root. Without this
ordering, project-scoped isolation would be silently bypassed.

## Dispatch model

Once routing resolves the candidate servers, dispatch determines how
results are collected. There are two separate paths, because
request/response methods and diagnostics have fundamentally different
semantics.

### Request/response — priority chain

For methods like `textDocument/definition` and
`textDocument/references`, the `servers` list order in `[lsp.language.*]`
defines priority. Dispatch iterates servers in that order:

1. Check capability — does this server support the method?
2. Send request.
3. If the response is non-empty, return it. Done.
4. If the response is empty or null, try the next server.

First non-empty result wins. No merging across servers.

Merging was rejected for two reasons. First, less-specific servers
produce noise: `bash-language-server` returns shell-level hover for a
PKGBUILD symbol that `termux-language-server` already explains with
package-specific context. Merging would show both, with no way to
signal which is authoritative. Second, non-list methods (hover,
definition) have ambiguous merge semantics — two hover results for the
same position can't be meaningfully combined.

For the PKGBUILD case: `termux-language-server` is listed first in
`servers`, so it gets first shot at every request. If it returns
nothing for a particular symbol (say, a shell builtin it doesn't
know about), `bash-language-server` handles it as fallback.

### Diagnostics — concatenation

Diagnostics are server-pushed, not request/response. Every server
with diagnostics enabled for the file's language binding receives
`didOpen` and produces diagnostics independently. Results are
concatenated — all servers contribute.

This is the right model because diagnostic domains are typically
non-overlapping. `termux-language-server` reports package validation
issues (missing dependencies, invalid fields). `bash-language-server`
reports shell syntax issues (unquoted variables, missing semicolons).
Both are useful; neither subsumes the other.

Opt-out is available at two levels:

- **Per-binding:** `{ name = "bash-language-server", diagnostics = false }`
  in the `servers` list suppresses diagnostics from that server for
  that language.
- **Language-level:** `diagnostics = false` on `[lsp.language.shellscript]`
  suppresses diagnostics from all servers for that language.

The effective filter is AND: both the language-level flag and the
per-binding flag must be true for diagnostics to be delivered from a
given server. This means language-level `false` is a wholesale kill
switch that overrides any per-binding setting.

## `file_patterns` filtering

`file_patterns` on `[lsp.server.*]` is a dispatch-layer narrowing
mechanism. It contains filename-level globs (matched against the
filename component, not the full path) that limit which files within
a language the server handles.

Servers without `file_patterns` handle all files for their language.
Servers with it only handle files whose name matches at least one
pattern.

`file_patterns` is applied inside `get_servers` before the capability
check. This means a server with non-matching `file_patterns` is never
considered — not for requests, not for diagnostics, not for document
lifecycle.

For the PKGBUILD case: `termux-language-server` has
`file_patterns = ["PKGBUILD", "*.ebuild"]`. When a file named
`install.sh` enters as `shellscript`, termux is filtered out by
`file_patterns` and only `bash-language-server` handles it. When
`PKGBUILD` enters, both servers pass the filter.

## The PKGBUILD walk-through

Putting it all together — a `textDocument/references` request for a
symbol in a file named `PKGBUILD`:

1. **Classification.** `PKGBUILD` has no extension. No shebang.
   Filename match against `[lsp.language.shellscript]` filenames →
   language is `shellscript`.

2. **Root resolution.** `FilesystemManager::resolve_root` finds the
   owning workspace root via longest-prefix match.

3. **Language config lookup.** `[lsp.language.shellscript]` has:
   ```toml
   servers = ["termux-language-server", "bash-language-server"]
   ```

4. **`file_patterns` filter.** `termux-language-server` has
   `file_patterns = ["PKGBUILD", "*.ebuild"]` — `PKGBUILD` matches.
   `bash-language-server` has no `file_patterns` — passes by default.

5. **Instance lookup.** For each server, `find_instance` checks
   `Scope::Root(root)` then `Scope::Workspace`. Returns the live
   client for each.

6. **Capability check.** Both servers support
   `textDocument/references`. Both pass.

7. **Priority chain dispatch.** `termux-language-server` is first in
   the list. Send `textDocument/references`. If it returns results,
   done. If empty, fall through to `bash-language-server`.

8. **Diagnostics (separate path).** Both servers have the file open
   (assuming diagnostics are enabled for both bindings).
   `termux-language-server` reports package validation issues.
   `bash-language-server` reports shell syntax issues. Both sets are
   concatenated in the diagnostic result.

## Dispatch errors

LSP-side errors during dispatch never reach the agent. All errors are
routed through `warn!()` via tracing, which `LoggingServer` delivers
to the user notification queue. The agent sees empty results or
whatever partial results were available from other servers in the
chain.

This separation is deliberate: the agent cannot act on "rust-analyzer
returned error code -32602." The user can — they can check their
config, restart the server, or file a bug. See the
[Logging, Hooks & TUI](logging-hooks-tui.md) page for the
three-audience model (agent, user real-time, user investigating).

Caller-input errors are different. Invalid regex patterns, bad file
paths, and other agent-supplied mistakes do surface in the tool result,
because the agent can fix them by adjusting its input.

## The `get_servers` interface

```rust
pub async fn get_servers(
    &self,
    path: &Path,
    capability: fn(&LspServer) -> bool,
    method: Option<DispatchMethod>,
) -> Vec<Arc<Mutex<LspClient>>>
```

This is the routing entry point. Every tool server calls it to resolve
a file path to an ordered list of server handles. The function:

1. Classifies the file to a language ID.
2. Resolves the owning workspace root — or, for a file outside all
   roots, falls through to the single-file tier.
3. Looks up the language config for server bindings.
4. Iterates bindings in priority order, filtering by per-binding
   `disabled_methods` (when a `method` is given), `file_patterns`,
   instance liveness, and the capability predicate.
5. Returns clients in binding order (priority order).

`get_servers` is non-blocking — it reads current capability state
without waiting for servers to finish initializing. Callers are
responsible for readiness via `wait_ready_for_path` or
`wait_ready_all` before invoking.

An empty result triggers a `warn!()` (unless the language has no
configured servers). Deduplication is handled by
`NotificationQueueSink` — the same "no server supports X" warning
won't spam the user across repeated tool calls.

A separate `diagnostic_servers` method wraps `get_servers` with the
diagnostics capability check and the additional config-level
`diagnostics_enabled` filter (language-level AND per-binding). This
is the entry point for `DiagnosticsServer`.

## Related pages

- [Configuration Model](configuration.md) — config sources, layering,
  classification, tier promotion.
- [Session Lifecycle](session-lifecycle.md) — when servers are spawned
  and how roots are discovered.
- [LSP Client Layer](lsp-client.md) — connection management,
  capabilities, settle/idle detection.
- [Logging, Hooks & TUI](logging-hooks-tui.md) — dispatch error
  routing and the three-audience model.
