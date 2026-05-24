---
name: catenary
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

Catenary's grep and glob MCP tools are stateless and available at
all times, including during editing mode. They work on any
directory but results are only LSP-enriched within tracked
workspace roots.

## Workspace Roots

Add roots with `catenary add-root <path>` via Bash to extend LSP
coverage to additional directories. Use `catenary rm-root <path>`
to remove roots added via the CLI — this does not affect roots
provided by the MCP connection.
