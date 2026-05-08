# Configuration Model

This page explains the design behind Catenary's configuration system:
why it is structured the way it is, how layers compose, and what
tradeoffs were made. For syntax reference and usage examples, see the
[Configuration](../configuration.md) guide.

## Why the language/server split

Early Catenary configs merged everything into `[language.*]` entries —
each language carried its own `command`, `args`, `settings`, and server
identity. This worked when the mapping was one-to-one: one language, one
server.

Two scenarios broke it:

1. **Multiple servers per language.** PKGBUILD files are shellscript,
   but they benefit from both `termux-language-server` (package-specific
   hover, diagnostics) and `bash-language-server` (shell fundamentals —
   definitions, references, symbols). A single `[language.shellscript]`
   entry can't hold two server definitions.

2. **One server for multiple languages.** `clangd` serves both C and
   C++. Under the old model, its `command`, `args`, and `settings` had
   to be duplicated across `[language.c]` and `[language.cpp]`.

The fix is a relational split:

- **`[language.*]`** answers "what" — which servers handle this
  language, and how files are classified into it.
- **`[server.*]`** answers "how" — the binary, arguments,
  initialization options, settings, severity filter, and dispatch
  filter for a server process.

A `[server.*]` entry is defined once and referenced by name from any
number of `[language.*]` entries. A `[language.*]` entry's `servers`
list can reference multiple servers. This is a many-to-many
relationship.

## Config layering

Five sources, loaded in order. Later sources override earlier ones on a
per-field basis:

1. **Default config** — an embedded TOML file (`defaults/languages.toml`)
   compiled into the binary. Contains classification data
   (`extensions`, `filenames`, `shebangs`) for all built-in languages.
   No server bindings — purely "what file extensions map to what
   language." This file is the single source of truth for language
   detection, replacing the hardcoded tables that existed previously.
   Users can inspect it to see the exact patterns for every language.

2. **User config** (`~/.config/catenary/config.toml`) — full config.
   Adds server bindings, server definitions, and all other sections
   (`[commands]`, `[notifications]`, `[icons]`, `[tui]`, `[tools]`).

3. **Project config** (`.catenary.toml` per workspace root) — scoped
   to `[language.*]`, `[server.*]`, and `[commands]`. Discovered at
   root addition time. See [Project config scope](#project-config-scope)
   below.

4. **Explicit file** (`CATENARY_CONFIG` env var or `--config` flag) —
   full config that overrides the user config.

5. **Environment variable overrides** (`CATENARY_*`) — individual
   field overrides. `__` maps to TOML nesting (e.g.,
   `CATENARY_ICONS__PRESET=nerd`).

### Merge rules

Within each layer, `Option<T>` fields use `None`-preserving merge:
`None` (field absent in the overlay) keeps the earlier layer's value;
`Some(v)` replaces it. This means a user config that specifies only
`servers` for a language inherits the default config's classification
fields (`extensions`, `filenames`, `shebangs`) without repeating them.

For nested structures:

- **Scalars replace.** `command`, `args`, `min_severity`, `diagnostics`.
- **Tables deep-merge by key.** A project `[server.rust-analyzer]` with
  only `settings` inherits `command` and `args` from the user's (or
  built-in) `[server.rust-analyzer]`.
- **Arrays replace.** `servers`, `file_patterns`, `extensions`,
  `filenames`, `shebangs`, and array-valued settings entries. No
  concatenation, no deduplication.

Array replacement is deliberate. Array-valued LSP settings are
project-specific (`extraPaths`, `check.targets`, `cargo.features`) —
concatenating a user default with a project override is wrong or
useless. There is no escape hatch for removing a harmful user-level
entry under concatenation. VS Code and Cargo both use the same
convention.

## Project config scope

`.catenary.toml` is restricted to `[language.*]`, `[server.*]`, and
`[commands]`. Other sections are rejected with a warning and guidance
to move them to user config. This is a deliberate narrowing from the
earlier model where project config could contain any section.

Why each remaining section is excluded:

- **`[notifications]`, `[icons]`, `[tui]`, `[tools]`** — these are
  user preferences, not project-specific. A notification threshold or
  icon preset shouldn't vary per-root.

### `[commands]` in project config

`[commands]` is the exception to the "user preferences only" rule.
Two fields are project-scoped:

- **`build`** — per-root build tool. The answer to "why can't I run
  cargo/npm/go directly?" is inherently per-project. The evaluator
  resolves `cwd` (from the hook JSON payload) to a root via
  longest-prefix match, then looks up that root's build tool.

- **`allow`** — replaces (not merges with) the user's `allow` list
  for this root's contribution. Use case: a homelab root gets
  `kubectl` and `ssh`, a Catenary repo root doesn't.

In multi-root sessions, `allow` lists are unioned across roots —
adding a root is an intentional scope expansion. `pipeline` and
`deny` follow the same per-root replacement semantics. Disabled
roots (`lsp = false`) contribute `[commands]` config but not LSP
config, so a root can specify its build tool without spawning
servers.

The earlier model (walk up from cwd, merge all sections) worked for
single-project sessions. Multi-root sessions — where `/add-dir` adds
roots with potentially conflicting configs — broke the assumption.
The scope was narrowed to what is genuinely per-root: language server
routing, server configuration, and command filtering.

## Per-root settings resolution

When a project `.catenary.toml` exists, its `[server.*]` entries are
deep-merged with user-level server definitions. The merged settings are
stored per-root on `LspServer` alongside the user-level baseline:

- **User-level `settings`** — the baseline, used when the server asks
  for configuration without a scope.
- **Per-root `settings_per_root`** — from `.catenary.toml` per root,
  deep-merged over the user baseline.

When a language server sends `workspace/configuration` requests with a
`scopeUri`, Catenary resolves the root via longest-prefix match against
workspace roots. If the matched root has project-level settings, those
are deep-merged over the user settings and returned. No `scopeUri` (or
no matching root) returns user settings only.

The interaction with `didChangeConfiguration`: this notification is
triggered only by `/add-dir` adding a root with a `.catenary.toml`.
The server re-sends `workspace/configuration` requests for its scopes
and gets updated values. Live reload of `.catenary.toml` is out of
scope — the user restarts the session to pick up project config edits.

## Classification

File classification — "what language is this file?" — is config-driven.
Three dimensions, checked in precedence order (highest first):

1. **Shebang** — the file's `#!` line declares its interpreter.
   Matched against the `shebangs` field on `[language.*]`.
2. **Filename** — exact filename match against the `filenames` field.
3. **Extension** — file extension match against the `extensions` field.

Each tier short-circuits: if a shebang match is found, filename and
extension checks are skipped. The merged config (defaults + user +
project) is the sole source of classification data — no hardcoded
fallback tables exist.

The default config document serves as both reference and fallback. It
defines classification data for every built-in language, and users can
override any of it through the normal merge rules. Setting a
classification field to an empty array clears the default (since arrays
replace). This makes the classification system fully extensible without
code changes — defining a custom language is just adding a
`[language.*]` entry with classification fields and a server binding.

Per-root classification tables from project configs override global
tables for files within that root. `FilesystemManager` resolves the
root for a file path and uses the appropriate classification table.

## Tier promotion

Tier promotion is the mechanism for handling conflicting server
configurations across workspace roots. It is triggered by Rule A:
when a project `.catenary.toml` contains a `[language.X]` entry.

Without project config, servers are shared. A workspace-capable server
(one that supports `workspaceFolders`) gets a single `Scope::Workspace`
instance serving all roots. Server settings that vary per root are
handled via `scopeUri` resolution — each root gets its own config when
the server asks for it.

This works for compatible settings. It breaks for server-global settings
that don't use `scopeUri`. Concrete example: root A wants
`cargo.target = "x86_64"`, root B wants `cargo.target = "aarch64"`.
A single rust-analyzer process can't satisfy both, because the
`cargo.target` setting isn't per-scope — it applies to the whole
workspace.

The solution: each root adds `[language.rust]` to its `.catenary.toml`.
This triggers Rule A — Catenary spawns a separate `Scope::Root`
instance for each root, with its own process and its own settings.

```toml
# Root A: .catenary.toml
[language.rust]
servers = ["rust-analyzer"]

[server.rust-analyzer.settings.rust-analyzer]
cargo.target = "x86_64-unknown-linux-gnu"
```

```toml
# Root B: .catenary.toml
[language.rust]
servers = ["rust-analyzer"]

[server.rust-analyzer.settings.rust-analyzer]
cargo.target = "aarch64-unknown-linux-gnu"
```

The rule is explicit and binding-driven. Users signal "I want an
isolated process for this project" by writing a `[language.*]` entry.
The alternative — implicit promotion based on which `[server.*]` fields
are present — was rejected because config shape would silently determine
instance topology, making the model hard to reason about.

The resolution matrix:

| Project has | User has | Result |
|---|---|---|
| nothing | `[language.X]` + `[server.Y]` | User's tier 2 Y serves this root |
| `[server.Y.settings]` only | `[language.X]` + `[server.Y]` | Tier 2 Y serves; project settings `scopeUri`-merged |
| `[language.X]` + no `[server.Y]` | `[language.X]` + `[server.Y]` | Tier 1 Y with user's spawn def; user's tier 2 Y serves other roots |
| `[language.X]` + `[server.Y]` | `[language.X]` + `[server.Y]` | Tier 1 Y with project's spawn def |

No automatic conflict detection or instance splitting. The user makes
the call by where they place the config.

## Related pages

- [Configuration](../configuration.md) — user-facing reference guide
  (syntax, examples, full language ID table).
- [Routing & Dispatch](routing.md) — how classified files route to
  server instances and how multi-server dispatch works.
- [Session Lifecycle](session-lifecycle.md) — when config is loaded
  and how project configs are discovered.
