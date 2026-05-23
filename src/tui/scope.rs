// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Event-driven scope lifecycle for the TUI message stream.
//!
//! Each MCP tool call is a scope that opens on pre-tool hook arrival,
//! streams children live, and auto-collapses to a summary line when the
//! post-tool hook signals completion. Closed scopes are inert.

use crate::session::SessionMessage;

// ── Scope state machine ─────────────────────────────────────────────

/// Lifecycle state of a scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeState {
    /// Pre-tool hook received, waiting for MCP request.
    Opening,
    /// MCP request received, children streaming in.
    Open,
    /// MCP response received, waiting for post-tool hook.
    Settling,
    /// Post-tool hook received, scope closed. Summary line.
    Closed,
    /// Next pre-tool hook arrived before post-tool hook — the scope
    /// was interrupted (user cancelled, error, agent moved on).
    Abandoned,
}

// ── Scope ───────────────────────────────────────────────────────────

/// A tool-call scope: lifecycle boundary around an MCP tool invocation.
///
/// Scopes are the primary display unit in the stream. Open scopes render
/// expanded with live children; closed scopes collapse to a summary line.
pub struct Scope {
    /// Scope identity — the pre-tool hook's scope UUID.
    pub scope_id: String,
    /// Session that owns this scope.
    pub session_id: String,
    /// The pre-tool hook that opened this scope.
    pub pre_hook: SessionMessage,
    /// The MCP tool call request (arrives after pre-hook).
    pub request: Option<SessionMessage>,
    /// The MCP tool call response.
    pub response: Option<SessionMessage>,
    /// The post-tool hook that closes this scope.
    pub post_hook: Option<SessionMessage>,
    /// LSP children accumulated during the tool call.
    pub children: Vec<SessionMessage>,
    /// Lifecycle state.
    pub state: ScopeState,
    /// Whether the user has manually expanded a closed scope.
    pub user_expanded: bool,
}

impl Scope {
    /// Create a new scope from a pre-tool hook message.
    ///
    /// The hook's `request_id` becomes the scope identity. The scope
    /// starts in `Opening` state, waiting for the MCP request.
    #[must_use]
    pub fn new(pre_hook: SessionMessage) -> Self {
        let scope_id = pre_hook.parent_id.clone().unwrap_or_default();
        let session_id = pre_hook.session_id.clone();
        Self {
            scope_id,
            session_id,
            pre_hook,
            request: None,
            response: None,
            post_hook: None,
            children: Vec::new(),
            state: ScopeState::Opening,
            user_expanded: false,
        }
    }

    /// Attach the MCP request and transition to `Open`.
    pub fn attach_request(&mut self, msg: SessionMessage) {
        self.request = Some(msg);
        self.state = ScopeState::Open;
    }

    /// Attach the MCP response and transition to `Settling`.
    pub fn attach_response(&mut self, msg: SessionMessage) {
        self.response = Some(msg);
        self.state = ScopeState::Settling;
    }

    /// Close this scope with the post-tool hook.
    #[allow(clippy::missing_const_for_fn, reason = "SessionMessage has Drop")]
    pub fn close(&mut self, post_hook: SessionMessage) {
        self.post_hook = Some(post_hook);
        self.state = ScopeState::Closed;
        self.user_expanded = false;
    }

    /// Mark this scope as abandoned (interrupted by a new pre-tool hook).
    pub const fn abandon(&mut self) {
        self.state = ScopeState::Abandoned;
        self.user_expanded = false;
    }

    /// Whether this scope should render expanded (showing children).
    #[must_use]
    pub const fn is_expanded(&self) -> bool {
        match self.state {
            ScopeState::Opening | ScopeState::Open | ScopeState::Settling => true,
            ScopeState::Closed | ScopeState::Abandoned => self.user_expanded,
        }
    }

    /// Whether this scope is still accepting new children.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.state,
            ScopeState::Opening | ScopeState::Open | ScopeState::Settling
        )
    }

    /// The display message for the scope header.
    ///
    /// Returns the MCP request if available (shows tool name), otherwise
    /// the pre-tool hook.
    #[must_use]
    #[allow(clippy::missing_const_for_fn, reason = "unwrap_or not const-stable")]
    pub fn header_message(&self) -> &SessionMessage {
        self.request.as_ref().unwrap_or(&self.pre_hook)
    }

    /// Number of visible children (LSP messages).
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

impl StreamEntry {
    /// The session ID for this entry.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Scope(scope) => &scope.session_id,
            Self::Standalone(msg) => &msg.session_id,
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
    use crate::session::test_support;

    fn pre_hook(scope_id: i64, session_id: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            parent_id: Some(format!("scope-{scope_id}")),
            ..test_support::message_with_ids(
                100 + scope_id,
                "hook",
                "pre-tool/editing-state",
                "",
                Some(scope_id),
                None,
            )
        }
    }

    fn mcp_request(scope_id: i64, session_id: &str, tool: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            payload: serde_json::json!({"params": {"name": tool}}),
            ..test_support::message_with_ids(
                200 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(scope_id + 1000),
                Some(scope_id),
            )
        }
    }

    fn mcp_response(scope_id: i64, session_id: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            ..test_support::message_with_ids(
                300 + scope_id,
                "mcp",
                "tools/call",
                "catenary",
                Some(scope_id + 1000),
                Some(scope_id),
            )
        }
    }

    fn post_hook(scope_id: i64, session_id: &str) -> SessionMessage {
        SessionMessage {
            session_id: session_id.to_string(),
            ..test_support::message_with_ids(
                400 + scope_id,
                "hook",
                "post-tool/diagnostics",
                "",
                Some(scope_id + 2000),
                Some(scope_id),
            )
        }
    }

    fn lsp_child(scope_id: i64, session_id: &str, method: &str) -> SessionMessage {
        let corr_id = scope_id + 1000;
        SessionMessage {
            session_id: session_id.to_string(),
            ..test_support::message_with_ids(
                500 + scope_id,
                "lsp",
                method,
                "rust-analyzer",
                Some(corr_id + 100),
                Some(corr_id),
            )
        }
    }

    #[test]
    fn scope_lifecycle_opening_to_closed() {
        let mut scope = Scope::new(pre_hook(1, "s1"));
        assert_eq!(scope.state, ScopeState::Opening);
        assert!(scope.is_expanded());
        assert!(scope.is_active());

        scope.attach_request(mcp_request(1, "s1", "grep"));
        assert_eq!(scope.state, ScopeState::Open);
        assert!(scope.is_expanded());

        scope.children.push(lsp_child(1, "s1", "workspace/symbol"));
        assert_eq!(scope.child_count(), 1);

        scope.attach_response(mcp_response(1, "s1"));
        assert_eq!(scope.state, ScopeState::Settling);
        assert!(scope.is_expanded());

        scope.close(post_hook(1, "s1"));
        assert_eq!(scope.state, ScopeState::Closed);
        assert!(!scope.is_expanded());
    }

    #[test]
    fn scope_user_expanded_toggle() {
        let mut scope = Scope::new(pre_hook(2, "s1"));
        scope.attach_request(mcp_request(2, "s1", "glob"));
        scope.close(post_hook(2, "s1"));
        assert!(!scope.is_expanded());

        scope.user_expanded = true;
        assert!(scope.is_expanded());

        scope.user_expanded = false;
        assert!(!scope.is_expanded());
    }

    #[test]
    fn scope_abandon_collapses() {
        let mut scope = Scope::new(pre_hook(3, "s1"));
        scope.attach_request(mcp_request(3, "s1", "grep"));
        assert!(scope.is_active());

        scope.abandon();
        assert_eq!(scope.state, ScopeState::Abandoned);
        assert!(!scope.is_expanded());
        assert!(!scope.is_active());
    }

    #[test]
    fn scope_header_message_prefers_request() {
        let mut scope = Scope::new(pre_hook(4, "s1"));
        assert_eq!(scope.header_message().r#type, "hook");

        let req = mcp_request(4, "s1", "grep");
        scope.attach_request(req);
        assert_eq!(scope.header_message().r#type, "mcp");
    }

    #[test]
    fn scope_id_from_parent_id() {
        let mut hook = pre_hook(42, "s1");
        hook.parent_id = Some("scope-uuid-42".to_string());
        let scope = Scope::new(hook);
        assert_eq!(scope.scope_id, "scope-uuid-42");
    }
}
