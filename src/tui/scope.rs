// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Scope lifecycle for the TUI message stream.
//!
//! A scope is any request/response pair logged to the messages DB.
//! All messages in a scope share one `parent_id` UUID. The first
//! message creates the scope (request/header), the response closes it,
//! and everything in between is a child.

use crate::session::SessionMessage;

// ── Scope state machine ─────────────────────────────────────────────

/// Lifecycle state of a scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeState {
    /// Request received, children streaming in.
    Open,
    /// Response received, scope complete.
    Closed,
}

// ── Scope ───────────────────────────────────────────────────────────

/// A request/response scope in the message stream.
///
/// Scopes are the primary display unit in the stream. Open scopes render
/// expanded with live children; closed scopes collapse to a summary line.
/// All messages in a scope share one `parent_id` UUID.
pub struct Scope {
    /// Scope identity — the `parent_id` UUID shared by all scope messages.
    pub scope_id: String,
    /// The request that opened this scope (first message with this UUID).
    pub request: SessionMessage,
    /// The response that closed this scope.
    pub response: Option<SessionMessage>,
    /// Children accumulated during the scope (LSP traffic, etc.).
    pub children: Vec<SessionMessage>,
    /// Lifecycle state.
    pub state: ScopeState,
    /// Whether the user has manually expanded a closed scope.
    pub user_expanded: bool,
}

impl Scope {
    /// Create a new scope from a request message.
    ///
    /// The message's `parent_id` becomes the scope identity. The scope
    /// starts in `Open` state, streaming children.
    #[must_use]
    pub fn new(request: SessionMessage) -> Self {
        let scope_id = request.parent_id.clone().unwrap_or_default();
        Self {
            scope_id,
            request,
            response: None,
            children: Vec::new(),
            state: ScopeState::Open,
            user_expanded: false,
        }
    }

    /// Close this scope with the response message.
    pub fn close(&mut self, response: SessionMessage) {
        self.response = Some(response);
        self.state = ScopeState::Closed;
    }

    /// Whether this scope should render expanded (showing children).
    #[must_use]
    pub const fn is_expanded(&self) -> bool {
        match self.state {
            ScopeState::Open => true,
            ScopeState::Closed => self.user_expanded,
        }
    }

    /// The display message for the scope header — always the request.
    #[must_use]
    pub const fn header_message(&self) -> &SessionMessage {
        &self.request
    }

    /// Number of visible children.
    #[must_use]
    pub const fn child_count(&self) -> usize {
        self.children.len()
    }
}

// ── Top-level entry ─────────────────────────────────────────────────

/// A top-level entry in the stream: either a scope or a standalone message.
///
/// Hooks without matching scopes (pre-agent, session-start, etc.) and
/// orphaned messages appear as standalone entries. `Scope` is boxed to
/// reduce peak size; `Standalone` stays inline to avoid double indirection
/// on the common read path.
#[allow(
    clippy::large_enum_variant,
    reason = "Standalone inline avoids heap indirection on the hot read path"
)]
pub enum StreamEntry {
    /// A tool-call scope with lifecycle.
    Scope(Box<Scope>),
    /// A standalone message not belonging to any scope.
    Standalone(SessionMessage),
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::session::test_support;

    fn mcp_request(scope_id: i64, session_id: &str, tool: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            payload: serde_json::json!({"params": {"name": tool}}),
            ..test_support::message_with_ids(
                200 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(&format!("scope-{scope_id}")),
            )
        }
    }

    fn mcp_response(scope_id: i64, session_id: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            payload: serde_json::json!({"result": {"content": [{"type": "text", "text": "ok"}]}}),
            ..test_support::message_with_ids(
                300 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(&format!("scope-{scope_id}")),
            )
        }
    }

    fn lsp_child(scope_id: i64, session_id: &str, method: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            ..test_support::message_with_ids(
                500 + scope_id,
                "lsp",
                method,
                "rust-analyzer",
                Some(&format!("scope-{scope_id}")),
            )
        }
    }

    #[test]
    fn scope_open_to_closed() {
        let mut scope = Scope::new(mcp_request(1, "s1", "grep"));
        assert_eq!(scope.state, ScopeState::Open);
        assert!(scope.is_expanded());

        scope.children.push(lsp_child(1, "s1", "workspace/symbol"));
        assert_eq!(scope.child_count(), 1);

        scope.close(mcp_response(1, "s1"));
        assert_eq!(scope.state, ScopeState::Closed);
        assert!(!scope.is_expanded());
    }

    #[test]
    fn scope_user_expanded_toggle() {
        let mut scope = Scope::new(mcp_request(2, "s1", "glob"));
        scope.close(mcp_response(2, "s1"));
        assert!(!scope.is_expanded());

        scope.user_expanded = true;
        assert!(scope.is_expanded());

        scope.user_expanded = false;
        assert!(!scope.is_expanded());
    }

    #[test]
    fn scope_header_is_always_request() {
        let scope = Scope::new(mcp_request(4, "s1", "grep"));
        assert_eq!(scope.header_message().r#type, "mcp");
        assert_eq!(scope.header_message().method, "tools/call");
    }

    #[test]
    fn scope_id_from_parent_id() {
        let scope = Scope::new(mcp_request(42, "s1", "grep"));
        assert_eq!(scope.scope_id, "scope-42");
    }
}
