// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// MCP server implementation (transport-agnostic).
mod server;

pub use server::{McpServer, RootsChangedCallback};
// The MCP JSON-RPC message types (the bridge↔daemon wire-protocol definition)
// now live in the `catenary-mcp` bridge crate; re-export them here so the
// daemon's existing `crate::mcp::*` consumers keep resolving unchanged.
pub use catenary_mcp::protocol::*;
