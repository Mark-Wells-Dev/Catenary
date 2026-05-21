# Catenary

Catenary is an MCP server that gives AI coding agents LSP-powered code
intelligence. It multiplexes one or more language servers behind a single
MCP interface, providing search, diagnostics, and navigation without
shell-based text scanning.

Four MCP tools — `grep` and `glob` for search, `start_editing` and
`done_editing` for batched diagnostics. The agent never needs to know
which language server handles which file.

- [Installation](installation.md) — install the binary and connect it to your CLI
- [Configuration](configuration.md) — configure language servers and settings
- [Notifications](notifications.md) — user-facing notifications via `systemMessage`
- [CLI & Dashboard](cli.md) — monitor sessions and browse events
- [Tracing Conventions](tracing-conventions.md) — severity guidelines and structured fields
- [Language Servers](lsp/README.md) — per-language setup guides
