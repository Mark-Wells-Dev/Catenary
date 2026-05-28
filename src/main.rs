// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary MCP server and CLI.
//!
//! This is the main entry point for the Catenary multiplexing bridge.
//! It can be run as an MCP server or as a CLI tool to list and monitor sessions.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::{Context, Result};
use clap::{FromArgMatches, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use catenary_mcp::cli::{self, HostFormat, QueryFormat};
use catenary_mcp::logging::LoggingServer;
use catenary_mcp::session;

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
    /// Searches from the current working directory. Results within tracked
    /// workspace roots include symbol context from LSP servers.
    Grep {
        /// Regex pattern (Rust/PCRE syntax, | for alternation).
        pattern: String,

        /// Scope the search (e.g., src/**/*.rs, **/*.{ts,js},
        /// /home/user/project/**/*.py).
        #[arg(name = "GLOB")]
        glob: Option<String>,

        /// Exclude matches (e.g., tests/**).
        #[arg(long)]
        exclude: Option<String>,

        /// Page number for paged results.
        #[arg(long, default_value = "1")]
        page: usize,

        /// Include files ignored by .gitignore.
        #[arg(long)]
        include_gitignored: bool,

        /// Include hidden files and directories.
        #[arg(long)]
        include_hidden: bool,
    },

    /// Browse the filesystem: file outlines, directory listings, glob patterns.
    ///
    /// Resolves against the current working directory. Results include symbol
    /// outlines when LSP data is available.
    Glob {
        /// File, directory, or glob (e.g., src/, **/*.{rs,toml},
        /// /home/user/project/src/).
        pattern: String,

        /// Exclude matches (e.g., tests/**).
        #[arg(long)]
        exclude: Option<String>,

        /// Page number for paged results.
        #[arg(long, default_value = "1")]
        page: usize,

        /// Include files ignored by .gitignore.
        #[arg(long)]
        include_gitignored: bool,

        /// Include hidden files and directories.
        #[arg(long)]
        include_hidden: bool,
    },

    /// Editing mode (start, stop).
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

    /// Check language server health. Tests all configured servers by default.
    /// Pass a server name for verbose single-server diagnostics.
    Doctor {
        /// Server name for verbose single-server mode (matches [server.*]
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
    Stop,

    /// Diagnostic and debugging tools (list, monitor, status, query, gc).
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
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
    /// Enter editing mode. Invoke via the host's shell tool.
    Start,
    /// Exit editing mode and print diagnostics. Invoke via the host's shell tool.
    Stop,
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

/// Diagnostic and debugging subcommands.
#[derive(Subcommand, Debug)]
enum DebugCommand {
    /// List active Catenary sessions.
    List,

    /// Monitor events from a session.
    Monitor {
        /// Session ID or row number (use 'catenary debug list' to see available sessions).
        id: String,

        /// Show raw JSON output.
        #[arg(long)]
        raw: bool,

        /// Disable colored output.
        #[arg(long)]
        nocolor: bool,

        /// Filter events by regex pattern.
        #[arg(long, short)]
        filter: Option<String>,
    },

    /// Show status of a session.
    Status {
        /// Session ID (use 'catenary debug list' to see available sessions).
        id: String,
    },

    /// Query events from the database.
    Query {
        /// Filter by session ID or prefix.
        #[arg(long)]
        session: Option<String>,

        /// Time filter (e.g., "1h", "today", "7d", "30m").
        #[arg(long)]
        since: Option<String>,

        /// Filter by event kind (e.g., `tool_call`, `diagnostics`).
        #[arg(long)]
        kind: Option<String>,

        /// Free-text search in event payload.
        #[arg(long)]
        search: Option<String>,

        /// Raw SQL query (power users).
        #[arg(long)]
        sql: Option<String>,

        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: QueryFormat,
    },

    /// Garbage-collect old session data.
    Gc {
        /// Delete events older than this duration (e.g., "7d", "30d").
        #[arg(long)]
        older_than: Option<String>,

        /// Delete all data for dead sessions.
        #[arg(long)]
        dead: bool,

        /// Delete all data for a specific session.
        #[arg(long)]
        session: Option<String>,
    },
}

/// Hook subcommands invoked by host CLI hooks.
#[derive(Subcommand, Debug)]
enum HookCommand {
    /// Pre-agent: signal turn start (`UserPromptSubmit` / `BeforeAgent`).
    #[command(name = "pre-agent")]
    PreAgent {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Pre-tool: editing state enforcement (`PreToolUse` / `BeforeTool`).
    #[command(name = "pre-tool")]
    PreTool {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Post-agent: force `done_editing` before agent finishes (`Stop` / `AfterAgent`).
    #[command(name = "post-agent")]
    PostAgent {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SessionStart`: clear stale editing state.
    #[command(name = "session-start")]
    SessionStart {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SessionEnd`: clean up session state (roots, editing).
    #[command(name = "session-end")]
    SessionEnd {
        /// Output format: "claude", "gemini", or "antigravity".
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
    /// Install the Catenary extension for Gemini CLI.
    Gemini {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,
    },
    /// Install the Catenary plugin for Antigravity CLI.
    Antigravity {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,
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
            // For agent-facing subcommands, append `-h` output so the
            // agent sees correct usage without a second round-trip.
            let raw = e.to_string();
            let subcommand = ["grep", "glob", "editing", "roots"]
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
            run_primer();
            Ok(())
        }
        #[cfg(unix)]
        Some(Command::Grep {
            pattern,
            glob,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        }) => build_runtime()?.block_on(run_grep(
            pattern,
            glob,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        )),
        #[cfg(not(unix))]
        Some(Command::Grep { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Glob {
            pattern,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        }) => build_runtime()?.block_on(run_glob(
            pattern,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        )),
        #[cfg(not(unix))]
        Some(Command::Glob { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Editing { command }) => match command {
            EditingCommand::Start => build_runtime()?.block_on(run_start_editing()),
            EditingCommand::Stop => build_runtime()?.block_on(run_done_editing()),
        },
        #[cfg(not(unix))]
        Some(Command::Editing { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Roots { command }) => match command {
            RootsCommand::Add { path } => {
                build_runtime()?.block_on(run_root_command(path, "tool/roots-add"))
            }
            RootsCommand::Rm { path } => {
                build_runtime()?.block_on(run_root_command(path, "tool/roots-rm"))
            }
            RootsCommand::Ls => {
                let mut out = cli::Output::stdout(false);
                build_runtime()?.block_on(cli::commands::run_ls_roots(&mut out))
            }
        },
        #[cfg(not(unix))]
        Some(Command::Roots { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        Some(Command::Config) => {
            let mut out = cli::Output::stdout(false);
            cli::config_template::print_template(&mut out);
            Ok(())
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
                    cli::install::run_install_gemini(&mut out, source.as_deref(), dry_run)
                }
                Some(InstallHost::Antigravity { source }) => {
                    cli::install::run_install_antigravity(&mut out, source.as_deref(), dry_run)
                }
            }
        }
        Some(Command::Update { check, force }) => {
            let mut out = cli::Output::stdout(false);
            cli::update::run_update(&mut out, check, force)
        }
        #[cfg(unix)]
        Some(Command::Stop) => build_runtime()?.block_on(run_stop()),
        #[cfg(not(unix))]
        Some(Command::Stop) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        Some(Command::Debug { command }) => match command {
            DebugCommand::List => {
                let mut out = cli::Output::stdout(false);
                cli::commands::run_list(&mut out)
            }
            DebugCommand::Monitor {
                id,
                raw,
                nocolor,
                filter,
            } => {
                let mut out = cli::Output::stdout(nocolor);
                cli::commands::run_monitor(&mut out, &id, raw, filter.as_deref())
            }
            DebugCommand::Status { id } => {
                let mut out = cli::Output::stdout(false);
                cli::commands::run_status(&mut out, &id)
            }
            DebugCommand::Query {
                session,
                since,
                kind,
                search,
                sql,
                format,
            } => {
                let conn = catenary_mcp::db::open_and_migrate()?;
                let mut out = cli::Output::stdout(false);
                cli::commands::run_query(
                    &mut out,
                    &conn,
                    session.as_deref(),
                    since.as_deref(),
                    kind.as_deref(),
                    search.as_deref(),
                    sql.as_deref(),
                    format,
                )
            }
            DebugCommand::Gc {
                older_than,
                dead,
                session,
            } => {
                let conn = catenary_mcp::db::open_and_migrate()?;
                let mut out = cli::Output::stdout(false);
                cli::commands::run_gc(
                    &mut out,
                    &conn,
                    older_than.as_deref(),
                    dead,
                    session.as_deref(),
                )
            }
        },
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
                HookCommand::PreAgent { format } => cli::hooks::run_pre_agent(format),
                HookCommand::PreTool { format } => cli::hooks::run_pre_tool(format),
                HookCommand::PostAgent { format } => cli::hooks::run_post_agent(format),
                HookCommand::SessionStart { format } => cli::hooks::run_session_start(format),
                HookCommand::SessionEnd { format } => cli::hooks::run_session_end(format),
            }
            Ok(())
        }
        #[cfg(unix)]
        Some(Command::Daemon) => run_daemon(),
        #[cfg(not(unix))]
        Some(Command::Daemon) => Err(anyhow::anyhow!("daemon mode requires Unix")),
    }
}

/// Print an overview of Catenary's workflow and commands.
///
/// Extracts the agent-facing subcommands from the derive-generated CLI
/// definition. When help text changes (via doc comments on the derive
/// structs), `primer` updates automatically.
fn run_primer() {
    use clap::CommandFactory;
    let app = Args::command();
    let agent_commands = ["editing", "grep", "glob", "roots"];
    let mut first = true;
    for name in agent_commands {
        let Some(sub) = app.find_subcommand(name) else {
            continue;
        };
        if !first {
            println!("\n─────────────────────────────────────────────────");
        }
        first = false;
        let mut sub = sub.clone();
        sub = sub
            .bin_name(format!("catenary {name}"))
            .disable_help_subcommand(true);
        let help = sub.render_help();
        println!("{help}");
    }
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
/// Prunes stale sessions based on the configured retention policy, then
/// enters a two-pane terminal interface showing sessions and events.
///
/// # Errors
///
/// Returns an error if configuration loading, session pruning, or TUI
/// initialisation fails.
fn run_dashboard() -> Result<()> {
    let config = catenary_mcp::config::Config::load()?;

    let conn = catenary_mcp::db::open_and_migrate()?;
    if let Err(e) = session::prune_sessions_with_conn(&conn, config.log_retention_days) {
        info!("session pruning failed: {e}");
    }

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
    tracing_subscriber::registry().with(logging.clone()).init();

    let config = catenary_mcp::config::Config::load()?;

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

    let disabled = roots
        .first()
        .and_then(|r| catenary_mcp::config::load_project_config(r).ok().flatten())
        .is_some_and(|pc| !pc.lsp);

    let (shared_session, shared_conn) = if disabled {
        info!("Catenary disabled by .catenary.toml (lsp = false) in {workspace_display}");
        // Activate with just the desktop notification sink so stale hook
        // detection can still fire OS notifications.
        let desktop_enabled = config
            .notifications
            .as_ref()
            .and_then(|n| n.desktop)
            .unwrap_or(true);
        let desktop_sink =
            catenary_mcp::notify::DesktopNotificationSink::with_enabled(desktop_enabled);
        logging.activate(vec![desktop_sink]);
        (None, None)
    } else {
        let conn = catenary_mcp::db::open_and_migrate()?;
        let instance_id: Arc<str> = "daemon".into();

        // Insert a session row so MessageDbSink's FK constraint
        // (messages.session_id → sessions.id) is satisfied. Without
        // this, every tracing event after activate() triggers an FK
        // violation → trace!() → recursive on_event → stack overflow.
        let started_at = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO sessions \
             (id, pid, display_name, started_at, alive) \
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![
                &*instance_id,
                std::process::id(),
                &workspace_display,
                &started_at
            ],
        )
        .context("insert daemon session row")?;

        let conn = Arc::new(std::sync::Mutex::new(conn));

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

        let session = Arc::new(catenary_mcp::bridge::session::Session::new(
            config,
            roots,
            logging.clone(),
            conn.clone(),
            instance_id,
            rt.handle().clone(),
            notification_router,
        ));

        // Spawn LSP servers in the background.
        let session_for_spawn = session.clone();
        rt.spawn(async move { session_for_spawn.spawn_all().await });

        (Some(session), Some(conn))
    };

    let session_for_shutdown = shared_session.clone();

    let manager = SessionManager::from_sockets(sockets, logging);
    let manager = match (shared_session, shared_conn) {
        (Some(session), Some(conn)) => manager.with_session(session, conn),
        _ => manager,
    };

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
    }

    // Drop removes socket files.
    drop(manager);

    info!(source = Source::DaemonLifecycle.as_str(), "daemon stopped",);

    result
}

/// Stops the running Catenary daemon.
///
/// Connects to the daemon's IPC socket and sends a shutdown request.
/// If no daemon is running, prints a message and returns successfully.
///
/// # Errors
///
/// Returns an error if the shutdown request fails after connecting.
#[cfg(unix)]
async fn run_stop() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();

    let Ok(stream) = tokio::net::UnixStream::connect(&ipc_path).await else {
        println!("No daemon running");
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

    println!("Daemon stopped");
    Ok(())
}

/// Runs a grep query against the running daemon.
///
/// Connects to the daemon's IPC socket, sends a [`GrepRequest`], and
/// prints the rendered output to stdout. The daemon resolves relative
/// patterns against `cwd` and dispatches to the grep pipeline.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn run_grep(
    pattern: String,
    glob: Option<String>,
    exclude: Option<String>,
    page: usize,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<()> {
    use catenary_mcp::router::{GrepRequest, METHOD_GREP};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let has_bre_alternation = pattern.contains("\\|");
    let cwd = std::env::current_dir().context("cannot determine working directory")?;
    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;

    let (reader, mut writer) = stream.into_split();

    let request = GrepRequest {
        cwd: Some(cwd),
        pattern,
        glob,
        exclude,
        page,
        include_gitignored,
        include_hidden,
    };
    let mut envelope = serde_json::to_value(&request)?;
    envelope
        .as_object_mut()
        .context("request is not an object")?
        .insert(
            "method".to_string(),
            serde_json::Value::String(METHOD_GREP.to_string()),
        );
    let mut payload = serde_json::to_string(&envelope)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let response: catenary_mcp::router::GrepResponse =
        serde_json::from_str(trimmed).context("invalid grep response from daemon")?;
    if response.output.is_empty() {
        println!("No results found");
        if has_bre_alternation {
            println!("hint: use `|` for alternation, not `\\|` (which matches a literal pipe)");
        }
    } else {
        println!("{}", response.output);
    }

    Ok(())
}

/// Runs a glob query against the running daemon.
///
/// Connects to the daemon's IPC socket, sends a [`GlobRequest`], and
/// prints the rendered output to stdout. The daemon resolves relative
/// patterns against `cwd` before dispatching to the glob pipeline.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn run_glob(
    pattern: String,
    exclude: Option<String>,
    page: usize,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<()> {
    use catenary_mcp::router::{GlobRequest, METHOD_GLOB};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let cwd = std::env::current_dir().context("cannot determine working directory")?;
    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;

    let (reader, mut writer) = stream.into_split();

    let request = GlobRequest {
        cwd: Some(cwd),
        pattern,
        exclude,
        page,
        include_gitignored,
        include_hidden,
    };
    let mut envelope = serde_json::to_value(&request)?;
    envelope
        .as_object_mut()
        .context("request is not an object")?
        .insert(
            "method".to_string(),
            serde_json::Value::String(METHOD_GLOB.to_string()),
        );
    let mut payload = serde_json::to_string(&envelope)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(());
    }

    let response: catenary_mcp::router::GlobResponse =
        serde_json::from_str(trimmed).context("invalid glob response from daemon")?;
    if !response.output.is_empty() {
        println!("{}", response.output);
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
async fn run_start_editing() -> Result<()> {
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
        println!("editing mode active");
    } else {
        let msg = response
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unexpected response");
        anyhow::bail!("{msg}");
    }

    Ok(())
}

/// Exits editing mode and prints diagnostics to stdout.
///
/// Connects to the daemon's IPC socket and sends `tool/editing-stop`.
/// The `PreToolUse` hook has already prepared the handoff — this command
/// retrieves the diagnostics and prints them.
///
/// # Errors
///
/// Returns an error if no daemon is running or the response is invalid.
#[cfg(unix)]
async fn run_done_editing() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("catenary daemon not running")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/editing-stop"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    // Read all response lines (diagnostics output may be multi-line).
    let mut buf_reader = BufReader::new(reader);
    let mut output = String::new();
    loop {
        let mut line = String::new();
        let n = buf_reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        output.push_str(&line);
    }

    let trimmed = output.trim();
    if !trimmed.is_empty() {
        println!("{trimmed}");
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
async fn run_root_command(path: PathBuf, method: &str) -> Result<()> {
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
            println!("{verb} root: {}", canonical.display());
        }
        "not_found" => {
            let msg = response
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("root not found in hook-managed roots");
            println!("{msg}");
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
    /// Expected Gemini CLI hooks, embedded at compile time.
    const GEMINI_HOOKS_EXPECTED: &str = include_str!("../hooks/hooks.json");
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

    // Gemini CLI: hooks at ~/.gemini/hooks/hooks.json
    let gemini_hooks = home.join(".gemini/hooks/hooks.json");
    check_host("Gemini CLI", &gemini_hooks, GEMINI_HOOKS_EXPECTED);

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

    // ── CLI hook subcommand tests ─────────────────────────────────

    #[test]
    fn test_cli_hook_pre_agent() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-agent", "--format=claude"]);
        let args = args.expect("hook pre-agent should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PreAgent { .. }));
    }

    #[test]
    fn test_cli_hook_pre_tool() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-tool", "--format=gemini"]);
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
        let args = Args::try_parse_from(["catenary", "hook", "session-start", "--format=gemini"]);
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
    fn test_cli_editing_stop() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "editing", "stop"]);
        let args = args.expect("editing stop should parse");
        assert!(matches!(
            args.command,
            Some(Command::Editing {
                command: EditingCommand::Stop
            })
        ));
    }

    // ── CLI grep subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_grep_minimal() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "grep", "foo"]);
        let args = args.expect("grep with pattern should parse");
        let Some(Command::Grep {
            pattern,
            glob,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo");
        assert!(glob.is_none());
        assert!(exclude.is_none());
        assert_eq!(page, 1);
        assert!(!include_gitignored);
        assert!(!include_hidden);
    }

    #[test]
    fn test_cli_grep_positional_glob() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "grep", "foo|bar", "src/**/*.rs"]);
        let args = args.expect("grep with positional glob should parse");
        let Some(Command::Grep {
            pattern,
            glob,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo|bar");
        assert_eq!(glob.as_deref(), Some("src/**/*.rs"));
        assert!(exclude.is_none());
        assert_eq!(page, 1);
        assert!(!include_gitignored);
        assert!(!include_hidden);
    }

    #[test]
    fn test_cli_grep_all_flags() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "foo|bar",
            "src/**/*.rs",
            "--exclude",
            "tests/",
            "--page",
            "3",
            "--include-gitignored",
            "--include-hidden",
        ]);
        let args = args.expect("grep with all flags should parse");
        let Some(Command::Grep {
            pattern,
            glob,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo|bar");
        assert_eq!(glob.as_deref(), Some("src/**/*.rs"));
        assert_eq!(exclude.as_deref(), Some("tests/"));
        assert_eq!(page, 3);
        assert!(include_gitignored);
        assert!(include_hidden);
    }

    #[test]
    fn test_cli_grep_missing_pattern() {
        use clap::Parser;
        let result = Args::try_parse_from(["catenary", "grep"]);
        assert!(result.is_err(), "grep without pattern should fail");
    }

    // ── CLI glob subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_glob_minimal() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "glob", "src/"]);
        let args = args.expect("glob with pattern should parse");
        let Some(Command::Glob {
            pattern,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Glob command");
        };
        assert_eq!(pattern, "src/");
        assert!(exclude.is_none());
        assert_eq!(page, 1);
        assert!(!include_gitignored);
        assert!(!include_hidden);
    }

    #[test]
    fn test_cli_glob_all_flags() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "glob",
            "**/*.rs",
            "--exclude",
            "target/**",
            "--page",
            "2",
            "--include-gitignored",
            "--include-hidden",
        ]);
        let args = args.expect("glob with all flags should parse");
        let Some(Command::Glob {
            pattern,
            exclude,
            page,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Glob command");
        };
        assert_eq!(pattern, "**/*.rs");
        assert_eq!(exclude.as_deref(), Some("target/**"));
        assert_eq!(page, 2);
        assert!(include_gitignored);
        assert!(include_hidden);
    }

    #[test]
    fn test_cli_glob_missing_pattern() {
        use clap::Parser;
        let result = Args::try_parse_from(["catenary", "glob"]);
        assert!(result.is_err(), "glob without pattern should fail");
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

    // ── CLI debug subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_debug_list() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "debug", "list"]);
        let args = args.expect("debug list should parse");
        assert!(matches!(
            args.command,
            Some(Command::Debug {
                command: DebugCommand::List
            })
        ));
    }

    #[test]
    fn test_cli_debug_monitor() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "debug", "monitor", "abc123"]);
        let args = args.expect("debug monitor should parse");
        let Some(Command::Debug {
            command: DebugCommand::Monitor { id, .. },
        }) = args.command
        else {
            unreachable!("expected Debug Monitor command");
        };
        assert_eq!(id, "abc123");
    }

    #[test]
    fn test_cli_debug_status() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "debug", "status", "abc123"]);
        let args = args.expect("debug status should parse");
        let Some(Command::Debug {
            command: DebugCommand::Status { id },
        }) = args.command
        else {
            unreachable!("expected Debug Status command");
        };
        assert_eq!(id, "abc123");
    }

    #[test]
    fn test_cli_primer() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "primer"]);
        let args = args.expect("primer should parse");
        assert!(matches!(args.command, Some(Command::Primer)));
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
}
