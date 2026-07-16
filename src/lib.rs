// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary is a bridge between MCP (Model Context Protocol) and LSP (Language Server Protocol).
//!
//! It allows AI coding assistants to access IDE-quality code intelligence by multiplexing
//! multiple language servers and exposing their capabilities via MCP tools.

/// The answer desk: Catenary answers Claude Code read-class permission prompts
/// and redacts secrets from tool output (misc 201).
pub mod answer_desk;
/// Bridge logic between MCP and LSP.
pub mod bridge;
/// Two-stage bucketing for grep tier 3 and glob tier 3 output.
pub mod bucketing;
/// Command-line interface definitions and utilities.
pub mod cli;
/// Companion-root derivation (auto-mounted sibling planning repos).
pub mod companions;
/// Configuration handling for language servers and session settings.
pub mod config;
/// Diagnostic noise filtering for LSP server output.
pub mod filter;
/// Health model: typed findings shared by `catenary doctor` and the TUI.
pub mod health;
/// The hit-batch frame protocol and the CLI-owns-the-walk skeleton (ws43).
pub mod hitstream;
/// IPC server for host CLI hook integration (diagnostics and root sync).
pub mod hook;
/// Guided-install engine: consented, verified per-ecosystem installs (tui-rework 06).
pub mod install;
/// The durable root lock — one cook per kitchen (root-ownership stage 2).
pub mod lock;
/// Multi-sink tracing dispatcher for Catenary telemetry.
pub mod logging;
/// LSP client implementation and server management.
pub mod lsp;
/// MCP server implementation and type definitions.
pub mod mcp;
/// Desktop notification support (OS-level notifications for error events).
pub mod notify;
/// Filesystem path resolvers for Catenary's base directories.
pub mod paths;
/// Protocol classification shared by core and display layers.
pub mod protocol;
/// CI-internal install recipes and the blessed-manifest (conformance harness).
pub mod recipes;
/// External signed-registry loader: fetched-verified → cache → seed (tui-rework 08).
pub mod registry;
/// Daemon session manager and MCP socket listener.
pub mod router;
/// Canonical `source` taxonomy for structured tracing events.
pub mod source;
/// Daemon-owned `state.json` live-state snapshot.
pub mod state_snapshot;
/// Symbol index for workspace-wide symbol extraction.
pub mod symbol_index;
/// Interactive TUI for session browsing and event tailing.
pub mod tui;
/// Out-of-tree agent worktree creation for Claude Code's `WorktreeCreate` hook.
pub mod worktree_create;
/// Guarded in-house disposal of Catenary-created worktrees (misc 151).
pub mod worktree_dispose;
/// The `worktree diff` / `worktree land` lifecycle verbs (misc 158).
pub mod worktree_land;
/// Bounded directory-deletion watch for subagent worktree roots (ticket 05).
pub mod worktree_watch;
