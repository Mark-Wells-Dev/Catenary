// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// MCP server implementation (transport-agnostic).
mod server;
/// MCP type definitions and JSON-RPC messages.
mod types;

pub use server::{McpServer, RootsChangedCallback, ToolHandler};
pub use types::*;
