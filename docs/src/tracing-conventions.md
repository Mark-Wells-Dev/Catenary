# Tracing Conventions

Catenary uses the `tracing` crate for all logging and telemetry.
`LoggingServer` subscribes to every `tracing` event and dispatches to
two sinks: a message DB (protocol messages and internal traces, with
TUI broadcast) and a user-notification queue.

## Severity guidelines

Events at `warn` and `error` reach the user-notification queue by
default (configurable via `[notifications] threshold`). Choose severity
by asking:

| User cares? | Actionable? | Frequent? | Severity |
|---|---|---|---|
| No | — | — | `debug!()` |
| Yes | No | — | `info!()` |
| Yes | Yes | Very | `warn!()` / `error!()` + verify dedup fields |
| Yes | Yes | Rare | `warn!()` / `error!()` |

Use `error!()` only for conditions that indicate a systemic failure
(e.g., root resolution failed, critical I/O error). Use `warn!()` for
degradation that the user should know about but that Catenary can
recover from (e.g., server died, roots/list failed).

### Server-forwarded events are firehose-only

Events forwarded verbatim from an LSP server's `window/logMessage` or
`window/showMessage` (tagged `source = lsp.logging` at the forwarding
site) **never** reach the notification queue, regardless of their mapped
severity. A server's `showMessage` type 1 maps to `error`, but it is
still just that server's own chatter about itself — and the maintainer
ruled (CatenaryInternal misc 125) that the notification queue is reserved
for Catenary's **own** user-actionable events. Server chatter stays fully
queryable in the JSONL firehose and visible on the TUI; a genuinely
broken server already surfaces where it matters (the `unavailable:`
banner on the diagnostics receipt). The `[notifications] threshold` gate
applies only to Catenary's own events.

## Reserved structured fields

```
kind       — "lsp" | "mcp" | "hook" — routes to protocol DB sink
method     — Protocol method name (LSP/MCP method)
server     — LSP server name ("rust-analyzer", "pylsp", ...)
client     — Client identifier ("claude-code", "antigravity")
request_id — In-process correlation id (i64)
parent_id  — Correlation id of the causing event (i64)
source     — Subsystem that emitted the event (see taxonomy below)
language   — Language id ("rust", "python", ...)
payload    — Raw protocol JSON string (for kind = lsp|mcp|hook)
```

Notification dedup key: `(source, server, language, message_stem)`.
Events at `warn`/`error` level should include these fields where
applicable so notifications with the same identity collapse.

## Source taxonomy

The `source` field uses a fixed two-level `subsystem.concern` taxonomy.
The `Source` enum in `src/source.rs` is the single source of truth;
convenience constants are derived from it for use in `tracing` macros.

### Subsystems

| Subsystem | Scope |
|---|---|
| `config` | Configuration loading and validation |
| `daemon` | Daemon process (socket listeners, connection management) |
| `hook` | Hook layer (pre/post tool hooks) |
| `logging` | Logging infrastructure itself |
| `lsp` | LSP client layer (server communication, lifecycle, routing) |
| `mcp` | MCP server layer (host communication, dispatch) |

### Concerns

| Concern | Meaning |
|---|---|
| `bootstrap` | Startup sequencing |
| `dispatch` | Message routing, method dispatch, capability checks |
| `lifecycle` | Spawn, init, crash, recovery, shutdown |
| `logging` | Forwarded server window messages (`window/logMessage`, `window/showMessage`) |
| `parse` | Parsing and deserialization |
| `stderr` | Raw server process stderr output |
| `validation` | Semantic correctness checks |

### Valid combinations

| Source | Description | Constant |
|---|---|---|
| `config.parse` | Config loading errors (TOML parsing, deserialization) | `ConfigParse` |
| `config.validation` | Semantic config errors (orphan servers, unsupported keys) | `ConfigValidation` |
| `daemon.dispatch` | Connection accept, correlation, session routing | `DaemonDispatch` |
| `daemon.lifecycle` | Daemon startup, shutdown, signal handling | `DaemonLifecycle` |
| `hook.dispatch` | Hook request routing and dispatch | `HookDispatch` |
| `logging.bootstrap` | Logging infrastructure startup sequencing | `LoggingBootstrap` |
| `lsp.dispatch` | LSP message routing, method dispatch, capability checks | `LspDispatch` |
| `lsp.lifecycle` | Server spawn, init, crash, recovery, shutdown | `LspLifecycle` |
| `lsp.logging` | Server `window/logMessage` / `window/showMessage` telemetry (firehose-only, never promoted to the notification queue) | `LspLogging` |
| `lsp.stderr` | Raw server process stderr output | `LspStderr` |
| `mcp.dispatch` | MCP message dispatch and roots handling | `McpDispatch` |

Not every subsystem uses every concern. Only the combinations listed
above are valid. New values must be added as variants to the `Source`
enum in `src/source.rs`.

## Protocol events

Protocol boundary components (`McpServer`, `Connection`/`LspServer`,
`HookServer`) emit structured `tracing::info!()` events with `kind`,
`method`, `request_id`, `parent_id`, and `payload` fields. These are
routed to the message DB sink by the `kind` field and do not reach
the notification queue.
