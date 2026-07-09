# Rust

## Install

### macOS

```bash
rustup component add rust-analyzer
```

### Linux

```bash
rustup component add rust-analyzer
```

### Windows

```bash
rustup component add rust-analyzer
```

## Config

Catenary ships a built-in definition for `rust-analyzer` — no `[lsp.server.*]`
config is needed. If `rust-analyzer` is on PATH (the `rustup` proxy installed
by `rustup component add rust-analyzer` provides it), it works automatically.

To customise, add to `~/.config/catenary/config.toml`:

```toml
[lsp.server.rust-analyzer]
env = { CLIPPY_DISABLE_DOCS_LINKS = "1" }

[lsp.server.rust-analyzer.initialization_options]
check.command = "clippy"
cargo.features = "all"
diagnostics.disabled = ["inactive-code"]
```

Setting `CLIPPY_DISABLE_DOCS_LINKS=1` strips "for further information
visit ..." suffixes from clippy diagnostics, saving agent context
tokens.

## Notes

- rust-analyzer is the official Rust language server
- Installing via rustup ensures it stays in sync with your Rust toolchain
- The `rust-analyzer` on PATH is the rustup proxy, which dispatches to the
  toolchain's own rust-analyzer — so it tracks a project-level
  `rust-toolchain.toml` automatically
- First run on a project may take time to index (watch for "Indexing" status)

## Links

- [rust-analyzer](https://rust-analyzer.github.io/)
- [rust-analyzer User Manual](https://rust-analyzer.github.io/manual.html)
