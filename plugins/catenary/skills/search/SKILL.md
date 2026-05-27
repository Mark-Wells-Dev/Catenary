---
name: search
description: >
  Catenary provides LSP-backed code search via Bash. Use
  `catenary grep <pattern>` for text + symbol search and
  `catenary glob <pattern>` for file outlines and directory
  listings. Always available, including during editing mode.
---

Both commands use the shell's cwd as the search root. Relative
patterns resolve against cwd — no directory parameter needed.

## `catenary grep <pattern>`

Search for a regex pattern. The regex engine uses Rust/PCRE-style
syntax: `|` for alternation (not `\|` — that matches a literal pipe).
Results are LSP-enriched within tracked workspace roots.

Flags: `--glob <pat>`, `--exclude <pat>`, `--page <n>`,
`--include-hidden`, `--include-gitignored`.

## `catenary glob <pattern>`

Browse files: path → outline, directory → listing, glob → matches.
Results include symbol outlines when LSP data is available.

Flags: `--exclude <pat>`, `--page <n>`, `--include-hidden`,
`--include-gitignored`.
