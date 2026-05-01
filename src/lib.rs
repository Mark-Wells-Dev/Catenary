// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary is a bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol).
//!
//! It allows AI coding assistants to access IDE-quality code intelligence by multiplexing
//! multiple language servers and exposing their capabilities via MCP tools.

/// Bridge logic between MCP and LSP.
pub mod bridge;
/// Two-stage bucketing for grep tier 3 and glob tier 3 output.
pub mod bucketing;
/// Command-line interface definitions and utilities.
pub mod cli;
/// Configuration handling for language servers and session settings.
pub mod config;
/// SQLite database connection management, schema creation, and migrations.
pub mod db;
/// Diagnostic noise filtering for LSP server output.
pub mod filter;
/// IPC server for host CLI hook integration (diagnostics and root sync).
pub mod hook;
/// Multi-sink tracing dispatcher for Catenary telemetry.
pub mod logging;
/// LSP client implementation and server management.
pub mod lsp;
/// MCP server implementation and type definitions.
pub mod mcp;
/// Protocol classification shared by core and display layers.
pub mod protocol;
/// Session management and event broadcasting.
pub mod session;
/// Canonical `source` taxonomy for structured tracing events.
pub mod source;
/// Symbol index for workspace-wide symbol extraction.
pub mod symbol_index;
/// Interactive TUI for session browsing and event tailing.
pub mod tui;
