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
    Primer,

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

        /// Exclude matches by glob pattern (e.g., tests/**).
        #[arg(long = "exclude-pattern")]
        exclude: Option<String>,

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
        /// File or directory path(s), or quoted glob pattern(s).
        ///
        /// Multiple values are unioned — `src/tui/*` expands to individual
        /// files and all are browsed. A path may also be a glob pattern: quote
        /// it (`catenary glob 'src/**/*.rs'`) so Catenary expands it
        /// gitignore-aware rather than the shell. Patterns may be absolute or
        /// cwd-relative, and the anchor belongs *in the pattern* —
        /// `catenary glob '/abs/dir/**/*.md'`; there is no separate directory
        /// argument.
        #[arg(name = "PATH", required = true)]
        paths: Vec<String>,

        /// Exclude matches by glob pattern (e.g., tests/**).
        #[arg(long = "exclude-pattern")]
        exclude: Option<String>,

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
    /// Bare — runs the LSP diagnostics pipeline over every file edited since
    /// the last run and prints a per-file receipt: each diagnosed file is
    /// listed, with its errors and warnings beneath it or `[clean]` beside it
    /// when the file is clean, then clears the set (`[no edited files]` for an
    /// empty set). With paths — diagnoses exactly those files (on-demand lint)
    /// and pays their editing debt, dropping them from the gate; the gate stays
    /// armed while any edited file remains unpaid. Paying is *diagnosing*, not
    /// fixing — a file's debt is cleared by looking at it, clean or dirty.
    /// Exits `0` whenever the run completed — clean *or* dirty — and `2` only
    /// on a genuine fault (no daemon, IPC failure); it never exits `1`, so a
    /// dirty result is not misread as a failed call. Editing begins implicitly
    /// on the first edit — there is no separate start step. Invoke via the
    /// host's shell tool.
    Diagnostics {
        /// File or directory path(s) to diagnose. Omit to report the whole
        /// edited set.
        ///
        /// Relative paths resolve against the current working directory. A
        /// named path is diagnosed and its editing debt paid — dropped from the
        /// gate — whether or not it was edited; a path outside the edited set is
        /// simply linted and pays nothing.
        #[arg(name = "PATH")]
        paths: Vec<String>,
    },

    /// Editing mode (start). Optional — editing starts implicitly on the
    /// first edit; `catenary diagnostics` ends it and prints diagnostics.
    Editing {
        #[command(subcommand)]
        command: EditingCommand,
    },

    /// Workspace root management (add, rm, ls).
    Roots {
        #[command(subcommand)]
        command: RootsCommand,
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

/// Workspace root management subcommands.
#[derive(Subcommand, Debug)]
enum RootsCommand {
    /// Add a workspace root.
    Add {
        /// Path to add as a workspace root.
        path: PathBuf,
    },
    /// Remove a workspace root.
    Rm {
        /// Path to remove from workspace roots.
        path: PathBuf,
    },
    /// List all tracked workspace roots with their source.
    Ls,
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
    /// cache dir, printing its absolute path (misc 144).
    #[command(name = "worktree-create")]
    WorktreeCreate {
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
            let subcommand = ["grep", "glob", "diagnostics", "editing", "roots"]
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
        Some(Command::Primer) => {
            let mut out = cli::Output::stdout(false);
            run_primer(&mut out);
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
            let paths = to_literal_paths(paths);
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_glob(
                &mut out,
                paths,
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
        Some(Command::Roots { command }) => {
            let mut out = cli::Output::stdout(false);
            match command {
                RootsCommand::Add { path } => {
                    build_runtime()?.block_on(run_root_command(&mut out, path, "tool/roots-add"))
                }
                RootsCommand::Rm { path } => {
                    build_runtime()?.block_on(run_root_command(&mut out, path, "tool/roots-rm"))
                }
                RootsCommand::Ls => {
                    build_runtime()?.block_on(cli::commands::run_ls_roots(&mut out))
                }
            }
        }
        #[cfg(not(unix))]
        Some(Command::Roots { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
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
    /// that expanded to nothing is reported loudly per-argument.
    Glob {
        /// Glob-pattern arguments (original spelling, daemon-reported) that
        /// expanded to zero matches. Rendered as `no matches for pattern:
        /// <pattern> (relative patterns anchor at cwd)` regardless of whether
        /// other arguments produced results (misc 118).
        no_match_patterns: Vec<String>,
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
/// contract (`bugs/13`).
///
/// - **Results** — `daemon_output` non-empty → printed verbatim; the daemon
///   prepends its own cwd/root anchor.
/// - **Empty** — `queried` ran but nothing came back → the cwd anchor is
///   always printed (the only signal distinguishing "ran here, found nothing"
///   from "did not run"). For grep, the zero-result echo follows: `no matches
///   for: <pattern>` plus the `searched:` scope so the agent can check both its
///   escaping and its paths. Glob's zero-result reporting is per-argument (see
///   below), so it prints nothing extra here.
/// - **No-match patterns** (glob) — a loud `no matches for pattern: <pattern>
///   (relative patterns anchor at cwd)` is appended for each glob-pattern
///   argument that expanded to nothing, regardless of whether *other* arguments
///   produced results (misc 118). This mirrors the per-argument `path does not
///   exist` for metachar-free absents: a pattern that matched nothing is never
///   silent.
/// - **Missing** — a loud `path does not exist: <path>` is appended for each
///   non-existent plain-path argument, regardless of the above.
fn render_search_outcome(
    out: &mut cli::Output,
    cwd: &Path,
    paths: &SearchPaths,
    daemon_output: &str,
    queried: bool,
    kind: &SearchKind,
) {
    let body = daemon_output.trim_end_matches('\n');
    if body.is_empty() {
        let _ = out.writeln(format_args!("cwd: {}", compress_home(cwd)));
        if queried
            && let SearchKind::Grep {
                pattern,
                bre_alternation,
            } = kind
        {
            let _ = out.writeln(format_args!("no matches for: {pattern}"));
            if !paths.forward.is_empty() {
                let _ = out.writeln(format_args!("searched: {}", forward_display(paths)));
            }
            if *bre_alternation {
                let _ = out.writeln(format_args!(
                    "hint: use `|` for alternation, not `\\|` (which matches a literal pipe)"
                ));
            }
        }
    } else {
        let _ = out.writeln(format_args!("{body}"));
    }
    // Per-argument glob no-match report — fired whether or not the body was
    // empty, so a pattern that matched nothing is loud even when a sibling
    // argument rendered.
    if let SearchKind::Glob { no_match_patterns } = kind {
        for pattern in no_match_patterns {
            let _ = out.writeln(format_args!(
                "no matches for pattern: {pattern} (relative patterns anchor at cwd)"
            ));
        }
    }
    for path in &paths.missing {
        let _ = out.writeln(format_args!("path does not exist: {path}"));
    }
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
fn run_primer(out: &mut cli::Output) {
    let _ = out.writeln(format_args!("{}", cli::teaching::emitted_payload()));
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

        let threshold: catenary_mcp::logging::Severity = config
            .notifications
            .as_ref()
            .map_or_else(catenary_mcp::config::SeverityConfig::default, |n| {
                n.threshold
            })
            .into();
        let notification_router = Arc::new(
            catenary_mcp::logging::notification_router::NotificationRouter::new(threshold),
        );

        // Daemon-owned live-state snapshot. Mirrors server lifecycle/progress
        // and the alert ring to runtime_dir()/catenary/state.json — the
        // out-of-process surface that replaces the language_servers table.
        let snapshot = catenary_mcp::state_snapshot::SnapshotWriter::new(
            rt.handle(),
            &catenary_mcp::paths::runtime_dir().join("catenary"),
            catenary_mcp::state_snapshot::DaemonInfo {
                instance_id: instance_id.to_string(),
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                started_at: catenary_mcp::state_snapshot::now_iso(),
            },
        );

        let session = Arc::new(catenary_mcp::bridge::session::Session::new(
            config,
            roots,
            logging.clone(),
            instance_id.clone(),
            rt.handle().clone(),
            notification_router,
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

    // Worktree-deletion reaper (workstream 30, ticket 05): the PROMPT teardown
    // trigger for `worktree:*` roots. `git worktree remove` fires no
    // `WorktreeRemove` hook, so without this the hourly GC above is the only live
    // reaper (≤1 h leak). This reaper drains the bounded directory-deletion watch
    // (registered at `SubagentStart` mount) and reaps the root within the
    // FS-event latency. Spawned AFTER `with_session` so the watcher + channel
    // exist; a no-op for a session-less manager or if the OS watcher was
    // unavailable. The GC stays the crash-safe backstop (the watch dies with the
    // daemon).
    manager.spawn_worktree_watch_reaper(rt.handle());

    // Ephemeral-root idle-expiry reaper (ephemeral-roots ticket 02): tears down
    // activity-mounted `ephemeral:*` roots after they go idle past the timeout.
    // Spawned AFTER `with_session` so the tracker + ephemeral clock exist; a
    // no-op for a session-less manager. These roots have no MCP heartbeat to pin
    // on, so the idle detector is their only release signal (DESIGN.md).
    manager.spawn_ephemeral_root_reaper(rt.handle());

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
/// to the stop; the post-stop reconnect warning is unchanged.
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

    // The shutdown ack reports how many bridges were connected. Each was
    // proxying stdin↔daemon-socket and exits when the socket closes, so the
    // host marks that MCP server failed. A plain host restart does NOT
    // relaunch it — only a `/mcp` reconnect re-runs the bridge and respawns
    // the daemon. Warn so the loss isn't silent.
    let connections = serde_json::from_str::<serde_json::Value>(line.trim())
        .ok()
        .and_then(|v| v.get("connections").and_then(serde_json::Value::as_u64))
        .unwrap_or(0);
    if connections > 0 {
        let plural = if connections == 1 { "" } else { "s" };
        let _ = out.writeln(format_args!(
            "warning: {connections} connected session{plural} will lose Catenary tooling — \
             each needs a `/mcp` reconnect (a host restart alone won't respawn the daemon)",
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
    exclude: Option<String>,
    count: bool,
    include_gitignored: bool,
    include_hidden: bool,
    flags: catenary_mcp::bridge::GrepFlags,
) -> Result<()> {
    use catenary_mcp::router::{GrepRequest, METHOD_GREP};

    // stdin mode: no paths + a readable piped/redirected stream. A plain
    // ripgrep pass over the stream, same flags, no enrichment, no daemon.
    if paths.is_empty() && is_readable_stdin() {
        return run_grep_stdin(out, &pattern, &flags, count);
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
        search_ipc(METHOD_GREP, &request).await?
    } else {
        SearchResponse::default()
    };

    if count {
        render_grep_count(
            out,
            response.matches.unwrap_or(0),
            response.files.unwrap_or(0),
            &response.skipped,
        );
    } else {
        render_search_outcome(out, &cwd, &resolved, &response.output, queried, &kind);
        // Skip lines follow the results/echo (and any missing-path lines), so a
        // named path skipped instead of searched never silently vanishes (misc
        // 135, bug 62). Nothing prints when nothing was skipped.
        render_grep_skips(out, &response.skipped);
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

/// stdin mode: a plain ripgrep pass over the piped stream.
///
/// No daemon, no enrichment, no `#scope` — a stream has no file or LSP context.
/// Carries the same flags as file mode (`-i`/`-s`/`-w`/`-F`/`-v`, context,
/// `--count`, `-l`), differing only in enrichment. `-l` prints `(standard
/// input)` when the stream matched (the GNU `grep -l` convention for a nameless
/// stream); `--count` prints the matching-line tally.
///
/// # Errors
///
/// Returns an error if the pattern is invalid or the stream cannot be read.
#[cfg(unix)]
fn run_grep_stdin(
    out: &mut cli::Output,
    pattern: &str,
    flags: &catenary_mcp::bridge::GrepFlags,
    count: bool,
) -> Result<()> {
    use catenary_mcp::bridge::{StreamOutcome, grep_stream};

    let stdin = std::io::stdin();
    let outcome = grep_stream(stdin.lock(), pattern, flags, count)?;
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
    /// grep: files in the search scope skipped instead of searched (misc 135,
    /// bug 62). Empty for a normal all-searched query.
    #[serde(default)]
    skipped: catenary_mcp::bridge::GrepSkips,
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

/// Sends a `tool/grep` or `tool/glob` request to the daemon and returns the
/// parsed [`SearchResponse`].
///
/// Connects to the daemon IPC socket, serializes `request` with `method`
/// injected, and reads the response. The first response line decides the shape:
/// a chunked [`GrepFrame`] stream (misc 140 phase 2, tagged by the `"frame"`
/// key) is reassembled into a [`SearchResponse`]; a legacy single JSON envelope
/// (glob, or a daemon that predates framing) is parsed directly. An empty line
/// maps to a default [`SearchResponse`]. A non-zero exit is reserved for genuine
/// faults — no daemon, transport failure, or a malformed response — so soft
/// conditions never cancel a parallel tool batch.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn search_ipc<R: serde::Serialize + Sync>(
    method: &str,
    request: &R,
) -> Result<SearchResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();
    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;
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
            } => {
                response.matches = matches;
                response.files = files;
                response.skipped = skipped;
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
/// Connects to the daemon's IPC socket, sends a [`GlobRequest`], and
/// prints the rendered output to stdout. The daemon resolves relative
/// paths against `cwd` before dispatching to the glob pipeline.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn run_glob(
    out: &mut cli::Output,
    paths: Vec<PathBuf>,
    exclude: Option<String>,
    count: bool,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<()> {
    use catenary_mcp::router::{GlobRequest, METHOD_GLOB};

    let cwd = std::env::current_dir().context("cannot determine working directory")?;

    let resolved = resolve_search_paths(&paths, &cwd);
    // Glob always scopes to explicit paths (clap requires at least one); query
    // only when at least one argument resolved to a path or pattern.
    let queried = !resolved.forward.is_empty();

    let response = if queried {
        let request = GlobRequest {
            cwd: Some(cwd.clone()),
            paths: resolved.forward.clone(),
            exclude,
            count,
            include_gitignored,
            include_hidden,
        };
        search_ipc(METHOD_GLOB, &request).await?
    } else {
        SearchResponse::default()
    };

    if count {
        render_glob_count(out, response.paths.unwrap_or(0));
    } else {
        // `no_match_patterns` is daemon-reported (original argument spelling);
        // move it into the kind while still borrowing `output` (disjoint fields).
        let kind = SearchKind::Glob {
            no_match_patterns: response.no_match_patterns,
        };
        render_search_outcome(out, &cwd, &resolved, &response.output, queried, &kind);
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
}

/// Implements `catenary diagnostics [paths…]`: prints diagnostics for the
/// edited files (bare) or the named paths (scoped), and pays the corresponding
/// editing debt.
///
/// Connects to the daemon's IPC socket and sends `tool/editing-stop` (the
/// internal handoff method name is unchanged by the user-facing rename). The
/// `PreToolUse` hook has already prepared the handoff — this command retrieves
/// the diagnostics and prints the per-file receipt. When `paths` is non-empty,
/// they ride the request's `files` param: the daemon diagnoses exactly those and
/// flips their gate flags on delivery (scoped). Relative paths resolve against
/// the CLI's cwd before dispatch — the daemon runs under a different cwd —
/// matching how `grep`/`glob` forward paths. Success (clean *or* dirty) returns
/// `Ok(())`, which the dispatcher maps to exit `0`.
///
/// # Errors
///
/// Returns an error (mapped to fault exit `2`) if no daemon is running, the
/// IPC fails, the working directory can't be resolved, or the response is
/// malformed.
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
/// Returns an error if the response is not valid JSON — a malformed response is
/// a fault (mapped to exit `2`).
#[cfg(unix)]
fn emit_diagnostics_response(out: &mut cli::Output, response: &str) -> Result<()> {
    let parsed: DiagnosticsResponse = serde_json::from_str(response.trim())
        .context("invalid diagnostics response from daemon")?;

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

/// Sends an add-root or rm-root request to the running daemon.
///
/// Canonicalizes the path, connects to the daemon's IPC socket, and
/// prints the result. If no daemon is running, prints an error and
/// exits non-zero.
///
/// # Errors
///
/// Returns an error if the path cannot be canonicalized or the daemon
/// request fails.
#[cfg(unix)]
async fn run_root_command(out: &mut cli::Output, path: PathBuf, method: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve path: {}", path.display()))?;

    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({
        "method": method,
        "path": canonical.display().to_string(),
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

    match status {
        "ok" => {
            let verb = if method.contains("roots-add") {
                "added"
            } else {
                "removed"
            };
            let _ = out.writeln(format_args!("{verb} root: {}", canonical.display()));
        }
        "not_found" => {
            let msg = response
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("root not found in hook-managed roots");
            let _ = out.writeln(format_args!("{msg}"));
        }
        _ => {
            let msg = response
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unexpected response");
            anyhow::bail!("{msg}");
        }
    }

    Ok(())
}

/// Check installed hooks against embedded expected hooks at daemon startup.
///
/// Compares each host CLI's installed hooks with the compile-time expected
/// version. Emits `error!()` for any mismatch — the `LoggingServer`
/// routes this to the notification queue and the desktop notification
/// sink automatically.
///
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

    fn check_host(host: &str, installed_path: &std::path::Path, expected: &str) {
        match std::fs::read_to_string(installed_path) {
            Ok(installed) if normalize_json(&installed) == normalize_json(expected) => {}
            Ok(_) => {
                tracing::error!(
                    source = Source::HookDispatch.as_str(),
                    "Stale {host} hooks detected. Run: catenary install",
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
        check_host("Claude Code", &hooks_path, CLAUDE_HOOKS_EXPECTED);
    }

    // Antigravity CLI: hooks at ~/.antigravity/hooks.json
    let antigravity_hooks = home.join(".antigravity/hooks.json");
    check_host(
        "Antigravity CLI",
        &antigravity_hooks,
        ANTIGRAVITY_HOOKS_EXPECTED,
    );
}

/// Resolve the Claude Code hooks.json path from `installed_plugins.json`.
///
/// Returns `None` if Claude Code is not installed or the plugin entry
/// cannot be resolved.
#[cfg(unix)]
fn resolve_claude_hooks_path(home: &std::path::Path) -> Option<std::path::PathBuf> {
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
        assert!(exclude.is_none());
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
        assert_eq!(exclude.as_deref(), Some("tests/"));
        assert!(count);
        assert!(include_gitignored);
        assert!(include_hidden);
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
        assert!(exclude.is_none());
        assert!(!count);
        assert!(!include_gitignored);
        assert!(!include_hidden);
    }

    #[test]
    fn test_cli_glob_variadic() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "glob",
            "src/tui/stream.rs",
            "src/tui/mod.rs",
            "src/tui/render.rs",
        ]);
        let args = args.expect("glob with multiple paths should parse");
        let Some(Command::Glob { paths, .. }) = args.command else {
            unreachable!("expected Glob command");
        };
        assert_eq!(
            paths,
            vec!["src/tui/stream.rs", "src/tui/mod.rs", "src/tui/render.rs"]
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
        assert_eq!(exclude.as_deref(), Some("target/**"));
        assert!(count);
        assert!(include_gitignored);
        assert!(include_hidden);
    }

    #[test]
    fn test_cli_glob_page_flag_is_rejected() {
        use clap::Parser;
        // `--page` was retired with paging (pipeable-output ticket 03).
        let result = Args::try_parse_from(["catenary", "glob", "src/", "--page", "2"]);
        assert!(result.is_err(), "glob --page should no longer parse");
    }

    #[test]
    fn test_cli_glob_missing_path() {
        use clap::Parser;
        let result = Args::try_parse_from(["catenary", "glob"]);
        assert!(result.is_err(), "glob without path should fail");
    }

    // ── CLI roots subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_roots_add() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots", "add", "/tmp/project"]);
        let args = args.expect("roots add should parse");
        let Some(Command::Roots {
            command: RootsCommand::Add { path },
        }) = args.command
        else {
            unreachable!("expected Roots Add command");
        };
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[test]
    fn test_cli_roots_rm() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots", "rm", "/tmp/project"]);
        let args = args.expect("roots rm should parse");
        let Some(Command::Roots {
            command: RootsCommand::Rm { path },
        }) = args.command
        else {
            unreachable!("expected Roots Rm command");
        };
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[test]
    fn test_cli_roots_ls() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots", "ls"]);
        let args = args.expect("roots ls should parse");
        assert!(matches!(
            args.command,
            Some(Command::Roots {
                command: RootsCommand::Ls
            })
        ));
    }

    #[test]
    fn test_cli_primer() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "primer"]);
        let args = args.expect("primer should parse");
        assert!(matches!(args.command, Some(Command::Primer)));
    }

    #[test]
    fn primer_renders_the_teaching_payload() {
        // The primer prints the shared teaching payload: invariants, the flag
        // synopses, and the `--help` breadcrumbs. Capturing through
        // `Output::buffer` proves the handler writes via `Output` (not raw
        // `println!`).
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out);
        let text = out.into_string();

        // The invariants tier.
        assert!(
            text.contains("The edit→diagnostics loop"),
            "primer should emit the edit→diagnostics invariant"
        );
        // The flag-synopsis tier plus its point-of-use `--help` breadcrumbs.
        for needle in [
            "catenary grep",
            "catenary glob",
            "catenary diagnostics",
            "catenary roots",
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
        run_primer(&mut out);
        let printed = out.into_string();
        assert_eq!(
            printed.trim_end_matches('\n'),
            cli::teaching::emitted_payload()
        );
    }

    #[test]
    fn primer_has_no_pointers_or_retired_commands() {
        // Inlining is the point — no `catenary primer` / `catenary commands`
        // pointer — and the retired `editing` / `sed` subcommands must not
        // appear in agent-facing guidance.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out);
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
        // misc 118: `catenary glob --help` teaches that a PATH may be a quoted
        // glob pattern, absolute or cwd-relative, with the anchor in the pattern.
        use clap::CommandFactory;
        let app = Args::command();
        let mut glob = app
            .find_subcommand("glob")
            .expect("glob subcommand present")
            .clone();
        let help = glob.render_long_help().to_string();
        assert!(
            help.contains("quoted glob pattern"),
            "short teaser present: {help}"
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
    }

    #[test]
    fn primer_teaches_glob_pattern_form() {
        // The pattern teaching is carried in the payload's invariants tier.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out);
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

    #[allow(
        clippy::needless_pass_by_value,
        reason = "test helper builds owned args inline at the call site"
    )]
    fn render(paths: SearchPaths, output: &str, queried: bool, kind: SearchKind) -> String {
        let mut out = cli::Output::buffer(80);
        render_search_outcome(
            &mut out,
            Path::new("/tmp/work"),
            &paths,
            output,
            queried,
            &kind,
        );
        out.into_string()
    }

    #[test]
    fn render_results_pass_through_verbatim() {
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src")],
            missing: vec![],
        };
        let text = render(
            paths,
            "cwd: ~/work\nsrc/\n\tmain.rs",
            true,
            SearchKind::Glob {
                no_match_patterns: vec![],
            },
        );
        assert!(text.contains("main.rs"), "{text}");
        assert!(!text.contains("no matches for pattern"), "{text}");
    }

    #[test]
    fn render_glob_zero_match_is_loud() {
        // A single pattern that matched nothing: cwd anchor + the loud
        // per-pattern report (which supersedes the old `no files matched`).
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src/**/none.rs")],
            missing: vec![],
        };
        let text = render(
            paths,
            "",
            true,
            SearchKind::Glob {
                no_match_patterns: vec!["src/**/none.rs".to_string()],
            },
        );
        assert!(text.contains("cwd:"), "cwd anchor always printed: {text}");
        assert!(
            text.contains(
                "no matches for pattern: src/**/none.rs (relative patterns anchor at cwd)"
            ),
            "{text}"
        );
    }

    #[test]
    fn render_glob_zero_match_loud_even_when_sibling_renders() {
        // The gap misc 118 closes: a pattern matching nothing is reported even
        // when another argument produced output (body non-empty).
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src/**/none.rs"), PathBuf::from("src")],
            missing: vec![],
        };
        let text = render(
            paths,
            "cwd: ~/work\nsrc/\n\tmain.rs",
            true,
            SearchKind::Glob {
                no_match_patterns: vec!["src/**/none.rs".to_string()],
            },
        );
        assert!(text.contains("main.rs"), "sibling still renders: {text}");
        assert!(
            text.contains(
                "no matches for pattern: src/**/none.rs (relative patterns anchor at cwd)"
            ),
            "zero-match pattern is loud alongside a rendered sibling: {text}"
        );
    }

    #[test]
    fn render_missing_plain_path_is_loud() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec!["src/not/real.rs".to_string()],
        };
        let text = render(
            paths,
            "",
            false,
            SearchKind::Glob {
                no_match_patterns: vec![],
            },
        );
        assert!(text.contains("cwd:"), "{text}");
        assert!(
            text.contains("path does not exist: src/not/real.rs"),
            "{text}"
        );
    }

    #[test]
    fn render_cwd_printed_on_empty() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec![],
        };
        let text = render(
            paths,
            "",
            true,
            SearchKind::Glob {
                no_match_patterns: vec![],
            },
        );
        assert!(
            text.contains("cwd:"),
            "empty result still anchors cwd: {text}"
        );
    }

    #[test]
    fn render_grep_empty_echoes_pattern_and_scope() {
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src")],
            missing: vec![],
        };
        let text = render(
            paths,
            "",
            true,
            SearchKind::Grep {
                pattern: "needle".to_string(),
                bre_alternation: false,
            },
        );
        assert!(text.contains("no matches for: needle"), "{text}");
        assert!(text.contains("searched: src"), "{text}");
    }

    #[test]
    fn render_grep_bre_alternation_hint() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec![],
        };
        let text = render(
            paths,
            "",
            true,
            SearchKind::Grep {
                pattern: "foo\\|bar".to_string(),
                bre_alternation: true,
            },
        );
        assert!(text.contains("alternation"), "BRE hint shown: {text}");
    }

    #[test]
    fn render_not_queried_skips_zero_result_line() {
        // All arguments missing: no query ran, so no "no matches for" —
        // the path-does-not-exist lines carry the explanation.
        let paths = SearchPaths {
            forward: vec![],
            missing: vec!["gone.rs".to_string()],
        };
        let text = render(
            paths,
            "",
            false,
            SearchKind::Grep {
                pattern: "needle".to_string(),
                bre_alternation: false,
            },
        );
        assert!(!text.contains("no matches for"), "{text}");
        assert!(text.contains("path does not exist: gone.rs"), "{text}");
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
}
