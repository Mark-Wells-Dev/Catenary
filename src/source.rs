// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Canonical `source` taxonomy for structured tracing events.
//!
//! Every `source = "..."` field in a `tracing` event must use one of the
//! constants defined here. The [`Source`] enum is the single source of
//! truth; constants are derived from it for ergonomic use in `tracing`
//! macros.
//!
//! The taxonomy is a two-level `subsystem.concern` hierarchy.
//!
//! # Subsystems
//!
//! - `config` — configuration loading and validation
//! - `daemon` — daemon process (socket listeners, connection management)
//! - `hook` — hook layer (pre/post tool hooks)
//! - `logging` — logging infrastructure itself
//! - `lsp` — LSP client layer (server communication, lifecycle, routing)
//! - `mcp` — MCP server layer (host communication, dispatch)
//!
//! # Concerns
//!
//! - `bootstrap` — startup sequencing
//! - `dispatch` — message routing, method dispatch, capability checks
//! - `lifecycle` — spawn, init, crash, recovery, shutdown
//! - `logging` — forwarded log streams (e.g., server's `window/logMessage`)
//! - `parse` — parsing and deserialization
//! - `stderr` — raw server process stderr output
//! - `validation` — semantic correctness checks

use std::fmt;
use std::str::FromStr;

/// Closed set of valid `source` values for tracing events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Configuration loading errors (TOML parsing, deserialization).
    ConfigParse,
    /// Semantic configuration errors (orphan servers, unsupported keys).
    ConfigValidation,
    /// Daemon connection accept, correlation, and session routing.
    DaemonDispatch,
    /// Daemon startup, shutdown, and signal handling.
    DaemonLifecycle,
    /// Hook request routing and dispatch.
    HookDispatch,
    /// Logging infrastructure startup sequencing.
    LoggingBootstrap,
    /// LSP message routing, method dispatch, and capability checks.
    LspDispatch,
    /// LSP server spawn, init, crash, recovery, and shutdown.
    LspLifecycle,
    /// Forwarded LSP server log streams (`window/logMessage`).
    LspLogging,
    /// Raw server process stderr output.
    LspStderr,
    /// MCP message dispatch and roots handling.
    McpDispatch,
}

impl Source {
    /// Returns the canonical string representation (`subsystem.concern`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigParse => "config.parse",
            Self::ConfigValidation => "config.validation",
            Self::DaemonDispatch => "daemon.dispatch",
            Self::DaemonLifecycle => "daemon.lifecycle",
            Self::HookDispatch => "hook.dispatch",
            Self::LoggingBootstrap => "logging.bootstrap",
            Self::LspDispatch => "lsp.dispatch",
            Self::LspLifecycle => "lsp.lifecycle",
            Self::LspLogging => "lsp.logging",
            Self::LspStderr => "lsp.stderr",
            Self::McpDispatch => "mcp.dispatch",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Source {
    type Err = ParseSourceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "config.parse" => Ok(Self::ConfigParse),
            "config.validation" => Ok(Self::ConfigValidation),
            "daemon.dispatch" => Ok(Self::DaemonDispatch),
            "daemon.lifecycle" => Ok(Self::DaemonLifecycle),
            "hook.dispatch" => Ok(Self::HookDispatch),
            "logging.bootstrap" => Ok(Self::LoggingBootstrap),
            "lsp.dispatch" => Ok(Self::LspDispatch),
            "lsp.lifecycle" => Ok(Self::LspLifecycle),
            "lsp.logging" => Ok(Self::LspLogging),
            "lsp.stderr" => Ok(Self::LspStderr),
            "mcp.dispatch" => Ok(Self::McpDispatch),
            _ => Err(ParseSourceError(s.to_string())),
        }
    }
}

/// Error returned when parsing an unknown `source` value.
#[derive(Debug, Clone)]
pub struct ParseSourceError(String);

impl fmt::Display for ParseSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown source: {:?}", self.0)
    }
}

impl std::error::Error for ParseSourceError {}
