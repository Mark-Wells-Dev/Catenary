# Notifications

Catenary routes user-facing notifications through the host CLI's
`systemMessage` field. This keeps operational information (server
crashes, config errors, degradation notices) visible to **you** without
polluting the agent's tool results.

## How it works

Every `tracing::warn!()` and `tracing::error!()` call in Catenary is a
potential notification. `LoggingServer` — Catenary's central tracing
subscriber — dispatches these events to a notification queue that
accumulates entries between drain points.

Server-forwarded LSP window messages (`window/logMessage` and
`window/showMessage`, tagged `source = lsp.logging`) are the one
exception: they are **firehose-only** and never enqueue, regardless of
severity. The queue is reserved for Catenary's own user-actionable
events, so a language server's own chatter — including a `showMessage`
type 1 that maps to `error` — stays queryable in the firehose and
visible on the TUI instead of interrupting you. (See CatenaryInternal
misc 125: such messages are typically transient, per-server hiccups, and
a genuinely broken server already shows up on the diagnostics receipt's
`unavailable:` banner.)

Notifications are delivered at **stationary points** — moments when
the host CLI naturally pauses to display system information:

| Hook | Drains? | Why |
|------|---------|-----|
| `SessionStart` | Yes | Fresh session — show startup warnings and anything from the previous cycle |
| `Stop` / `AfterAgent` (allow) | Yes | Agent is done — safe to surface accumulated notices |
| `Stop` / `AfterAgent` (block) | No | Agent must fix something first — preserve queue for next allow |
| `PreToolUse` | No | Mid-flight — don't interrupt |

## Configuration

The `[notifications]` section controls which events reach the queue:

```toml
[notifications]
threshold = "warn"    # default
```

| Value | Effect |
|-------|--------|
| `"debug"` | Everything (very verbose) |
| `"info"` | Informational and above |
| `"warn"` | Warnings and errors (default) |
| `"error"` | Errors only |

## Dedup

Notifications are deduplicated within a session. Two events with the
same identity key — `(source, server, language, message_stem)` — produce
only one queue entry. The message stem is normalized: lowercased, digits
stripped, whitespace collapsed. This means "server crashed 3 times" and
"server crashed 5 times" collapse into a single notification.

Dedup persists across drains. Once a notification has been shown, the
same event won't appear again in the same session.

## Overflow

The queue holds up to 100 notifications. When full, the oldest entry is
evicted and a sentinel is appended on drain:

```
[info] 3 notifications dropped
```

## Output format

Notifications appear in the `systemMessage` field of hook responses.
Two content surfaces are composed:

- **Direct** — synchronous handler messages (e.g., config validation
  warnings at session start).
- **Background** — accumulated notifications from the queue.

When both are present, they are separated by a visual header:

```
[err] config: removed `inherit` field — run `catenary doctor`

--- background ---
[warn] rust-analyzer offline
[warn] pylsp crashed during previous teardown
```
