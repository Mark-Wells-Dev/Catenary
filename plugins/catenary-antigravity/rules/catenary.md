---
trigger: always_on
---

Catenary — code intelligence you drive from the shell: you search, browse, and
diagnose through `catenary` subcommands (shell commands run via your shell
tool, not MCP tools), and a hook enforces the rules below before every tool
call.

The edit→diagnostics loop
  Editing tracks itself — your first edit starts it, no start step. Every
  edited file a language server covers joins a debt gate. Pay it by running
  `catenary diagnostics` (bare, its own step): it DIAGNOSES the whole edited
  set and prints a per-file receipt — each file named, clean ones marked
  `[clean]`, dirty ones listing their errors and warnings. Paying is
  diagnosing, not fixing: a file leaves the gate once you look at it, clean or
  dirty. Exit `0` means the run completed and its receipt is trustworthy,
  clean or dirty; it never exits `1`, so a dirty result is not a failed call.
  Scope it — `catenary diagnostics path…` — to diagnose and pay off just those
  files; the gate stays armed for any edited file left unpaid. Diagnostics are
  pulled: you see them only when you run it — each run re-diagnoses the set.

Bare-only vs pipe-friendly
  `catenary diagnostics` is bare-only: run it as the sole command — no pipe, no
  `&&`/`;`, no redirect — then read its output. `catenary grep` and
  `catenary glob` are pipe-friendly: they compose freely (`| head`, `| grep`)
  and their output is always complete, so a pipe never drops results (use
  `--count` for a bare tally).

Work in isolated subagents
  Coverage is automatic — you never manage roots. Two agents in one workspace
  step on each other and, sharing its language server, break settle detection.

Navigate through Catenary
  Search contents with `catenary grep`, find files with `catenary glob`. Quote
  glob patterns so Catenary expands them gitignore-aware, not the shell
  (`catenary grep 'fn main' 'src/**/*.rs'`, `catenary glob 'src/**/*.rs'`); the
  pattern is itself the path, so there is no separate directory argument
  (`catenary glob '/abs/dir/**/*.md'`). Where a code intelligence source
  covers a hit its enrichment rides along; where none does, `catenary grep`
  still returns the match. With no path argument, `… | catenary grep PAT` is a
  plain pass over the stream — complete matches, no enrichment — so matches
  come back with no source coverage.

Flag synopses (long forms)
  catenary grep PATTERN [PATH…]
    --ignore-case --case-sensitive --word-regexp --fixed-strings
    --invert-match --files-with-matches --count
    --{after,before,}-context N --exclude-pattern GLOB
    --include-{gitignored,hidden}
    --glob GLOB — scope which files are searched (vs the PATH positional)
    --type TYPE — restrict to a ripgrep file type (e.g. rust, md)
    full: catenary grep --help
  catenary glob [PATH…]
    --exclude-pattern GLOB — drop paths matching GLOB (repeatable)
    --count — report the path tally instead of the paths
    --include-gitignored — include files .gitignore hides
    --include-hidden — include hidden files and directories
    full: catenary glob --help
