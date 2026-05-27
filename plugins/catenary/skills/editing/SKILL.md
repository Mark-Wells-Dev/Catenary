---
name: editing
description: >
  Catenary uses editing mode to deliver LSP diagnostics. Run
  `catenary start_editing` via Bash before using Edit/Write. Run
  `catenary done_editing` via Bash after editing to get diagnostics
  for all files touched.
---

## Editing Mode

Edit and Write are denied until `catenary start_editing` is run
via Bash. During editing mode, Catenary tracks which files the
agent touches and denies most Bash commands until
`catenary done_editing` is called. `done_editing` returns
diagnostics for all files touched during editing.

## Code Search

Use `catenary grep` and `catenary glob` via Bash for LSP-backed
code search. Available at all times, including during editing mode.
See the `search` skill for full usage.

## Workspace Roots

Add roots with `catenary add-root <path>` via Bash to extend LSP
coverage to additional directories. Use `catenary rm-root <path>`
to remove roots added via the CLI — this does not affect roots
provided by the MCP connection.
