// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CLI utilities for terminal output formatting and colors.

pub mod command_filter;
pub mod commands;
pub mod config_template;
pub mod context_files;
pub mod doctor;
pub mod hooks;
pub mod install;
pub mod jsonl_reader;
pub mod teaching;
pub mod update;
pub mod version;
#[cfg(unix)]
pub mod worktree;

use clap::ValueEnum;
use crossterm::tty::IsTty;
use std::io::{self, Write, stdout};

/// Output format for hook commands.
///
/// Determines how hook output is structured for the host CLI.
/// Required on all hook-facing subcommands (`notify`, `sync-roots`).
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum HostFormat {
    /// Claude Code hooks (`PostToolUse` / `PreToolUse`).
    Claude,
    /// Antigravity CLI hooks (`PreToolUse` / `Stop`).
    Antigravity,
    /// OpenCode plugin (`tool.execute.before`).
    ///
    /// The CLI value is `opencode` (one word) — the OpenCode plugin invokes
    /// `catenary hook pre-tool --format=opencode`. Without this override clap's
    /// `ValueEnum` would derive the kebab-case `open-code`, rejecting the
    /// plugin's call.
    #[value(name = "opencode")]
    OpenCode,
}

impl HostFormat {
    /// Short lowercase label for the host CLI format.
    ///
    /// Used in IPC requests so the daemon can store the format as
    /// `client_name` in the sessions table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
            Self::OpenCode => "opencode",
        }
    }

    /// The host CLI's file-read tool name (for guidance template resolution).
    #[must_use]
    pub const fn read_tool(self) -> &'static str {
        match self {
            Self::Claude => "Read",
            Self::Antigravity => "read_file",
            Self::OpenCode => "read",
        }
    }

    /// The host CLI's file-edit tool name (for guidance template resolution).
    #[must_use]
    pub const fn edit_tool(self) -> &'static str {
        match self {
            Self::Claude => "Edit",
            Self::Antigravity => "write_to_file",
            Self::OpenCode => "edit",
        }
    }
}

/// Output format for the `query` command.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum QueryFormat {
    /// Human-readable table.
    Table,
    /// JSON array of raw firehose records: timestamp key is `ts` (UTC), session id rides in `scope_id`; empty keys are omitted.
    Json,
}

/// Configuration for color output.
#[derive(Debug, Clone)]
pub struct ColorConfig {
    /// Whether color output is enabled.
    pub enabled: bool,
}

impl ColorConfig {
    /// Create a new `ColorConfig`, auto-detecting TTY unless `nocolor` is true.
    #[must_use]
    pub fn new(nocolor: bool) -> Self {
        Self {
            enabled: !nocolor && stdout().is_tty(),
        }
    }

    /// ANSI escape code for green (incoming/request).
    #[must_use]
    pub fn green(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[32m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for blue (outgoing/response).
    #[must_use]
    pub fn blue(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[34m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for red (errors).
    #[must_use]
    pub fn red(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[31m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for cyan (language names).
    #[must_use]
    pub fn cyan(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for yellow (warnings/skipped).
    #[must_use]
    pub fn yellow(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[33m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for bold text.
    #[must_use]
    pub fn bold(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    /// ANSI escape code for dim text.
    #[must_use]
    pub fn dim(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

/// Get the terminal width, defaulting to 80 if unable to detect.
#[must_use]
pub fn terminal_width() -> usize {
    crossterm::terminal::size().map_or(80, |(w, _)| w as usize)
}

/// Object-safe extension of [`Write`] that supports consuming the
/// writer to extract captured bytes (test buffers).
trait OutputWriter: Write + Send {
    /// Consume the writer and return captured bytes, if applicable.
    fn into_bytes(self: Box<Self>) -> Option<Vec<u8>>;
}

impl OutputWriter for io::Stdout {
    fn into_bytes(self: Box<Self>) -> Option<Vec<u8>> {
        None
    }
}

impl OutputWriter for io::Stderr {
    fn into_bytes(self: Box<Self>) -> Option<Vec<u8>> {
        None
    }
}

impl OutputWriter for Vec<u8> {
    fn into_bytes(self: Box<Self>) -> Option<Vec<u8>> {
        Some(*self)
    }
}

/// Consolidated output destination for CLI commands.
///
/// Wraps a writer with color configuration and terminal width.
/// Production code uses [`Output::stdout`]; tests use [`Output::buffer`]
/// to capture and assert on output.
pub struct Output {
    w: Box<dyn OutputWriter>,
    /// Color configuration for styled output.
    pub colors: ColorConfig,
    /// Terminal width for layout calculations.
    pub width: usize,
}

impl Output {
    /// Create an `Output` that writes to stdout.
    #[must_use]
    pub fn stdout(nocolor: bool) -> Self {
        Self {
            w: Box::new(io::stdout()),
            colors: ColorConfig::new(nocolor),
            width: terminal_width(),
        }
    }

    /// Create an `Output` that writes to stderr.
    ///
    /// The teaching stream for the search surface (VERBS streams ruling): the
    /// pipe-friendly `grep`/`glob` carry results on stdout and *everything about
    /// them* — the loud zero-match line, the four teaching moments, pagination
    /// meta — on stderr, so a `| head` never truncates results with prose and an
    /// explicit `2>/dev/null` is consent to lose the teaching.
    #[must_use]
    pub fn stderr(nocolor: bool) -> Self {
        Self {
            w: Box::new(io::stderr()),
            colors: ColorConfig::new(nocolor),
            width: terminal_width(),
        }
    }

    /// Write formatted text followed by a newline.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write fails.
    pub fn writeln(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        writeln!(self.w, "{args}")
    }

    /// Write formatted text without a trailing newline.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write fails.
    pub fn write_str(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
        write!(self.w, "{args}")
    }

    /// Write a pre-rendered result block as ONE atomic `write_all`, its trailing
    /// newline included, then flush.
    ///
    /// The block-level analogue of [`crate::hitstream::sink::ResultSink::write_line`]
    /// for the query result path: the whole body is assembled into one owned
    /// buffer and flushed with a single `write_all`, so `io::Stdout`'s line
    /// buffering never splits it into per-line syscalls that a physically-merged
    /// stderr advisory could interleave (bug 112 — a glob directory `dir/*` note
    /// fusing mid-line into a stdout result under `2>&1` piping). Flushing here,
    /// before the caller writes any advisory to stderr, drains stdout fully so the
    /// two streams never interleave under a merged fd.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the write or flush fails.
    pub fn write_block(&mut self, body: &str) -> io::Result<()> {
        let mut buf = String::with_capacity(body.len() + 1);
        buf.push_str(body);
        buf.push('\n');
        self.w.write_all(buf.as_bytes())?;
        self.w.flush()
    }

    /// Create an in-memory buffer that captures written bytes.
    ///
    /// Pairs with [`Output::into_string`] to capture and assert on emitted
    /// output in tests. Colors are disabled; `width` is fixed to the given
    /// value. Lives outside `#[cfg(test)]` so handlers in the `catenary`
    /// binary crate — a separate compilation unit that links the library
    /// without its `cfg(test)` items — can still capture output in tests.
    #[must_use]
    pub fn buffer(width: usize) -> Self {
        Self {
            w: Box::new(Vec::<u8>::new()),
            colors: ColorConfig::new(true),
            width,
        }
    }

    /// Consume the output and return any captured bytes as a string.
    ///
    /// Returns an empty string when the writer captured nothing — e.g. an
    /// [`Output::stdout`] destination, which retains no bytes. Pair with
    /// [`Output::buffer`] to capture emitted text.
    #[must_use]
    pub fn into_string(self) -> String {
        self.w
            .into_bytes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }
}

impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.w.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Truncate a string to `max_len` characters, adding "..." if truncated.
#[must_use]
pub fn truncate(s: &str, max_len: usize) -> String {
    if max_len <= 3 {
        return ".".repeat(max_len.min(3));
    }
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

/// Column width configuration for the list command.
///
/// Languages are displayed on a second line, so they are not included here.
#[derive(Debug)]
pub struct ColumnWidths {
    /// Width of the row number column.
    pub row_num: usize,
    /// Width of the ID column.
    pub id: usize,
    /// Width of the PID column.
    pub pid: usize,
    /// Width of the client column.
    pub client: usize,
    /// Width of the workspace column.
    pub workspace: usize,
    /// Width of the started time column.
    pub started: usize,
}

impl ColumnWidths {
    /// Calculate column widths based on terminal width.
    /// Columns: # | ID | PID | CLIENT | WORKSPACE | STARTED
    #[must_use]
    pub const fn calculate(term_width: usize) -> Self {
        // Fixed minimum widths
        let row_num = 3; // "#"
        let pid = 8; // "PID"
        let started = 12; // "STARTED"

        // Calculate flexible widths
        // Reserve space for separators (5 spaces between 6 columns)
        let fixed_space = row_num + pid + started + 5;
        let flexible_space = term_width.saturating_sub(fixed_space);

        let min_id = 12;
        let min_client = 20;
        let min_workspace = 20;

        let total_min_flex = min_id + min_client + min_workspace;

        if flexible_space <= total_min_flex {
            Self {
                row_num,
                id: min_id,
                pid,
                client: min_client,
                workspace: min_workspace,
                started,
            }
        } else {
            // All extra space goes to workspace
            let extra = flexible_space - total_min_flex;
            Self {
                row_num,
                id: min_id,
                pid,
                client: min_client,
                workspace: min_workspace + extra,
                started,
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("test", 4), "test");
    }

    #[test]
    fn test_truncate_long_string() {
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("abcdefghij", 7), "abcd...");
    }

    #[test]
    fn test_truncate_edge_cases() {
        assert_eq!(truncate("hello", 3), "...");
        assert_eq!(truncate("hello", 2), "..");
        assert_eq!(truncate("hello", 1), ".");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn test_color_config_disabled() {
        let config = ColorConfig::new(true);
        assert!(!config.enabled);
        assert_eq!(config.green("test"), "test");
        assert_eq!(config.blue("test"), "test");
        assert_eq!(config.red("test"), "test");
        assert_eq!(config.cyan("test"), "test");
        assert_eq!(config.yellow("test"), "test");
        assert_eq!(config.bold("test"), "test");
        assert_eq!(config.dim("test"), "test");
    }

    // ── write_block (bug 112: atomic result-body write) ──────────────

    /// `write_block` appends exactly one trailing newline and emits the body
    /// verbatim — the atomic-line-write leg that keeps a multi-line result body
    /// one `write_all` (never split across per-line syscalls a merged-fd stderr
    /// advisory could interleave).
    #[test]
    fn write_block_appends_single_newline() {
        let mut out = Output::buffer(80);
        out.write_block("line one\nline two\nline three")
            .expect("write_block");
        assert_eq!(out.into_string(), "line one\nline two\nline three\n");
    }

    /// A body that already ends in a newline still gets exactly one newline
    /// appended (the caller trims trailing newlines before handing the body over,
    /// so this documents the raw contract: one `\n` is always added).
    #[test]
    fn write_block_adds_exactly_one_newline_even_when_body_lacks_one() {
        let mut out = Output::buffer(80);
        out.write_block("solo").expect("write_block");
        assert_eq!(out.into_string(), "solo\n");
    }

    // ── HostFormat tests ────────────────────────────────────────────

    #[test]
    fn host_format_edit_tool_names() {
        assert_eq!(HostFormat::Claude.edit_tool(), "Edit");
        assert_eq!(HostFormat::Antigravity.edit_tool(), "write_to_file");
        assert_eq!(HostFormat::OpenCode.edit_tool(), "edit");
    }

    #[test]
    fn host_format_read_tool_names() {
        assert_eq!(HostFormat::Claude.read_tool(), "Read");
        assert_eq!(HostFormat::Antigravity.read_tool(), "read_file");
        assert_eq!(HostFormat::OpenCode.read_tool(), "read");
    }

    #[test]
    fn test_calculate_column_widths() {
        let widths = ColumnWidths::calculate(120);
        assert_eq!(widths.row_num, 3);
        assert_eq!(widths.pid, 8);
        assert_eq!(widths.started, 12);
        // Flexible columns should have reasonable widths
        assert!(widths.id >= 12);
        assert!(widths.workspace >= 20);
        assert!(widths.client >= 20);
    }

    #[test]
    fn test_calculate_column_widths_shrinks() {
        let widths = ColumnWidths::calculate(60);
        // Should use minimum widths for narrow terminals
        assert_eq!(widths.id, 12);
        assert_eq!(widths.workspace, 20);
        assert_eq!(widths.client, 20);
    }

    #[test]
    fn test_calculate_column_widths_wide() {
        // Wide terminal: all extra space goes to workspace
        let widths = ColumnWidths::calculate(200);
        assert!(
            widths.workspace > widths.client,
            "workspace ({}) should be wider than client ({})",
            widths.workspace,
            widths.client,
        );
        // Client stays at minimum
        assert_eq!(widths.client, 20);
    }
}
