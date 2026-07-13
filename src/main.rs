// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary MCP server and CLI.
//!
//! This is the main entry point for the Catenary multiplexing bridge.
//! It can be run as an MCP server or as a CLI tool to list and monitor sessions.

#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::{Context, Result};
use clap::{FromArgMatches, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use catenary_mcp::cli::{self, HostFormat, QueryFormat};
use catenary_mcp::logging::LoggingServer;

use catenary_mcp::source::Source;

/// Command-line arguments for Catenary.
#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "LSP-powered code intelligence for AI agents")]
#[command(version = env!("CATENARY_VERSION"))]
struct Args {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands supported by Catenary.
#[derive(Subcommand, Debug)]
enum Command {
    /// Overview of Catenary's workflow and commands.
    Primer {
        /// Declared client identity (e.g. `claude`) — keys client-specific
        /// teaching to that host's installed hook set (misc 177); omitted, the
        /// client-neutral payload prints. Declared, never auto-detected: hooks
        /// are hand-crafted per host and there is no standardized hook
        /// protocol to sniff against.
        client: Option<HostFormat>,
    },

    /// Search for a pattern with LSP-enriched results.
    ///
    /// A strict ripgrep superset: the ripgrep flag surface drives it (so a
    /// `grep`-fluent agent needs no relearning), and stdin is read when piped
    /// (`… | catenary grep PAT`) — a plain ripgrep pass over the stream, same
    /// flags, no enrichment. Searches from the current working directory
    /// otherwise; results within tracked workspace roots gain a `#scope` symbol
    /// anchor. Case defaults to smart-case (insensitive unless the pattern has an
    /// uppercase letter); `-i` forces insensitive, `-s` forces sensitive.
    Grep {
        /// Regex pattern (Rust `regex` syntax, | for alternation; no look-around —
        /// grep runs the linear ripgrep engine).
        pattern: String,

        /// File or directory path(s) to scope the search.
        ///
        /// Multiple values are unioned — `src/tui/*` expands
        /// to individual files and all are searched.
        #[arg(name = "PATH")]
        scope: Vec<String>,

        /// Case-insensitive matching (overrides the smart-case default).
        #[arg(short_alias = 'i', long = "ignore-case")]
        ignore_case: bool,

        /// Case-sensitive matching (overrides the smart-case default).
        #[arg(short_alias = 's', long = "case-sensitive")]
        case_sensitive: bool,

        /// Only match whole words (word-boundary anchored).
        #[arg(short_alias = 'w', long = "word-regexp")]
        word: bool,

        /// Treat the pattern as a literal string, not a regex.
        #[arg(short_alias = 'F', long = "fixed-strings")]
        fixed_strings: bool,

        /// Select non-matching lines (invert the match).
        #[arg(short_alias = 'v', long = "invert-match")]
        invert_match: bool,

        /// Print only the paths of files containing a match (takes precedence
        /// over results; `--count` still wins over this).
        #[arg(short_alias = 'l', long = "files-with-matches")]
        files_with_matches: bool,

        /// Show NUM lines of context after each match.
        #[arg(short_alias = 'A', long = "after-context", value_name = "NUM")]
        after_context: Option<usize>,

        /// Show NUM lines of context before each match.
        #[arg(short_alias = 'B', long = "before-context", value_name = "NUM")]
        before_context: Option<usize>,

        /// Show NUM lines of context before AND after each match.
        #[arg(short_alias = 'C', long = "context", value_name = "NUM")]
        context: Option<usize>,

        /// Include only files matching this glob (repeatable; a leading `!`
        /// excludes, ripgrep semantics).
        #[arg(short_alias = 'g', long = "glob", value_name = "GLOB")]
        glob: Vec<String>,

        /// Include only files of this ripgrep type, e.g. `rust`, `md`
        /// (repeatable).
        #[arg(short_alias = 't', long = "type", value_name = "TYPE")]
        type_filter: Vec<String>,

        /// Exclude matches by glob pattern, e.g. `tests/**` (repeatable; each
        /// occurrence adds a pattern, and a path is dropped when any matches).
        #[arg(long = "exclude-pattern", value_name = "GLOB")]
        exclude: Vec<String>,

        /// Report the match count ("N matches in M files") instead of results.
        #[arg(short_alias = 'c', long)]
        count: bool,

        /// Include files ignored by .gitignore.
        #[arg(long)]
        include_gitignored: bool,

        /// Include hidden files and directories.
        #[arg(long)]
        include_hidden: bool,

        /// No-op: line numbers are unconditional in the output (`path:N`
        /// prefixes every result). Accepted for ripgrep/grep muscle-memory
        /// parity; hidden because it changes nothing.
        #[arg(short = 'n', long = "line-number", hide = true)]
        line_number: bool,

        /// No-op: filenames are always shown in the output. Accepted for
        /// ripgrep/grep muscle-memory parity; hidden because it changes
        /// nothing.
        #[arg(short = 'H', long = "with-filename", hide = true)]
        with_filename: bool,
    },

    /// Browse the filesystem: file outlines, directory listings.
    ///
    /// Resolves against the current working directory. Results include symbol
    /// outlines when LSP data is available.
    Glob {
        /// A single glob pattern, quoted.
        ///
        /// The positional is a pattern, decoded syntactically, always
        /// (`catenary glob 'src/**/*.rs'`) — quote it so Catenary expands it
        /// gitignore-aware, not the shell. A metachar-free spelling is a
        /// self-matching literal (`catenary glob src/main.rs` outlines the file;
        /// `catenary glob 'src/*'` lists the directory). Patterns may be absolute
        /// or cwd-relative, and the anchor belongs *in the pattern*
        /// (`catenary glob '/abs/dir/**/*.md'`); there is no separate directory
        /// argument. Exactly one pattern — multiple patterns are a brace
        /// alternation (`'{src,tests}/**/*.rs'`); an unquoted pattern the shell
        /// expanded to several words is refused with teaching.
        #[arg(name = "PATH")]
        paths: Vec<String>,

        /// Exclude matches by glob pattern, e.g. `tests/**` (repeatable; each
        /// occurrence adds a pattern, and a path is dropped when any matches).
        #[arg(long = "exclude-pattern", value_name = "GLOB")]
        exclude: Vec<String>,

        /// Report the path count ("N paths") instead of results.
        #[arg(long)]
        count: bool,

        /// Include files ignored by .gitignore.
        #[arg(long)]
        include_gitignored: bool,

        /// Include hidden files and directories.
        #[arg(long)]
        include_hidden: bool,
    },

    /// Print diagnostics for the files you've edited, or lint the named paths.
    ///
    /// Bare — inside a hooked session, runs the LSP diagnostics pipeline over
    /// every file edited since the last run and prints a per-file receipt:
    /// each diagnosed file is listed, with its errors and warnings beneath it
    /// or `[clean]` beside it when the file is clean, then clears the set
    /// (`[no edited files]` for an empty set). The bare form pays the edit
    /// gate a hooked session arms; without one there is no gate and no edited
    /// set, so a bare run is a fault. With paths — diagnoses exactly those
    /// files or directories (on-demand lint), hooked session or not, and pays
    /// their editing debt when one is owed, dropping them from the gate; the
    /// gate stays armed while any edited file remains unpaid. Paying is
    /// *diagnosing*, not fixing — a file's debt is cleared by looking at it,
    /// clean or dirty. Exits `0` whenever the run completed — clean *or*
    /// dirty — and `2` only on a genuine fault (no daemon, IPC failure, a bare
    /// run with no hooked session); it never exits `1`, so a dirty result is
    /// not misread as a failed call. Editing begins implicitly on the first
    /// edit — there is no separate start step. Invoke via the host's shell
    /// tool.
    Diagnostics {
        /// File or directory path(s) to diagnose. Omit to report the whole
        /// edited set.
        ///
        /// Relative paths resolve against the current working directory. A
        /// named path is diagnosed whether or not it was edited — no hooked
        /// session required — and any editing debt it carries is paid, dropping
        /// it from the gate; a path outside the edited set is simply linted and
        /// pays nothing.
        #[arg(name = "PATH")]
        paths: Vec<String>,
    },

    /// Editing mode (start). Optional — editing starts implicitly on the
    /// first edit; `catenary diagnostics` ends it and prints diagnostics.
    Editing {
        #[command(subcommand)]
        command: EditingCommand,
    },

    /// Pin a workspace root: stop idle expiry, pre-warm its language servers,
    /// and upgrade an ephemeral mount to pinned.
    ///
    /// Coverage is automatic — pinning changes a root's *lifetime*, not whether
    /// it is served. Matches the stored/normalized path.
    ///
    /// Pins persist: the root is recorded in your user config's `[roots] pinned`
    /// list (comment-preserving) so it survives a daemon restart, re-added at the
    /// next boot. Hand-edits are first-class — adding a path to that array is
    /// itself a pin, effective at the next daemon start. A pinned path missing at
    /// boot is kept (never rewritten) and flagged by `catenary doctor`.
    Pin {
        /// Path to pin as a workspace root.
        path: PathBuf,
    },

    /// Unpin a workspace root: drop the pin contributor added by `catenary pin`.
    ///
    /// Matches the stored/normalized path, so it works even after the directory
    /// is removed, and is idempotent. Touches only the pin contributor — the
    /// worktree, ephemeral, and mcp classes own their own lifecycles. Also
    /// removes the entry from your user config's `[roots] pinned` list, so the
    /// pin does not return on the next daemon start.
    Unpin {
        /// Path to unpin.
        path: PathBuf,
    },

    /// List the current workspace roots with their contributor classes.
    ///
    /// Bare `catenary roots` lists the roots. The old `roots add`/`roots rm`
    /// spellings are retired — use `catenary pin` / `catenary unpin`.
    Roots {
        #[command(subcommand)]
        command: Option<RootsCommand>,
    },

    /// Manage Catenary worktrees (ls, add, rm).
    ///
    /// The sanctioned replacement for `git worktree` (denied on the agent
    /// surface): `ls` shows the registry+sidecar view, `add` creates a durable
    /// feats-class checkout with a sibling symlink, `rm` removes a worktree
    /// class-appropriately (misc 151).
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },

    /// Output a recommended annotated config template.
    Config,

    /// Print the allowed-command surface (allow / pipeline / denied) the
    /// command filter enforces. Invoke via the host's shell tool to see which
    /// shell commands Catenary permits; the denial message points here.
    Commands,

    /// Check language server health. Tests all configured servers by default.
    /// Pass a server name for verbose single-server diagnostics.
    Doctor {
        /// Server name for verbose single-server mode (matches [lsp.server.*]
        /// config keys). When omitted, tests all servers with one-line
        /// summaries.
        server: Option<String>,

        /// Workspace root for the probe and `.catenary.toml` project
        /// config lookup. Defaults to the current working directory.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Disable colored output.
        #[arg(long)]
        nocolor: bool,

        /// Show a unified diff for every stale file (hooks.json and constrained-bash.py).
        #[arg(long)]
        diff: bool,
    },

    /// Install or update the Catenary plugin for a host CLI.
    Install {
        #[command(subcommand)]
        host: Option<InstallHost>,

        /// Show what would change without acting.
        #[arg(long, global = true)]
        dry_run: bool,
    },

    /// Self-update the Catenary binary from GitHub releases.
    Update {
        /// Print whether an update is available without downloading.
        #[arg(long)]
        check: bool,

        /// Re-download even if versions match.
        #[arg(long)]
        force: bool,
    },

    /// Start the Catenary daemon explicitly.
    ///
    /// The counterpart to `stop`: brings the daemon up through the same
    /// single-instance start-or-connect path the bridge uses, so a manual
    /// `catenary stop` (or a killed daemon) has a one-command remedy without a
    /// per-session `/mcp` reconnect. Idempotent — if a daemon is already up, it
    /// connects, reports that, and leaves it running. Live sessions reconnect
    /// transparently once the daemon is back (bug 80).
    Start,

    /// Stop the running Catenary daemon.
    ///
    /// With a terminal on stdin and one or more connected sessions, prints the
    /// session board (host, workspace roots, connected-since) and asks for
    /// confirmation before disconnecting them — declining leaves the daemon
    /// running. `--force` skips the prompt, as does a non-interactive stdin
    /// (scripts, the documented upgrade flow); the post-stop reconnect warning
    /// is unchanged.
    Stop {
        /// Stop without the interactive confirmation prompt, even when live
        /// sessions are connected.
        #[arg(long)]
        force: bool,
    },

    /// Print the CLI version and the running daemon's version.
    ///
    /// `catenary --version` prints only the binary's own version (instant, no
    /// I/O). This subcommand additionally queries the running daemon — a daemon
    /// lags a freshly-rebuilt CLI until it is restarted, and this surfaces that
    /// staleness at a glance.
    Version,

    /// Query the JSONL telemetry firehose (LSP/MCP/hook protocol + trace).
    ///
    /// Reads the append-only logs directly, so it works even when the daemon
    /// is down. Filters select which shards to read — `--session` / `--server`
    /// / `--tool` resolve by file name, `--cwd` / `--level` / `--kind` /
    /// `--search` filter records after open. `--follow` tails the selection
    /// live.
    Query {
        /// Filter by session id (or prefix) → that session's file.
        #[arg(long)]
        session: Option<String>,

        /// Filter by LSP server name (rootless file + all rootful instances).
        #[arg(long)]
        server: Option<String>,

        /// Filter by search tool invocation dir ("grep" or "glob").
        #[arg(long)]
        tool: Option<String>,

        /// Keep only records whose recorded cwd is this path or under it.
        #[arg(long)]
        cwd: Option<String>,

        /// Time filter (e.g., "1h", "today", "7d", "30m").
        #[arg(long)]
        since: Option<String>,

        /// Read a specific daemon instance dir (default: the freshest one).
        #[arg(long)]
        instance: Option<String>,

        /// Read every instance dir, not just the freshest one.
        #[arg(long)]
        all_instances: bool,

        /// Minimum severity to show (error, warn, info, debug).
        #[arg(long)]
        level: Option<String>,

        /// Filter by record kind (lsp, mcp, hook, internal).
        #[arg(long)]
        kind: Option<String>,

        /// Free-text substring over method, message, and payload.
        #[arg(long)]
        search: Option<String>,

        /// Live-tail the selected files instead of a one-shot read.
        #[arg(long)]
        follow: bool,

        /// Maximum rows to show (0 = unlimited).
        #[arg(long, default_value = "100")]
        limit: usize,

        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: QueryFormat,
    },

    /// Hook subcommands (invoked by host CLI hooks).
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },

    /// Run as the Catenary daemon (internal, spawned by bridge proxy).
    #[command(hide = true)]
    Daemon,
}

/// Editing mode subcommands.
#[derive(Subcommand, Debug)]
enum EditingCommand {
    /// Enter editing mode (optional). Editing now starts implicitly on the
    /// first edit; this remains as an idempotent confirmation so a stray
    /// invocation never errors. Invoke via the host's shell tool.
    Start,
}

/// Workspace root subcommands.
///
/// Bare `catenary roots` lists the roots; `ls` is a kept alias. The mutating
/// verbs moved to the top-level `catenary pin` / `catenary unpin` (misc 146) —
/// `add`/`rm` remain only to teach the rename.
#[derive(Subcommand, Debug)]
enum RootsCommand {
    /// Retired: use `catenary pin <path>`.
    Add {
        /// Path (retired — use `catenary pin`).
        path: PathBuf,
    },
    /// Retired: use `catenary unpin <path>`.
    Rm {
        /// Path (retired — use `catenary unpin`).
        path: PathBuf,
    },
    /// List all tracked workspace roots (alias for bare `catenary roots`).
    Ls,
}

/// Worktree lifecycle subcommands (misc 151).
#[derive(Subcommand, Debug)]
enum WorktreeCommand {
    /// List Catenary-managed worktrees (path, class, creator, age, clean/dirty,
    /// root state, and — for feats — ahead/behind upstream).
    Ls,
    /// Create a durable feats-class worktree with a sibling symlink.
    Add {
        /// Branch to check out (created from HEAD if it does not exist).
        branch: String,
        /// Optional explicit worktree path (default: the feats state scheme).
        path: Option<PathBuf>,
    },
    /// Remove a worktree — class-appropriate (agent asserts captured work; feats
    /// refuses dirty). `--force` discards a dirty worktree through the proper
    /// disposal path (retire the root, sweep the sidecar) — the explicit
    /// exception to the never-auto-clean rule; it names the dirty files it drops.
    Rm {
        /// Path of the worktree to remove.
        path: PathBuf,
        /// Discard a dirty worktree (uncommitted, untracked, or unpushed work).
        /// Names the dropped files. Use only when the work is superseded or
        /// abandoned — dirty worktrees are never auto-cleaned.
        #[arg(long)]
        force: bool,
    },
    /// Print a worktree's complete diff vs HEAD (tracked changes plus untracked
    /// files as new-file hunks; a valid `git apply` patch).
    Diff {
        /// Path of the worktree to diff.
        path: PathBuf,
        /// Print only the changed paths, one per line, instead of the full diff.
        #[arg(long)]
        name_only: bool,
    },
    /// Land a worktree's changes into its owning repo (`git apply --3way`), arm
    /// the diagnostics batch, and remove the worktree. Never commits.
    Land {
        /// Path of the worktree to land.
        path: PathBuf,
        /// Land the changes without removing the worktree afterward.
        #[arg(long)]
        keep: bool,
    },
}

/// Hook subcommands invoked by host CLI hooks.
#[derive(Subcommand, Debug)]
enum HookCommand {
    /// Pre-tool: editing state enforcement (`PreToolUse` / `BeforeTool`).
    #[command(name = "pre-tool")]
    PreTool {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Post-agent: force `done_editing` before agent finishes (`Stop` / `AfterAgent`).
    #[command(name = "post-agent")]
    PostAgent {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SessionStart`: clear stale editing state.
    #[command(name = "session-start")]
    SessionStart {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PreInvocation`: first-sighting teaching injection (Antigravity).
    #[command(name = "pre-invocation")]
    PreInvocation {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SessionEnd`: clean up session state (roots, editing).
    #[command(name = "session-end")]
    SessionEnd {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SubagentStart`: mount the subagent's worktree as a root.
    #[command(name = "subagent-start")]
    SubagentStart {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `WorktreeRemove`: tear down the subagent's worktree root.
    #[command(name = "worktree-remove")]
    WorktreeRemove {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `WorktreeCreate`: create the subagent's worktree out-of-tree under the
    /// durable state dir, printing its absolute path (misc 144 / misc 150).
    #[command(name = "worktree-create")]
    WorktreeCreate {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PermissionRequest`: observe a permission prompt (pure observer — suspends
    /// the subagent's worktree idle expiry while blocked; misc 150).
    #[command(name = "permission-request")]
    PermissionRequest {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    // ── Reserved no-op shims ──────────────────────────────────────────
    // The full Claude Code hook-event surface is registered in hooks.json
    // (maintainer ruling, pre-v2) so future behavioral changes land in the
    // binary without another hooks.json churn. Events with no behavior yet
    // terminate in `cli::hooks::run_reserved_shim` — drain stdin, exit 0,
    // no daemon, no output. Observability wiring is post-v2.
    /// `Setup`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "setup")]
    Setup {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `UserPromptSubmit`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "user-prompt-submit")]
    UserPromptSubmit {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `UserPromptExpansion`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "user-prompt-expansion")]
    UserPromptExpansion {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PermissionDenied`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "permission-denied")]
    PermissionDenied {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PostToolUse`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "post-tool-use")]
    PostToolUse {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PostToolUseFailure`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "post-tool-use-failure")]
    PostToolUseFailure {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PostInvocation` (Antigravity — fires after tool calls finish):
    /// reserved no-op shim (observability wiring post-v2).
    #[command(name = "post-invocation")]
    PostInvocation {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `Notification`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "notification")]
    Notification {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `TaskCreated`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "task-created")]
    TaskCreated {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `TaskCompleted`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "task-completed")]
    TaskCompleted {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `StopFailure`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "stop-failure")]
    StopFailure {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `TeammateIdle`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "teammate-idle")]
    TeammateIdle {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `InstructionsLoaded`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "instructions-loaded")]
    InstructionsLoaded {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `ConfigChange`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "config-change")]
    ConfigChange {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `CwdChanged`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "cwd-changed")]
    CwdChanged {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PreCompact`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "pre-compact")]
    PreCompact {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `PostCompact`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "post-compact")]
    PostCompact {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `Elicitation`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "elicitation")]
    Elicitation {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `ElicitationResult`: reserved no-op shim (observability wiring post-v2).
    #[command(name = "elicitation-result")]
    ElicitationResult {
        /// Output format: "claude" or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
}

/// Host targets for the install command.
#[derive(Subcommand, Debug)]
enum InstallHost {
    /// Install the Catenary plugin for Claude Code.
    Claude {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,
    },
    /// Gemini CLI: support withdrawn (decision 030) — prints a withdrawal note
    /// and installs nothing. The subcommand is kept so the withdrawal is
    /// announced rather than surfacing as an unknown-host error.
    Gemini {
        /// Ignored: retained so `catenary install gemini <source>` still parses.
        source: Option<String>,
    },
    /// Install the Catenary plugin for Antigravity CLI.
    Antigravity {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,
    },
    /// Install the Catenary plugin for OpenCode.
    #[command(name = "opencode")]
    OpenCode {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,

        /// Install into the current workspace (`<root>/.opencode/`) instead of
        /// the global `~/.config/opencode/` location.
        #[arg(long)]
        workspace: bool,
    },
}

/// Entry point for the Catenary binary.
///
/// Subcommands that need async (Doctor, the in-process MCP server on
/// non-Unix) build a standard tokio runtime on demand. The daemon
/// builds its own runtime with larger thread stacks to accommodate its
/// async state machines. The bridge proxy is entirely synchronous.
#[allow(clippy::too_many_lines, reason = "Dispatch table for all subcommands")]
fn main() -> Result<()> {
    use clap::CommandFactory;
    let cli = Args::command();
    cli::command_filter::set_cli_command(cli.clone());
    let matches = match cli.clone().try_get_matches() {
        Ok(m) => m,
        Err(e) => {
            // Let clap handle --help and --version normally (exit 0).
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                e.exit();
            }
            // For agent-facing subcommands, append `-h` output so the
            // agent sees correct usage without a second round-trip.
            let raw = e.to_string();
            let subcommand = [
                "grep",
                "glob",
                "diagnostics",
                "editing",
                "pin",
                "unpin",
                "roots",
                "worktree",
            ]
            .into_iter()
            .find(|cmd| raw.contains(&format!("catenary {cmd}")));
            if let Some(cmd) = subcommand
                && let Some(sub) = cli.find_subcommand(cmd)
            {
                let mut sub = sub.clone();
                sub = sub
                    .bin_name(format!("catenary {cmd}"))
                    .disable_help_subcommand(true);
                let help = sub.render_help().to_string();
                // Extract just the "error:" line, drop clap's tip/Usage/--help boilerplate.
                let error_line = raw
                    .lines()
                    .find(|l| l.starts_with("error:"))
                    .unwrap_or(&raw);
                eprint!("{error_line}\n\n{help}");
                std::process::exit(2);
            }
            e.exit();
        }
    };
    let args = Args::from_arg_matches(&matches).context("parse CLI arguments")?;

    match args.command {
        None => {
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                run_dashboard()
            } else {
                #[cfg(unix)]
                {
                    catenary_mcp::router::run_bridge()
                }
                #[cfg(not(unix))]
                {
                    Err(anyhow::anyhow!(
                        "daemon mode requires Unix — Windows support is planned"
                    ))
                }
            }
        }
        Some(Command::Primer { client }) => {
            let mut out = cli::Output::stdout(false);
            run_primer(&mut out, client);
            Ok(())
        }
        #[cfg(unix)]
        Some(Command::Grep {
            pattern,
            scope,
            ignore_case,
            case_sensitive,
            word,
            fixed_strings,
            invert_match,
            files_with_matches,
            after_context,
            before_context,
            context,
            glob,
            type_filter,
            exclude,
            count,
            include_gitignored,
            include_hidden,
            // Hidden ripgrep-parity no-ops: line numbers and filenames are
            // unconditional in the output, so these accept-and-ignore.
            line_number: _,
            with_filename: _,
        }) => {
            let paths = to_literal_paths(scope);
            // `-C N` sets both sides; `-A`/`-B` override their side (ripgrep
            // precedence — the more specific flag wins).
            let flags = catenary_mcp::bridge::GrepFlags {
                ignore_case,
                case_sensitive,
                word,
                fixed_strings,
                invert: invert_match,
                files_with_matches,
                before_context: before_context.or(context).unwrap_or(0),
                after_context: after_context.or(context).unwrap_or(0),
                globs: glob,
                types: type_filter,
            };
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_grep(
                &mut out,
                pattern,
                paths,
                exclude,
                count,
                include_gitignored,
                include_hidden,
                flags,
            ))
        }
        #[cfg(not(unix))]
        Some(Command::Grep { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Glob {
            paths,
            exclude,
            count,
            include_gitignored,
            include_hidden,
        }) => {
            // Arity 1 is grammar (VERBS teaching moment 1): the bare form and
            // N>1 are usage errors — teaching on stderr, exit 2 (clap's
            // invalid-arg class). The trigger is structural; the diagnosis is
            // generous.
            let [pattern] = paths.as_slice() else {
                eprint!("{}", glob_arity_refusal(&paths));
                std::process::exit(2);
            };
            let pattern = PathBuf::from(pattern);
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_glob(
                &mut out,
                pattern,
                exclude,
                count,
                include_gitignored,
                include_hidden,
            ))
        }
        #[cfg(not(unix))]
        Some(Command::Glob { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Diagnostics { paths }) => {
            // Scoped paths are first-class (ws37 ticket 02, retiring bug 32's
            // accept-and-warn note): bare reports the whole edited set; with
            // paths the named files are diagnosed on demand and their editing
            // debt paid. The paths ride the consume request's `files` param.
            let mut out = cli::Output::stdout(false);
            // Exit-code contract (ws37 ticket 01, amending cli-prerelease
            // ticket 11): `0` = the run completed and its results are valid,
            // clean *or* dirty; `2` = a genuine fault (no daemon, IPC failure,
            // malformed response) surfaced as `Err`. `1` is NEVER emitted — the
            // clean/dirty distinction lives entirely in the stdout receipt, so
            // an agent's harness reads `0` as "trust this output" for a dirty
            // run too, instead of discarding valid diagnostics.
            match build_runtime().and_then(|rt| rt.block_on(run_done_editing(&mut out, &paths))) {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("{e:#}");
                    std::process::exit(2);
                }
            }
        }
        #[cfg(not(unix))]
        Some(Command::Diagnostics { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Editing { command }) => {
            let mut out = cli::Output::stdout(false);
            let EditingCommand::Start = command;
            build_runtime()?.block_on(run_start_editing(&mut out))
        }
        #[cfg(not(unix))]
        Some(Command::Editing { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Pin { path }) => {
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_root_command(&mut out, path, "tool/roots-add"))
        }
        #[cfg(not(unix))]
        Some(Command::Pin { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Unpin { path }) => {
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_root_command(&mut out, path, "tool/roots-rm"))
        }
        #[cfg(not(unix))]
        Some(Command::Unpin { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Roots { command }) => {
            let mut out = cli::Output::stdout(false);
            match command {
                // Bare `catenary roots` (and the kept `ls` alias) lists the roots.
                None | Some(RootsCommand::Ls) => {
                    build_runtime()?.block_on(cli::commands::run_ls_roots(&mut out))
                }
                // Retired spellings: teach the rename (honest rename, not a
                // silent alias). Agents are taught the same by the command
                // filter before the command runs.
                Some(RootsCommand::Add { .. }) => Err(anyhow::anyhow!(
                    "`catenary roots add` is retired — use `catenary pin <path>` \
                     to pin a workspace root."
                )),
                Some(RootsCommand::Rm { .. }) => Err(anyhow::anyhow!(
                    "`catenary roots rm` is retired — use `catenary unpin <path>` \
                     to unpin a workspace root."
                )),
            }
        }
        #[cfg(not(unix))]
        Some(Command::Roots { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Worktree { command }) => {
            let mut out = cli::Output::stdout(false);
            match command {
                WorktreeCommand::Ls => build_runtime()?.block_on(cli::worktree::run_ls(&mut out)),
                WorktreeCommand::Add { branch, path } => {
                    cli::worktree::run_add(&mut out, &branch, path.as_deref())
                }
                WorktreeCommand::Rm { path, force } => {
                    build_runtime()?.block_on(cli::worktree::run_rm(&mut out, path, force))
                }
                WorktreeCommand::Diff { path, name_only } => {
                    cli::worktree::run_diff(&mut out, &path, name_only)
                }
                WorktreeCommand::Land { path, keep } => {
                    build_runtime()?.block_on(cli::worktree::run_land(&mut out, path, keep))
                }
            }
        }
        #[cfg(not(unix))]
        Some(Command::Worktree { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        Some(Command::Config) => {
            let mut out = cli::Output::stdout(false);
            cli::config_template::print_template(&mut out);
            Ok(())
        }
        Some(Command::Commands) => {
            let mut out = cli::Output::stdout(false);
            cli::commands::run_commands(&mut out)
        }
        Some(Command::Doctor {
            server,
            root,
            nocolor,
            diff,
        }) => {
            let rt = build_runtime()?;
            let mut out = cli::Output::stdout(nocolor);
            if let Some(server_name) = server {
                rt.block_on(cli::doctor::run_doctor_single(
                    &mut out,
                    &server_name,
                    &root,
                ))
            } else {
                rt.block_on(cli::doctor::run_doctor(&mut out, &root, diff))
            }
        }
        Some(Command::Install { host, dry_run }) => {
            let mut out = cli::Output::stdout(false);
            match host {
                None => cli::install::run_install_list(&mut out),
                Some(InstallHost::Claude { source }) => {
                    cli::install::run_install_claude(&mut out, source.as_deref(), dry_run)
                }
                Some(InstallHost::Gemini { source }) => {
                    cli::install::run_install_gemini_withdrawn(&mut out, source.as_deref())
                }
                Some(InstallHost::Antigravity { source }) => {
                    cli::install::run_install_antigravity(&mut out, source.as_deref(), dry_run)
                }
                Some(InstallHost::OpenCode { source, workspace }) => {
                    cli::install::run_install_opencode(
                        &mut out,
                        source.as_deref(),
                        workspace,
                        dry_run,
                    )
                }
            }
        }
        Some(Command::Update { check, force }) => {
            let mut out = cli::Output::stdout(false);
            cli::update::run_update(&mut out, check, force)
        }
        #[cfg(unix)]
        Some(Command::Start) => {
            let mut out = cli::Output::stdout(false);
            run_start(&mut out)
        }
        #[cfg(not(unix))]
        Some(Command::Start) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Stop { force }) => {
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_stop(&mut out, force))
        }
        #[cfg(not(unix))]
        Some(Command::Stop { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Version) => {
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(cli::version::run_version(&mut out))
        }
        #[cfg(not(unix))]
        Some(Command::Version) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        Some(Command::Query {
            session,
            server,
            tool,
            cwd,
            since,
            instance,
            all_instances,
            level,
            kind,
            search,
            follow,
            limit,
            format,
        }) => {
            let mut out = cli::Output::stdout(false);
            cli::commands::run_query(
                &mut out,
                &cli::commands::QueryArgs {
                    session: session.as_deref(),
                    server: server.as_deref(),
                    tool: tool.as_deref(),
                    cwd: cwd.as_deref(),
                    since: since.as_deref(),
                    instance: instance.as_deref(),
                    all_instances,
                    level: level.as_deref(),
                    kind: kind.as_deref(),
                    search: search.as_deref(),
                    follow,
                    limit,
                    format,
                },
            )
        }
        Some(Command::Hook { command }) => {
            // Install minimal tracing subscriber for hook CLI: only the
            // desktop notification sink. When the daemon is unreachable,
            // error!() events fire OS notifications directly from the
            // hook process.
            let hook_logging = LoggingServer::new();
            tracing_subscriber::registry()
                .with(hook_logging.clone())
                .init();
            let desktop_sink = catenary_mcp::notify::DesktopNotificationSink::new();
            hook_logging.activate(vec![desktop_sink]);

            match command {
                HookCommand::PreTool { format } => cli::hooks::run_pre_tool(format),
                HookCommand::PostAgent { format } => cli::hooks::run_post_agent(format),
                HookCommand::SessionStart { format } => cli::hooks::run_session_start(format),
                HookCommand::PreInvocation { format } => cli::hooks::run_pre_invocation(format),
                HookCommand::SessionEnd { format } => cli::hooks::run_session_end(format),
                HookCommand::SubagentStart { format } => cli::hooks::run_subagent_start(format),
                HookCommand::WorktreeRemove { format } => cli::hooks::run_worktree_remove(format),
                // WorktreeCreate owns git worktree creation under Claude Code's
                // success/failure contract: on any failure, error loudly on
                // stderr and exit nonzero so the host fails worktree creation.
                HookCommand::WorktreeCreate { format } => {
                    if let Err(e) = cli::hooks::run_worktree_create(format) {
                        eprintln!("catenary hook worktree-create: {e:#}");
                        std::process::exit(1);
                    }
                }
                HookCommand::PermissionRequest { format } => {
                    cli::hooks::run_permission_request(format);
                }
                // Reserved no-op shims (full-surface registration, pre-v2
                // ruling): drain stdin, exit 0 — no daemon connection; the
                // only output is the host dialect's empty answer (Antigravity
                // gets `{}`, Claude gets silence). Observability wiring is
                // post-v2.
                HookCommand::Setup { format }
                | HookCommand::UserPromptSubmit { format }
                | HookCommand::UserPromptExpansion { format }
                | HookCommand::PermissionDenied { format }
                | HookCommand::PostToolUse { format }
                | HookCommand::PostToolUseFailure { format }
                | HookCommand::PostInvocation { format }
                | HookCommand::Notification { format }
                | HookCommand::TaskCreated { format }
                | HookCommand::TaskCompleted { format }
                | HookCommand::StopFailure { format }
                | HookCommand::TeammateIdle { format }
                | HookCommand::InstructionsLoaded { format }
                | HookCommand::ConfigChange { format }
                | HookCommand::CwdChanged { format }
                | HookCommand::PreCompact { format }
                | HookCommand::PostCompact { format }
                | HookCommand::Elicitation { format }
                | HookCommand::ElicitationResult { format } => {
                    cli::hooks::run_reserved_shim(format);
                }
            }
            Ok(())
        }
        #[cfg(unix)]
        Some(Command::Daemon) => run_daemon(),
        #[cfg(not(unix))]
        Some(Command::Daemon) => Err(anyhow::anyhow!("daemon mode requires Unix")),
    }
}

/// Converts positional arguments to `PathBuf`s.
///
/// A plain string→path mapping; literal-vs-pattern classification and glob
/// resolution happen later in [`resolve_search_paths`].
fn to_literal_paths(values: Vec<String>) -> Vec<PathBuf> {
    values.into_iter().map(PathBuf::from).collect()
}

/// The `catenary glob` arity refusal (VERBS teaching moment 1) — for the bare
/// form (0 arguments) and N>1 (usually an unquoted pattern the shell expanded).
///
/// The trigger is structural (arity ≠ 1); the diagnosis is generous. For the
/// bare form the message keys on the nullglob rationale — an unquoted zero-match
/// delivers zero args, so a cwd default would silently answer the wrong
/// question. For N>1, if the words share a common extension (the shape a
/// `*.rs`/`*.md` expansion leaves), the likely expanded pattern is named so the
/// agent sees exactly what to quote. Brace alternation covers legitimate
/// multi-pattern needs. Ends with a trailing newline; caller prints it to
/// stderr verbatim.
fn glob_arity_refusal(args: &[String]) -> String {
    if args.is_empty() {
        return "\
error: catenary glob takes one pattern — got none.

The positional is a pattern; quote it (`catenary glob 'src/**/*.rs'`). A bare \
`glob` has no cwd default: under `nullglob` an unquoted zero-match delivers zero \
arguments, so a default would silently answer the wrong question. To list the \
cwd: `catenary glob '*'`.
"
        .to_string();
    }

    // Generous diagnosis: a shared extension across the words is the fingerprint
    // of a shell glob expansion (`*.rs` → `a.rs b.rs …`) — name the pattern to
    // quote.
    let shared_ext = shared_extension(args);
    let mut msg = format!(
        "error: catenary glob takes one pattern — got {} arguments. This usually \
means the shell expanded an unquoted pattern into several filenames.\n\n",
        args.len()
    );
    if let Some(ext) = shared_ext {
        let _ = std::fmt::Write::write_fmt(
            &mut msg,
            format_args!(
                "Quote it so Catenary expands it, not the shell: `catenary glob '*.{ext}'`.\n"
            ),
        );
    } else {
        msg.push_str("Quote the pattern so Catenary expands it, not the shell.\n");
    }
    msg.push_str("Multiple patterns are a brace alternation: `catenary glob '{a,b}'`.\n");
    msg
}

/// The extension shared by *every* argument, when there is one — the fingerprint
/// of a shell glob expansion like `*.rs`. `None` when the arguments differ (or
/// any lacks an extension), so the refusal falls back to the generic wording.
fn shared_extension(args: &[String]) -> Option<String> {
    let first = Path::new(args.first()?)
        .extension()?
        .to_string_lossy()
        .into_owned();
    args.iter()
        .all(|a| {
            Path::new(a)
                .extension()
                .is_some_and(|e| e.to_string_lossy() == first)
        })
        .then_some(first)
}

/// Returns `true` if the string contains glob metacharacters (`*`, `?`, `[`, `{`).
fn contains_glob_metachar(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

/// Classified search-path arguments under the literal-first contract.
///
/// Produced by [`resolve_search_paths`] and consumed by
/// [`render_search_outcome`] to honor the three-outcome, always-exit-0
/// contract for `catenary grep`/`glob` (`bugs/13`).
struct SearchPaths {
    /// Arguments forwarded to the daemon: paths that exist plus glob patterns
    /// (original spelling). On a zero-result search these are echoed back so
    /// the agent sees exactly what was searched.
    forward: Vec<PathBuf>,
    /// Plain-path arguments that do not exist, reported loudly as
    /// `path does not exist: <path>`.
    missing: Vec<String>,
}

/// Resolves each search-path argument under the literal-first contract.
///
/// For each argument (resolved against `cwd` for the existence probe):
/// - **exists** (file, directory, or symlink — including a broken one) →
///   forwarded as a concrete path. Probing existence *first* preserves
///   literal matching of filenames that contain glob metacharacters and keeps
///   each variadic argument independent.
/// - **missing + glob metacharacter** (`* ? [ {`) → forwarded as a pattern;
///   the daemon expands it via its gitignore-aware walker.
/// - **missing + no metacharacter** → recorded in `missing` for a loud
///   `path does not exist` (exit 0, never a hard error that would cancel a
///   parallel tool batch).
fn resolve_search_paths(args: &[PathBuf], cwd: &Path) -> SearchPaths {
    let mut forward = Vec::new();
    let mut missing = Vec::new();
    for arg in args {
        let resolved = if arg.is_absolute() {
            arg.clone()
        } else {
            cwd.join(arg)
        };
        if resolved.symlink_metadata().is_ok() || contains_glob_metachar(&arg.to_string_lossy()) {
            forward.push(arg.clone());
        } else {
            missing.push(arg.to_string_lossy().into_owned());
        }
    }
    SearchPaths { forward, missing }
}

/// Which search command is being rendered — selects the zero-result wording.
enum SearchKind {
    /// `catenary grep`. On no match, echoes the pattern (so the agent can
    /// check its escaping) and the searched scope.
    Grep {
        /// The search pattern, echoed on a zero-result search.
        pattern: String,
        /// The pattern contained `\|`, a basic-regex alternation that ripgrep
        /// reads as a literal pipe — nudge toward `|`.
        bre_alternation: bool,
    },
    /// `catenary glob`. On a zero-result search, each glob-pattern argument
    /// that expanded to nothing is reported loudly per-argument. The three
    /// teaching vecs (VERBS moments 2–4) are emitted on stderr.
    Glob {
        /// Glob-pattern arguments (original spelling, daemon-reported) that
        /// expanded to zero matches. Rendered as `no matches for pattern:
        /// <pattern> (relative patterns anchor at cwd)` regardless of whether
        /// other arguments produced results (misc 118). Each is followed by a
        /// raw-string gitignore/hidden disclosure when the pattern names an
        /// existing-but-hidden target (moment 2).
        no_match_patterns: Vec<String>,
        /// Display paths of matched directories — each gets a
        /// `for its listing: catenary glob '<dir>/*'` hint (moment 4).
        dir_hints: Vec<String>,
        /// Result basenames carrying a glob metacharacter — one note teaches the
        /// escaped `'\<name>'` spelling (moment 3).
        metachar_names: Vec<String>,
    },
}

/// Compresses a path by replacing the `$HOME` prefix with `~`.
fn compress_home(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Ok(rel) = path.strip_prefix(&home)
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

/// Whether a search's scope was anchored at the invoking cwd: no path
/// arguments at all (a cwd-scoped search — pathless grep binds to the cwd,
/// bug 31), or at least one relative path/pattern argument, which the request
/// builder (`GrepRequest::to_params`) joins to the cwd before the daemon
/// expands it.
///
/// Missing-path arguments count as "had arguments": a search whose only
/// arguments were missing plain paths never queried, so its body is empty and
/// the zero-result arm prints the `cwd:` anchor unconditionally.
fn cwd_anchored(paths: &SearchPaths) -> bool {
    (paths.forward.is_empty() && paths.missing.is_empty())
        || paths.forward.iter().any(|p| p.is_relative())
}

/// Joins the forwarded path arguments for the `searched:` echo line.
fn forward_display(paths: &SearchPaths) -> String {
    paths
        .forward
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders a search command's outcome under the three-outcome, always-exit-0
/// contract (`bugs/13`) and the VERBS streams ruling: **`out` carries results,
/// `err` carries everything about them**.
///
/// - **Results** (stdout) — `daemon_output` non-empty → printed verbatim on
///   `out`. A grep whose scope was anchored at the cwd (pathless, or at least
///   one relative path/pattern argument) prints the `cwd:` scope-disclosure
///   anchor on `out` first — grep hits under the cwd render cwd-relative, so
///   without the anchor an agent whose shell cwd is not the tree it believes it
///   is searching reads another tree's matches as its own, undetectably (misc
///   172). The anchor stays on **stdout** so an explicit `2>/dev/null` cannot
///   drop the scope disclosure; it is scope, not an announcement banner.
///   Absolute-only scopes stay byte-identical. Glob is exempt: its listing
///   already renders absolute paths, which disclose the scope on their own.
/// - **Empty** (stderr) — `queried` ran but nothing came back → stdout stays
///   empty (the zero-match shape: empty set, empty stdout, exit 0) and the cwd
///   anchor + zero-match teaching ride `err`. For grep: `cwd:` (the signal
///   distinguishing "ran here, found nothing" from "did not run") then `no
///   matches for: <pattern>` plus the `searched:` scope.
/// - **No-match patterns** (glob, stderr) — a loud `no matches for pattern:
///   <pattern> (relative patterns anchor at cwd)` per glob-pattern argument that
///   expanded to nothing, regardless of whether *other* arguments produced
///   results (misc 118), each followed by a raw-string gitignore/hidden
///   disclosure when the pattern names an existing-but-hidden target (teaching
///   moment 2). Rides `err`.
/// - **Teaching moments 3 & 4** (glob, stderr) — the metachar-bearing
///   matched-name note and the directory-listing hint ride `err`.
/// - **Missing** (grep names, stderr) — a loud `path does not exist: <path>` per
///   non-existent plain-path argument. Rides `err`.
fn render_search_outcome(
    out: &mut cli::Output,
    err: &mut cli::Output,
    cwd: &Path,
    paths: &SearchPaths,
    daemon_output: &str,
    queried: bool,
    kind: &SearchKind,
) {
    let body = daemon_output.trim_end_matches('\n');
    if body.is_empty() {
        // Zero-match shape: stdout empty, exit 0. All teaching rides stderr.
        let _ = err.writeln(format_args!("cwd: {}", compress_home(cwd)));
        if queried
            && let SearchKind::Grep {
                pattern,
                bre_alternation,
            } = kind
        {
            let _ = err.writeln(format_args!("no matches for: {pattern}"));
            if !paths.forward.is_empty() {
                let _ = err.writeln(format_args!("searched: {}", forward_display(paths)));
            }
            if *bre_alternation {
                let _ = err.writeln(format_args!(
                    "hint: use `|` for alternation, not `\\|` (which matches a literal pipe)"
                ));
            }
        }
    } else {
        // The scope-disclosure anchor for cwd-anchored grep results (misc 172),
        // kept on stdout so `2>/dev/null` cannot lose it (see the doc comment).
        if matches!(kind, SearchKind::Grep { .. }) && cwd_anchored(paths) {
            let _ = out.writeln(format_args!("cwd: {}", compress_home(cwd)));
        }
        let _ = out.writeln(format_args!("{body}"));
    }
    // Per-argument glob no-match report (stderr) — fired whether or not the body
    // was empty, so a pattern that matched nothing is loud even when a sibling
    // argument rendered. Teaching moment 2's disclosure follows each.
    if let SearchKind::Glob {
        no_match_patterns,
        dir_hints,
        metachar_names,
    } = kind
    {
        for pattern in no_match_patterns {
            let _ = err.writeln(format_args!(
                "no matches for pattern: {pattern} (relative patterns anchor at cwd)"
            ));
            if let Some(line) = glob_zero_match_disclosure(pattern, cwd) {
                let _ = err.writeln(format_args!("{line}"));
            }
        }
        // Teaching moment 3: a matched name bears a glob metacharacter, so it is
        // reachable only by the escaped spelling. One note per distinct name.
        for name in metachar_names {
            let _ = err.writeln(format_args!(
                "note: `{name}` contains a glob metacharacter — to match it by \
                 name, escape it: `catenary glob '{}'`",
                escape_glob_metachars(name)
            ));
        }
        // Teaching moment 4: a pattern resolved a directory; hand over the
        // listing spelling.
        for dir in dir_hints {
            let _ = err.writeln(format_args!("for its listing: `catenary glob '{dir}/*'`"));
        }
    }
    for path in &paths.missing {
        let _ = err.writeln(format_args!("path does not exist: {path}"));
    }
}

/// Teaching moment 2's disclosure: one stat on the **raw** pattern string
/// (output-only), naming an existing-but-hidden target and the flag that reaches
/// it. Returns `None` when the raw string does not resolve to a path on disk (a
/// metachar-bearing pattern, or a genuine absent — the optional ignore-off
/// recount is not a commitment, VERBS open questions).
///
/// Resolved against `cwd` for a relative pattern. The gitignore case is the
/// primary lever (`--include-gitignored`); a dot-leading component that the
/// wildcard language skips is the hidden analogue (`--include-hidden`). A path
/// that exists but is *not* gitignore/hidden-shadowed yields no disclosure — the
/// zero match came from something else (e.g. an exclude), and a spurious
/// "add --include-gitignored" would misdirect.
fn glob_zero_match_disclosure(pattern: &str, cwd: &Path) -> Option<String> {
    let raw = Path::new(pattern);
    let resolved = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    // One stat on the raw string. A broken symlink still "exists".
    if resolved.symlink_metadata().is_err() {
        return None;
    }
    // Hidden analogue: the target's own name leads with a dot (the wildcard
    // language does not cross a leading dot), so `--include-hidden` is the lever.
    // Keyed on the basename only — an ancestor dotdir (e.g. a tempdir under
    // `~/.cache`) is not the agent's hidden target and must not misdirect.
    let is_hidden = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.starts_with('.'));
    if is_hidden {
        return Some(format!(
            "`{pattern}` exists but is hidden — add `--include-hidden`"
        ));
    }
    Some(format!(
        "`{pattern}` exists but is gitignored — add `--include-gitignored`"
    ))
}

/// Escapes glob metacharacters in a literal name so it can be passed back as a
/// pattern that matches the name verbatim (teaching moment 3, `rg -g`-style
/// backslash escaping). `*` → `\*`, and likewise `?`, `[`, `]`, `{`, `}`.
fn escape_glob_metachars(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if matches!(ch, '*' | '?' | '[' | ']' | '{' | '}') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Print the shared teaching payload — Catenary's full prevention content.
///
/// `catenary primer` is one of three surfaces that render the same payload
/// ([`cli::teaching::emitted_payload`]); the `SessionStart` / `SubagentStart`
/// hooks inline the identical body into the agent's context. Keeping the
/// single source means the on-demand command and the pushed hook context can
/// never drift. The commands-surface tier is resolved live from the config, so
/// the allow / pipeline / deny surface is always this session's actual one, and
/// a daemon-staleness note is prepended when the serving daemon runs a
/// different build than this CLI.
///
/// `client` is the declared client identity (`catenary primer claude`, misc
/// 177): a client whose installed hook set registers `WorktreeCreate` gets the
/// "Dispatching isolated work" section; bare `catenary primer` prints the
/// client-neutral payload, byte-identical to before the parameter existed.
fn run_primer(out: &mut cli::Output, client: Option<HostFormat>) {
    let _ = out.writeln(format_args!("{}", cli::teaching::emitted_payload(client)));
}

/// Builds a standard tokio multi-thread runtime.
///
/// Used by subcommands that need async (Doctor, in-process MCP server
/// on non-Unix). The daemon builds its own runtime with larger stacks.
fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")
}

/// Launch the interactive TUI dashboard.
///
/// Renders the daemon's `state.json` snapshot as a live ops board (server and
/// session health). The dashboard never reads the firehose — `catenary query`
/// is the separate firehose surface.
///
/// # Errors
///
/// Returns an error if configuration loading or TUI initialisation fails.
fn run_dashboard() -> Result<()> {
    let config = catenary_mcp::config::Config::load()?;
    catenary_mcp::tui::run(config.icons.unwrap_or_default())
}

/// Runs the Catenary daemon on a dedicated thread with a 16 MB stack.
///
/// The synchronous initialization path (`Config::load` →
/// `Session::new` → `LspClientManager::new`) has a deep call stack
/// that exceeds the default 8 MB main thread stack in debug builds.
/// Only the accept loop runs async via a tiny `block_on` future.
///
/// # Errors
///
/// Returns an error if setup or the accept loop fails.
#[cfg(unix)]
fn run_daemon() -> Result<()> {
    std::thread::Builder::new()
        .name("daemon".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(run_daemon_main)
        .context("spawn daemon thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("daemon thread panicked"))?
}

/// Daemon entry point, runs on a thread with a large stack.
#[cfg(unix)]
#[allow(
    clippy::too_many_lines,
    reason = "Daemon setup requires sequential initialization steps"
)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "SessionManager lifetime is correct — explicit drop(manager) at function end"
)]
fn run_daemon_main() -> Result<()> {
    use catenary_mcp::router::SessionManager;

    // Build runtime first — bind_daemon_sockets needs the tokio
    // reactor for UnixListener::bind.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build daemon runtime")?;
    let _rt_guard = rt.enter();

    // Bind sockets immediately so bridge proxies can connect while
    // heavy initialization (config, DB, LSP servers) proceeds.
    let sockets = catenary_mcp::router::bind_daemon_sockets()?;

    let logging = LoggingServer::new();
    // Floor the tracing stream before it reaches the DB sink. Without a filter
    // the registry captures everything down to TRACE, and the `log`->`tracing`
    // bridge (third-party crates) spews debug events persisted to `messages`
    // forever (no row retention) — the multi-GB DB wedge. The flood is third-party
    // `log` records (measured: ~99.8% `ignore::walk`, emitted during directory
    // scans). The bridge tags each event with its ORIGIN MODULE PATH as the
    // tracing target (`ignore::walk`, …), NOT a literal `log` target — so the old
    // `debug,log=warn` directive never matched it. Default everything to `warn` and
    // allowlist Catenary's own crates (`catenary` bin, `catenary_mcp` lib) at
    // `debug`. Override with CATENARY_LOG.
    let filter = tracing_subscriber::EnvFilter::try_from_env("CATENARY_LOG").unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("warn,catenary=debug,catenary_mcp=debug")
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(logging.clone())
        .init();

    // One-time reclaim of the legacy SQLite database (observability ticket 07).
    // Safe here: the socket bind above proved we are the sole daemon.
    drain_legacy_db();

    let mut config = catenary_mcp::config::Config::load()?;

    // Materialize the JSON Schemas to a local cache path and associate them with
    // the config files at the taplo server Catenary spawns, so config edits get
    // live validation + unknown-key squiggles offline, with zero setup (misc
    // 133). Best-effort — a filesystem error leaves the config untouched.
    catenary_mcp::config::schema::install_toml_schema_association(&mut config);

    let raw_roots: Vec<PathBuf> = match std::env::var("CATENARY_ROOTS") {
        Ok(val) if !val.is_empty() => std::env::split_paths(&val).collect(),
        _ => vec![PathBuf::from(".")],
    };
    let roots: Vec<PathBuf> = raw_roots
        .into_iter()
        .map(|r| r.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;

    let workspace_display = roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");

    // The daemon always activates a full session. Per-root `disable_lsp`
    // (workstream 34 ticket 00) replaces the old coarse daemon-wide
    // `lsp = false` kill switch: `Session::sync_roots` filters disabled roots
    // out of the LSP layer per contributor, which composes across a
    // multi-connection daemon (one process serving several projects) — the
    // startup check, keyed on the primary root only, could not.
    let shared_session = {
        let instance_id: Arc<str> = format!("daemon:{}", uuid::Uuid::new_v4()).into();

        // Firehose reaping knobs, captured before `config` moves into the
        // session (ticket 01).
        let reap_policy = config.reap_policy();
        let retention_days = config.log_retention_days;

        // Parent-agent additionalContext side channel (misc 151): the
        // dirty-worktree "kept" notice queues here for delivery on the parent's
        // next hook response. Shared by every per-session `Session`.
        let parent_context = catenary_mcp::bridge::ParentContextQueue::new();

        // Daemon-owned live-state snapshot. Mirrors server lifecycle/progress
        // and the alert ring to runtime_dir()/catenary/state.json — the
        // out-of-process surface that replaces the language_servers table.
        let snapshot = catenary_mcp::state_snapshot::SnapshotWriter::new(
            rt.handle(),
            &catenary_mcp::paths::runtime_dir().join("catenary"),
            // `DaemonInfo::current` sources the recorded version from the same
            // `CATENARY_VERSION` the skew check compares against, so a non-tag
            // build is never falsely flagged stale (tui-rework 09, item 1).
            catenary_mcp::state_snapshot::DaemonInfo::current(
                instance_id.to_string(),
                std::process::id(),
                catenary_mcp::state_snapshot::now_iso(),
            ),
        );

        let session = Arc::new(catenary_mcp::bridge::session::Session::new(
            config,
            roots,
            logging.clone(),
            instance_id.clone(),
            rt.handle().clone(),
            parent_context,
            Some(snapshot),
        ));

        // Spawn LSP servers in the background.
        let session_for_spawn = session.clone();
        rt.spawn(async move { session_for_spawn.spawn_all().await });

        // Firehose reaping (ticket 01). Run the startup instance cap once —
        // every non-self instance dir belongs to a dead daemon (one daemon per
        // host) — then schedule the periodic staleness sweep. On-write reaping
        // (rotation + per-tool byte budget) rides the JSONL sink itself.
        {
            let cache_root = catenary_mcp::paths::cache_dir().join("catenary");
            let self_inst = instance_id.to_string();
            rt.spawn_blocking(move || {
                catenary_mcp::logging::reaper::reap_instances(
                    &cache_root,
                    &self_inst,
                    reap_policy,
                    std::time::SystemTime::now(),
                );
            });

            let firehose_root = catenary_mcp::paths::cache_dir()
                .join("catenary")
                .join(instance_id.as_ref());
            let state_json = catenary_mcp::paths::runtime_dir()
                .join("catenary")
                .join("state.json");
            rt.spawn(async move {
                let mut ticker =
                    tokio::time::interval(catenary_mcp::logging::reaper::STALENESS_SWEEP_INTERVAL);
                ticker.tick().await; // consume the immediate first tick
                loop {
                    ticker.tick().await;
                    let root = firehose_root.clone();
                    let state = state_json.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        catenary_mcp::logging::reaper::sweep_stale(
                            &root,
                            &state,
                            retention_days,
                            std::time::SystemTime::now(),
                        );
                    })
                    .await;
                }
            });
        }

        Some(session)
    };

    let session_for_shutdown = shared_session.clone();

    let manager = SessionManager::from_sockets(sockets, logging);
    let manager = match shared_session {
        Some(session) => manager.with_session(session),
        None => manager,
    };

    // Worktree-root GC (workstream 30, ticket 03): the crash-safe leak backstop.
    // Spawned AFTER `with_session` so the root tracker exists — the call no-ops
    // for a session-less (test/transport-only) manager. A detached hourly
    // background task that reaps `worktree:*` roots whose dir is gone on disk (a
    // missed `WorktreeRemove`); the firehose reapers above and the SessionEnd
    // sweep are the other tiers. A worktree whose dir *lingers* after a
    // crash-before-git-cleanup while its session is dead is an accepted residual
    // (bounded by daemon restart — the in-memory RootTracker is rebuilt on
    // reconnect): there is no clean per-session "dead" signal, and we don't add a
    // staleness heuristic.
    manager.spawn_worktree_root_gc(rt.handle());

    // Worktree-deletion reaper (workstream 30, ticket 05; bug 106): the RELEASE
    // EDGE for `worktree:*` roots. A worktree root is pinned-class — it does not
    // expire on an idle clock (bug 106); its lifetime is the directory. `git
    // worktree remove` fires no `WorktreeRemove` hook, so this watch (registered
    // at `SubagentStart` mount) reaps the root within the FS-event latency the
    // instant its dir is deleted, retiring it through the full retire discipline
    // (misc 183 — never orphan the server set). Spawned AFTER `with_session` so
    // the watcher + channel exist; a no-op for a session-less manager or if the OS
    // watcher was unavailable. The hourly GC above stays the crash-safe backstop
    // (the watch dies with the daemon).
    manager.spawn_worktree_watch_reaper(rt.handle());

    // Ephemeral-root idle-expiry reaper (ephemeral-roots ticket 02): tears down
    // activity-mounted `ephemeral:*` roots after they go idle past the timeout.
    // Spawned AFTER `with_session` so the tracker + ephemeral clock exist; a
    // no-op for a session-less manager. These roots have no MCP heartbeat to pin
    // on, so the idle detector is their only release signal (DESIGN.md).
    manager.spawn_ephemeral_root_reaper(rt.handle());

    // External signed-registry refresh (tui-rework 08): resolve the published,
    // signed recipes+blessed-manifest artifact on start and on the slow
    // hours-class cadence, degrading fetched-verified → cache → seed and surfacing
    // any bad-signature / stale finding. The shipped default is seed-only, so this
    // is a network-free no-op until the maintainer stands up the registry.
    manager.spawn_registry_refresh(rt.handle());

    info!(
        source = Source::DaemonLifecycle.as_str(),
        "daemon serving workspace: {workspace_display}",
    );

    // Check installed hooks against expected — fire error!() (→ desktop
    // notification) if stale. The LoggingServer is active at this point,
    // so error events route through the desktop notification sink.
    check_stale_hooks();

    // Wire signals to the daemon's shutdown token so accept_loop
    // exits on SIGINT/SIGTERM.
    let shutdown = manager.shutdown_token();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("register SIGTERM")?;
    rt.spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    "received SIGINT",
                );
            }
            _ = async { sigterm.recv().await } => {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    "received SIGTERM",
                );
            }
        }
        shutdown.cancel();
    });

    let result = rt.block_on(manager.accept_loop());

    // Graceful LSP shutdown.
    if let Some(session) = session_for_shutdown {
        info!(
            source = Source::DaemonLifecycle.as_str(),
            "shutting down LSP servers",
        );
        rt.block_on(session.shutdown());
        // Flush the JSONL firehose (drain queue + join writer) after LSP
        // shutdown so its final telemetry is captured before exit.
        session.flush_telemetry();
    }

    // Drop removes socket files.
    drop(manager);

    info!(source = Source::DaemonLifecycle.as_str(), "daemon stopped",);

    result
}

/// One-time reclaim of the legacy `SQLite` database (observability ticket 07).
///
/// Older daemons left a `catenary.db` (plus its `-wal` / `-shm` siblings) under
/// [`state_dir`](catenary_mcp::paths::state_dir). `SQLite` is gone; the file is
/// regenerable telemetry the daemon owned, so it is deleted outright on startup
/// — no prompt, no migration. Safe here: the socket bind earlier proved this is
/// the sole daemon.
#[cfg(unix)]
fn drain_legacy_db() {
    let db = catenary_mcp::paths::state_dir()
        .join("catenary")
        .join("catenary.db");
    let reclaimed = drain_db_at(&db);
    if reclaimed > 0 {
        info!(
            source = Source::DaemonLifecycle.as_str(),
            "reclaimed legacy catenary.db ({reclaimed} bytes)",
        );
    }
}

/// Delete `db` plus its `-wal` / `-shm` siblings, returning the total bytes
/// reclaimed (0 when none exist). Best-effort: a file that cannot be removed is
/// skipped and not counted.
#[cfg(unix)]
fn drain_db_at(db: &Path) -> u64 {
    let mut reclaimed: u64 = 0;
    for suffix in ["", "-wal", "-shm"] {
        let mut os = db.to_path_buf().into_os_string();
        os.push(suffix);
        let path = PathBuf::from(os);
        if let Ok(meta) = std::fs::metadata(&path) {
            let size = meta.len();
            if std::fs::remove_file(&path).is_ok() {
                reclaimed += size;
            }
        }
    }
    reclaimed
}

/// Starts the Catenary daemon explicitly and idempotently (bug 80, leg 2).
///
/// Delegates to [`catenary_mcp::router::ensure_daemon_running`] — the same
/// single-instance start path the bridge init uses — and prints whether a daemon
/// was already up or a fresh one was started. Synchronous (no tokio runtime):
/// the start path is blocking socket I/O and a process spawn.
///
/// # Errors
///
/// Returns an error if the daemon cannot be started.
#[cfg(unix)]
fn run_start(out: &mut cli::Output) -> Result<()> {
    use catenary_mcp::router::DaemonStartOutcome;

    match catenary_mcp::router::ensure_daemon_running()? {
        DaemonStartOutcome::AlreadyRunning => {
            let _ = out.writeln(format_args!("Daemon already running"));
        }
        DaemonStartOutcome::Started => {
            let _ = out.writeln(format_args!("Daemon started"));
        }
    }
    Ok(())
}

/// Stops the running Catenary daemon.
///
/// Connects to the daemon's IPC socket and sends a shutdown request.
/// If no daemon is running, prints a message and returns successfully.
///
/// With a terminal on stdin and one or more sessions on the `state.json`
/// board, the human is shown the board (host, roots, connected-since) and
/// asked to confirm *before* the kill — declining exits `0` with the daemon
/// still running (feedback 08 finding 3). `force` (`--force`) and a
/// non-interactive stdin (scripts, the documented upgrade flow) skip straight
/// to the stop; the post-stop note (bridges respawn and reconnect on their
/// own — bug 80) prints either way.
///
/// # Errors
///
/// Returns an error if the shutdown request fails after connecting.
#[cfg(unix)]
async fn run_stop(out: &mut cli::Output, force: bool) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Human-TTY confirmation: the daemon already records its connected sessions
    // in the `state.json` snapshot, so read the board and confirm before the
    // disconnect rather than apologizing after. Only a real terminal on stdin
    // can answer, so a piped/redirected stdin (and `--force`) proceeds silently.
    if !force && std::io::stdin().is_terminal() {
        let sessions = live_session_board();
        if !sessions.is_empty() {
            let _ = out.writeln(format_args!("{}", render_stop_board(&sessions)));
            if !confirm_stop(out)? {
                let _ = out.writeln(format_args!("Left the daemon running."));
                return Ok(());
            }
        }
    }

    let ipc_path = catenary_mcp::router::socket_path();

    let Ok(stream) = tokio::net::UnixStream::connect(&ipc_path).await else {
        let _ = out.writeln(format_args!("No daemon running"));
        return Ok(());
    };

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/shutdown"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let _ = out.writeln(format_args!("Daemon stopped"));

    // The shutdown ack reports how many bridges were connected. Each bridge's
    // reader sees the socket close and runs the bug-80 reconnect: respawn the
    // daemon (connect_or_start), replay the captured initialize, resume — the
    // host↔bridge stdio link never breaks, so no `/mcp` is needed and a
    // deliberate stop with live sessions is really a bounce. Note it so the
    // brief blip isn't mysterious.
    let connections = serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|v| v.get("connections").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    if connections > 0 {
        let plural = if connections == 1 { "" } else { "s" };
        // Bug 80's reconnect-aware proxy made this a bounce, not a strand:
        // each live bridge respawns the daemon and replays its session on
        // its own — no `/mcp` needed. The old exception — a binary swapped
        // under a running bridge, where `current_exe` reads `… (deleted)`
        // and the respawn exec'd a dead path — is healed since misc 182
        // (the respawn trims the marker and execs the living path), so it
        // survives only in bridges from builds older than that fix.
        let _ = out.writeln(format_args!(
            "note: {connections} connected session{plural} will respawn the daemon and \
             reconnect on their own (a bridge from an older build may need `/mcp` after \
             a binary swap)",
        ));
    }
    Ok(())
}

/// Reads the daemon's `state.json` snapshot and returns its session board.
///
/// The snapshot is the daemon's own record of connected sessions (host,
/// workspace roots, connected-since); reading it is a cheap file read with no
/// daemon round-trip. A missing or unparseable snapshot yields an empty board,
/// so `catenary stop` never prompts when it cannot see any sessions.
#[cfg(unix)]
fn live_session_board() -> Vec<catenary_mcp::state_snapshot::SessionEntry> {
    use catenary_mcp::tui::data::{DataSource, StateJsonDataSource};

    StateJsonDataSource::new()
        .load()
        .map(|snapshot| snapshot.sessions)
        .unwrap_or_default()
}

/// Renders the pre-stop session board for the TTY confirmation.
///
/// Lists each connected session's host/client name, connected-since, and
/// workspace root(s) — the facts the human needs to weigh the disconnect,
/// drawn from the daemon's `state.json` snapshot (feedback 08 finding 3).
/// Returns the board as a multi-line string with no trailing prompt.
#[cfg(unix)]
fn render_stop_board(sessions: &[catenary_mcp::state_snapshot::SessionEntry]) -> String {
    use catenary_mcp::tui::format::elapsed_short;

    let n = sessions.len();
    let plural = if n == 1 { "" } else { "s" };
    let mut lines = vec![
        format!("{n} connected session{plural} will lose Catenary tooling if the daemon stops:"),
        String::new(),
    ];
    for session in sessions {
        let client = if session.client.name.is_empty() {
            "unknown"
        } else {
            session.client.name.as_str()
        };
        let since = elapsed_short(&session.started_at);
        let header = if since.is_empty() {
            format!("  {client}")
        } else {
            format!("  {client} · connected {since} ago")
        };
        lines.push(header);
        if session.roots.is_empty() {
            lines.push("    (no workspace roots)".to_string());
        } else {
            for root in &session.roots {
                lines.push(format!("    {root}"));
            }
        }
    }
    lines.join("\n")
}

/// Prompts the human at the terminal: stop the daemon anyway? Defaults to *no*.
///
/// Reads one line from stdin. Only an explicit `y`/`yes` (case-insensitive)
/// confirms; anything else — a bare Enter, `n`, or EOF — declines, so the safe
/// default is to leave the daemon running.
///
/// # Errors
///
/// Returns an error if writing the prompt or reading the reply fails.
#[cfg(unix)]
fn confirm_stop(out: &mut cli::Output) -> Result<bool> {
    use std::io::Write;

    out.write_str(format_args!("\nStop the daemon anyway? [y/N] "))?;
    out.flush()?;

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer)? == 0 {
        // EOF with no input — decline (safe default).
        let _ = out.writeln(format_args!(""));
        return Ok(false);
    }
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Runs a grep query against the running daemon, or a plain ripgrep pass over
/// stdin when the stream is piped.
///
/// When no path arguments are given and stdin is a readable stream (a pipe,
/// socket, or redirected file — ripgrep's `is_readable_stdin` rule), this is
/// stdin mode: a plain ripgrep pass over the stream, carrying the same flags but
/// with no enrichment (a stream has no file/LSP context) and no daemon
/// round-trip. Otherwise it connects to the daemon's IPC socket, sends a
/// [`GrepRequest`], and prints the rendered output to stdout. A tty or
/// `/dev/null` stdin is NOT readable, so a bare `catenary grep PAT` still
/// searches the cwd.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
#[allow(
    clippy::too_many_arguments,
    reason = "1:1 with the clap-parsed grep flags"
)]
async fn run_grep(
    out: &mut cli::Output,
    pattern: String,
    paths: Vec<PathBuf>,
    exclude: Vec<String>,
    count: bool,
    include_gitignored: bool,
    include_hidden: bool,
    flags: catenary_mcp::bridge::GrepFlags,
) -> Result<()> {
    use catenary_mcp::router::{GrepRequest, METHOD_GREP};

    // stdin mode: no paths + a readable piped/redirected stream. A plain
    // ripgrep pass over the stream, same flags, no enrichment, no daemon.
    //
    // A zero-byte pipe (`ssh -n`, most agent-harness shell tools) is a readable
    // stream that yields nothing, so a naive stdin-mode dispatch greps an empty
    // stream: no results, exit 0 — indistinguishable from a legitimate no-match,
    // the worst wrong for a search tool an agent trusts (misc 174). Read the
    // stream first: only genuine stream input (any bytes) stays in stdin mode;
    // an empty stream falls through to the cwd filesystem search, matching the
    // TTY case (which `is_readable_stdin` already sends there).
    if paths.is_empty() && is_readable_stdin() {
        let buffered = read_stdin_bytes()?;
        if !buffered.is_empty() {
            return run_grep_stdin(out, &buffered, &pattern, &flags, count);
        }
        // Empty stream: fall through to the cwd filesystem search below.
    }

    let cwd = std::env::current_dir().context("cannot determine working directory")?;

    let resolved = resolve_search_paths(&paths, &cwd);
    // No path arguments means a cwd-scoped search; otherwise query only when
    // at least one argument resolved to a path or pattern.
    let queried = paths.is_empty() || !resolved.forward.is_empty();
    let kind = SearchKind::Grep {
        pattern: pattern.clone(),
        bre_alternation: pattern.contains("\\|"),
    };

    let response = if queried {
        let request = GrepRequest {
            cwd: Some(cwd.clone()),
            pattern,
            paths: resolved.forward.clone(),
            exclude,
            count,
            include_gitignored,
            include_hidden,
            // Opt into chunked framing (misc 140 phase 2). A daemon that predates
            // it ignores the field and replies with the single envelope, which
            // `search_ipc` still parses.
            chunked: true,
            flags,
        };
        // Daemon-served when up; in-process (honestly labeled) when down.
        if let Some(stream) = connect_daemon_ipc().await {
            search_ipc_on(stream, METHOD_GREP, &request).await?
        } else {
            let resp = catenary_mcp::router::run_grep_daemon_less(&request).await?;
            emit_no_daemon_marker();
            SearchResponse::from(resp)
        }
    } else {
        SearchResponse::default()
    };

    // A usage error (uncompilable pattern, bug 105) is stderr + exit 2 on both
    // the bare and `--count` forms — never a zero indistinguishable from a
    // genuine no-match, never a swallowed parse error the exit code hides.
    if let Some(msg) = &response.error {
        eprintln!("{msg}");
        std::process::exit(2);
    }

    let mut err = cli::Output::stderr(false);
    if count {
        render_grep_count(
            out,
            response.matches.unwrap_or(0),
            response.files.unwrap_or(0),
            &response.skipped,
        );
    } else {
        render_search_outcome(
            out,
            &mut err,
            &cwd,
            &resolved,
            &response.output,
            queried,
            &kind,
        );
        // Skip lines follow the results/echo (and any missing-path lines), so a
        // named path skipped instead of searched never silently vanishes (misc
        // 135, bug 62). Nothing prints when nothing was skipped. Skips are
        // teaching about the scope, so they ride stderr with the rest.
        render_grep_skips(&mut err, &response.skipped);
    }
    Ok(())
}

/// Returns `true` when stdin is a readable stream — a pipe (FIFO), a socket, or
/// a redirected regular file — and `false` for a terminal or a character device
/// like `/dev/null`. Mirrors ripgrep's `is_readable_stdin`: only then does a
/// pathless `catenary grep PAT` read the stream instead of searching the cwd.
///
/// Resolved via `/dev/stdin` metadata (which follows the fd-0 symlink), so it
/// needs no `unsafe` fd introspection. A missing/unstattable `/dev/stdin` is
/// treated as not-readable (cwd search), the safe default.
#[cfg(unix)]
fn is_readable_stdin() -> bool {
    use std::os::unix::fs::FileTypeExt;
    let Ok(meta) = std::fs::metadata("/dev/stdin") else {
        return false;
    };
    let ft = meta.file_type();
    ft.is_fifo() || ft.is_socket() || ft.is_file()
}

/// Reads all of stdin into a buffer.
///
/// Buffering (rather than streaming `stdin.lock()` straight into the searcher)
/// lets the caller distinguish a genuine stream from a zero-byte pipe before
/// committing to stdin mode (misc 174): an empty buffer means the "readable"
/// stream carried nothing and the search should fall back to the filesystem.
/// Grep inputs are agent-scale, so holding the stream in memory is fine.
///
/// # Errors
///
/// Returns an error if the stream cannot be read.
#[cfg(unix)]
fn read_stdin_bytes() -> Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut buf)
        .context("read stdin stream")?;
    Ok(buf)
}

/// stdin mode: a plain ripgrep pass over the piped stream.
///
/// No daemon, no enrichment, no `#scope` — a stream has no file or LSP context.
/// Carries the same flags as file mode (`-i`/`-s`/`-w`/`-F`/`-v`, context,
/// `--count`, `-l`), differing only in enrichment. `-l` prints `(standard
/// input)` when the stream matched (the GNU `grep -l` convention for a nameless
/// stream); `--count` prints the matching-line tally.
///
/// Takes the already-buffered stream bytes so the caller can gate on a zero-byte
/// pipe (misc 174) before reaching here — this path only runs for genuine
/// (non-empty) stream input.
///
/// # Errors
///
/// Returns an error if the pattern is invalid or the search fails.
#[cfg(unix)]
fn run_grep_stdin(
    out: &mut cli::Output,
    input: &[u8],
    pattern: &str,
    flags: &catenary_mcp::bridge::GrepFlags,
    count: bool,
) -> Result<()> {
    use catenary_mcp::bridge::{StreamOutcome, grep_stream};

    let outcome = grep_stream(input, pattern, flags, count)?;
    match outcome {
        StreamOutcome::Count(n) => {
            let _ = out.writeln(format_args!("{n} matches"));
        }
        StreamOutcome::FilesWithMatches(matched) => {
            if matched {
                let _ = out.writeln(format_args!("(standard input)"));
            }
        }
        StreamOutcome::Lines(lines) => {
            if !lines.is_empty() {
                let _ = out.writeln(format_args!("{lines}"));
            }
        }
    }
    Ok(())
}

/// Parsed daemon response for `catenary grep`/`glob` over IPC.
///
/// A normal query carries rendered `output`; a `--count` query carries the
/// totals instead (`matches`/`files` for grep, `paths` for glob) with an
/// empty `output`. Fields absent from the wire default to empty/`None`, so an
/// empty response line deserializes to [`SearchResponse::default`].
#[cfg(unix)]
#[derive(Default, serde::Deserialize)]
struct SearchResponse {
    /// Rendered tree output (empty for a count response).
    #[serde(default)]
    output: String,
    /// grep `--count`: matching-line total.
    #[serde(default)]
    matches: Option<usize>,
    /// grep `--count`: distinct-file total.
    #[serde(default)]
    files: Option<usize>,
    /// glob `--count`: resolved-path total.
    #[serde(default)]
    paths: Option<usize>,
    /// glob: original spellings of glob-pattern arguments that expanded to
    /// zero matches, reported per-argument (misc 118).
    #[serde(default)]
    no_match_patterns: Vec<String>,
    /// glob: display paths of matched directories, for the teaching moment 4
    /// listing hint on stderr.
    #[serde(default)]
    dir_hints: Vec<String>,
    /// glob: result basenames carrying a glob metacharacter, for the teaching
    /// moment 3 escaped-spelling note on stderr.
    #[serde(default)]
    metachar_names: Vec<String>,
    /// grep: files in the search scope skipped instead of searched (misc 135,
    /// bug 62). Empty for a normal all-searched query.
    #[serde(default)]
    skipped: catenary_mcp::bridge::GrepSkips,
    /// A usage error (uncompilable pattern, bug 105) that aborted the search.
    /// `Some` routes the CLI to stderr + exit 2 on both the bare and `--count`
    /// forms; `None` for every successful query.
    #[serde(default)]
    error: Option<String>,
}

/// Adapts a daemon-less [`GrepResponse`](catenary_mcp::router::GrepResponse) into
/// the CLI's [`SearchResponse`] so the daemon-served and in-process paths render
/// through the identical code (bug 80, leg 4).
#[cfg(unix)]
impl From<catenary_mcp::router::GrepResponse> for SearchResponse {
    fn from(r: catenary_mcp::router::GrepResponse) -> Self {
        Self {
            output: r.output,
            matches: r.matches,
            files: r.files,
            paths: None,
            no_match_patterns: Vec::new(),
            dir_hints: Vec::new(),
            metachar_names: Vec::new(),
            skipped: r.skipped,
            error: r.error,
        }
    }
}

/// Adapts a daemon-less [`GlobResponse`](catenary_mcp::router::GlobResponse) into
/// the CLI's [`SearchResponse`] (bug 80, leg 4).
#[cfg(unix)]
impl From<catenary_mcp::router::GlobResponse> for SearchResponse {
    fn from(r: catenary_mcp::router::GlobResponse) -> Self {
        Self {
            output: r.output,
            matches: None,
            files: None,
            paths: r.paths,
            no_match_patterns: r.no_match_patterns,
            dir_hints: r.dir_hints,
            metachar_names: r.metachar_names,
            skipped: catenary_mcp::bridge::GrepSkips::default(),
            error: r.error,
        }
    }
}

/// Renders the `catenary grep --count` summary: `N matches in M files`.
///
/// `matches` is the matching-line total (one per rendered leaf row); `files`
/// is the number of distinct files holding them. When any file was skipped
/// instead of searched, a ` (K skipped: <reason>)` suffix follows so a skip is
/// never conflated with a no-match (misc 135, bug 62); with nothing skipped the
/// line is byte-identical to before.
fn render_grep_count(
    out: &mut cli::Output,
    matches: usize,
    files: usize,
    skipped: &catenary_mcp::bridge::GrepSkips,
) {
    let suffix = skipped.count_suffix().unwrap_or_default();
    let _ = out.writeln(format_args!("{matches} matches in {files} files{suffix}"));
}

/// Appends the per-file and aggregate skip lines to a default (or `-l`) grep
/// result — a named skipped file as `skipped (<reason>): <path>`, walked files
/// collapsed to `<n> file(s) skipped (<reason>)` (misc 135, bug 62). Emits
/// nothing when nothing was skipped, so a normal result is unchanged.
fn render_grep_skips(out: &mut cli::Output, skipped: &catenary_mcp::bridge::GrepSkips) {
    for line in skipped.render_lines() {
        let _ = out.writeln(format_args!("{line}"));
    }
}

/// Renders the `catenary glob --count` summary: `N paths`.
fn render_glob_count(out: &mut cli::Output, paths: usize) {
    let _ = out.writeln(format_args!("{paths} paths"));
}

/// Connects to the daemon IPC socket, returning `None` when the daemon is down.
///
/// The daemon-down signal for the honest-degradation path (bug 80, leg 4): a
/// missing socket file or a refused connection means no daemon, so `catenary
/// grep`/`glob` fall back to the in-process pipeline. Any *other* connect error
/// also maps to `None` — a daemon that cannot be reached is, from the CLI's
/// vantage, down.
#[cfg(unix)]
async fn connect_daemon_ipc() -> Option<tokio::net::UnixStream> {
    let ipc_path = catenary_mcp::router::socket_path();
    tokio::net::UnixStream::connect(&ipc_path).await.ok()
}

/// The mandatory daemon-less honesty marker (bug 80, leg 4).
///
/// Printed to **stderr only** — stdout stays byte-identical to a daemon-served
/// uncovered answer — so `unenriched-because-uncovered` and
/// `unenriched-because-no-daemon` are never indistinguishable. This is CLI
/// output, not a `tracing` event: it must not fire a desktop notification or
/// land on the TUI health surface.
#[cfg(unix)]
fn emit_no_daemon_marker() {
    eprintln!("[no daemon \u{2014} results unenriched; start one with catenary start]");
}

/// Sends a search request over an already-connected daemon IPC `stream` and
/// returns the parsed [`SearchResponse`].
///
/// Serializes `request` with `method` injected, reads the response, and
/// reassembles a chunked [`GrepFrame`] stream (misc 140 phase 2) or parses a
/// legacy single envelope. Split from [`search_ipc`] so the daemon-down branch
/// can decide *before* connecting whether to fall back to the in-process
/// pipeline (bug 80, leg 4).
///
/// # Errors
///
/// Returns an error if the query fails or the response is malformed.
#[cfg(unix)]
async fn search_ipc_on<R: serde::Serialize + Sync>(
    stream: tokio::net::UnixStream,
    method: &str,
    request: &R,
) -> Result<SearchResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();

    let mut envelope = serde_json::to_value(request)?;
    envelope
        .as_object_mut()
        .context("request is not an object")?
        .insert(
            "method".to_string(),
            serde_json::Value::String(method.to_string()),
        );
    let mut payload = serde_json::to_string(&envelope)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(SearchResponse::default());
    }
    let first: serde_json::Value =
        serde_json::from_str(trimmed).context("invalid search response from daemon")?;

    // Version-skew hinge: a framed response tags every line with `"frame"`; a
    // legacy single envelope never does. The absent tag routes to the
    // single-envelope parse, so a pre-framing daemon (and every glob response)
    // still deserializes.
    if first.get("frame").is_some() {
        return read_grep_frames(first, &mut buf_reader).await;
    }
    serde_json::from_value(first).context("invalid search response from daemon")
}

/// Reassembles a chunked grep response ([`GrepFrame`] stream) into a
/// [`SearchResponse`] (misc 140 phase 2).
///
/// `first` is the already-parsed first frame; subsequent frames are read from
/// `reader`. Chunk payloads concatenate into the rendered output — trimmed of
/// its trailing newline to reproduce the pre-framing render byte-for-byte — and
/// the terminator supplies the count/skip tallies. An unrecognized frame tag (a
/// future daemon speaking a newer protocol) fails with a comprehensible error
/// rather than a silent misparse.
///
/// # Errors
///
/// Returns an error on a transport failure, a malformed/unrecognized frame, or a
/// stream that ends before its terminator.
#[cfg(unix)]
async fn read_grep_frames<R>(
    first: serde_json::Value,
    reader: &mut tokio::io::BufReader<R>,
) -> Result<SearchResponse>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use catenary_mcp::router::GrepFrame;
    use tokio::io::AsyncBufReadExt;

    let mut response = SearchResponse::default();
    let mut output = String::new();
    let mut pending = Some(first);

    loop {
        let value = if let Some(v) = pending.take() {
            v
        } else {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                anyhow::bail!("grep response ended before its terminator frame");
            }
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            serde_json::from_str(t).context("invalid grep frame from daemon")?
        };

        let frame: GrepFrame = serde_json::from_value(value).context(
            "unrecognized grep frame from daemon — restart the daemon to match versions",
        )?;
        match frame {
            GrepFrame::Chunk { data } => output.push_str(&data),
            GrepFrame::End {
                matches,
                files,
                skipped,
                error,
            } => {
                response.matches = matches;
                response.files = files;
                response.skipped = skipped;
                response.error = error;
                break;
            }
        }
    }

    output.truncate(output.trim_end().len());
    response.output = output;
    Ok(response)
}

/// Runs a glob query against the running daemon.
///
/// The one-verb form (VERBS): the single positional is a pattern, decoded
/// syntactically, always — no `resolve_search_paths` literal-first probe, no
/// `path does not exist` classification (a metachar-free absent forwards as a
/// pattern and gets the loud zero-match report + disclosure, not a silent
/// missing line). The daemon absolutizes the relative pattern against `cwd` and
/// expands it gitignore-aware. Arity 1 is enforced by the caller
/// ([`main`])/clap before this runs; `pattern` is exactly one argument.
///
/// stdout carries results only; the loud zero-match line and the teaching
/// moments ride stderr (the streams ruling).
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn run_glob(
    out: &mut cli::Output,
    pattern: PathBuf,
    exclude: Vec<String>,
    count: bool,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<()> {
    use catenary_mcp::router::{GlobRequest, METHOD_GLOB};

    let cwd = std::env::current_dir().context("cannot determine working directory")?;

    // Always-pattern: the positional forwards as-is; there is no missing/literal
    // classification (the `path does not exist` line is grep's name-operand
    // teaching, not glob's — VERBS Dispositions). The `SearchPaths` is a
    // single-forward, no-missing set so `render_search_outcome`'s
    // missing-path loop is a no-op for glob.
    let resolved = SearchPaths {
        forward: vec![pattern.clone()],
        missing: Vec::new(),
    };

    let request = GlobRequest {
        cwd: Some(cwd.clone()),
        paths: resolved.forward.clone(),
        exclude,
        count,
        include_gitignored,
        include_hidden,
    };
    // Daemon-served when up; in-process (honestly labeled) when down.
    let response = if let Some(stream) = connect_daemon_ipc().await {
        search_ipc_on(stream, METHOD_GLOB, &request).await?
    } else {
        let resp = catenary_mcp::router::run_glob_daemon_less(&request).await?;
        emit_no_daemon_marker();
        SearchResponse::from(resp)
    };

    // A usage error (an invalid pattern the query never ran on) is stderr +
    // exit 2 (VERBS: the arity/bare/invalid-pattern class).
    if let Some(msg) = &response.error {
        eprintln!("{msg}");
        std::process::exit(2);
    }

    if count {
        render_glob_count(out, response.paths.unwrap_or(0));
    } else {
        let mut err = cli::Output::stderr(false);
        // `no_match_patterns`/`dir_hints`/`metachar_names` are daemon-reported
        // (original argument spelling / display paths); move them into the kind
        // while still borrowing `output` (disjoint fields).
        let kind = SearchKind::Glob {
            no_match_patterns: response.no_match_patterns,
            dir_hints: response.dir_hints,
            metachar_names: response.metachar_names,
        };
        render_search_outcome(
            out,
            &mut err,
            &cwd,
            &resolved,
            &response.output,
            true,
            &kind,
        );
    }
    Ok(())
}

/// Confirms editing mode is active on the running daemon.
///
/// Connects to the daemon's IPC socket and sends a status query.
/// The actual state transition happens in the `PreToolUse` hook — this
/// command only prints confirmation for the agent's stdout.
///
/// # Errors
///
/// Returns an error if no daemon is running.
#[cfg(unix)]
async fn run_start_editing(out: &mut cli::Output) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/editing-start"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
    let status = response
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("ok");

    if status == "ok" {
        let _ = out.writeln(format_args!("editing mode active"));
    } else {
        let msg = response
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unexpected response");
        anyhow::bail!("{msg}");
    }

    Ok(())
}

/// Sentinel printed when the edited set is genuinely empty (0 covered files in
/// the handoff): "nothing to report" made observable rather than a silent exit.
///
/// A clean COVERED set is NOT silent — the daemon renders a `[clean]` receipt
/// line per covered file (ws37 ticket 01, retiring silent-on-clean). So an empty
/// daemon receipt means the set had no covered files; this sentinel keeps that
/// case distinguishable from a set that produced a receipt.
#[cfg(unix)]
const NO_EDITED_FILES_SENTINEL: &str = "[no edited files]";

/// The daemon's `tool/editing-stop` response envelope (mirrors the grep/glob
/// JSON pattern): the rendered per-file receipt `output` plus the covered-file
/// count.
///
/// The daemon still sends a clean/dirty `status`, but the CLI no longer reads it
/// (ws37 ticket 01): the run exits `0` whether clean or dirty, and the receipt
/// itself carries the distinction, so `status` is ignored here.
#[cfg(unix)]
#[derive(Default, serde::Deserialize)]
struct DiagnosticsResponse {
    /// Rendered per-file receipt: every diagnosed file, `[clean]` beside the
    /// clean ones and diagnostics beneath the dirty ones.
    #[serde(default)]
    output: String,
    /// Count of covered files in the handoff. Lets the CLI print
    /// `[no edited files]` for a genuinely empty set (covered == 0) rather than
    /// a silent exit. Absent on a pre-fix daemon → defaults to 0.
    #[serde(default)]
    covered: usize,
    /// Daemon-detected fault (bug 100): a bare run with no staged handoff —
    /// no hooked session armed a gate, or the prepare's TTL blew — so the run
    /// never happened. Present → the CLI surfaces the message on stderr and
    /// exits `2` instead of printing a receipt-shaped success.
    #[serde(default)]
    error: Option<String>,
}

/// Implements `catenary diagnostics [paths…]`: prints diagnostics for the
/// edited files (bare) or the named paths (scoped), and pays the corresponding
/// editing debt.
///
/// Connects to the daemon's IPC socket and sends `tool/editing-stop` (the
/// internal handoff method name is unchanged by the user-facing rename). In a
/// hooked session the `PreToolUse` hook has already prepared the handoff — the
/// bare form retrieves the batch and prints the per-file receipt. When `paths`
/// is non-empty, they ride the request's `files` param: the daemon diagnoses
/// exactly those, with no handoff required (bug 100 — the hookless on-demand
/// form), and flips their gate flags on delivery when a hooked session staged
/// one. A bare run with no staged handoff is a daemon-detected fault (there is
/// no gate to pay). Relative paths resolve against the CLI's cwd before
/// dispatch — the daemon runs under a different cwd — matching how
/// `grep`/`glob` forward paths. Success (clean *or* dirty) returns `Ok(())`,
/// which the dispatcher maps to exit `0`.
///
/// # Errors
///
/// Returns an error (mapped to fault exit `2`) if no daemon is running, the
/// IPC fails, the working directory can't be resolved, the response is
/// malformed, or the daemon reports a fault (a bare run with no staged
/// handoff).
#[cfg(unix)]
async fn run_done_editing(out: &mut cli::Output, paths: &[String]) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();

    // Resolve relative scoped paths against the CLI's cwd (the daemon runs under
    // a different cwd). Bare form (no paths) sends an empty set → the daemon
    // re-diagnoses the whole batch and flips its flags on delivery.
    let files: Vec<String> = if paths.is_empty() {
        Vec::new()
    } else {
        let cwd = std::env::current_dir().context("cannot determine working directory")?;
        paths
            .iter()
            .map(|p| {
                let path = std::path::Path::new(p);
                let resolved = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    cwd.join(path)
                };
                resolved.to_string_lossy().into_owned()
            })
            .collect()
    };

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("catenary daemon not running")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/editing-stop", "files": files});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    // Read the full response (a single JSON line; read to EOF defensively).
    let mut buf_reader = BufReader::new(reader);
    let mut response = String::new();
    loop {
        let mut line = String::new();
        let n = buf_reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        response.push_str(&line);
    }

    emit_diagnostics_response(out, &response)
}

/// Parse a `tool/editing-stop` response and print the per-file receipt. Split
/// from the IPC for unit testing.
///
/// The receipt is rendered daemon-side (every diagnosed file listed, `[clean]`
/// beside the clean ones and diagnostics beneath the dirty ones); this fn prints
/// it and owns the genuinely-empty case. `Ok(())` (clean *or* dirty) maps to
/// exit `0`.
///
/// # Errors
///
/// Returns an error if the response is not valid JSON, or if the daemon
/// reported a fault in the envelope's `error` field (bug 100: a bare run with
/// no staged handoff). Both map to exit `2`.
#[cfg(unix)]
fn emit_diagnostics_response(out: &mut cli::Output, response: &str) -> Result<()> {
    let parsed: DiagnosticsResponse = serde_json::from_str(response.trim())
        .context("invalid diagnostics response from daemon")?;

    // A daemon-detected fault: the run never happened, so there is no receipt
    // to print — surface the teaching message as the error (stderr + exit 2).
    if let Some(message) = parsed.error {
        anyhow::bail!("{message}");
    }

    let trimmed = parsed.output.trim();
    if trimmed.is_empty() {
        // A clean COVERED set is no longer silent — the daemon renders a
        // `[clean]` receipt line per covered file — so empty output means the
        // set had no covered files. Print the sentinel for a genuinely empty
        // set (covered == 0). The residual covered > 0 case (every handed-off
        // file dropped during resolve/validate) is a rare defensive edge with
        // nothing to render.
        if parsed.covered == 0 {
            let _ = out.writeln(format_args!("{NO_EDITED_FILES_SENTINEL}"));
        }
    } else {
        let _ = out.writeln(format_args!("{trimmed}"));
    }

    Ok(())
}

/// Sends a pin (`roots-add`) or unpin (`roots-rm`) request to the running
/// daemon and prints the outcome.
///
/// Resolves the path to the form matched against the tracked set — canonical
/// when the directory exists, its lexically-absolutized spelling when it does
/// not — so an `unpin` of an already-removed directory still deregisters the
/// pin instead of hard-erroring (bug 54 gap 1). Both resolution
/// ([`resolve_root_path`](cli::commands::resolve_root_path)) and response
/// interpretation ([`render_root_outcome`](cli::commands::render_root_outcome))
/// are pure; this function owns only the IPC. A `not_found` on unpin is a
/// benign, idempotent no-op.
///
/// # Errors
///
/// Returns an error if no daemon is running or the daemon reports a failure.
#[cfg(unix)]
async fn run_root_command(out: &mut cli::Output, path: PathBuf, method: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let resolved = cli::commands::resolve_root_path(&path);

    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({
        "method": method,
        "path": resolved.display().to_string(),
    });
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
    let status = response
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let daemon_msg = response.get("message").and_then(|v| v.as_str());
    let is_pin = method.contains("roots-add");

    match cli::commands::render_root_outcome(
        status,
        is_pin,
        &resolved.display().to_string(),
        daemon_msg,
    ) {
        Ok(line) => {
            let _ = out.writeln(format_args!("{line}"));
            Ok(())
        }
        Err(msg) => anyhow::bail!("{msg}"),
    }
}

/// Check installed hooks against embedded expected hooks at daemon startup.
///
/// Compares each host CLI's installed hooks with the compile-time expected
/// version. Emits `error!()` for any mismatch — the `LoggingServer`
/// routes this to the notification queue and the desktop notification
/// sink automatically.
///
/// Builds the stale-hooks daemon-startup notification.
///
/// Names the exact `catenary install <host>` subcommand to run: bare
/// `catenary install` only *lists* detected hosts (see `cli::install`), so the
/// notification must carry the host subcommand to be actionable (bug 70). This
/// mirrors the doctor surface's `run: catenary install <host>` wording.
#[cfg(unix)]
fn stale_hooks_message(host: &str, install_cmd: &str) -> String {
    format!("Stale {host} hooks detected. Run: catenary install {install_cmd}")
}

/// Only checks hosts that have hooks installed (missing hosts are ignored).
#[cfg(unix)]
fn check_stale_hooks() {
    /// Expected Claude Code hooks, embedded at compile time.
    const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../plugins/catenary/hooks/hooks.json");
    /// Expected Antigravity CLI hooks, embedded at compile time.
    const ANTIGRAVITY_HOOKS_EXPECTED: &str =
        include_str!("../plugins/catenary-antigravity/hooks.json");

    fn normalize_json(s: &str) -> String {
        serde_json::from_str::<serde_json::Value>(s)
            .ok()
            .and_then(|v| serde_json::to_string(&v).ok())
            .unwrap_or_else(|| s.trim().to_string())
    }

    fn check_host(host: &str, install_cmd: &str, installed_path: &std::path::Path, expected: &str) {
        match std::fs::read_to_string(installed_path) {
            Ok(installed) if normalize_json(&installed) == normalize_json(expected) => {}
            Ok(_) => {
                tracing::error!(
                    source = Source::HookDispatch.as_str(),
                    "{}",
                    stale_hooks_message(host, install_cmd),
                );
            }
            Err(_) => {} // Hooks file not found — host not installed, skip.
        }
    }

    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home);

    // Claude Code: hooks live inside the plugin install path, which is
    // recorded in installed_plugins.json. Resolve the actual path.
    if let Some(hooks_path) = resolve_claude_hooks_path(&home) {
        check_host("Claude Code", "claude", &hooks_path, CLAUDE_HOOKS_EXPECTED);
    }

    // Antigravity: the plugin dir is the install location (`catenary install
    // antigravity` writes it; `detect_antigravity` and the context-file
    // rewrite resolve the same path). The old `~/.antigravity/hooks.json`
    // probe was a dead path — the not-found arm silently skipped it, so
    // Antigravity staleness was never actually checked.
    let antigravity_hooks = home.join(".gemini/config/plugins/catenary/hooks.json");
    check_host(
        "Antigravity CLI",
        "antigravity",
        &antigravity_hooks,
        ANTIGRAVITY_HOOKS_EXPECTED,
    );
}

/// Resolve the hooks.json copy that Claude Code actually EXECUTES.
///
/// The comparand depends on the marketplace source type (misc 180):
///
/// - **Directory sources** (dev installs): Claude Code resolves hook content
///   LIVE from the marketplace's source directory, not the version-keyed
///   plugin cache. The live copy is `<installLocation>/plugins/catenary/hooks/
///   hooks.json` (the plugin's `source: ./plugins/catenary` under the
///   marketplace root recorded in `known_marketplaces.json`). Comparing the
///   frozen cache here reads stale/current backwards.
/// - **Release / marketplace sources** (github): the frozen version-keyed
///   plugin cache IS what executes, so compare `<installPath>/hooks/hooks.json`
///   from `installed_plugins.json`.
///
/// Returns `None` if Claude Code is not installed or the copy cannot be
/// resolved.
#[cfg(unix)]
fn resolve_claude_hooks_path(home: &std::path::Path) -> Option<std::path::PathBuf> {
    // Directory source: the live hooks live under the marketplace source dir.
    if let Some(live) = resolve_claude_directory_hooks_path(home) {
        return Some(live);
    }

    // Release / marketplace source: the frozen version-keyed cache executes.
    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let plugins_json = std::fs::read_to_string(plugins_file).ok()?;
    let plugins: serde_json::Value = serde_json::from_str(&plugins_json).ok()?;
    let entries = plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)?;
    let entry = entries.first()?;
    let install_path = entry
        .get("installPath")
        .and_then(serde_json::Value::as_str)?;
    Some(std::path::PathBuf::from(install_path).join("hooks/hooks.json"))
}

/// The live hooks copy for a directory-source install, or `None` when the
/// marketplace is not a directory source.
///
/// For a directory source, `known_marketplaces.json` records
/// `source.source == "directory"` and the source directory in `source.path`
/// (falling back to the sibling `installLocation`). The plugin's hooks live at
/// `<dir>/plugins/catenary/hooks/hooks.json`, mirroring the marketplace's
/// `plugins[].source` of `./plugins/catenary`.
#[cfg(unix)]
fn resolve_claude_directory_hooks_path(home: &std::path::Path) -> Option<std::path::PathBuf> {
    let marketplaces_file = home.join(".claude/plugins/known_marketplaces.json");
    let marketplaces_json = std::fs::read_to_string(marketplaces_file).ok()?;
    let marketplaces: serde_json::Value = serde_json::from_str(&marketplaces_json).ok()?;
    let entry = marketplaces.get("catenary")?;
    let source = entry.get("source")?;
    if source.get("source").and_then(serde_json::Value::as_str) != Some("directory") {
        return None;
    }
    let dir = source
        .get("path")
        .or_else(|| entry.get("installLocation"))
        .and_then(serde_json::Value::as_str)?;
    Some(std::path::PathBuf::from(dir).join("plugins/catenary/hooks/hooks.json"))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Legacy DB drain tests (observability ticket 07) ───────────

    #[cfg(unix)]
    #[test]
    fn drain_db_at_removes_db_and_wal_shm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("catenary.db");
        std::fs::write(&db, vec![b'x'; 100]).expect("write db");
        std::fs::write(dir.path().join("catenary.db-wal"), vec![b'y'; 50]).expect("write wal");
        std::fs::write(dir.path().join("catenary.db-shm"), vec![b'z'; 25]).expect("write shm");

        let reclaimed = drain_db_at(&db);

        assert_eq!(reclaimed, 175, "reclaimed bytes = db + wal + shm");
        assert!(!db.exists(), "db removed");
        assert!(!dir.path().join("catenary.db-wal").exists(), "wal removed");
        assert!(!dir.path().join("catenary.db-shm").exists(), "shm removed");
    }

    #[cfg(unix)]
    #[test]
    fn drain_db_at_is_noop_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(drain_db_at(&dir.path().join("catenary.db")), 0);
    }

    // ── Stale-hooks notification tests ────────────────────────────

    #[cfg(unix)]
    #[test]
    fn stale_hooks_message_names_install_subcommand() {
        // Bare `catenary install` only lists hosts; the notification must name
        // the host subcommand so following it verbatim actually reinstalls
        // (bug 70), matching the doctor surface's `run: catenary install <host>`.
        assert_eq!(
            stale_hooks_message("Claude Code", "claude"),
            "Stale Claude Code hooks detected. Run: catenary install claude",
        );
        assert_eq!(
            stale_hooks_message("Antigravity CLI", "antigravity"),
            "Stale Antigravity CLI hooks detected. Run: catenary install antigravity",
        );
    }

    // ── Hooks-comparand resolution (misc 180) ─────────────────────

    /// Write `installed_plugins.json` pointing the plugin at a version-keyed
    /// cache dir (the frozen copy), the release/marketplace layout.
    #[cfg(unix)]
    fn write_installed_plugins(home: &std::path::Path, install_path: &std::path::Path) {
        let plugins_dir = home.join(".claude/plugins");
        std::fs::create_dir_all(&plugins_dir).expect("create plugins dir");
        let installed = serde_json::json!({
            "version": 2,
            "plugins": {
                "catenary@catenary": [{
                    "scope": "user",
                    "installPath": install_path.to_string_lossy(),
                    "version": "2.0.2",
                }],
            },
        });
        std::fs::write(
            plugins_dir.join("installed_plugins.json"),
            serde_json::to_string_pretty(&installed).expect("serialize installed_plugins"),
        )
        .expect("write installed_plugins.json");
    }

    /// Write `known_marketplaces.json` for the catenary marketplace with the
    /// given inner `source.source` type and directory path.
    #[cfg(unix)]
    fn write_known_marketplaces(home: &std::path::Path, source_type: &str, dir: &std::path::Path) {
        let plugins_dir = home.join(".claude/plugins");
        std::fs::create_dir_all(&plugins_dir).expect("create plugins dir");
        let marketplaces = serde_json::json!({
            "catenary": {
                "source": { "source": source_type, "path": dir.to_string_lossy() },
                "installLocation": dir.to_string_lossy(),
            },
        });
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            serde_json::to_string_pretty(&marketplaces).expect("serialize known_marketplaces"),
        )
        .expect("write known_marketplaces.json");
    }

    #[cfg(unix)]
    #[test]
    fn directory_source_resolves_live_source_hooks_not_the_frozen_cache() {
        // A directory-source install: the frozen version-keyed cache exists and
        // differs from the live source dir. Claude Code executes the LIVE copy,
        // so the comparand must be `<installLocation>/plugins/catenary/hooks/
        // hooks.json` — not the cache — or the staleness verdict inverts.
        let home = tempfile::tempdir().expect("tempdir");
        let cache = home
            .path()
            .join(".claude/plugins/cache/catenary/catenary/2.0.2");
        let source = home.path().join("Projects/Catenary");
        write_installed_plugins(home.path(), &cache);
        write_known_marketplaces(home.path(), "directory", &source);

        let resolved = resolve_claude_hooks_path(home.path()).expect("resolve hooks path");
        assert_eq!(
            resolved,
            source.join("plugins/catenary/hooks/hooks.json"),
            "directory source must compare against the live source dir, not the cache",
        );
    }

    #[cfg(unix)]
    #[test]
    fn github_source_keeps_the_frozen_cache_comparand() {
        // A github (release) install: the frozen version-keyed cache IS what
        // executes, so the comparand stays `<installPath>/hooks/hooks.json`.
        let home = tempfile::tempdir().expect("tempdir");
        let cache = home
            .path()
            .join(".claude/plugins/cache/catenary/catenary/2.0.2");
        let marketplace = home.path().join(".claude/plugins/marketplaces/catenary");
        write_installed_plugins(home.path(), &cache);
        write_known_marketplaces(home.path(), "github", &marketplace);

        let resolved = resolve_claude_hooks_path(home.path()).expect("resolve hooks path");
        assert_eq!(
            resolved,
            cache.join("hooks/hooks.json"),
            "github source keeps the frozen-cache comparand",
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_marketplaces_falls_back_to_the_frozen_cache() {
        // No known_marketplaces.json (or unreadable): fall back to the frozen
        // cache path rather than fabricating a directory comparand.
        let home = tempfile::tempdir().expect("tempdir");
        let cache = home
            .path()
            .join(".claude/plugins/cache/catenary/catenary/2.0.2");
        write_installed_plugins(home.path(), &cache);

        let resolved = resolve_claude_hooks_path(home.path()).expect("resolve hooks path");
        assert_eq!(resolved, cache.join("hooks/hooks.json"));
    }

    // ── CLI hook subcommand tests ─────────────────────────────────

    #[test]
    fn test_cli_hook_pre_tool() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-tool", "--format=claude"]);
        let args = args.expect("hook pre-tool should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PreTool { .. }));
    }

    #[test]
    fn test_cli_hook_post_agent() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "post-agent", "--format=claude"]);
        let args = args.expect("hook post-agent should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PostAgent { .. }));
    }

    #[test]
    fn test_cli_hook_session_start() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "session-start", "--format=claude"]);
        let args = args.expect("hook session-start should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::SessionStart { .. }));
    }

    #[test]
    fn test_cli_hook_session_end() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "session-end", "--format=claude"]);
        let args = args.expect("hook session-end should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::SessionEnd { .. }));
    }

    #[test]
    fn test_cli_hook_pre_invocation() {
        use clap::Parser;
        let args =
            Args::try_parse_from(["catenary", "hook", "pre-invocation", "--format=antigravity"]);
        let args = args.expect("hook pre-invocation should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(
            command,
            HookCommand::PreInvocation {
                format: HostFormat::Antigravity
            }
        ));
    }

    #[test]
    fn test_cli_hook_subagent_start() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "subagent-start", "--format=claude"]);
        let args = args.expect("hook subagent-start should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::SubagentStart { .. }));
    }

    #[test]
    fn test_cli_hook_worktree_remove() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "worktree-remove", "--format=claude"]);
        let args = args.expect("hook worktree-remove should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::WorktreeRemove { .. }));
    }

    #[test]
    fn test_cli_hook_worktree_create() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "worktree-create", "--format=claude"]);
        let args = args.expect("hook worktree-create should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::WorktreeCreate { .. }));
    }

    #[test]
    fn test_cli_hook_permission_request() {
        use clap::Parser;
        let args =
            Args::try_parse_from(["catenary", "hook", "permission-request", "--format=claude"]);
        let args = args.expect("hook permission-request should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PermissionRequest { .. }));
    }

    #[test]
    fn test_cli_hook_antigravity_format() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-tool", "--format=antigravity"]);
        let args = args.expect("hook pre-tool with antigravity format should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(
            command,
            HookCommand::PreTool {
                format: HostFormat::Antigravity
            }
        ));
    }

    #[test]
    fn test_cli_hook_opencode_format() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-tool", "--format=opencode"]);
        let args = args.expect("hook pre-tool with opencode format should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(
            command,
            HookCommand::PreTool {
                format: HostFormat::OpenCode
            }
        ));
    }

    #[test]
    fn test_cli_hook_reserved_shims_parse() {
        use clap::Parser;
        // The reserved no-op shims (full-surface registration, pre-v2 ruling):
        // every kebab-named event must parse as a `catenary hook` subcommand
        // with the house `--format` flag.
        const RESERVED_SHIMS: [&str; 19] = [
            "setup",
            "user-prompt-submit",
            "user-prompt-expansion",
            "post-invocation",
            "permission-denied",
            "post-tool-use",
            "post-tool-use-failure",
            "notification",
            "task-created",
            "task-completed",
            "stop-failure",
            "teammate-idle",
            "instructions-loaded",
            "config-change",
            "cwd-changed",
            "pre-compact",
            "post-compact",
            "elicitation",
            "elicitation-result",
        ];
        for name in RESERVED_SHIMS {
            let args = Args::try_parse_from(["catenary", "hook", name, "--format=claude"]);
            let args = args.expect("reserved hook shim should parse");
            assert!(
                matches!(args.command, Some(Command::Hook { .. })),
                "`catenary hook {name}` did not parse as a Hook command"
            );
        }
    }

    /// Collect every `"command"` string from `{"type": "command", …}` hook
    /// objects anywhere in a hooks.json tree. Shape-agnostic (recursive walk),
    /// so it covers both the Claude layout (`hooks` → event → registrations →
    /// `hooks` array) and the Antigravity layout (named groups with bare or
    /// matcher-nested entries).
    fn collect_hook_commands(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("type").and_then(serde_json::Value::as_str) == Some("command")
                    && let Some(cmd) = map.get("command").and_then(serde_json::Value::as_str)
                {
                    out.push(cmd.to_owned());
                }
                for v in map.values() {
                    collect_hook_commands(v, out);
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    collect_hook_commands(v, out);
                }
            }
            _ => {}
        }
    }

    /// Every hook registration in an embedded hooks.json must parse as a
    /// `catenary hook …` invocation — a registration can never point at a
    /// subcommand the CLI does not have (full-surface registration, pre-v2).
    ///
    /// A registration may carry a `|| …` shell fallback after the invocation
    /// (the `SessionStart` missing-binary bootstrap hint): only the leading
    /// `catenary hook …` leg is parsed; the fallback is the shell's, not the
    /// CLI's.
    fn assert_hook_registrations_parse(embedded: &str, label: &str) {
        use clap::Parser;
        let json: serde_json::Value =
            serde_json::from_str(embedded).expect("embedded hooks.json is valid JSON");
        let mut commands = Vec::new();
        collect_hook_commands(&json, &mut commands);
        assert!(
            !commands.is_empty(),
            "{label}: no hook commands found in embedded hooks.json"
        );
        for registered in &commands {
            let invocation = registered
                .split("||")
                .next()
                .expect("split always yields a first segment")
                .trim();
            let argv: Vec<&str> = invocation.split_whitespace().collect();
            let parsed = Args::try_parse_from(&argv);
            assert!(
                parsed.is_ok(),
                "{label}: registered command {registered:?} does not parse: {:?}",
                parsed.err()
            );
            assert!(
                matches!(
                    parsed.expect("parse result checked above").command,
                    Some(Command::Hook { .. })
                ),
                "{label}: registered command {registered:?} is not a `catenary hook` invocation"
            );
        }
    }

    #[test]
    fn test_claude_hooks_json_registrations_all_parse() {
        assert_hook_registrations_parse(
            include_str!("../plugins/catenary/hooks/hooks.json"),
            "Claude Code",
        );
    }

    #[test]
    fn test_antigravity_hooks_json_registrations_all_parse() {
        assert_hook_registrations_parse(
            include_str!("../plugins/catenary-antigravity/hooks.json"),
            "Antigravity",
        );
    }

    #[test]
    fn claude_session_start_carries_the_missing_binary_hint() {
        // A marketplace install without the binary must not fail opaquely:
        // the SessionStart registration carries a shell fallback that fires
        // only when the `catenary` invocation itself cannot run
        // (command-not-found — the hook handler is fail-open and exits 0
        // whenever the binary exists), answering with a `systemMessage`
        // teaching the install one-liner plus a best-effort desktop
        // notification (notify-send, then osascript). The fallback is a
        // hint, not a bootstrap — the sha256-pinned download-on-first-run
        // leg is post-release by construction (the pins for a release's
        // assets cannot exist in the tree the tag points at).
        let embedded = include_str!("../plugins/catenary/hooks/hooks.json");
        let json: serde_json::Value =
            serde_json::from_str(embedded).expect("embedded hooks.json is valid JSON");
        let mut commands = Vec::new();
        collect_hook_commands(&json, &mut commands);
        let session_start = commands
            .iter()
            .find(|c| c.contains("session-start"))
            .expect("SessionStart is registered");
        for needle in [
            "|| {",
            "notify-send",
            "osascript",
            "\"systemMessage\"",
            "command -v brew",
            "brew install twowells/tap/catenary",
            "install.sh | sh",
            "start a new session",
        ] {
            assert!(
                session_start.contains(needle),
                "missing-binary hint lacks {needle:?}: {session_start}"
            );
        }
        // Only SessionStart carries the teaching fallback — one teaching
        // surface, no per-event noise.
        let with_teach = commands
            .iter()
            .filter(|c| c.contains("systemMessage"))
            .count();
        assert_eq!(
            with_teach, 1,
            "exactly one registration (SessionStart) carries the teaching fallback"
        );
    }

    #[test]
    fn reserved_registrations_fail_open_on_version_skew() {
        // New-hooks/old-binary skew (a refreshed plugin cache ahead of the
        // installed binary — live-sighted 2026-07-10): clap answers an
        // unrecognized subcommand with exit 2, which Claude Code reads as a
        // deliberate hook BLOCK on blocking-capable events — a stale binary
        // bricked prompt submission. Reserved shims never deliberately block,
        // so their registrations fail open at the REGISTRATION layer:
        // `|| true` (Claude; silence is its empty answer). Behavioral
        // registrations (pre-tool, session-start, …) exist in every shipped
        // binary and may deliberately block — they carry no such tail.
        let embedded = include_str!("../plugins/catenary/hooks/hooks.json");
        let json: serde_json::Value =
            serde_json::from_str(embedded).expect("embedded hooks.json is valid JSON");
        let mut commands = Vec::new();
        collect_hook_commands(&json, &mut commands);
        let fail_open = commands.iter().filter(|c| c.ends_with("|| true")).count();
        assert_eq!(
            fail_open, 18,
            "every reserved Claude registration fails open on version skew"
        );

        // Antigravity's dialect is JSON-in/JSON-out, so its reserved
        // registrations answer the documented empty object on skew instead
        // of silence.
        let embedded = include_str!("../plugins/catenary-antigravity/hooks.json");
        let json: serde_json::Value =
            serde_json::from_str(embedded).expect("embedded hooks.json is valid JSON");
        let mut commands = Vec::new();
        collect_hook_commands(&json, &mut commands);
        let fail_open = commands
            .iter()
            .filter(|c| c.ends_with("|| printf '%s' '{}'"))
            .count();
        assert_eq!(
            fail_open, 2,
            "both reserved Antigravity registrations answer {{}} on version skew"
        );
    }

    #[test]
    fn test_cli_config_subcommand() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "config"]);
        let args = args.expect("config subcommand should parse");
        assert!(matches!(args.command, Some(Command::Config)));
    }

    // ── CLI editing subcommand tests ───────────────────────────────

    #[test]
    fn test_cli_editing_start() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "editing", "start"]);
        let args = args.expect("editing start should parse");
        assert!(matches!(
            args.command,
            Some(Command::Editing {
                command: EditingCommand::Start
            })
        ));
    }

    #[test]
    fn test_cli_diagnostics() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "diagnostics"]);
        let args = args.expect("diagnostics should parse");
        let Some(Command::Diagnostics { paths }) = args.command else {
            unreachable!("expected Diagnostics");
        };
        assert!(paths.is_empty(), "bare diagnostics has no path args");
    }

    #[test]
    fn diagnostics_with_paths_parses_as_scoped() {
        // Scoped paths are first-class (ws37 ticket 02): they parse into `paths`
        // and ride the consume request's `files` param so the daemon diagnoses
        // exactly those and drops them from the gate.
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "diagnostics", "a.rs", "b.rs"])
            .expect("scoped paths must parse");
        let Some(Command::Diagnostics { paths }) = args.command else {
            unreachable!("expected Diagnostics");
        };
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn editing_stop_retired() {
        // `catenary editing stop` was renamed to `catenary diagnostics`; the
        // old subcommand no longer parses.
        use clap::Parser;
        assert!(Args::try_parse_from(["catenary", "editing", "stop"]).is_err());
    }

    #[test]
    fn test_cli_version_subcommand() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "version"]);
        let args = args.expect("version subcommand should parse");
        assert!(matches!(args.command, Some(Command::Version)));
    }

    // ── CLI stop subcommand tests (misc 123) ──────────────────────

    #[test]
    fn stop_defaults_to_confirming() {
        use clap::Parser;
        let bare = Args::try_parse_from(["catenary", "stop"]).expect("bare stop parses");
        assert!(
            matches!(bare.command, Some(Command::Stop { force: false })),
            "bare `catenary stop` keeps the confirmation prompt",
        );
        let forced =
            Args::try_parse_from(["catenary", "stop", "--force"]).expect("stop --force parses");
        assert!(
            matches!(forced.command, Some(Command::Stop { force: true })),
            "`--force` sets the skip-prompt flag",
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_board_lists_client_roots_and_connected_since() {
        use catenary_mcp::state_snapshot::{ClientInfo, SessionEntry, now_iso};

        let sessions = vec![
            SessionEntry {
                client: ClientInfo {
                    name: "claude".to_string(),
                    version: None,
                },
                started_at: now_iso(),
                roots: vec!["/home/mark/Projects/Catenary".to_string()],
                ..SessionEntry::default()
            },
            // An empty client name renders as "unknown".
            SessionEntry {
                client: ClientInfo::default(),
                started_at: now_iso(),
                roots: vec!["/home/mark/Projects/homelab".to_string()],
                ..SessionEntry::default()
            },
        ];

        let board = render_stop_board(&sessions);
        assert!(
            board.starts_with("2 connected sessions will lose Catenary tooling"),
            "plural header: {board}",
        );
        assert!(board.contains("claude · connected"), "client name: {board}");
        assert!(
            board.contains("unknown · connected"),
            "empty client falls back to unknown: {board}",
        );
        assert!(
            board.contains("    /home/mark/Projects/Catenary"),
            "first root listed: {board}",
        );
        assert!(
            board.contains("    /home/mark/Projects/homelab"),
            "second root listed: {board}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_board_singular_and_missing_roots() {
        use catenary_mcp::state_snapshot::{ClientInfo, SessionEntry, now_iso};

        let sessions = vec![SessionEntry {
            client: ClientInfo {
                name: "opencode".to_string(),
                version: None,
            },
            started_at: now_iso(),
            roots: vec![],
            ..SessionEntry::default()
        }];

        let board = render_stop_board(&sessions);
        assert!(
            board.starts_with("1 connected session will lose Catenary tooling"),
            "singular header: {board}",
        );
        assert!(
            board.contains("(no workspace roots)"),
            "rootless session labeled: {board}",
        );
    }

    // ── CLI grep subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_grep_minimal() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "grep", "foo"]);
        let args = args.expect("grep with pattern should parse");
        let Some(Command::Grep {
            pattern,
            scope,
            ignore_case,
            case_sensitive,
            word,
            fixed_strings,
            invert_match,
            files_with_matches,
            after_context,
            before_context,
            context,
            glob,
            type_filter,
            exclude,
            count,
            include_gitignored,
            include_hidden,
            line_number,
            with_filename,
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo");
        assert!(scope.is_empty());
        assert!(exclude.is_empty());
        assert!(!count);
        assert!(!include_gitignored);
        assert!(!include_hidden);
        // Hidden ripgrep-parity no-ops default off.
        assert!(!line_number);
        assert!(!with_filename);
        // Ripgrep-parity flags default off / unset.
        assert!(!ignore_case);
        assert!(!case_sensitive);
        assert!(!word);
        assert!(!fixed_strings);
        assert!(!invert_match);
        assert!(!files_with_matches);
        assert!(after_context.is_none());
        assert!(before_context.is_none());
        assert!(context.is_none());
        assert!(glob.is_empty());
        assert!(type_filter.is_empty());
    }

    #[test]
    fn test_cli_grep_ripgrep_flags_parse() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "foo",
            "-i",
            "-w",
            "-F",
            "-v",
            "-l",
            "-A",
            "2",
            "-B",
            "1",
            "-C",
            "3",
            "-g",
            "*.rs",
            "-g",
            "!target/**",
            "-t",
            "rust",
        ]);
        let args = args.expect("grep with ripgrep flags should parse");
        let Some(Command::Grep {
            ignore_case,
            word,
            fixed_strings,
            invert_match,
            files_with_matches,
            after_context,
            before_context,
            context,
            glob,
            type_filter,
            ..
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert!(ignore_case);
        assert!(word);
        assert!(fixed_strings);
        assert!(invert_match);
        assert!(files_with_matches);
        assert_eq!(after_context, Some(2));
        assert_eq!(before_context, Some(1));
        assert_eq!(context, Some(3));
        assert_eq!(glob, vec!["*.rs", "!target/**"]);
        assert_eq!(type_filter, vec!["rust"]);
    }

    #[test]
    fn test_cli_grep_long_flag_spellings() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "foo",
            "--case-sensitive",
            "--files-with-matches",
            "--glob",
            "*.md",
            "--type",
            "md",
        ]);
        let args = args.expect("grep with long flags should parse");
        let Some(Command::Grep {
            case_sensitive,
            files_with_matches,
            glob,
            type_filter,
            ..
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert!(case_sensitive);
        assert!(files_with_matches);
        assert_eq!(glob, vec!["*.md"]);
        assert_eq!(type_filter, vec!["md"]);
    }

    #[test]
    fn test_cli_grep_single_path() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "grep", "foo|bar", "src/main.rs"]);
        let args = args.expect("grep with single path should parse");
        let Some(Command::Grep { pattern, scope, .. }) = args.command else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo|bar");
        assert_eq!(scope, vec!["src/main.rs"]);
    }

    #[test]
    fn test_cli_grep_variadic_paths() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "pattern",
            "src/tui/stream.rs",
            "src/tui/mod.rs",
        ]);
        let args = args.expect("grep with multiple paths should parse");
        let Some(Command::Grep { pattern, scope, .. }) = args.command else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "pattern");
        assert_eq!(scope, vec!["src/tui/stream.rs", "src/tui/mod.rs"]);
    }

    #[test]
    fn test_cli_grep_all_flags() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "foo|bar",
            "src/",
            "--exclude-pattern",
            "tests/",
            "--count",
            "--include-gitignored",
            "--include-hidden",
        ]);
        let args = args.expect("grep with all flags should parse");
        let Some(Command::Grep {
            pattern,
            scope,
            exclude,
            count,
            include_gitignored,
            include_hidden,
            ..
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo|bar");
        assert_eq!(scope, vec!["src/"]);
        assert_eq!(exclude, vec!["tests/".to_string()]);
        assert!(count);
        assert!(include_gitignored);
        assert!(include_hidden);
    }

    #[test]
    fn test_cli_grep_exclude_pattern_repeatable() {
        use clap::Parser;
        // `--exclude-pattern` is repeatable (bug 89), matching its siblings
        // `--glob`/`--type`: a second occurrence appends rather than erroring.
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "foo",
            "--exclude-pattern",
            "tests/**",
            "--exclude-pattern",
            "vendor/**",
        ])
        .expect("repeated --exclude-pattern should parse");
        let Some(Command::Grep { exclude, .. }) = args.command else {
            unreachable!("expected Grep command");
        };
        assert_eq!(
            exclude,
            vec!["tests/**".to_string(), "vendor/**".to_string()]
        );
    }

    #[test]
    fn test_cli_grep_page_flag_is_rejected() {
        use clap::Parser;
        // `--page` was retired with paging (pipeable-output ticket 03).
        let result = Args::try_parse_from(["catenary", "grep", "foo", "--page", "2"]);
        assert!(result.is_err(), "grep --page should no longer parse");
    }

    #[test]
    fn test_cli_grep_missing_pattern() {
        use clap::Parser;
        let result = Args::try_parse_from(["catenary", "grep"]);
        assert!(result.is_err(), "grep without pattern should fail");
    }

    #[test]
    fn test_cli_grep_line_number_is_accepted_noop() {
        use clap::Parser;
        // misc 134: `-n`/`--line-number` parse (ripgrep muscle memory) but do
        // nothing — line numbers are unconditional in the output (`path:N`).
        for spelling in ["-n", "--line-number"] {
            let args = Args::try_parse_from(["catenary", "grep", "foo", spelling])
                .expect("grep -n/--line-number should parse");
            let Some(Command::Grep { line_number, .. }) = args.command else {
                unreachable!("expected Grep command");
            };
            assert!(line_number, "{spelling} sets the no-op flag");
        }
    }

    #[test]
    fn test_cli_grep_with_filename_is_accepted_noop() {
        use clap::Parser;
        // misc 134: `-H`/`--with-filename` parse but do nothing — filenames are
        // always shown in the output.
        for spelling in ["-H", "--with-filename"] {
            let args = Args::try_parse_from(["catenary", "grep", "foo", spelling])
                .expect("grep -H/--with-filename should parse");
            let Some(Command::Grep { with_filename, .. }) = args.command else {
                unreachable!("expected Grep command");
            };
            assert!(with_filename, "{spelling} sets the no-op flag");
        }
    }

    #[test]
    fn test_cli_grep_count_short_alias() {
        use clap::Parser;
        // misc 134: `-c` is a hidden ripgrep-letter short for `--count`.
        let args = Args::try_parse_from(["catenary", "grep", "foo", "-c"])
            .expect("grep -c should parse as --count");
        let Some(Command::Grep { count, .. }) = args.command else {
            unreachable!("expected Grep command");
        };
        assert!(count, "-c tallies like --count");
    }

    #[test]
    fn test_cli_grep_suppressor_flags_still_rejected() {
        use clap::Parser;
        // misc 134: the no-ops accept only the affirmative spelling. A
        // suppressor whose requested behavior we don't honor stays an honest
        // error rather than lying by silently accepting it.
        for flag in ["--no-line-number", "--no-filename"] {
            assert!(
                Args::try_parse_from(["catenary", "grep", "foo", flag]).is_err(),
                "{flag} must not parse"
            );
        }
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn test_cli_grep_help_shows_only_long_forms() {
        // misc 134 (maintainer ruling): only long forms are agent-facing. Every
        // grep short is a hidden `short_alias`, so `catenary grep --help` renders
        // long forms only. clap prints `-x, --long` when a short is visible, so
        // the absence of every `-x, --long` combined form proves no short shows.
        use clap::CommandFactory;
        let app = Args::command();
        let mut grep = app
            .find_subcommand("grep")
            .expect("grep subcommand present")
            .clone();
        let help = grep.render_long_help().to_string();
        for combined in [
            "-i, --ignore-case",
            "-s, --case-sensitive",
            "-w, --word-regexp",
            "-F, --fixed-strings",
            "-v, --invert-match",
            "-l, --files-with-matches",
            "-A, --after-context",
            "-B, --before-context",
            "-C, --context",
            "-g, --glob",
            "-t, --type",
            "-c, --count",
        ] {
            assert!(
                !help.contains(combined),
                "grep --help must not show short form `{combined}`: {help}"
            );
        }
        // The long forms are still documented (help still teaches the surface).
        for long in [
            "--ignore-case",
            "--count",
            "--after-context",
            "--glob",
            "--type",
        ] {
            assert!(help.contains(long), "long form {long} present: {help}");
        }
        // The hidden ripgrep-parity no-ops appear nowhere in help.
        for hidden in ["--line-number", "--with-filename"] {
            assert!(
                !help.contains(hidden),
                "hidden no-op {hidden} must not surface in help: {help}"
            );
        }
    }

    // ── CLI glob subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_glob_minimal() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "glob", "src/"]);
        let args = args.expect("glob with path should parse");
        let Some(Command::Glob {
            paths,
            exclude,
            count,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Glob command");
        };
        assert_eq!(paths, vec!["src/"]);
        assert!(exclude.is_empty());
        assert!(!count);
        assert!(!include_gitignored);
        assert!(!include_hidden);
    }

    #[test]
    fn test_cli_glob_parses_multiple_positionals_for_the_arity_refusal() {
        use clap::Parser;
        // clap still *collects* multiple positionals so the arity refusal can
        // give a generous diagnosis (VERBS moment 1) — the `[pattern]` slice
        // match in the arm is what refuses N≠1, not clap. Parse must succeed.
        let args = Args::try_parse_from([
            "catenary",
            "glob",
            "src/tui/stream.rs",
            "src/tui/mod.rs",
            "src/tui/render.rs",
        ]);
        let args = args.expect("glob collects multiple positionals for the arity diagnosis");
        let Some(Command::Glob { paths, .. }) = args.command else {
            unreachable!("expected Glob command");
        };
        assert_eq!(
            paths,
            vec!["src/tui/stream.rs", "src/tui/mod.rs", "src/tui/render.rs"]
        );
    }

    #[test]
    fn glob_arity_refusal_bare_teaches_the_nullglob_rationale() {
        let msg = glob_arity_refusal(&[]);
        assert!(msg.contains("takes one pattern — got none"), "{msg}");
        assert!(
            msg.contains("nullglob"),
            "bare form keys on nullglob: {msg}"
        );
        assert!(
            msg.contains("catenary glob '*'"),
            "cwd listing spelling: {msg}"
        );
    }

    #[test]
    fn glob_arity_refusal_multi_names_the_likely_expansion() {
        // A shared extension is the fingerprint of a shell glob expansion
        // (`*.rs` → several `.rs` files) — the refusal names the pattern to quote.
        let args = vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()];
        let msg = glob_arity_refusal(&args);
        assert!(msg.contains("got 3 arguments"), "{msg}");
        assert!(
            msg.contains("catenary glob '*.rs'"),
            "names the likely expansion: {msg}"
        );
        assert!(msg.contains("{a,b}"), "brace alternation hint: {msg}");
    }

    #[test]
    fn glob_arity_refusal_multi_without_shared_ext_is_generic() {
        let args = vec!["Makefile".to_string(), "src".to_string()];
        let msg = glob_arity_refusal(&args);
        assert!(msg.contains("got 2 arguments"), "{msg}");
        assert!(
            !msg.contains("catenary glob '*."),
            "no spurious extension guess: {msg}"
        );
    }

    #[test]
    fn test_cli_glob_all_flags() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "glob",
            "src/",
            "--exclude-pattern",
            "target/**",
            "--count",
            "--include-gitignored",
            "--include-hidden",
        ]);
        let args = args.expect("glob with all flags should parse");
        let Some(Command::Glob {
            paths,
            exclude,
            count,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Glob command");
        };
        assert_eq!(paths, vec!["src/"]);
        assert_eq!(exclude, vec!["target/**".to_string()]);
        assert!(count);
        assert!(include_gitignored);
        assert!(include_hidden);
    }

    #[test]
    fn test_cli_glob_exclude_pattern_repeatable() {
        use clap::Parser;
        // `--exclude-pattern` is repeatable (bug 89): a second occurrence
        // appends instead of hard-erroring `cannot be used multiple times`.
        let args = Args::try_parse_from([
            "catenary",
            "glob",
            "src/",
            "--exclude-pattern",
            "target/**",
            "--exclude-pattern",
            "vendor/**",
        ])
        .expect("repeated --exclude-pattern should parse");
        let Some(Command::Glob { exclude, .. }) = args.command else {
            unreachable!("expected Glob command");
        };
        assert_eq!(
            exclude,
            vec!["target/**".to_string(), "vendor/**".to_string()]
        );
    }

    #[test]
    fn test_cli_glob_page_flag_is_rejected() {
        use clap::Parser;
        // `--page` was retired with paging (pipeable-output ticket 03).
        let result = Args::try_parse_from(["catenary", "glob", "src/", "--page", "2"]);
        assert!(result.is_err(), "glob --page should no longer parse");
    }

    #[test]
    fn test_cli_glob_bare_parses_empty_and_is_refused_at_runtime() {
        use clap::Parser;
        // Bare `glob` now parses to an empty positional Vec (the `required`
        // constraint was dropped so the arity refusal can give its own teaching);
        // the refusal is enforced in the command arm via the `[pattern]` match,
        // and `glob_arity_refusal(&[])` supplies the nullglob-keyed message
        // (VERBS moment 1, stderr + exit 2).
        let args = Args::try_parse_from(["catenary", "glob"]).expect("bare glob parses");
        let Some(Command::Glob { paths, .. }) = args.command else {
            unreachable!("expected Glob command");
        };
        assert!(paths.is_empty(), "bare glob has no positionals");
        let msg = glob_arity_refusal(&paths);
        assert!(msg.contains("takes one pattern — got none"), "{msg}");
    }

    // ── CLI pin / unpin / roots subcommand tests ────────────────────

    #[test]
    fn test_cli_pin() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "pin", "/tmp/project"]);
        let args = args.expect("pin should parse");
        let Some(Command::Pin { path }) = args.command else {
            unreachable!("expected Pin command");
        };
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[test]
    fn test_cli_unpin() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "unpin", "/tmp/project"]);
        let args = args.expect("unpin should parse");
        let Some(Command::Unpin { path }) = args.command else {
            unreachable!("expected Unpin command");
        };
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[test]
    fn test_cli_roots_bare_lists() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots"]);
        let args = args.expect("bare roots should parse");
        assert!(matches!(
            args.command,
            Some(Command::Roots { command: None })
        ));
    }

    #[test]
    fn test_cli_roots_ls_alias() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots", "ls"]);
        let args = args.expect("roots ls should parse");
        assert!(matches!(
            args.command,
            Some(Command::Roots {
                command: Some(RootsCommand::Ls)
            })
        ));
    }

    #[test]
    fn test_cli_roots_add_rm_still_parse_for_the_rename_teaching() {
        // The retired spellings still *parse* (so the dispatch can teach the
        // rename) — they route to a teaching error, not to a pin/unpin.
        use clap::Parser;
        assert!(matches!(
            Args::try_parse_from(["catenary", "roots", "add", "/tmp/p"])
                .expect("roots add parses")
                .command,
            Some(Command::Roots {
                command: Some(RootsCommand::Add { .. })
            })
        ));
        assert!(matches!(
            Args::try_parse_from(["catenary", "roots", "rm", "/tmp/p"])
                .expect("roots rm parses")
                .command,
            Some(Command::Roots {
                command: Some(RootsCommand::Rm { .. })
            })
        ));
    }

    #[test]
    fn test_cli_primer() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "primer"]);
        let args = args.expect("primer should parse");
        assert!(matches!(
            args.command,
            Some(Command::Primer { client: None })
        ));
    }

    #[test]
    fn test_cli_primer_with_declared_client() {
        // misc 177: the optional positional declares the client identity —
        // `catenary primer claude` — using the same `--format` vocabulary the
        // hook definitions declare with.
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "primer", "claude"]);
        let args = args.expect("primer with a client should parse");
        assert!(matches!(
            args.command,
            Some(Command::Primer {
                client: Some(HostFormat::Claude)
            })
        ));
    }

    #[test]
    fn primer_renders_the_teaching_payload() {
        // The primer prints the shared teaching payload: invariants, the flag
        // synopses, and the `--help` breadcrumbs. Capturing through
        // `Output::buffer` proves the handler writes via `Output` (not raw
        // `println!`).
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out, None);
        let text = out.into_string();

        // The invariants tier.
        assert!(
            text.contains("The edit→diagnostics loop"),
            "primer should emit the edit→diagnostics invariant"
        );
        // The flag-synopsis tier plus its point-of-use `--help` breadcrumbs.
        // Root management (`pin`/`unpin`/`roots`) lives in `catenary -h`, not
        // the primer — coverage is automatic (misc 146).
        for needle in [
            "catenary grep",
            "catenary glob",
            "catenary diagnostics",
            "--count",
            "full: catenary grep --help",
            "full: catenary glob --help",
        ] {
            assert!(
                text.contains(needle),
                "primer payload should document {needle}"
            );
        }
    }

    #[test]
    fn primer_is_the_shared_emitted_payload() {
        // The primer surface is byte-equal to the SessionStart emitted payload
        // (both call `cli::teaching::emitted_payload`, which includes the
        // daemon-staleness note under the same condition), modulo the trailing
        // newline `writeln` adds. Deterministic regardless of daemon staleness:
        // both sides observe the same daemon state, so they agree.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out, None);
        let printed = out.into_string();
        assert_eq!(
            printed.trim_end_matches('\n'),
            cli::teaching::emitted_payload(None)
        );
    }

    #[test]
    fn primer_claude_carries_the_dispatch_section() {
        // misc 177: `catenary primer claude` renders the SSOT payload keyed by
        // the declared Claude identity — the same rendering the Claude
        // SessionStart hook inlines — which carries the "Dispatching isolated
        // work" section. Bare `catenary primer` stays client-neutral.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out, Some(HostFormat::Claude));
        let claude = out.into_string();
        assert!(
            claude.contains("Dispatching isolated work"),
            "primer claude should teach worktree dispatch: {claude}"
        );
        assert!(
            claude.contains("isolation: \"worktree\""),
            "primer claude should name the isolation flag: {claude}"
        );
        assert_eq!(
            claude.trim_end_matches('\n'),
            cli::teaching::emitted_payload(Some(HostFormat::Claude)),
            "primer claude must be the SSOT claude rendering"
        );

        let mut out = cli::Output::buffer(80);
        run_primer(&mut out, None);
        let bare = out.into_string();
        assert!(
            !bare.contains("Dispatching isolated work"),
            "bare primer must not carry the dispatch section: {bare}"
        );
    }

    #[test]
    fn primer_has_no_pointers_or_retired_commands() {
        // Inlining is the point — no `catenary primer` / `catenary commands`
        // pointer — and the retired `editing` / `sed` subcommands must not
        // appear in agent-facing guidance.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out, None);
        let text = out.into_string();
        for retired in [
            "catenary editing",
            "editing start",
            "editing stop",
            "catenary primer",
            "catenary commands",
            "catenary sed",
        ] {
            assert!(
                !text.contains(retired),
                "primer must not mention `{retired}`"
            );
        }
    }

    #[test]
    #[allow(clippy::expect_used, reason = "test assertions")]
    fn glob_help_teaches_quoted_pattern_form() {
        // VERBS: `catenary glob --help` teaches the one-verb form — the positional
        // is a single quoted pattern (arity 1), decoded syntactically, with the
        // anchor in the pattern; a metachar-free spelling is a self-matching
        // literal.
        use clap::CommandFactory;
        let app = Args::command();
        let mut glob = app
            .find_subcommand("glob")
            .expect("glob subcommand present")
            .clone();
        let help = glob.render_long_help().to_string();
        assert!(
            help.contains("single glob pattern"),
            "one-verb teaser present: {help}"
        );
        assert!(
            help.contains("catenary glob 'src/**/*.rs'"),
            "cwd-relative example present: {help}"
        );
        assert!(
            help.contains("catenary glob '/abs/dir/**/*.md'"),
            "absolute example present: {help}"
        );
        assert!(
            help.contains("gitignore-aware"),
            "gitignore teaching present: {help}"
        );
        assert!(
            help.contains("self-matching literal"),
            "metachar-free-is-a-literal teaching present: {help}"
        );
        assert!(
            help.contains("brace") && help.contains("alternation"),
            "arity-1 / brace alternation teaching present: {help}"
        );
    }

    #[test]
    fn primer_teaches_glob_pattern_form() {
        // The pattern teaching is carried in the payload's invariants tier.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out, None);
        let text = out.into_string();
        assert!(
            text.contains("catenary glob 'src/**/*.rs'"),
            "primer teaches the glob pattern form: {text}"
        );
    }

    // ── CLI install subcommand tests ────────────────────────────────

    #[test]
    fn test_cli_install_bare() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install"]);
        let args = args.expect("install should parse");
        let Some(Command::Install { host, dry_run }) = args.command else {
            unreachable!("expected Install command");
        };
        assert!(host.is_none());
        assert!(!dry_run);
    }

    #[test]
    fn test_cli_install_claude() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "claude"]);
        let args = args.expect("install claude should parse");
        let Some(Command::Install {
            host: Some(InstallHost::Claude { source }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install Claude command");
        };
        assert!(source.is_none());
    }

    #[test]
    fn test_cli_install_claude_with_source() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "install",
            "claude",
            "/home/user/Projects/Catenary",
        ]);
        let args = args.expect("install claude with source should parse");
        let Some(Command::Install {
            host: Some(InstallHost::Claude { source }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install Claude command");
        };
        assert_eq!(source.as_deref(), Some("/home/user/Projects/Catenary"));
    }

    #[test]
    fn test_cli_install_gemini() {
        use clap::Parser;
        // Gemini CLI support is withdrawn (decision 030), but the subcommand is
        // deliberately retained so `catenary install gemini` announces the
        // withdrawal instead of erroring as an unknown host.
        let args = Args::try_parse_from(["catenary", "install", "gemini"]);
        let args = args.expect("install gemini should parse");
        assert!(matches!(
            args.command,
            Some(Command::Install {
                host: Some(InstallHost::Gemini { .. }),
                ..
            })
        ));
    }

    #[test]
    fn test_cli_install_antigravity() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "antigravity"]);
        let args = args.expect("install antigravity should parse");
        assert!(matches!(
            args.command,
            Some(Command::Install {
                host: Some(InstallHost::Antigravity { .. }),
                ..
            })
        ));
    }

    #[test]
    fn test_cli_install_opencode() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "opencode"]);
        let args = args.expect("install opencode should parse");
        let Some(Command::Install {
            host: Some(InstallHost::OpenCode { source, workspace }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install OpenCode command");
        };
        assert!(source.is_none());
        assert!(!workspace, "workspace should default to false");
    }

    #[test]
    fn test_cli_install_opencode_workspace() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "opencode", "--workspace"]);
        let args = args.expect("install opencode --workspace should parse");
        let Some(Command::Install {
            host: Some(InstallHost::OpenCode { workspace, .. }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install OpenCode command");
        };
        assert!(workspace, "--workspace should set the flag");
    }

    #[test]
    fn test_cli_install_dry_run() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "--dry-run", "claude"]);
        let args = args.expect("install --dry-run claude should parse");
        let Some(Command::Install { dry_run, .. }) = args.command else {
            unreachable!("expected Install command");
        };
        assert!(dry_run);
    }

    // ── CLI update subcommand tests ───────────────────────────────

    #[test]
    fn test_cli_update_bare() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update"]);
        let args = args.expect("update should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(!check);
        assert!(!force);
    }

    #[test]
    fn test_cli_update_check() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update", "--check"]);
        let args = args.expect("update --check should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(check);
        assert!(!force);
    }

    #[test]
    fn test_cli_update_force() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update", "--force"]);
        let args = args.expect("update --force should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(!check);
        assert!(force);
    }

    #[test]
    fn test_cli_update_check_and_force() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update", "--check", "--force"]);
        let args = args.expect("update --check --force should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(check);
        assert!(force);
    }

    // ── to_literal_paths tests ─────────────────────────────────────────

    #[test]
    fn test_to_literal_paths_empty() {
        let paths = to_literal_paths(vec![]);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_to_literal_paths_single() {
        let paths = to_literal_paths(vec!["src/main.rs".to_string()]);
        assert_eq!(paths, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn test_to_literal_paths_multiple() {
        let paths = to_literal_paths(vec![
            "src/tui/stream.rs".to_string(),
            "src/tui/mod.rs".to_string(),
        ]);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("src/tui/stream.rs"),
                PathBuf::from("src/tui/mod.rs"),
            ]
        );
    }

    // ── contains_glob_metachar tests ─────────────────────────────────

    #[test]
    fn test_contains_glob_metachar_star() {
        assert!(contains_glob_metachar("src/**/*.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_question() {
        assert!(contains_glob_metachar("src/?.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_bracket() {
        assert!(contains_glob_metachar("src/[ab].rs"));
    }

    #[test]
    fn test_contains_glob_metachar_brace() {
        assert!(contains_glob_metachar("src/{a,b}.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_none() {
        assert!(!contains_glob_metachar("src/main.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_directory() {
        assert!(!contains_glob_metachar("src/tui/"));
    }

    // ── resolve_search_paths tests ───────────────────────────────────

    #[test]
    fn resolve_existing_path_is_forwarded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("real.rs"), "x").expect("write");
        let res = resolve_search_paths(&[PathBuf::from("real.rs")], tmp.path());
        assert_eq!(res.forward, vec![PathBuf::from("real.rs")]);
        assert!(res.missing.is_empty());
    }

    #[test]
    fn resolve_metachar_named_file_is_literal_not_pattern() {
        // A file literally named with a glob metacharacter resolves as a
        // path — existence is probed before pattern classification.
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = "weird[1].rs";
        std::fs::write(tmp.path().join(name), "x").expect("write");
        let res = resolve_search_paths(&[PathBuf::from(name)], tmp.path());
        assert_eq!(res.forward, vec![PathBuf::from(name)]);
        assert!(res.missing.is_empty());
    }

    #[test]
    fn resolve_missing_glob_is_forwarded_as_pattern() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let res = resolve_search_paths(&[PathBuf::from("src/**/none.rs")], tmp.path());
        // Missing + glob metacharacter → forwarded for the daemon to expand.
        assert_eq!(res.forward, vec![PathBuf::from("src/**/none.rs")]);
        assert!(res.missing.is_empty());
    }

    #[test]
    fn resolve_missing_plain_path_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let res = resolve_search_paths(&[PathBuf::from("src/not/real.rs")], tmp.path());
        assert!(res.forward.is_empty());
        assert_eq!(res.missing, vec!["src/not/real.rs".to_string()]);
    }

    #[test]
    fn resolve_mixed_arguments_classified_independently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("real.rs"), "x").expect("write");
        let args = [
            PathBuf::from("real.rs"),
            PathBuf::from("*.toml"),
            PathBuf::from("gone.rs"),
        ];
        let res = resolve_search_paths(&args, tmp.path());
        assert_eq!(
            res.forward,
            vec![PathBuf::from("real.rs"), PathBuf::from("*.toml")]
        );
        assert_eq!(res.missing, vec!["gone.rs".to_string()]);
    }

    // ── render_search_outcome tests ──────────────────────────────────

    /// A test glob `SearchKind` with the two teaching vecs empty (the common
    /// shape for the render tests, which cover zero-match and results, not the
    /// dir-hint/metachar-note moments).
    fn glob_kind(no_match_patterns: Vec<String>) -> SearchKind {
        SearchKind::Glob {
            no_match_patterns,
            dir_hints: vec![],
            metachar_names: vec![],
        }
    }

    /// Runs `render_search_outcome` over separate stdout/stderr buffers and
    /// returns `(stdout, stderr)`. The VERBS streams ruling splits results
    /// (stdout) from teaching (stderr), so the tests assert per stream.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "test helper builds owned args inline at the call site"
    )]
    fn render(
        paths: SearchPaths,
        output: &str,
        queried: bool,
        kind: SearchKind,
    ) -> (String, String) {
        let mut out = cli::Output::buffer(80);
        let mut err = cli::Output::buffer(80);
        render_search_outcome(
            &mut out,
            &mut err,
            Path::new("/tmp/work"),
            &paths,
            output,
            queried,
            &kind,
        );
        (out.into_string(), err.into_string())
    }

    #[test]
    fn render_results_pass_through_verbatim() {
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src")],
            missing: vec![],
        };
        let (out, err) = render(
            paths,
            "cwd: ~/work\nsrc/\n\tmain.rs",
            true,
            glob_kind(vec![]),
        );
        assert!(out.contains("main.rs"), "results on stdout: {out}");
        assert!(
            !err.contains("no matches for pattern"),
            "no zero-match teaching: {err}"
        );
    }

    #[test]
    fn render_glob_zero_match_is_loud() {
        // A single pattern that matched nothing: stdout empty (exit 0 shape);
        // stderr carries the cwd anchor + the loud per-pattern report.
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src/**/none.rs")],
            missing: vec![],
        };
        let (out, err) = render(
            paths,
            "",
            true,
            glob_kind(vec!["src/**/none.rs".to_string()]),
        );
        assert!(out.is_empty(), "zero-match stdout is empty: {out:?}");
        assert!(err.contains("cwd:"), "cwd anchor on stderr: {err}");
        assert!(
            err.contains(
                "no matches for pattern: src/**/none.rs (relative patterns anchor at cwd)"
            ),
            "loud zero-match on stderr: {err}"
        );
    }

    #[test]
    fn render_glob_zero_match_loud_even_when_sibling_renders() {
        // The gap misc 118 closes: a pattern matching nothing is reported even
        // when another argument produced output (body non-empty). Results ride
        // stdout, the zero-match teaching rides stderr.
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src/**/none.rs"), PathBuf::from("src")],
            missing: vec![],
        };
        let (out, err) = render(
            paths,
            "cwd: ~/work\nsrc/\n\tmain.rs",
            true,
            glob_kind(vec!["src/**/none.rs".to_string()]),
        );
        assert!(out.contains("main.rs"), "sibling renders on stdout: {out}");
        assert!(
            err.contains(
                "no matches for pattern: src/**/none.rs (relative patterns anchor at cwd)"
            ),
            "zero-match pattern is loud on stderr alongside a rendered sibling: {err}"
        );
    }

    #[test]
    fn render_glob_teaching_moments_ride_stderr() {
        // Moments 3 (metachar-bearing matched name) and 4 (pattern resolved a
        // directory) ride stderr; the listing itself stays on stdout, byte-exact.
        let paths = SearchPaths {
            forward: vec![PathBuf::from("*")],
            missing: vec![],
        };
        let kind = SearchKind::Glob {
            no_match_patterns: vec![],
            dir_hints: vec!["src".to_string()],
            metachar_names: vec!["*.md".to_string()],
        };
        let (out, err) = render(paths, "/abs/src/main.rs  (10 lines)", true, kind);
        assert_eq!(
            out, "/abs/src/main.rs  (10 lines)\n",
            "the listing is unchanged on stdout"
        );
        assert!(
            err.contains("for its listing: `catenary glob 'src/*'`"),
            "moment 4 dir hint on stderr: {err}"
        );
        assert!(
            err.contains("`*.md`") && err.contains("catenary glob '\\*.md'"),
            "moment 3 escaped-spelling note on stderr: {err}"
        );
    }

    #[test]
    fn render_missing_plain_path_is_loud() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec!["src/not/real.rs".to_string()],
        };
        let (out, err) = render(paths, "", false, glob_kind(vec![]));
        assert!(out.is_empty(), "no results on stdout: {out:?}");
        assert!(err.contains("cwd:"), "cwd anchor on stderr: {err}");
        assert!(
            err.contains("path does not exist: src/not/real.rs"),
            "missing path on stderr: {err}"
        );
    }

    #[test]
    fn render_cwd_printed_on_empty() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec![],
        };
        let (out, err) = render(paths, "", true, glob_kind(vec![]));
        assert!(out.is_empty(), "empty result: empty stdout: {out:?}");
        assert!(
            err.contains("cwd:"),
            "empty result still anchors cwd on stderr: {err}"
        );
    }

    #[test]
    fn render_grep_empty_echoes_pattern_and_scope() {
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src")],
            missing: vec![],
        };
        let (out, err) = render(
            paths,
            "",
            true,
            SearchKind::Grep {
                pattern: "needle".to_string(),
                bre_alternation: false,
            },
        );
        assert!(out.is_empty(), "empty result: empty stdout: {out:?}");
        assert!(
            err.contains("no matches for: needle"),
            "zero echo on stderr: {err}"
        );
        assert!(err.contains("searched: src"), "scope on stderr: {err}");
    }

    #[test]
    fn render_grep_bre_alternation_hint() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec![],
        };
        let (_out, err) = render(
            paths,
            "",
            true,
            SearchKind::Grep {
                pattern: "foo\\|bar".to_string(),
                bre_alternation: true,
            },
        );
        assert!(err.contains("alternation"), "BRE hint on stderr: {err}");
    }

    // ── misc 172: cwd-anchored results disclose their scope ─────────

    #[test]
    fn render_grep_results_relative_scope_prints_cwd_anchor() {
        // The misc-172 sighting shape: a relative glob resolved against the
        // shell cwd. Hits render cwd-relative, so the anchor line is the only
        // way an agent can see WHICH tree the matches came from — it stays on
        // stdout (with the results) so `2>/dev/null` cannot lose the disclosure.
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src/**/*.rs")],
            missing: vec![],
        };
        let (out, _err) = render(
            paths,
            "src/paths.rs:181:    dirs::config_dir()",
            true,
            SearchKind::Grep {
                pattern: "dirs::config_dir".to_string(),
                bre_alternation: false,
            },
        );
        assert!(
            out.starts_with("cwd: /tmp/work\n"),
            "relative-scope results must open with the cwd anchor on stdout: {out}"
        );
        assert!(out.contains("dirs::config_dir()"), "{out}");
    }

    #[test]
    fn render_grep_results_pathless_scope_prints_cwd_anchor() {
        // A pathless grep binds to the cwd (bug 31) — same disclosure duty.
        let paths = SearchPaths {
            forward: vec![],
            missing: vec![],
        };
        let (out, _err) = render(
            paths,
            "src/main.rs:1:fn main() {}",
            true,
            SearchKind::Grep {
                pattern: "fn main".to_string(),
                bre_alternation: false,
            },
        );
        assert!(
            out.starts_with("cwd: /tmp/work\n"),
            "pathless results must open with the cwd anchor on stdout: {out}"
        );
    }

    #[test]
    fn render_grep_results_absolute_scope_stays_byte_identical() {
        // Absolute-only scopes derive nothing from the cwd: no anchor, and the
        // body passes through byte-identically (no churn for the recommended
        // absolute-path workflow).
        let paths = SearchPaths {
            forward: vec![PathBuf::from("/abs/tree/src")],
            missing: vec![],
        };
        let (out, _err) = render(
            paths,
            "probe.rs:3:    let marker = 1;",
            true,
            SearchKind::Grep {
                pattern: "marker".to_string(),
                bre_alternation: false,
            },
        );
        assert_eq!(
            out, "probe.rs:3:    let marker = 1;\n",
            "absolute-scope results must not grow an anchor line"
        );
    }

    #[test]
    fn render_glob_results_never_grow_a_cwd_anchor() {
        // Glob listings render absolute paths — the scope is already
        // disclosed per line, so the anchor stays grep-only.
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src/**/*.rs")],
            missing: vec![],
        };
        let (out, _err) = render(
            paths,
            "/abs/tree/src/main.rs  (10 lines)",
            true,
            glob_kind(vec![]),
        );
        assert_eq!(
            out, "/abs/tree/src/main.rs  (10 lines)\n",
            "glob results must pass through unchanged on stdout"
        );
    }

    #[test]
    fn render_not_queried_skips_zero_result_line() {
        // All arguments missing: no query ran, so no "no matches for" —
        // the path-does-not-exist lines carry the explanation, on stderr.
        let paths = SearchPaths {
            forward: vec![],
            missing: vec!["gone.rs".to_string()],
        };
        let (_out, err) = render(
            paths,
            "",
            false,
            SearchKind::Grep {
                pattern: "needle".to_string(),
                bre_alternation: false,
            },
        );
        assert!(!err.contains("no matches for"), "no zero echo: {err}");
        assert!(
            err.contains("path does not exist: gone.rs"),
            "missing on stderr: {err}"
        );
    }

    // ── teaching moment 2 disclosure + moment 3 escaping ────────────

    #[test]
    fn glob_zero_match_disclosure_reports_gitignored_target() {
        // A raw-string stat that resolves (the file exists on disk) but the
        // pattern matched nothing → the gitignored disclosure names the flag.
        let tmp = tempfile::tempdir().expect("tempdir");
        let secret = tmp.path().join("secret.env");
        std::fs::write(&secret, "K=V\n").expect("write");
        let line = glob_zero_match_disclosure(&secret.to_string_lossy(), tmp.path())
            .expect("existing target discloses");
        assert!(line.contains("exists but is gitignored"), "{line}");
        assert!(line.contains("--include-gitignored"), "{line}");
    }

    #[test]
    fn glob_zero_match_disclosure_reports_hidden_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let hidden = tmp.path().join(".env");
        std::fs::write(&hidden, "K=V\n").expect("write");
        let line = glob_zero_match_disclosure(&hidden.to_string_lossy(), tmp.path())
            .expect("existing hidden target discloses");
        assert!(line.contains("exists but is hidden"), "{line}");
        assert!(line.contains("--include-hidden"), "{line}");
    }

    #[test]
    fn glob_zero_match_disclosure_silent_for_a_genuine_absent() {
        // A raw string that resolves to nothing on disk (a metachar-bearing
        // pattern, or a real absent) yields no disclosure — the optional
        // ignore-off recount is not a commitment.
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            glob_zero_match_disclosure("*.nope", tmp.path()).is_none(),
            "a metachar pattern does not raw-stat to a path"
        );
        assert!(
            glob_zero_match_disclosure("truly_absent", tmp.path()).is_none(),
            "a genuine absent discloses nothing"
        );
    }

    #[test]
    fn escape_glob_metachars_backslash_escapes_each() {
        assert_eq!(escape_glob_metachars("*.md"), "\\*.md");
        assert_eq!(escape_glob_metachars("a?b"), "a\\?b");
        assert_eq!(escape_glob_metachars("[x].rs"), "\\[x\\].rs");
        assert_eq!(escape_glob_metachars("{a,b}"), "\\{a,b\\}");
        assert_eq!(escape_glob_metachars("plain.rs"), "plain.rs");
    }

    // ── --count rendering tests ────────────────────────────────────

    #[test]
    fn grep_count_matches_in_files() {
        let mut out = cli::Output::buffer(80);
        render_grep_count(&mut out, 12, 3, &catenary_mcp::bridge::GrepSkips::default());
        assert_eq!(out.into_string(), "12 matches in 3 files\n");
    }

    #[test]
    fn grep_count_zero_is_well_formed() {
        let mut out = cli::Output::buffer(80);
        render_grep_count(&mut out, 0, 0, &catenary_mcp::bridge::GrepSkips::default());
        assert_eq!(out.into_string(), "0 matches in 0 files\n");
    }

    #[test]
    fn grep_count_reports_skip_without_conflating_no_match() {
        // A named file skipped (binary content) is a skip, not a no-match: the
        // `--count` line reports it in a suffix, never as `0 … 0` silence
        // (misc 135, bug 62).
        let skipped = catenary_mcp::bridge::GrepSkips {
            named: vec![("blob.bin".to_string(), "binary".to_string())],
            walked: vec![],
        };
        let mut out = cli::Output::buffer(80);
        render_grep_count(&mut out, 0, 0, &skipped);
        assert_eq!(
            out.into_string(),
            "0 matches in 0 files (1 skipped: binary)\n"
        );
    }

    #[test]
    fn grep_count_skip_suffix_aggregates_binary_reason() {
        // Content classification leaves a single skip reason (misc 140): named
        // and walked binary skips fold into one honest `binary` tally.
        let skipped = catenary_mcp::bridge::GrepSkips {
            named: vec![("blob.bin".to_string(), "binary".to_string())],
            walked: vec![("binary".to_string(), 2)],
        };
        let mut out = cli::Output::buffer(80);
        render_grep_count(&mut out, 5, 1, &skipped);
        assert_eq!(
            out.into_string(),
            "5 matches in 1 files (3 skipped: binary)\n"
        );
    }

    #[test]
    fn grep_skips_render_named_and_walked_lines() {
        // A named file gets a per-file line; walked files aggregate by reason.
        let skipped = catenary_mcp::bridge::GrepSkips {
            named: vec![("blob.bin".to_string(), "binary".to_string())],
            walked: vec![("binary".to_string(), 3)],
        };
        let mut out = cli::Output::buffer(80);
        render_grep_skips(&mut out, &skipped);
        assert_eq!(
            out.into_string(),
            "skipped (binary): blob.bin\n3 files skipped (binary)\n"
        );
    }

    #[test]
    fn grep_skips_empty_renders_nothing() {
        // Nothing skipped → no lines, so a normal result is byte-identical.
        let mut out = cli::Output::buffer(80);
        render_grep_skips(&mut out, &catenary_mcp::bridge::GrepSkips::default());
        assert_eq!(out.into_string(), "");
    }

    #[test]
    fn glob_count_paths() {
        let mut out = cli::Output::buffer(80);
        render_glob_count(&mut out, 7);
        assert_eq!(out.into_string(), "7 paths\n");
    }

    // ── diagnostics exit contract + receipt (ws37 ticket 01) ────────
    //
    // The dispatcher maps `Ok(())` → exit `0` (the run completed, clean OR
    // dirty) and `Err` → exit `2` (a fault). It emits `1` for nothing, so these
    // tests pin the CLI boundary: clean/dirty/empty all return `Ok(())`, and
    // only a malformed response is an `Err`.

    #[cfg(unix)]
    #[test]
    fn diagnostics_clean_receipt_prints_clean_and_exits_0() {
        // Clean is explicit, never silence (retiring misc 111 / decision 022):
        // the daemon renders a `[clean]` receipt line per covered file, and the
        // CLI prints it. `Ok(())` maps to exit 0.
        let mut out = cli::Output::buffer(80);
        emit_diagnostics_response(
            &mut out,
            r#"{"output":"/root/file.rs [clean]","covered":1}"#,
        )
        .expect("clean receipt returns Ok → exit 0");
        assert_eq!(
            out.into_string().trim(),
            "/root/file.rs [clean]",
            "a clean covered run prints the `[clean]` receipt, not silence",
        );
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_zero_files_prints_no_edited_files_sentinel() {
        // The genuinely-empty set (covered == 0, empty receipt) prints
        // `[no edited files]` so a drained/empty set is observable rather than a
        // silent exit. `Ok(())` maps to exit 0.
        let mut out = cli::Output::buffer(80);
        emit_diagnostics_response(&mut out, r#"{"output":"","covered":0}"#)
            .expect("empty set returns Ok → exit 0");
        assert_eq!(
            out.into_string().trim(),
            NO_EDITED_FILES_SENTINEL,
            "the empty case must print [no edited files]",
        );
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_zero_files_sentinel_when_covered_absent() {
        // A pre-fix daemon omits `covered`; it defaults to 0, so an empty
        // receipt collapses to the [no edited files] sentinel.
        let mut out = cli::Output::buffer(80);
        emit_diagnostics_response(&mut out, r#"{"output":""}"#)
            .expect("empty receipt returns Ok → exit 0");
        assert_eq!(out.into_string().trim(), NO_EDITED_FILES_SENTINEL);
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_dirty_receipt_prints_and_exits_0() {
        // A dirty run's diagnostics print beneath the file, and the CLI still
        // returns `Ok(())` → exit 0 (never 1): a dirty result is a valid result,
        // not a failed call. This is the whole point of the amended contract.
        let mut out = cli::Output::buffer(80);
        emit_diagnostics_response(&mut out, r#"{"output":":1:1 [error] e: boom"}"#)
            .expect("dirty receipt returns Ok → exit 0, never 1");
        assert!(out.into_string().contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_warnings_only_prints_and_exits_0() {
        // A warnings-only run: the daemon labels it clean, but the warnings
        // still render in the receipt and the CLI returns `Ok(())` → exit 0.
        let mut out = cli::Output::buffer(80);
        emit_diagnostics_response(&mut out, r#"{"output":":2:1 [warning] w: meh"}"#)
            .expect("receipt returns Ok → exit 0");
        assert!(out.into_string().contains("meh"));
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_malformed_response_is_fault() {
        // A malformed response is a fault (mapped to exit 2 by the dispatcher),
        // not silently treated as a completed run.
        let mut out = cli::Output::buffer(80);
        assert!(emit_diagnostics_response(&mut out, "not json").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_error_envelope_is_fault_with_teaching_text() {
        // Bug 100: the daemon answers a bare run with no staged handoff with a
        // fault envelope. The CLI surfaces the teaching message as the error
        // (dispatcher → stderr + exit 2) and prints no receipt — the run never
        // happened, so there is nothing trustworthy to put on stdout.
        let mut out = cli::Output::buffer(80);
        let err = emit_diagnostics_response(
            &mut out,
            r#"{"status":"error","error":"no diagnostics run staged — bare `catenary diagnostics` pays the edit gate inside a hooked session","covered":0}"#,
        )
        .expect_err("a fault envelope maps to Err → exit 2");
        assert!(
            err.to_string().contains("hooked session"),
            "the fault message reaches the agent verbatim, got: {err:#}",
        );
        assert!(
            out.into_string().trim().is_empty(),
            "a faulted run prints no receipt (and no [no edited files] sentinel)",
        );
    }

    // ── grep zero-byte-pipe foot-gun (misc 174) ───────────────────

    #[cfg(unix)]
    #[test]
    fn grep_stdin_zero_byte_stream_is_the_fallback_gate() {
        // The dispatch peels off the empty stream by its emptiness — a zero-byte
        // pipe buffers to nothing, so `run_grep` falls through to the cwd
        // filesystem search instead of silently greping an empty stream (misc
        // 174). Genuine (non-empty) stream input stays in stdin mode.
        let empty: &[u8] = b"";
        assert!(
            empty.is_empty(),
            "a zero-byte pipe buffers empty → filesystem fallback",
        );
        let genuine: &[u8] = b"hello\n";
        assert!(
            !genuine.is_empty(),
            "any bytes → genuine stream mode, unchanged",
        );
    }

    #[cfg(unix)]
    #[test]
    fn grep_stdin_over_buffered_bytes_matches_lines() {
        // Genuine stream mode survives the buffer refactor: a non-empty byte
        // buffer produces the same plain-ripgrep line output as before, with no
        // enrichment. This is the path a real (non-empty) pipe reaches.
        let flags = catenary_mcp::bridge::GrepFlags::default();
        let mut out = cli::Output::buffer(80);
        run_grep_stdin(&mut out, b"alpha\nbeta\ngamma\n", "beta", &flags, false)
            .expect("stream search succeeds");
        let rendered = out.into_string();
        assert!(
            rendered.contains("beta"),
            "matched line renders: {rendered}"
        );
        assert!(
            !rendered.contains("alpha") && !rendered.contains("gamma"),
            "non-matching lines are excluded: {rendered}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn grep_stdin_over_buffered_bytes_counts_and_lists() {
        let flags = catenary_mcp::bridge::GrepFlags::default();

        // --count over the buffer tallies matching lines.
        let mut out = cli::Output::buffer(80);
        run_grep_stdin(&mut out, b"a\nba\nc\na\n", "a", &flags, true)
            .expect("count search succeeds");
        assert!(out.into_string().contains("3 matches"));

        // -l over a buffer that matched prints the nameless-stream marker.
        let list_flags = catenary_mcp::bridge::GrepFlags {
            files_with_matches: true,
            ..Default::default()
        };
        let mut out = cli::Output::buffer(80);
        run_grep_stdin(&mut out, b"needle\n", "needle", &list_flags, false)
            .expect("files-with-matches search succeeds");
        assert!(out.into_string().contains("(standard input)"));
    }
}
