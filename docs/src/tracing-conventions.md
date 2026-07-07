# Tracing Conventions

Catenary uses the `tracing` crate for all logging and telemetry.
`LoggingServer` subscribes to every `tracing` event and dispatches to
its sinks: the per-root-sharded **JSONL firehose** (`catenary query`,
everything), the **desktop-notification sink** (error severity, the urgent
interrupt), and — in the daemon — the **state.json snapshot writer** (the
TUI health surface). The store-and-forward `systemMessage` notification queue
retired in the TUI rework: warns no longer interrupt a transcript, they
persist as health findings on the dashboard.

## Severity guidelines

Severity chooses the delivery channel:

- **`error!()`** — reaches the desktop-notification sink (the OS-level urgent
  interrupt, deduped per daemon lifetime, suppressible via
  `[notifications] desktop = false` or `CATENARY_NOTIFY=0`) **and** the TUI
  health surface **and** the firehose.
- **`warn!()`** — reaches the TUI health surface (a warn *is* a health finding
  — stale hooks, version skew, 027 degradation) **and** the firehose. It no
  longer interrupts.
- **`info!()` / `debug!()`** — the firehose only.

Choose severity by asking:

| User cares? | Actionable? | Interrupt-worthy? | Severity |
|---|---|---|---|
| No | — | — | `debug!()` |
| Yes | No | — | `info!()` |
| Yes | Yes | Yes (systemic failure) | `error!()` |
| Yes | Yes | No (recoverable / a health finding) | `warn!()` |

Use `error!()` only for conditions that indicate a systemic failure
(e.g., root resolution failed, critical I/O error) — an error fires a desktop
notification, so it must earn the interrupt. Use `warn!()` for
degradation that the user should know about but that Catenary can
recover from (e.g., server died, roots/list failed); it surfaces durably on
the dashboard rather than interrupting.

### Server-forwarded events are firehose-only

Events forwarded verbatim from an LSP server's `window/logMessage` or
`window/showMessage` are tagged `source = lsp.logging` at the forwarding
site. A server's `showMessage` type 1 maps to `error`, but it is still just
that server's own chatter about itself — not Catenary's own user-actionable
event — so the maintainer ruled (CatenaryInternal misc 125) it is
firehose-only and belongs on the TUI's secondary Activity/Alerts surface, never
the desktop interrupt. It stays fully queryable in the JSONL firehose; a
genuinely broken server surfaces where it matters (a routed-but-broken server
is a health finding).

## Reserved structured fields

```
kind       — "lsp" | "mcp" | "hook" — marks a protocol event in the firehose
method     — Protocol method name (LSP/MCP method)
server     — LSP server name ("rust-analyzer", "pylsp", ...)
client     — Client identifier ("claude-code", "antigravity")
request_id — In-process correlation id (i64)
parent_id  — Correlation id of the causing event (i64)
source     — Subsystem that emitted the event (see taxonomy below)
language   — Language id ("rust", "python", ...)
payload    — Raw protocol JSON string (for kind = lsp|mcp|hook)
```

`warn!()`/`error!()` events should carry `source`, `server`, and `language`
where applicable: they key the firehose query surface (`catenary query`) and
the health finding an event maps to, so an event with a stable identity groups
cleanly instead of scrolling by as noise.

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
| `lsp.logging` | Server `window/logMessage` / `window/showMessage` telemetry (firehose-only, never a desktop interrupt) | `LspLogging` |
| `lsp.stderr` | Raw server process stderr output | `LspStderr` |
| `mcp.dispatch` | MCP message dispatch and roots handling | `McpDispatch` |

Not every subsystem uses every concern. Only the combinations listed
above are valid. New values must be added as variants to the `Source`
enum in `src/source.rs`.

## Protocol events

Protocol boundary components (`McpServer`, `Connection`/`LspServer`,
`HookServer`) emit structured `tracing::info!()` events with `kind`,
`method`, `request_id`, `parent_id`, and `payload` fields. At `info`
severity they land in the firehose only — never a desktop interrupt.
