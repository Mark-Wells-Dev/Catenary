# Markdown

Catenary's out-of-box markdown server is
[Lattice](https://github.com/TwoWells/Lattice) — a Two Wells sibling project
(same AGPL-3.0-or-later + commercial dual-license as Catenary). Lattice is a
markdown predicate linter / backlink reconciler shipped as an LSP server. It
answers `documentSymbol`, workspace symbols, references, rename, hover,
folding, and document links, so it powers Catenary's `catenary grep` /
`catenary glob` enrichment and the `catenary diagnostics` pipeline on markdown.

## Install

Lattice ships as the `lattice` binary. Install it from the
[Lattice repository](https://github.com/TwoWells/Lattice) (build from source
with `cargo install`, or grab a release binary) and put `lattice` on your PATH.

```bash
# From a checkout of TwoWells/Lattice
cargo install --path .
```

Verify it is reachable:

```bash
lattice --version
```

## Config

Catenary ships a built-in definition for `lattice` — no `[server.*]` config is
needed. If `lattice` is on PATH it works automatically:

```toml
[server.lattice]
command = "lattice"
args = ["serve"]
suppress_symbol_kind = true
```

`suppress_symbol_kind = true` is set by the built-in default. Lattice embeds
structural context in its symbol *names* (heading and link prefixes,
`Field:` for predicates), so suppressing Catenary's own `SymbolKind` label
avoids double-prefixing in `catenary glob` output. See
[Suppress SymbolKind](../configuration.md#suppress-symbolkind).

### Root markers

The default markdown `root_markers` are `[".lattice.toml", ".git"]`:

- `.git` roots Lattice at the repository — correct, since its backlink graph
  spans the repo's markdown.
- `.lattice.toml` (optional) gives a tighter root and project configuration
  when present, even in a subdirectory.

## Model

Lattice treats a markdown tree as a predicate/backlink graph:

- **Predicates** live in CommonMark title text.
- **Backlinks** live in YAML frontmatter.

`catenary diagnostics` surfaces Lattice's link/predicate checks on edited
markdown, and grep/glob enrichment exposes the document structure Lattice
reports.

## Opting out (marksman)

[marksman](https://github.com/artempyanykh/marksman) remains a shipped server
definition — it is simply no longer the default. Re-enable it with a one-line
binding (no need to redefine the server):

```toml
[language.markdown]
servers = ["marksman"]
```

If `marksman` is on PATH, that binding is all you need.

## Notes

- OOB markdown intelligence requires `lattice` on PATH. Without it the daemon
  emits a benign `Failed to spawn LSP server: lattice` warning — not a wedge;
  every other server and all of Catenary's core (grep/glob/diagnostics on other
  languages) is unaffected.

## Links

- [Lattice GitHub](https://github.com/TwoWells/Lattice)
- [Marksman GitHub](https://github.com/artempyanykh/marksman)
