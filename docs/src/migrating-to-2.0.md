# Migrating to 2.0

Catenary 2.0 is a major release. **Every breaking change is in user
configuration** — your `~/.config/catenary/config.toml` and any project
`.catenary.toml`. Agent-facing workflow changes (implicit editing start, the
`catenary diagnostics` command) need no migration: the host's session primer
teaches them fresh each session, so they are not an upgrade concern.

If you have never written a `[commands]` section, only the last item — the new
optional knobs — applies to you, and even those have safe defaults.

## At a glance

| Change | Affects you if… | Action |
|--------|-----------------|--------|
| [Output redirection denied by default](#1-output-redirection-is-denied-by-default) | agent commands rely on `>`, `>>`, `&>`, or `2>file` | set `allow_file_redirects = true`, or route writes through the edit tool |
| [`awk`/`sed` dropped from the default pipeline](#2-awk-and-sed-removed-from-the-default-pipeline) | your `[commands].pipeline` lists `awk` or `sed` | remove them; use `catenary sed` for sweeps |
| [Project `[commands]` enforcement keys ignored](#3-project-commands-enforcement-keys-are-ignored) | you set enforcement keys in a `.catenary.toml` | move them to user config |
| [New diagnostics + notification knobs](#4-new-optional-knobs) | — (optional) | nothing required; tune if desired |

## 1. Output redirection is denied by default

`allow_file_redirects` now defaults to **`false`**. Output redirections that
point at a file — `>`, `>>`, `&>`, `2>file` — are denied unless you opt in.

A redirected write bypasses Catenary's tracked Edit/Write path, so the next
`catenary diagnostics` batch can be incomplete: it reports fixes for the files
Catenary saw change while the redirected write went unseen.

File-descriptor duplications (`2>&1`, `>&2`) and device sinks (`/dev/null`,
`/dev/stdout`, `/dev/stderr`) are **always allowed**, regardless of this flag —
they do not write a file the diagnostics batch could miss.

**If you depend on file redirection in agent commands**, opt back in at user
scope:

```toml
# ~/.config/catenary/config.toml
[commands]
allow_file_redirects = true
```

Otherwise, no action is needed — prefer routing file writes through the host's
edit tool so they land in the diagnostics batch.

See [Command Filtering → Keys](configuration.md#keys).

## 2. `awk` and `sed` removed from the default pipeline

The recommended `[commands].pipeline` no longer includes `awk` or `sed`. The
shipped default is now:

```toml
pipeline = ["grep", "wc", "jq", "sort", "tr", "cut", "uniq"]
```

Both `awk` and `sed` can execute arbitrary code and write files in-band
(`sed -i`/`sed w`, `awk`'s `system()` and `print > file`). The filter
quote-masks an interpreter's program string before parsing, so it cannot see
those side effects — which would silently bypass the tracked Edit/Write path.
For that reason they are denied at every pipeline position now.

**If your `pipeline` list includes `awk` or `sed`**, remove them. Your existing
file is honored as written, so they keep working until you regenerate or edit
your config — but they are an exec/write hole and should be dropped.

For sweeping multi-file edits, use the new **`catenary sed`** surface
(see [below](#4-new-optional-knobs) and
[CLI & Dashboard](cli.md)): its substitutions flow through the tracked edit
path, so diagnostics stay complete.

Regenerate the recommended template any time with `catenary config`.

See [Command Filtering → Example](configuration.md#example).

## 3. Project `[commands]` enforcement keys are ignored

In a project `.catenary.toml`, the `[commands]` table now honors **only**
`build` (the per-root build tool). Every enforcement key is **user-level only**
and is ignored at project scope:

- `client_enforcement_only`
- `allow_file_redirects`
- `allow`
- `pipeline`
- `deny`
- `deny_flags`
- `guidance`

Catenary warns when it sees one of these in a project file.

**Why:** the command filter resolves daemon-globally — one Catenary daemon
serves every connected session. A project that changed enforcement would change
the filter *every* session sees, including agents in unrelated repositories.
The tighten/turn-on direction would fail silently (enforcement simply never
engages), so the keys are refused at project scope outright.

**If you set any of these in a `.catenary.toml`**, move them to your user config
at `~/.config/catenary/config.toml`. `build` stays where it is:

```toml
# .catenary.toml  — still valid
[commands]
build = "make"
```

```toml
# ~/.config/catenary/config.toml  — enforcement lives here now
[commands]
allow_file_redirects = true
allow = ["git", "gh", "cp", "rm", "mkdir", "mv", "touch", "cat", "head", "diff"]
pipeline = ["grep", "wc", "jq", "sort", "tr", "cut", "uniq"]

[commands.deny]
git = ["grep", "ls-files", "ls-tree"]
```

See [Command Filtering → Project-scoped commands](configuration.md#project-scoped-commands).

## 4. New optional knobs

These are additive — they have defaults and require no action — but they are new
surfaces you may want to set.

### `catenary diagnostics` tuning

```toml
[tools]
diagnostics_per_page = 50         # default
diagnostics_severity = "error"    # default
```

- **`diagnostics_per_page`** (default `50`) — single-shot preview budget. When a
  run produces more diagnostics than this, the preview shows the first N (errors
  before warnings) and the complete set is written to a per-session file under
  the runtime dir, named in a trailing `… N more — full report at <path>` line.
- **`diagnostics_severity`** (default `"error"`) — the minimum severity that
  marks a run "dirty" (exit code `1`). One of `"error"`, `"warning"`, `"info"`,
  `"hint"`. With the default, the exit code means "does it compile": warnings
  still print but exit `0`.

See [Configuration → Diagnostics](configuration.md#diagnostics).

### Notification threshold

```toml
[notifications]
threshold = "warn"    # default
```

- **`threshold`** (default `"warn"`) — minimum severity promoted to user-facing
  notifications via the host's `systemMessage`. One of `"debug"`, `"info"`,
  `"warn"`, `"error"`.

See [Configuration → Notifications](configuration.md#notifications) and
[Notifications](notifications.md).

## Not a migration concern

These changed in 2.0 but need no config action — the session primer teaches the
current workflow each session:

- Editing starts **implicitly** on the first edit; there is no `editing start`
  step (it remains an idempotent no-op).
- `editing stop` is now **`catenary diagnostics`** — it ends the edit batch and
  prints diagnostics for every modified file.
