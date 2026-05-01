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

## Reserved structured fields

```
kind       — "lsp" | "mcp" | "hook" — routes to protocol DB sink
method     — Protocol method name (LSP/MCP method)
server     — LSP server name ("rust-analyzer", "pylsp", ...)
client     — Client identifier ("claude-code", "gemini-cli")
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

| Source | Subsystem |
|---|---|
| `config.parse` | Config loading errors |
| `config.validation` | Semantic config errors |
| `lsp.lifecycle` | Server spawn / init / crash / recovery |
| `lsp.logging` | Server `window/logMessage` telemetry |
| `lsp.stderr` | Raw server process stderr output |
| `lsp.protocol` | Protocol-level failures not tied to a specific server |
| `mcp.dispatch` | MCP message dispatch and roots |
| `hook.router` | Hook request routing |
| `bridge.routing` | File-to-server routing errors |
| `bridge.tool` | Tool-level diagnostics (glob, grep, etc.) |

## Protocol events

Protocol boundary components (`McpServer`, `Connection`/`LspServer`,
`HookServer`) emit structured `tracing::info!()` events with `kind`,
`method`, `request_id`, `parent_id`, and `payload` fields. These are
routed to the message DB sink by the `kind` field and do not reach
the notification queue.
