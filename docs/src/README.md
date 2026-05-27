# Catenary

Catenary gives AI coding agents LSP-powered code intelligence. It
manages a pool of language servers and exposes them through CLI commands
and hooks — search, diagnostics, and navigation without shell-based text
scanning.

Two CLI search commands — `catenary grep` and `catenary glob` — plus an
editing lifecycle (`catenary start_editing` / `catenary done_editing`)
for batched diagnostics. The agent never needs to know which language
server handles which file.

- [Installation](installation.md) — install the binary and connect it to your CLI
- [Configuration](configuration.md) — configure language servers and settings
- [Notifications](notifications.md) — user-facing notifications via `systemMessage`
- [CLI & Dashboard](cli.md) — monitor sessions and browse events
- [Tracing Conventions](tracing-conventions.md) — severity guidelines and structured fields
- [Language Servers](lsp/README.md) — per-language setup guides
