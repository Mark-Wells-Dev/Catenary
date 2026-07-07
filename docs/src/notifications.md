# Notifications

Catenary tells you when something needs attention through three channels,
split by urgency. The store-and-forward `systemMessage` notification queue —
which accumulated warns and drained them into a hook response one turn later —
retired in the TUI rework: it structurally delivered stale truths (a warn could
arrive minutes after the problem was already resolved), and a state-based
health surface cannot (a fixed problem simply isn't there).

## The channels

Every `tracing::warn!()` and `tracing::error!()` in Catenary carries an
operational signal. `LoggingServer` — Catenary's central tracing subscriber —
dispatches each event by severity:

| Severity | Desktop notification | TUI health surface | Firehose |
|----------|:---:|:---:|:---:|
| `error!()` | ✓ (urgent interrupt) | ✓ (a finding) | ✓ |
| `warn!()` | — | ✓ (a finding) | ✓ |
| `info!()` / `debug!()` | — | — | ✓ |

- **Desktop notifications** (`src/notify.rs`, `DesktopNotificationSink`) fire an
  OS-level notification for **error**-severity events only — the urgent
  interrupt, the daemon speaking for itself with no host conversation required.
  Deduped per daemon lifetime. Suppress with `[notifications] desktop = false`
  or the `CATENARY_NOTIFY=0` environment variable.
- **The TUI health surface** carries everything user-actionable, warns
  included: a warn *is* a health finding (stale hooks, version skew, coverage
  degradation). Findings persist on the dashboard's problems pane until the
  problem is fixed, rather than scrolling by in a transcript. `catenary doctor`
  renders the same findings one-shot.
- **The firehose** (`src/logging/jsonl_sink.rs`) records every event, queryable
  after the fact with `catenary query`.

Server-forwarded LSP window messages (`window/logMessage` and
`window/showMessage`, tagged `source = lsp.logging`) are firehose-only and never
a desktop interrupt, regardless of severity — a language server's own chatter,
including a `showMessage` type 1 that maps to `error`, is not Catenary's own
user-actionable event. It surfaces on the TUI's secondary Activity/Alerts
surface (see CatenaryInternal misc 125).

## The parent-agent context leg

One notice keeps an agent-facing delivery: when a subagent stops leaving a
dirty worktree, Catenary keeps the worktree (never auto-deleting unlanded work)
and the actionable audience is the **parent agent** that spawned it. That
notice rides Claude Code's `hookSpecificOutput.additionalContext` on the
parent's next eligible hook response (`PreToolUse` or `Stop` when allowing),
delivered from a per-session queue that is dropped on session end. It is not a
user notification — the user leg retired with the queue (misc 151).

## Configuration

The `[notifications]` section has a single knob:

```toml
[notifications]
desktop = true    # default — OS notifications for error-severity events
```

There is no severity `threshold`: it was the floor of the retired queue. Warns
are not severity-tunable — they persist on the dashboard, always. A leftover
`threshold` key does not break startup (it is ignored) but is flagged by the
unknown-key health finding, so `catenary doctor` and your editor point it out.

## Doctor and the TUI: one model, two renderers

`catenary doctor` is the standalone one-shot renderer of the health model —
scriptable, greppable, and **daemon-down capable** (it feeds the model with its
own `initialize` probes); the TUI is its live twin, rendering the same findings
continuously from the daemon's `state.json`.
