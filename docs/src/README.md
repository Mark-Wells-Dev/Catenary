# Catenary

Catenary gives AI coding agents LSP-powered code intelligence. It
manages a pool of language servers and exposes them through CLI commands
and hooks — search, diagnostics, and navigation without shell-based text
scanning.

Two CLI search commands — `catenary grep` and `catenary glob` — plus a
tracked editing surface: edits flow through the host's Edit/Write tools
(or native `sed -i` for sweeps, whose writes the hook tracks too), and
`catenary diagnostics` reports the errors and warnings for every file
touched. Editing is tracked
automatically — the first edit starts it, there is no start step. The
agent never needs to know which language server handles which file.

- [Installation](installation.md) — install the binary and connect it to your CLI
- [Configuration](configuration.md) — configure language servers and settings
- [Notifications](notifications.md) — user-facing notifications via `systemMessage`
- [CLI & Dashboard](cli.md) — monitor sessions and browse events
- [Tracing Conventions](tracing-conventions.md) — severity guidelines and structured fields
- [Language Servers](lsp/README.md) — per-language setup guides
