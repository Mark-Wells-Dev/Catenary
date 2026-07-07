// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Guided-mutation UX: the consent overlay state and the pending-restart tracker
//! (tui-rework 05).
//!
//! A fix-it becomes an action here. The cursored finding or entity yields a
//! [`Mutation`]; the user reviews exactly what will be written — key, value, and
//! target file — in a modal overlay and confirms with Enter or declines with the
//! escape key. No write is ever silent. Because a config change needs a daemon
//! restart to take effect, an applied mutation leaves a [`PendingRestart`] marker
//! that clears only when the snapshot shows the daemon has returned under a new
//! identity (a fresh instance id or start time).

use std::path::PathBuf;

use crate::config::{BindingSpec, ConfigLayer, LanguageConfig, Mutation};
use crate::state_snapshot::Snapshot;

/// A change written to disk but not yet live.
///
/// Config changes require a daemon restart today (a reload path is a separate
/// engine ticket), so the affected finding stays honestly marked "pending
/// restart" until the daemon comes back on the new config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRestart {
    /// One-line description of the applied change (`key → value @ layer`).
    pub summary: String,
    /// The daemon instance id observed at apply time.
    daemon_id: String,
    /// The daemon start time observed at apply time.
    started_at: String,
}

impl PendingRestart {
    /// Record a pending restart against the daemon identity in `snapshot`.
    #[must_use]
    pub fn new(summary: String, snapshot: &Snapshot) -> Self {
        Self {
            summary,
            daemon_id: snapshot.daemon.instance_id.clone(),
            started_at: snapshot.daemon.started_at.clone(),
        }
    }

    /// Whether the daemon in `snapshot` has restarted since this marker was
    /// recorded — a new instance id or start time means the change is now live,
    /// so the marker should clear. A daemon that is down (no snapshot yet) keeps
    /// the marker: it may be mid-restart.
    #[must_use]
    pub fn cleared_by(&self, snapshot: &Snapshot) -> bool {
        let d = &snapshot.daemon;
        if d.generated_at.is_empty() {
            return false;
        }
        (d.instance_id.as_str(), d.started_at.as_str())
            != (self.daemon_id.as_str(), self.started_at.as_str())
    }
}

/// The modal consent state for a pending guided mutation.
///
/// Holds the base [`Mutation`], the structural layer candidates it may target,
/// the chosen layer, and — for a mutation that takes a value (a binary path) —
/// the editable buffer. Nothing is written until [`effective_mutation`] is
/// applied on confirm.
///
/// [`effective_mutation`]: Self::effective_mutation
pub struct ActionState {
    /// The base mutation; an editable value, if any, overrides it on apply.
    mutation: Mutation,
    /// The layers the mutation may target (structural; from the mutation).
    candidates: Vec<ConfigLayer>,
    /// Index into [`Self::candidates`] of the chosen layer.
    layer_idx: usize,
    /// An editable single value (a binary path), when the mutation takes one.
    value: Option<String>,
    /// The last apply error, shown in the overlay until dismissed.
    pub error: Option<String>,
}

impl ActionState {
    /// Build a consent state for `mutation`, offering `candidates` and starting
    /// the layer choice at `default_layer` (clamped). `value` seeds the editable
    /// field for mutations that take one (`None` for the rest).
    #[must_use]
    pub fn new(
        mutation: Mutation,
        candidates: Vec<ConfigLayer>,
        default_layer: usize,
        value: Option<String>,
    ) -> Self {
        let layer_idx = if candidates.is_empty() {
            0
        } else {
            default_layer.min(candidates.len() - 1)
        };
        Self {
            mutation,
            candidates,
            layer_idx,
            value,
            error: None,
        }
    }

    /// The base mutation (before any value edit).
    #[must_use]
    pub const fn mutation(&self) -> &Mutation {
        &self.mutation
    }

    /// The currently chosen target layer, if any candidate exists.
    #[must_use]
    pub fn current_layer(&self) -> Option<&ConfigLayer> {
        self.candidates.get(self.layer_idx)
    }

    /// The number of candidate layers (≥ 2 ⇒ a layer choice is offered).
    #[must_use]
    pub const fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    /// The editable value buffer, when this mutation takes one.
    #[must_use]
    pub fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }

    /// Whether this mutation edits a free value (a binary path).
    #[must_use]
    pub const fn takes_value(&self) -> bool {
        self.value.is_some()
    }

    /// Cycle to the next candidate layer (wraps).
    pub const fn cycle_layer(&mut self) {
        if self.candidates.len() > 1 {
            self.layer_idx = (self.layer_idx + 1) % self.candidates.len();
        }
    }

    /// Append a character to the editable value (no-op when there is none).
    pub fn push_char(&mut self, c: char) {
        if let Some(v) = self.value.as_mut() {
            v.push(c);
        }
    }

    /// Delete the last character of the editable value (no-op when empty).
    pub fn backspace(&mut self) {
        if let Some(v) = self.value.as_mut() {
            v.pop();
        }
    }

    /// The mutation as it will actually be written — the base mutation with the
    /// editable value substituted in (for a binary-path edit).
    #[must_use]
    pub fn effective_mutation(&self) -> Mutation {
        match (&self.mutation, &self.value) {
            (Mutation::SetServerCommand { server, .. }, Some(command)) => {
                Mutation::SetServerCommand {
                    server: server.clone(),
                    command: command.clone(),
                }
            }
            (m, _) => m.clone(),
        }
    }

    /// The one-line preview value (the edited buffer for a value mutation, else
    /// the mutation's own rendered value).
    #[must_use]
    pub fn preview_value(&self) -> String {
        self.value
            .as_ref()
            .map_or_else(|| self.mutation.value_label(), |v| format!("\"{v}\""))
    }
}

/// The binding list for a language, in the shape [`Mutation::SetServerEnabled`]
/// writes — every sibling preserved so the enable/disable override never drops
/// one.
#[must_use]
pub fn binding_specs(lang: &LanguageConfig) -> Vec<BindingSpec> {
    lang.servers()
        .iter()
        .map(|b| BindingSpec {
            name: b.name.clone(),
            diagnostics: b.diagnostics,
            disabled_methods: b
                .disabled_methods
                .iter()
                .map(|m| m.as_str().to_string())
                .collect(),
        })
        .collect()
}

/// The first user config source carrying pre-namespacing tables a migration can
/// rewrite, or `None` when nothing needs migrating.
#[must_use]
pub fn find_migration_source() -> Option<PathBuf> {
    for src in crate::config::config_sources() {
        if let Ok(contents) = std::fs::read_to_string(&src)
            && let Ok(raw) = toml::from_str::<toml::Value>(&contents)
            && has_legacy_tables(&raw)
        {
            return Some(src);
        }
    }
    None
}

/// Whether `raw` carries a pre-namespacing top-level `[server]`/`[language]`
/// table or a legacy `[linter.<name>]` definition (the migration's inputs).
fn has_legacy_tables(raw: &toml::Value) -> bool {
    if raw.get("server").and_then(toml::Value::as_table).is_some()
        || raw
            .get("language")
            .and_then(toml::Value::as_table)
            .is_some()
    {
        return true;
    }
    raw.get("linter")
        .and_then(toml::Value::as_table)
        .is_some_and(|t| {
            t.iter()
                .any(|(k, v)| k != "rule" && k != "disable" && v.is_table())
        })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use super::*;
    use crate::state_snapshot::DaemonSnapshot;

    fn snap(instance: &str, started: &str) -> Snapshot {
        Snapshot {
            daemon: DaemonSnapshot {
                instance_id: instance.to_string(),
                started_at: started.to_string(),
                generated_at: crate::state_snapshot::now_iso(),
                ..DaemonSnapshot::default()
            },
            ..Snapshot::default()
        }
    }

    #[test]
    fn pending_restart_clears_only_on_new_daemon_identity() {
        let before = snap("daemon:aaa", "2026-07-07T10:00:00Z");
        let pending = PendingRestart::new("set command".to_string(), &before);
        // Same daemon → still pending.
        assert!(!pending.cleared_by(&before));
        // Restart (new instance id + start time) → cleared.
        let after = snap("daemon:bbb", "2026-07-07T10:05:00Z");
        assert!(pending.cleared_by(&after));
    }

    #[test]
    fn pending_restart_survives_daemon_down() {
        let before = snap("daemon:aaa", "2026-07-07T10:00:00Z");
        let pending = PendingRestart::new("set command".to_string(), &before);
        let down = Snapshot::default(); // no generated_at
        assert!(!pending.cleared_by(&down), "a down daemon keeps the marker");
    }

    #[test]
    fn effective_mutation_substitutes_edited_value() {
        let base = Mutation::SetServerCommand {
            server: "gopls".to_string(),
            command: "gopls".to_string(),
        };
        let state = ActionState::new(
            base,
            vec![ConfigLayer::User],
            0,
            Some("/opt/gopls".to_string()),
        );
        let Mutation::SetServerCommand { command, .. } = state.effective_mutation() else {
            panic!("expected a command mutation")
        };
        assert_eq!(command, "/opt/gopls");
    }

    #[test]
    fn value_edit_push_and_backspace() {
        let base = Mutation::SetServerCommand {
            server: "x".to_string(),
            command: String::new(),
        };
        let mut state = ActionState::new(base, vec![ConfigLayer::User], 0, Some(String::new()));
        state.push_char('a');
        state.push_char('b');
        state.backspace();
        assert_eq!(state.value(), Some("a"));
    }

    #[test]
    fn cycle_layer_wraps_over_candidates() {
        let base = Mutation::SetLinterDisabled {
            rule: "shellcheck".to_string(),
            disabled: true,
        };
        let root = PathBuf::from("/p/root");
        let mut state = ActionState::new(
            base,
            vec![ConfigLayer::User, ConfigLayer::Project(root)],
            0,
            None,
        );
        assert!(matches!(state.current_layer(), Some(ConfigLayer::User)));
        state.cycle_layer();
        assert!(matches!(
            state.current_layer(),
            Some(ConfigLayer::Project(_))
        ));
        state.cycle_layer();
        assert!(matches!(state.current_layer(), Some(ConfigLayer::User)));
    }
}
