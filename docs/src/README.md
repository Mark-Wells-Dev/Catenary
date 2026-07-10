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

- [Migrating to 2.0](migrating-to-2.0.md) — what changed in the 2.0 release
- [Installation](installation.md) — install the binary and connect it to your CLI
- [Configuration](configuration.md) — configure language servers and settings
- [Notifications](notifications.md) — how Catenary surfaces warnings and errors
- [CLI & Dashboard](cli.md) — the command surface and TUI dashboard
- [Language Servers](lsp/README.md) — per-language setup guides
- [Architecture](architecture/README.md) — how the daemon, surfaces, and routing fit together
- [Tracing Conventions](tracing-conventions.md) — severity guidelines and structured fields
