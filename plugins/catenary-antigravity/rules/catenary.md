## Catenary Editing Workflow

Run `catenary start_editing` before editing files.
Edit files normally using the host's edit tool.
Run `catenary done_editing` after editing to see diagnostics.

## Workspace Roots

Run `catenary add-root <path>` to add a workspace root.
Run `catenary rm-root <path>` to remove a workspace root.

## Code Search

Use `catenary grep` and `catenary glob` for code search.
These are CLI commands, always available, and do not require editing mode.
Relative patterns resolve against the shell's cwd.
Regex uses Rust/PCRE-style syntax: `|` for alternation (not `\|`).
