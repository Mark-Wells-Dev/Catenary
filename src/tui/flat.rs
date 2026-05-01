// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Flat line generation: converts pipeline [`DisplayEntry`] trees into a
//! linear sequence of [`FlatLine`]s suitable for viewport slicing and rendering.
//!
//! The main entry point is [`PanelState::flat_lines()`], which runs the full
//! display pipeline (pair merge → scope collapse → run collapse) and then
//! flattens the result, expanding scopes and detail lines as needed.

use std::rc::Rc;

use super::format::{
    format_collapsed_plain, format_message_plain, format_pair_plain, format_scope_plain,
};
use super::panel::{PanelState, frontmatter_lines};
use super::pipeline::{DisplayEntry, SegmentPosition, pair_merge, run_collapse, scope_collapse};

/// A line in the flattened view — message header, detail, or collapsed run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlatLine {
    /// A message header (the one-line summary).
    MessageHeader {
        /// Index into the messages vec (request index for pairs).
        message_index: usize,
        /// If this header is a merged pair, the response message index.
        paired_response: Option<usize>,
    },
    /// A detail line within an expanded message.
    Detail {
        /// Index into the messages vec (request index for pairs).
        message_index: usize,
        /// Index of this detail line within the expansion.
        detail_index: usize,
    },
    /// A collapsed run header (summary of N consecutive messages).
    CollapsedHeader {
        /// Index of the first message in the run.
        start_index: usize,
        /// Index of the last message in the run (inclusive).
        end_index: usize,
        /// Number of messages in the run.
        count: usize,
    },
    /// A scope header (parent of grouped children).
    ScopeHeader {
        /// The parent `DisplayEntry` (for rendering the summary line).
        parent: Rc<DisplayEntry>,
        /// Number of child entries in the scope.
        child_count: usize,
        /// Segment position within a segmented scope.
        position: SegmentPosition,
        /// Expansion key (first child's message index for this segment).
        expansion_key: usize,
    },
    /// A `---` separator line between frontmatter and children.
    Separator,
    /// An indented child line within an expanded scope.
    ScopeChild {
        /// Depth level for indentation.
        depth: usize,
        /// Message index of the scope parent (expansion key).
        scope_parent_index: usize,
        /// The inner `FlatLine` (`MessageHeader`, `CollapsedHeader`, etc.).
        inner: Box<Self>,
    },
}

impl PanelState<'_> {
    /// Build a flat list of lines: message headers interleaved with detail
    /// lines for any expanded messages. Uses pair merge then run collapse
    /// to combine adjacent request/response pairs and consecutive same-key
    /// messages into single lines.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "scope collapse adds a fourth variant arm"
    )]
    pub fn flat_lines(&self) -> Vec<FlatLine> {
        let lower_pattern = self.filter_pattern.as_ref().map(|p| p.to_lowercase());
        let merged = pair_merge(&self.messages);
        let scoped = scope_collapse(merged, &self.messages);
        let entries = run_collapse(scoped, &self.messages);
        let mut lines = Vec::new();

        for entry in &entries {
            match entry {
                DisplayEntry::Single {
                    index,
                    parent_id: _,
                } => {
                    let index = *index;
                    let msg = &self.messages[index];
                    if let Some(ref pat) = lower_pattern {
                        let plain = format_message_plain(msg);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    lines.push(FlatLine::MessageHeader {
                        message_index: index,
                        paired_response: None,
                    });
                    if self.expanded.contains(&index) {
                        let count = frontmatter_lines(msg, self.theme).len();
                        for detail_index in 0..count {
                            lines.push(FlatLine::Detail {
                                message_index: index,
                                detail_index,
                            });
                        }
                    }
                }
                DisplayEntry::Paired {
                    request_index,
                    response_index,
                    parent_id: _,
                } => {
                    let request_index = *request_index;
                    let response_index = *response_index;
                    let req = &self.messages[request_index];
                    let resp = &self.messages[response_index];
                    if let Some(ref pat) = lower_pattern {
                        let plain = format_pair_plain(req, resp);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    lines.push(FlatLine::MessageHeader {
                        message_index: request_index,
                        paired_response: Some(response_index),
                    });
                    if self.expanded.contains(&request_index) {
                        let count = frontmatter_lines(req, self.theme).len();
                        for detail_index in 0..count {
                            lines.push(FlatLine::Detail {
                                message_index: request_index,
                                detail_index,
                            });
                        }
                    }
                }
                DisplayEntry::Collapsed {
                    start_index,
                    end_index,
                    count,
                    parent_id: _,
                } => {
                    let start_index = *start_index;
                    let end_index = *end_index;
                    let count = *count;
                    if let Some(ref pat) = lower_pattern {
                        let plain =
                            format_collapsed_plain(&self.messages, start_index, end_index, count);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    if self.expanded.contains(&start_index) {
                        // Show individual messages when expanded.
                        for idx in start_index..=end_index {
                            lines.push(FlatLine::MessageHeader {
                                message_index: idx,
                                paired_response: None,
                            });
                            if self.expanded.contains(&idx) {
                                let msg = &self.messages[idx];
                                let detail_count = frontmatter_lines(msg, self.theme).len();
                                for detail_index in 0..detail_count {
                                    lines.push(FlatLine::Detail {
                                        message_index: idx,
                                        detail_index,
                                    });
                                }
                            }
                        }
                    } else {
                        lines.push(FlatLine::CollapsedHeader {
                            start_index,
                            end_index,
                            count,
                        });
                    }
                }
                DisplayEntry::Scope {
                    parent,
                    children,
                    position,
                } => {
                    let child_count = children.len();
                    if let Some(ref pat) = lower_pattern {
                        let plain =
                            format_scope_plain(parent, child_count, *position, &self.messages);
                        if !plain.to_lowercase().contains(pat) {
                            continue;
                        }
                    }
                    let scope_key = entry.expansion_index();
                    lines.push(FlatLine::ScopeHeader {
                        parent: Rc::clone(parent),
                        child_count,
                        position: *position,
                        expansion_key: scope_key,
                    });
                    if self.expanded.contains(&scope_key) {
                        self.emit_scope_frontmatter(parent, 1, scope_key, &mut lines);
                        self.flatten_scope_children(children, scope_key, 1, &mut lines);
                    }
                }
            }
        }
        lines
    }

    /// Emit frontmatter `Detail` lines and a `Separator` for a scope parent.
    ///
    /// Extracts the request message index from the parent entry, generates
    /// frontmatter lines, and pushes them as `ScopeChild` wrappers. If the
    /// parent has a non-empty payload, a `Separator` is appended after the
    /// last detail line.
    fn emit_scope_frontmatter(
        &self,
        parent: &DisplayEntry,
        depth: usize,
        scope_parent_index: usize,
        lines: &mut Vec<FlatLine>,
    ) {
        let msg_index = match parent {
            DisplayEntry::Single { index, .. } => Some(*index),
            DisplayEntry::Paired { request_index, .. } => Some(*request_index),
            _ => None,
        };
        if let Some(mi) = msg_index {
            let fm_count = frontmatter_lines(&self.messages[mi], self.theme).len();
            for detail_index in 0..fm_count {
                lines.push(FlatLine::ScopeChild {
                    depth,
                    scope_parent_index,
                    inner: Box::new(FlatLine::Detail {
                        message_index: mi,
                        detail_index,
                    }),
                });
            }
            if fm_count > 0 {
                lines.push(FlatLine::ScopeChild {
                    depth,
                    scope_parent_index,
                    inner: Box::new(FlatLine::Separator),
                });
            }
        }
    }

    /// Check whether a `DisplayEntry` matches a lowercased filter pattern.
    fn entry_matches_filter(&self, entry: &DisplayEntry, pattern: &str) -> bool {
        let plain = match entry {
            DisplayEntry::Single { index, .. } => format_message_plain(&self.messages[*index]),
            DisplayEntry::Paired {
                request_index,
                response_index,
                ..
            } => format_pair_plain(
                &self.messages[*request_index],
                &self.messages[*response_index],
            ),
            DisplayEntry::Collapsed {
                start_index,
                end_index,
                count,
                ..
            } => format_collapsed_plain(&self.messages, *start_index, *end_index, *count),
            DisplayEntry::Scope {
                parent,
                children,
                position,
            } => format_scope_plain(parent, children.len(), *position, &self.messages),
        };
        plain.to_lowercase().contains(pattern)
    }

    /// Emit a `ScopeChild(MessageHeader)` with optional frontmatter detail lines.
    fn emit_scope_child_message(
        &self,
        message_index: usize,
        paired_response: Option<usize>,
        depth: usize,
        scope_parent_index: usize,
        lines: &mut Vec<FlatLine>,
    ) {
        lines.push(FlatLine::ScopeChild {
            depth,
            scope_parent_index,
            inner: Box::new(FlatLine::MessageHeader {
                message_index,
                paired_response,
            }),
        });
        if self.expanded.contains(&message_index) {
            let msg = &self.messages[message_index];
            let count = frontmatter_lines(msg, self.theme).len();
            for detail_index in 0..count {
                lines.push(FlatLine::ScopeChild {
                    depth,
                    scope_parent_index,
                    inner: Box::new(FlatLine::Detail {
                        message_index,
                        detail_index,
                    }),
                });
            }
        }
    }

    /// Flatten scope children into `ScopeChild` flat lines.
    ///
    /// Applies filtering and run collapse at depth before emitting lines.
    fn flatten_scope_children(
        &self,
        children: &[DisplayEntry],
        scope_parent_index: usize,
        depth: usize,
        lines: &mut Vec<FlatLine>,
    ) {
        let lower_pattern = self.filter_pattern.as_ref().map(|p| p.to_lowercase());
        let owned: Vec<DisplayEntry> = lower_pattern.as_ref().map_or_else(
            || children.to_vec(),
            |pat| {
                children
                    .iter()
                    .filter(|c| self.entry_matches_filter(c, pat))
                    .cloned()
                    .collect()
            },
        );
        let collapsed_children = run_collapse(owned, &self.messages);

        for child in &collapsed_children {
            match child {
                DisplayEntry::Single { index, .. } => {
                    self.emit_scope_child_message(*index, None, depth, scope_parent_index, lines);
                }
                DisplayEntry::Paired {
                    request_index,
                    response_index,
                    ..
                } => {
                    self.emit_scope_child_message(
                        *request_index,
                        Some(*response_index),
                        depth,
                        scope_parent_index,
                        lines,
                    );
                }
                DisplayEntry::Collapsed {
                    start_index,
                    end_index,
                    count,
                    ..
                } => {
                    if self.expanded.contains(start_index) {
                        for idx in *start_index..=*end_index {
                            self.emit_scope_child_message(
                                idx,
                                None,
                                depth,
                                scope_parent_index,
                                lines,
                            );
                        }
                    } else {
                        lines.push(FlatLine::ScopeChild {
                            depth,
                            scope_parent_index,
                            inner: Box::new(FlatLine::CollapsedHeader {
                                start_index: *start_index,
                                end_index: *end_index,
                                count: *count,
                            }),
                        });
                    }
                }
                DisplayEntry::Scope {
                    parent: nested_parent,
                    children: nested_children,
                    position: nested_position,
                } => {
                    let nested_child_count = nested_children.len();
                    let nested_key = child.expansion_index();
                    lines.push(FlatLine::ScopeChild {
                        depth,
                        scope_parent_index,
                        inner: Box::new(FlatLine::ScopeHeader {
                            parent: Rc::clone(nested_parent),
                            child_count: nested_child_count,
                            position: *nested_position,
                            expansion_key: nested_key,
                        }),
                    });
                    if self.expanded.contains(&nested_key) {
                        self.emit_scope_frontmatter(nested_parent, depth + 1, nested_key, lines);
                        self.flatten_scope_children(nested_children, nested_key, depth + 1, lines);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::IconConfig;
    use crate::session::SessionMessage;
    use crate::tui::icons::IconSet;
    use crate::tui::panel::{PanelState, frontmatter_lines};
    use crate::tui::theme::Theme;

    fn test_theme() -> Theme {
        Theme::new()
    }

    fn test_icons() -> IconSet {
        IconSet::from_config(IconConfig::default())
    }

    use crate::session::test_support;

    fn make_message(r#type: &str, method: &str, server: &str) -> SessionMessage {
        test_support::message(r#type, method, server)
    }

    fn make_non_collapsing_messages(n: usize) -> Vec<SessionMessage> {
        (0..n)
            .map(|i| make_message("hook", &format!("test-{i}"), "catenary"))
            .collect()
    }

    fn make_message_with_payload(
        r#type: &str,
        method: &str,
        server: &str,
        payload: serde_json::Value,
    ) -> SessionMessage {
        test_support::message_with_payload(r#type, method, server, payload)
    }

    fn make_hook_diag_message(file: &str, count: u64) -> SessionMessage {
        make_message_with_payload(
            "hook",
            "post-tool",
            "catenary",
            serde_json::json!({
                "file": file,
                "count": count,
                "preview": "\t:12:1 [error] rustc: something"
            }),
        )
    }

    fn make_message_with_id_parent(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
    ) -> SessionMessage {
        test_support::message_with_ids(id, r#type, method, server, request_id, parent_id)
    }

    #[test]
    fn test_flat_lines_no_expansion() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = make_non_collapsing_messages(5);
        panel.load_messages(messages);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 5);
        for (i, fl) in flat.iter().enumerate() {
            assert_eq!(
                *fl,
                FlatLine::MessageHeader {
                    message_index: i,
                    paired_response: None,
                }
            );
        }
    }

    #[test]
    fn test_flat_lines_one_expanded() {
        let theme = test_theme();
        let icons = test_icons();
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        let messages = vec![
            make_message("lsp", "initialized", "rust-analyzer"),
            make_hook_diag_message("/src/lib.rs", 3),
            make_message("mcp", "tools/list", "catenary"),
        ];
        panel.load_messages(messages);
        panel.expanded.insert(1);

        let flat = panel.flat_lines();
        let detail_count = frontmatter_lines(&panel.messages[1], &theme).len();
        assert!(detail_count > 0, "hook diag message should have details");
        assert_eq!(flat.len(), 3 + detail_count);
        assert_eq!(
            flat[0],
            FlatLine::MessageHeader {
                message_index: 0,
                paired_response: None,
            }
        );
        assert_eq!(
            flat[1],
            FlatLine::MessageHeader {
                message_index: 1,
                paired_response: None,
            }
        );
        for i in 0..detail_count {
            assert_eq!(
                flat[2 + i],
                FlatLine::Detail {
                    message_index: 1,
                    detail_index: i
                }
            );
        }
        assert_eq!(
            flat[2 + detail_count],
            FlatLine::MessageHeader {
                message_index: 2,
                paired_response: None,
            }
        );
    }

    #[test]
    fn test_frontmatter_lines_paired_uses_request() {
        // Expand a Paired entry. Detail lines should contain the request
        // payload, not the response payload.
        let theme = test_theme();
        let icons = test_icons();

        let req = test_support::message_with_ids_payload(
            1,
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            Some(100),
            None,
            serde_json::json!({"params": {"uri": "file:///src/main.rs"}}),
        );

        let resp = test_support::message_with_ids_payload(
            2,
            "lsp",
            "textDocument/hover",
            "rust-analyzer",
            Some(100),
            None,
            serde_json::json!({"result": {"contents": "fn main()"}}),
        );

        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(vec![req, resp]);
        // Expand the paired entry (request index = 0).
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        // Collect all Detail line texts.
        let detail_text: String = flat
            .iter()
            .filter_map(|fl| {
                if let FlatLine::Detail {
                    message_index,
                    detail_index,
                } = fl
                {
                    let lines = frontmatter_lines(&panel.messages[*message_index], &theme);
                    lines.get(*detail_index).map(|line| {
                        line.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                    })
                } else {
                    None
                }
            })
            .collect();

        assert!(
            detail_text.contains("main.rs"),
            "detail should contain request payload: {detail_text}"
        );
        assert!(
            !detail_text.contains("fn main()"),
            "detail should NOT contain response payload: {detail_text}"
        );
    }

    #[test]
    fn test_scope_flat_lines_collapsed() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 1, "collapsed scope should be 1 line: {flat:?}");
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { child_count: 2, .. }),
            "should be ScopeHeader with 2 children: {:?}",
            flat[0]
        );
    }

    #[test]
    fn test_scope_flat_lines_expanded() {
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 3, "expanded scope should be 3 lines: {flat:?}");
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { .. }),
            "first should be ScopeHeader"
        );
        assert!(
            matches!(
                flat[1],
                FlatLine::ScopeChild {
                    depth: 1,
                    scope_parent_index: 0,
                    ..
                }
            ),
            "second should be ScopeChild at depth 1: {:?}",
            flat[1]
        );
        assert!(
            matches!(
                flat[2],
                FlatLine::ScopeChild {
                    depth: 1,
                    scope_parent_index: 0,
                    ..
                }
            ),
            "third should be ScopeChild at depth 1: {:?}",
            flat[2]
        );
    }

    fn make_message_with_id_parent_payload(
        id: i64,
        r#type: &str,
        method: &str,
        server: &str,
        request_id: Option<i64>,
        parent_id: Option<i64>,
        payload: serde_json::Value,
    ) -> SessionMessage {
        test_support::message_with_ids_payload(
            id, r#type, method, server, request_id, parent_id, payload,
        )
    }

    #[test]
    fn test_scope_frontmatter_emission() {
        // Scope parent with non-empty payload. Expand. Verify:
        // ScopeHeader → ScopeChild(Detail)... → ScopeChild(Separator) → ScopeChild(MessageHeader).
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent_payload(
                1,
                "mcp",
                "tools/call",
                "catenary",
                Some(500),
                None,
                serde_json::json!({"params": {"name": "grep"}}),
            ),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
        ];
        let fm_count = frontmatter_lines(&messages[0], &theme).len();
        assert!(fm_count > 0, "parent should have frontmatter lines");

        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0); // expansion key = parent index

        let flat = panel.flat_lines();
        // Expected: ScopeHeader + fm_count Detail lines + 1 Separator + 1 child MessageHeader
        let expected_len = 1 + fm_count + 1 + 1;
        assert_eq!(
            flat.len(),
            expected_len,
            "expected {expected_len} lines (1 header + {fm_count} frontmatter + 1 separator + 1 child): {flat:?}"
        );
        assert!(matches!(flat[0], FlatLine::ScopeHeader { .. }));
        // Frontmatter detail lines
        for i in 0..fm_count {
            assert!(
                matches!(
                    &flat[1 + i],
                    FlatLine::ScopeChild { inner, .. }
                        if matches!(inner.as_ref(), FlatLine::Detail { .. })
                ),
                "line {} should be ScopeChild(Detail): {:?}",
                1 + i,
                flat[1 + i]
            );
        }
        // Separator
        assert!(
            matches!(
                &flat[1 + fm_count],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::Separator)
            ),
            "line {} should be ScopeChild(Separator): {:?}",
            1 + fm_count,
            flat[1 + fm_count]
        );
        // Child message header
        assert!(
            matches!(
                &flat[2 + fm_count],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::MessageHeader { .. })
            ),
            "last line should be ScopeChild(MessageHeader): {:?}",
            flat[2 + fm_count]
        );
    }

    #[test]
    fn test_scope_frontmatter_empty_payload() {
        // Scope parent with empty payload — no frontmatter or separator.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        // ScopeHeader + 2 children, no frontmatter or separator
        assert_eq!(
            flat.len(),
            3,
            "empty payload scope should have no frontmatter: {flat:?}"
        );
        assert!(matches!(flat[0], FlatLine::ScopeHeader { .. }));
        // No Separator anywhere
        let has_separator = flat.iter().any(|fl| {
            matches!(fl, FlatLine::ScopeChild { inner, .. } if matches!(inner.as_ref(), FlatLine::Separator))
        });
        assert!(
            !has_separator,
            "empty payload scope should have no separator"
        );
    }

    #[test]
    fn test_separator_in_flat_lines() {
        // Verify Separator appears between last frontmatter Detail and first child.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent_payload(
                1,
                "mcp",
                "tools/call",
                "catenary",
                Some(500),
                None,
                serde_json::json!({"params": {"name": "glob"}}),
            ),
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
        ];
        let fm_count = frontmatter_lines(&messages[0], &theme).len();
        assert!(fm_count > 0, "parent should have frontmatter");

        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        // Find the separator
        let sep_idx = flat.iter().position(|fl| {
            matches!(fl, FlatLine::ScopeChild { inner, .. } if matches!(inner.as_ref(), FlatLine::Separator))
        });
        assert!(sep_idx.is_some(), "should have a separator: {flat:?}");
        let sep_idx = sep_idx.expect("checked above");

        // Line before separator should be a Detail
        assert!(
            matches!(
                &flat[sep_idx - 1],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::Detail { .. })
            ),
            "line before separator should be Detail: {:?}",
            flat[sep_idx - 1]
        );
        // Line after separator should be a MessageHeader (first child)
        assert!(
            matches!(
                &flat[sep_idx + 1],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::MessageHeader { .. })
            ),
            "line after separator should be MessageHeader: {:?}",
            flat[sep_idx + 1]
        );
    }

    // ── Run collapse at depth tests ─────────────────────────────────────

    fn make_progress_with_parent(id: i64, server: &str, parent_id: i64) -> SessionMessage {
        test_support::message_with_ids_payload(
            id,
            "lsp",
            "$/progress",
            server,
            None,
            Some(parent_id),
            serde_json::json!({"token": "ra/indexing"}),
        )
    }

    #[test]
    fn test_run_collapse_at_depth_progress() {
        // Scope with a non-collapsing child + 10 same-token progress children.
        // Expand scope. Progress children collapse into a single CollapsedHeader.
        let theme = test_theme();
        let icons = test_icons();
        let mut messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            // Non-collapsing child (hook → collapse_key = None).
            make_message_with_id_parent(2, "hook", "PreToolUse", "catenary", None, Some(500)),
        ];
        for i in 0..10 {
            messages.push(make_progress_with_parent(10 + i, "rust-analyzer", 500));
        }
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Expansion key = parent index = 0.
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        // ScopeHeader + hook child + 1 CollapsedHeader (10 progress tokens).
        assert_eq!(
            flat.len(),
            3,
            "10 same-key progress should collapse: {flat:?}"
        );
        assert!(
            matches!(flat[0], FlatLine::ScopeHeader { .. }),
            "first should be ScopeHeader: {:?}",
            flat[0]
        );
        assert!(
            matches!(
                &flat[2],
                FlatLine::ScopeChild {
                    depth: 1,
                    inner,
                    ..
                } if matches!(inner.as_ref(), FlatLine::CollapsedHeader { count: 10, .. })
            ),
            "third should be ScopeChild(CollapsedHeader(10)): {:?}",
            flat[2]
        );
    }

    #[test]
    fn test_run_collapse_at_depth_mixed() {
        // Scope: adjacent req/resp pair, 5 progress tokens, another pair.
        // Expand scope. Verify: Paired, Collapsed(5), Paired.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            // Scope parent
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            // Adjacent pair 1
            make_message_with_id_parent(
                2,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(601),
                Some(500),
            ),
            make_message_with_id_parent(
                3,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(601),
                Some(500),
            ),
            // 5 progress tokens
            make_progress_with_parent(4, "rust-analyzer", 500),
            make_progress_with_parent(5, "rust-analyzer", 500),
            make_progress_with_parent(6, "rust-analyzer", 500),
            make_progress_with_parent(7, "rust-analyzer", 500),
            make_progress_with_parent(8, "rust-analyzer", 500),
            // Adjacent pair 2
            make_message_with_id_parent(
                9,
                "lsp",
                "textDocument/definition",
                "rust-analyzer",
                Some(602),
                Some(500),
            ),
            make_message_with_id_parent(
                10,
                "lsp",
                "textDocument/definition",
                "rust-analyzer",
                Some(602),
                Some(500),
            ),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Expansion key = parent index.
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        // ScopeHeader + Paired + Collapsed(5) + Paired = 4 lines.
        assert_eq!(
            flat.len(),
            4,
            "expected header + pair + collapsed + pair: {flat:?}"
        );
        // Line 1: Paired (hover)
        assert!(
            matches!(
                &flat[1],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::MessageHeader { paired_response: Some(_), .. })
            ),
            "line 1 should be paired: {:?}",
            flat[1]
        );
        // Line 2: Collapsed(5 progress)
        assert!(
            matches!(
                &flat[2],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::CollapsedHeader { count: 5, .. })
            ),
            "line 2 should be collapsed(5): {:?}",
            flat[2]
        );
        // Line 3: Paired (definition)
        assert!(
            matches!(
                &flat[3],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::MessageHeader { paired_response: Some(_), .. })
            ),
            "line 3 should be paired: {:?}",
            flat[3]
        );
    }

    #[test]
    fn test_pair_merge_at_depth() {
        // Scope with 2 adjacent messages forming a req/resp pair.
        // Expand scope. Children show 1 paired line.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_id_parent(
                2,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(601),
                Some(500),
            ),
            make_message_with_id_parent(
                3,
                "lsp",
                "textDocument/hover",
                "rust-analyzer",
                Some(601),
                Some(500),
            ),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        // ScopeHeader + 1 Paired child.
        assert_eq!(flat.len(), 2, "expected header + 1 paired child: {flat:?}");
        assert!(
            matches!(
                &flat[1],
                FlatLine::ScopeChild {
                    depth: 1,
                    inner,
                    ..
                } if matches!(
                    inner.as_ref(),
                    FlatLine::MessageHeader { message_index: 1, paired_response: Some(2) }
                )
            ),
            "child should be paired(1, 2): {:?}",
            flat[1]
        );
    }

    #[test]
    fn test_nested_collapse_expansion() {
        // Scope with a non-collapsing child + 10 same-key children → collapsed
        // run. Expand scope, then expand the collapsed run. Individual messages
        // appear at same depth, replacing the collapsed header.
        let theme = test_theme();
        let icons = test_icons();
        let mut messages = vec![
            make_message_with_id_parent(1, "mcp", "tools/call", "catenary", Some(500), None),
            make_message_with_id_parent(2, "hook", "PreToolUse", "catenary", None, Some(500)),
        ];
        for i in 0..10 {
            messages.push(make_progress_with_parent(10 + i, "rust-analyzer", 500));
        }
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        // Expand scope (parent index = 0).
        panel.expanded.insert(0);
        let flat = panel.flat_lines();
        assert_eq!(flat.len(), 3, "scope + hook child + collapsed run");

        // Expand the collapsed run.
        let collapsed_start = match &flat[2] {
            FlatLine::ScopeChild { inner, .. } => match inner.as_ref() {
                FlatLine::CollapsedHeader { start_index, .. } => *start_index,
                other => panic!("expected CollapsedHeader, got {other:?}"),
            },
            other => panic!("expected ScopeChild, got {other:?}"),
        };
        panel.expanded.insert(collapsed_start);

        let flat = panel.flat_lines();
        // Count MessageHeader children — should be 11 (hook + 10 progress).
        let msg_header_count = flat
            .iter()
            .filter(|fl| {
                matches!(
                    fl,
                    FlatLine::ScopeChild { depth: 1, inner, .. }
                        if matches!(inner.as_ref(), FlatLine::MessageHeader { .. })
                )
            })
            .count();
        assert_eq!(
            msg_header_count, 11,
            "expanded run should show hook child + 10 individual messages: {flat:?}"
        );
        // No CollapsedHeader remains.
        let has_collapsed = flat.iter().any(|fl| {
            matches!(
                fl,
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::CollapsedHeader { .. })
            )
        });
        assert!(
            !has_collapsed,
            "expanded run should have no CollapsedHeader: {flat:?}"
        );
    }

    #[test]
    fn test_filter_at_depth() {
        // Scope with children from 3 servers (interleaved). Filter on
        // one server. Expand scope. Non-matching children hidden,
        // remaining same-server entries run-collapsed.
        //
        // Tool name includes "rust-analyzer" so the scope header
        // passes the root-level filter too.
        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            make_message_with_id_parent_payload(
                1,
                "mcp",
                "tools/call",
                "catenary",
                Some(500),
                None,
                serde_json::json!({"params": {"name": "search-rust-analyzer"}}),
            ),
            // Interleaved: ra, taplo, ts, ra, taplo, ts
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(3, "lsp", "workspace/symbol", "taplo", None, Some(500)),
            make_message_with_id_parent(4, "lsp", "workspace/symbol", "ts-server", None, Some(500)),
            make_message_with_id_parent(
                5,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
            make_message_with_id_parent(6, "lsp", "workspace/symbol", "taplo", None, Some(500)),
            make_message_with_id_parent(7, "lsp", "workspace/symbol", "ts-server", None, Some(500)),
        ];
        let mut panel = PanelState::new("test".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.filter_pattern = Some("rust-analyzer".to_string());
        panel.expanded.insert(0);

        let flat = panel.flat_lines();
        // Should contain exactly one CollapsedHeader with count 2
        // (the two rust-analyzer entries). Other lines are frontmatter.
        let collapsed: Vec<_> = flat
            .iter()
            .filter(|fl| {
                matches!(
                    fl,
                    FlatLine::ScopeChild { inner, .. }
                        if matches!(inner.as_ref(), FlatLine::CollapsedHeader { .. })
                )
            })
            .collect();
        assert_eq!(
            collapsed.len(),
            1,
            "should have exactly 1 collapsed run: {flat:?}"
        );
        assert!(
            matches!(
                collapsed[0],
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::CollapsedHeader { count: 2, .. })
            ),
            "should be collapsed(2): {:?}",
            collapsed[0]
        );
        // No individual MessageHeader children (all filtered non-ra entries
        // removed, remaining ra entries collapsed).
        let has_msg_headers = flat.iter().any(|fl| {
            matches!(
                fl,
                FlatLine::ScopeChild { inner, .. }
                    if matches!(inner.as_ref(), FlatLine::MessageHeader { .. })
            )
        });
        assert!(
            !has_msg_headers,
            "no individual MessageHeaders expected: {flat:?}"
        );
    }

    #[test]
    fn test_depth_indentation() {
        // Scope with children. Expand scope. Render panel. Verify
        // depth-1 children render with 4-space indent in the output.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let theme = test_theme();
        let icons = test_icons();
        let messages = vec![
            {
                let mut m = make_message_with_id_parent(
                    1,
                    "mcp",
                    "tools/call",
                    "catenary",
                    Some(500),
                    None,
                );
                m.payload = serde_json::json!({"params": {"name": "grep"}});
                m
            },
            make_message_with_id_parent(
                2,
                "lsp",
                "workspace/symbol",
                "rust-analyzer",
                None,
                Some(500),
            ),
        ];
        let mut panel = PanelState::new("test1234".to_string(), &theme, &icons);
        panel.load_messages(messages);
        panel.expanded.insert(0);

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal creation");
        terminal
            .draw(|f| {
                let area = f.area();
                crate::tui::panel::render_panel(&panel, area, f.buffer_mut(), true, true);
            })
            .expect("draw");

        let buf = terminal.backend().buffer().clone();
        // Find the scope header row (contains "children") and the child
        // row (contains "workspace/symbol"). The child should be indented
        // 4+ spaces beyond the header.
        let mut header_indent = None;
        let mut child_indent = None;
        for y in 0..buf.area.height {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            if header_indent.is_none() && row.contains("child") {
                let leading = row.len() - row.trim_start().len();
                header_indent = Some(leading);
            } else if child_indent.is_none() && row.contains("workspace/symbol") {
                let leading = row.len() - row.trim_start().len();
                child_indent = Some(leading);
            }
        }
        let header_indent = header_indent.expect("should find scope header row");
        let child_indent = child_indent.expect("should find child row");
        assert!(
            child_indent >= header_indent + 4,
            "child should be indented 4+ spaces beyond header: header={header_indent}, child={child_indent}"
        );
    }
}
