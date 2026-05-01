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

Add to `~/.config/catenary/config.toml`:

```toml
[server.rust-analyzer]
command = "rustup"
args = ["run", "stable", "rust-analyzer"]
env = { CLIPPY_DISABLE_DOCS_LINKS = "1" }

[server.rust-analyzer.initialization_options]
check.command = "clippy"
cargo.features = "all"
diagnostics.disabled = ["inactive-code"]

[language.rust]
servers = ["rust-analyzer"]
```

Setting `CLIPPY_DISABLE_DOCS_LINKS=1` strips "for further information
visit ..." suffixes from clippy diagnostics, saving agent context
tokens.

## Notes

- rust-analyzer is the official Rust language server
- Installing via rustup ensures it stays in sync with your Rust toolchain
- Using `rustup run stable` avoids conflicts with project-level
  `rust-toolchain.toml` files, which can pin a toolchain that doesn't
  have rust-analyzer installed
- First run on a project may take time to index (watch for "Indexing" status)

## Links

- [rust-analyzer](https://rust-analyzer.github.io/)
- [rust-analyzer User Manual](https://rust-analyzer.github.io/manual.html)
