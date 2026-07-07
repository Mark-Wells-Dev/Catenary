// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grid application state: the snapshot + config it reads, the findings it
//! derives, the four panes' cursors, and the collapse/filter toggles.
//!
//! The App is a **pure `state.json` + config-file reader** (DESIGN non-goal: no
//! second data path, never the firehose). Findings recompute only when the
//! snapshot content changes; the per-tick reload re-reads the small snapshot
//! and re-clamps cursors, so a growing time-in-state surfaces without a rebuild.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;

use crate::config::Config;
use crate::state_snapshot::Snapshot;

use super::data::DataSource;
use super::findings::{self, OwnedFinding, Owner};
use super::icons::IconSet;
use super::model::{self, EntityKey, Pane, ProblemRow, Row, Verdict};
use super::theme::Theme;

/// Per-pane cursor + scroll state.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    /// Selected row index (into the pane's row list).
    pub index: usize,
    /// First visible row index.
    pub scroll: usize,
    /// Rows visible in the pane body (set at render time).
    pub visible: usize,
}

impl Cursor {
    /// Clamp the cursor into `[0, len)` and keep it in view.
    pub fn settle(&mut self, len: usize) {
        if len == 0 {
            self.index = 0;
            self.scroll = 0;
            return;
        }
        if self.index >= len {
            self.index = len - 1;
        }
        let visible = self.visible.max(1);
        if self.index < self.scroll {
            self.scroll = self.index;
        } else if self.index >= self.scroll + visible {
            self.scroll = self.index + 1 - visible;
        }
    }
}

/// The grid application.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent UI toggles (problems-only, dormant, keybinds, quit, rebuild)"
)]
pub struct App<'a> {
    /// Terminal theme (borrowed for the app's lifetime).
    pub theme: &'a Theme,
    /// Icon set (borrowed).
    pub icons: &'a IconSet,
    /// The snapshot data source.
    data: Box<dyn DataSource>,
    /// Project root for project-scoped checks.
    project_root: PathBuf,

    /// The latest snapshot.
    pub snapshot: Snapshot,
    /// The resolved user config, or `None` if it failed to load.
    pub config: Option<Config>,
    /// The config-load error message, when [`Self::config`] is `None`.
    pub config_error: Option<String>,
    /// All findings from the current snapshot + config, with owners.
    pub findings: Vec<OwnedFinding>,
    /// The current verdict.
    pub verdict: Verdict,

    /// Root/server tree rows.
    pub root_rows: Vec<Row>,
    /// Client/session tree rows.
    pub session_rows: Vec<Row>,
    /// Problems-pane rows.
    pub problem_rows: Vec<ProblemRow>,

    /// Focused pane.
    pub focus: Pane,
    /// The most recently focused tree (drives the detail pane).
    pub last_tree: Pane,
    /// Root-tree cursor.
    pub root_cursor: Cursor,
    /// Session-tree cursor.
    pub session_cursor: Cursor,
    /// Problems-pane cursor.
    pub problem_cursor: Cursor,

    /// Expanded roots (empty → all collapsed; density law).
    expanded_roots: HashSet<String>,
    /// Collapsed clients (empty → all expanded).
    collapsed_clients: HashSet<String>,
    /// Whether the dormant-inventory tail is expanded.
    dormant_expanded: bool,
    /// Problems-only filter: collapse both trees to broken things.
    pub problems_only: bool,

    /// Whether the keybinding hint is expanded.
    pub keybinds_expanded: bool,
    /// Quit flag.
    pub quit: bool,

    /// Cached filesystem-detected languages, keyed by the root-path set.
    lang_cache: HashSet<String>,
    lang_cache_key: Vec<String>,
    /// The `generated_at` the findings were built from.
    last_generated_at: String,
    /// Set by a toggle/filter change to force a row rebuild next reload.
    needs_rebuild: bool,
    /// Whether to gather the environment-reading findings (config files,
    /// install health) alongside the hermetic snapshot seam. Always `true` in
    /// production; `false` under the fleet-scale tests, which inject a config
    /// and drive [`findings::gather_snapshot`] for a deterministic finding set.
    env_findings: bool,
}

impl<'a> App<'a> {
    /// Build the app and load the first snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial snapshot read fails.
    pub fn new(
        theme: &'a Theme,
        icons: &'a IconSet,
        project_root: PathBuf,
        data: Box<dyn DataSource>,
    ) -> Result<Self> {
        let (config, config_error) = load_config();
        Self::build(theme, icons, project_root, data, config, config_error, true)
    }

    /// Build the app from parts, with the config already resolved and a flag
    /// selecting the full-vs-hermetic finding gather.
    fn build(
        theme: &'a Theme,
        icons: &'a IconSet,
        project_root: PathBuf,
        data: Box<dyn DataSource>,
        config: Option<Config>,
        config_error: Option<String>,
        env_findings: bool,
    ) -> Result<Self> {
        let snapshot = data.load()?;
        let mut app = Self {
            theme,
            icons,
            data,
            project_root,
            snapshot,
            config,
            config_error,
            findings: Vec::new(),
            verdict: Verdict::default(),
            root_rows: Vec::new(),
            session_rows: Vec::new(),
            problem_rows: Vec::new(),
            focus: Pane::RootTree,
            last_tree: Pane::RootTree,
            root_cursor: Cursor::default(),
            session_cursor: Cursor::default(),
            problem_cursor: Cursor::default(),
            expanded_roots: HashSet::new(),
            collapsed_clients: HashSet::new(),
            dormant_expanded: false,
            problems_only: false,
            keybinds_expanded: false,
            quit: false,
            lang_cache: HashSet::new(),
            lang_cache_key: Vec::new(),
            last_generated_at: String::new(),
            needs_rebuild: true,
            env_findings,
        };
        app.recompute_findings();
        app.rebuild_rows();
        app.last_generated_at = app.snapshot.daemon.generated_at.clone();
        Ok(app)
    }

    /// Build the app with an injected config and the hermetic snapshot seam
    /// ([`findings::gather_snapshot`]) — no config-file or install-health reads.
    /// The seam the fleet-scale render tests drive for a deterministic finding
    /// set independent of the machine's real config or install state.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial snapshot read fails.
    #[cfg(test)]
    pub fn with_injected_config(
        theme: &'a Theme,
        icons: &'a IconSet,
        project_root: PathBuf,
        config: Config,
        data: Box<dyn DataSource>,
    ) -> Result<Self> {
        Self::build(theme, icons, project_root, data, Some(config), None, false)
    }

    /// Whether the daemon is present (a snapshot has been generated).
    #[must_use]
    pub const fn daemon_present(&self) -> bool {
        !self.snapshot.daemon.generated_at.is_empty()
    }

    /// Re-read the snapshot; recompute findings + rows only when the snapshot
    /// content changed or a toggle marked the model dirty.
    pub fn reload(&mut self) {
        if let Ok(snap) = self.data.load() {
            self.snapshot = snap;
        }
        let changed = self.snapshot.daemon.generated_at != self.last_generated_at;
        if changed {
            if self.env_findings {
                let (config, config_error) = load_config();
                self.config = config;
                self.config_error = config_error;
            }
            self.recompute_findings();
            self.needs_rebuild = true;
            self.last_generated_at = self.snapshot.daemon.generated_at.clone();
        }
        if self.needs_rebuild {
            self.rebuild_rows();
            self.needs_rebuild = false;
        }
    }

    /// Recompute the active-language set and all findings from the current
    /// snapshot + config.
    fn recompute_findings(&mut self) {
        let active = self.active_languages();
        let env = self.env_findings;
        let snapshot = &self.snapshot;
        let project_root = &self.project_root;
        self.findings = self.config.as_ref().map_or_else(Vec::new, |cfg| {
            if env {
                findings::gather(snapshot, cfg, project_root, active)
            } else {
                findings::gather_snapshot(snapshot, cfg, active)
            }
        });
        self.verdict = model::verdict(&self.findings);
    }

    /// Active workspace languages: the cheap live-instance set unioned with a
    /// cached filesystem detection (recomputed only when the root set changes).
    fn active_languages(&mut self) -> HashSet<String> {
        let mut key: Vec<String> = self.snapshot.roots.iter().map(|r| r.path.clone()).collect();
        key.sort();
        if key != self.lang_cache_key {
            self.lang_cache = self.detect_fs_languages();
            self.lang_cache_key = key;
        }
        let mut active = findings::live_languages(&self.snapshot);
        active.extend(self.lang_cache.iter().cloned());
        active
    }

    /// Detect languages from files under the tracked roots (best-effort; empty
    /// when config is absent or no root exists on disk).
    fn detect_fs_languages(&self) -> HashSet<String> {
        let Some(cfg) = &self.config else {
            return HashSet::new();
        };
        let roots: Vec<PathBuf> = self
            .snapshot
            .roots
            .iter()
            .map(|r| PathBuf::from(&r.path))
            .filter(|p| p.exists())
            .collect();
        if roots.is_empty() {
            return HashSet::new();
        }
        let configured: HashSet<&str> = cfg.language.keys().map(String::as_str).collect();
        let manager = crate::bridge::filesystem_manager::FilesystemManager::with_classification(
            crate::bridge::filesystem_manager::ClassificationTables::from_config(cfg),
        );
        manager.detect_workspace_languages(&roots, &configured)
    }

    /// Rebuild the three pane row lists from the current snapshot + findings +
    /// toggle state.
    fn rebuild_rows(&mut self) {
        let (er, dc) = self.effective_collapse();
        self.root_rows = model::build_root_tree(
            &self.snapshot,
            &self.findings,
            self.config.as_ref(),
            &er,
            self.dormant_expanded,
        );
        self.session_rows = model::build_session_tree(&self.snapshot, &self.findings, &dc);
        if self.problems_only {
            // Keep only the broken things: a server/client is a problem iff it
            // owns a problem finding, not merely because it is a server row.
            let problem_servers = self.problem_owners(|o| match o {
                Owner::Server(name) => Some(name.clone()),
                _ => None,
            });
            let problem_clients = self.problem_owners(|o| match o {
                Owner::Client(name) => Some(name.clone()),
                _ => None,
            });
            self.root_rows
                .retain(|r| root_row_is_problem(r, &problem_servers));
            self.session_rows
                .retain(|r| session_row_is_problem(r, &problem_clients));
        }
        self.problem_rows = model::build_problems(&self.findings);
        self.clamp_cursors();
    }

    /// The set of owner names carrying a problem finding, projected by `pick`.
    fn problem_owners(&self, pick: impl Fn(&Owner) -> Option<String>) -> HashSet<String> {
        self.findings
            .iter()
            .filter(|f| f.finding.is_problem())
            .filter_map(|f| pick(&f.owner))
            .collect()
    }

    /// The expanded-roots / collapsed-clients sets in effect, honoring the
    /// problems-only filter (which force-expands every root so its problems
    /// show).
    fn effective_collapse(&self) -> (HashSet<String>, HashSet<String>) {
        if self.problems_only {
            let all_roots: HashSet<String> = self
                .snapshot
                .servers
                .iter()
                .map(|s| {
                    if s.scope_root.is_empty() {
                        "(single file)".to_string()
                    } else {
                        s.scope_root.clone()
                    }
                })
                .chain(self.snapshot.roots.iter().map(|r| r.path.clone()))
                .collect();
            (all_roots, HashSet::new())
        } else {
            (self.expanded_roots.clone(), self.collapsed_clients.clone())
        }
    }

    fn clamp_cursors(&mut self) {
        self.root_cursor.settle(self.root_rows.len());
        self.session_cursor.settle(self.session_rows.len());
        self.problem_cursor.settle(self.problem_rows.len());
    }

    /// Row count of the focused list pane.
    #[must_use]
    pub const fn focused_len(&self) -> usize {
        match self.focus {
            Pane::RootTree => self.root_rows.len(),
            Pane::SessionTree => self.session_rows.len(),
            Pane::Problems => self.problem_rows.len(),
            Pane::Detail => 0,
        }
    }

    const fn focused_cursor(&mut self) -> Option<&mut Cursor> {
        match self.focus {
            Pane::RootTree => Some(&mut self.root_cursor),
            Pane::SessionTree => Some(&mut self.session_cursor),
            Pane::Problems => Some(&mut self.problem_cursor),
            Pane::Detail => None,
        }
    }

    /// Move focus to the next pane (Tab).
    pub const fn cycle_focus(&mut self) {
        self.focus = self.focus.next();
        self.track_tree();
    }

    /// Move focus to the previous pane (Shift-Tab).
    pub const fn cycle_focus_back(&mut self) {
        self.focus = self.focus.prev();
        self.track_tree();
    }

    /// Focus a specific pane (mouse click).
    pub const fn set_focus(&mut self, pane: Pane) {
        self.focus = pane;
        self.track_tree();
    }

    const fn track_tree(&mut self) {
        if matches!(self.focus, Pane::RootTree | Pane::SessionTree) {
            self.last_tree = self.focus;
        }
    }

    /// Move the cursor down `n` selectable rows in the focused pane.
    pub fn cursor_down(&mut self, n: usize) {
        for _ in 0..n {
            self.step(true);
        }
    }

    /// Move the cursor up `n` selectable rows in the focused pane.
    pub fn cursor_up(&mut self, n: usize) {
        for _ in 0..n {
            self.step(false);
        }
    }

    /// One selectable-row step in the focused pane (trees skip inline-finding
    /// rows; problems rows are all selectable).
    fn step(&mut self, down: bool) {
        let selectable = self.selectable_indices();
        if selectable.is_empty() {
            return;
        }
        let cur = match self.focus {
            Pane::RootTree => self.root_cursor.index,
            Pane::SessionTree => self.session_cursor.index,
            Pane::Problems => self.problem_cursor.index,
            Pane::Detail => return,
        };
        // Position of the current (or next-following) selectable index.
        let at = selectable
            .iter()
            .position(|&i| i == cur)
            .or_else(|| selectable.iter().position(|&i| i > cur))
            .unwrap_or(0);
        let next = if down {
            (at + 1).min(selectable.len() - 1)
        } else {
            at.saturating_sub(1)
        };
        let target = selectable[next];
        let len = self.focused_len();
        if let Some(c) = self.focused_cursor() {
            c.index = target;
            c.settle(len);
        }
    }

    /// The selectable row indices of the focused pane.
    fn selectable_indices(&self) -> Vec<usize> {
        match self.focus {
            Pane::RootTree => selectable(&self.root_rows),
            Pane::SessionTree => selectable(&self.session_rows),
            Pane::Problems => (0..self.problem_rows.len()).collect(),
            Pane::Detail => Vec::new(),
        }
    }

    /// Jump to the first selectable row.
    pub fn jump_home(&mut self) {
        let first = self.selectable_indices().first().copied().unwrap_or(0);
        let len = self.focused_len();
        if let Some(c) = self.focused_cursor() {
            c.index = first;
            c.settle(len);
        }
    }

    /// Jump to the last selectable row.
    pub fn jump_end(&mut self) {
        let last = self.selectable_indices().last().copied().unwrap_or(0);
        let len = self.focused_len();
        if let Some(c) = self.focused_cursor() {
            c.index = last;
            c.settle(len);
        }
    }

    /// Page down by the visible height.
    pub fn page_down(&mut self) {
        let step = self.focused_cursor().map_or(1, |c| c.visible.max(1));
        self.cursor_down(step);
    }

    /// Page up by the visible height.
    pub fn page_up(&mut self) {
        let step = self.focused_cursor().map_or(1, |c| c.visible.max(1));
        self.cursor_up(step);
    }

    /// The entity the detail pane renders for — the cursored node of the most
    /// recently focused tree.
    #[must_use]
    pub fn detail_entity(&self) -> Option<EntityKey> {
        let (rows, cursor) = match self.last_tree {
            Pane::SessionTree => (&self.session_rows, &self.session_cursor),
            _ => (&self.root_rows, &self.root_cursor),
        };
        rows.get(cursor.index).and_then(Row::entity)
    }

    /// `Enter`/click activation on the focused list row: toggle a collapse, or
    /// (in the problems pane) focus the owner.
    pub fn activate(&mut self) {
        match self.focus {
            Pane::RootTree => {
                if let Some(t) = self
                    .root_rows
                    .get(self.root_cursor.index)
                    .and_then(Row::toggle)
                {
                    self.apply_toggle(&t);
                }
            }
            Pane::SessionTree => {
                if let Some(t) = self
                    .session_rows
                    .get(self.session_cursor.index)
                    .and_then(Row::toggle)
                {
                    self.apply_toggle(&t);
                }
            }
            Pane::Problems => {
                if let Some(owner) = self
                    .problem_rows
                    .get(self.problem_cursor.index)
                    .map(|r| r.owner.clone())
                {
                    self.focus_owner(&owner);
                }
            }
            Pane::Detail => {}
        }
    }

    fn apply_toggle(&mut self, toggle: &model::Toggle) {
        match toggle {
            model::Toggle::Root(path) => {
                if !self.expanded_roots.remove(path) {
                    self.expanded_roots.insert(path.clone());
                }
            }
            model::Toggle::Client(name) => {
                if !self.collapsed_clients.remove(name) {
                    self.collapsed_clients.insert(name.clone());
                }
            }
            model::Toggle::Dormant => self.dormant_expanded = !self.dormant_expanded,
        }
        self.rebuild_rows();
    }

    /// Focus the board on a finding's owner: jump the appropriate tree's cursor
    /// (expanding as needed) to the owning node.
    pub fn focus_owner(&mut self, owner: &Owner) {
        match owner {
            Owner::Server(name) => {
                for s in &self.snapshot.servers {
                    if &s.server == name {
                        let path = if s.scope_root.is_empty() {
                            "(single file)".to_string()
                        } else {
                            s.scope_root.clone()
                        };
                        self.expanded_roots.insert(path);
                    }
                }
                self.rebuild_rows();
                if let Some(i) = self
                    .root_rows
                    .iter()
                    .position(|r| matches!(r, Row::Server(s) if &s.server == name))
                {
                    self.root_cursor.index = i;
                    self.root_cursor.settle(self.root_rows.len());
                }
                self.focus = Pane::RootTree;
                self.last_tree = Pane::RootTree;
            }
            Owner::Client(name) => {
                self.collapsed_clients.remove(name);
                self.rebuild_rows();
                if let Some(i) = self
                    .session_rows
                    .iter()
                    .position(|r| matches!(r, Row::Client(c) if &c.name == name))
                {
                    self.session_cursor.index = i;
                    self.session_cursor.settle(self.session_rows.len());
                }
                self.focus = Pane::SessionTree;
                self.last_tree = Pane::SessionTree;
            }
            Owner::Global => {}
        }
    }

    /// Toggle the problems-only filter.
    pub fn toggle_problems_only(&mut self) {
        self.problems_only = !self.problems_only;
        self.rebuild_rows();
    }

    /// Toggle the keybinding hint.
    pub const fn toggle_keybinds(&mut self) {
        self.keybinds_expanded = !self.keybinds_expanded;
    }

    /// Yank text for the focused row (a scope id → the `catenary query` bridge).
    #[must_use]
    pub fn selected_yank_text(&self) -> Option<String> {
        match self.focus {
            Pane::RootTree => self
                .root_rows
                .get(self.root_cursor.index)
                .and_then(Row::yank_text),
            Pane::SessionTree => self
                .session_rows
                .get(self.session_cursor.index)
                .and_then(Row::yank_text),
            _ => None,
        }
    }
}

/// The selectable row indices of a tree row list.
fn selectable(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.selectable())
        .map(|(i, _)| i)
        .collect()
}

/// Whether a root-tree row survives the problems-only filter: a root with a
/// problem, a server that owns a problem finding, or a problem finding line.
/// Healthy roots/servers and dormant inventory are dropped.
fn root_row_is_problem(row: &Row, problem_servers: &HashSet<String>) -> bool {
    match row {
        Row::Root(r) => r.worst.is_some_and(crate::health::Severity::is_problem),
        Row::Server(e) => problem_servers.contains(&e.server),
        Row::InlineFinding { severity, .. } => severity.is_problem(),
        _ => false,
    }
}

/// Whether a session-tree row survives the problems-only filter: a client with
/// an install problem or a problem finding line. Sessions/subagents are not
/// themselves broken things, so they collapse away.
fn session_row_is_problem(row: &Row, problem_clients: &HashSet<String>) -> bool {
    match row {
        Row::Client(c) => {
            c.worst.is_some_and(crate::health::Severity::is_problem)
                || problem_clients.contains(&c.name)
        }
        Row::InlineFinding { severity, .. } => severity.is_problem(),
        _ => false,
    }
}

/// Load the user config, returning either the config or a load-error string.
fn load_config() -> (Option<Config>, Option<String>) {
    match Config::load() {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::state_snapshot::{DaemonSnapshot, ServerEntry};
    use crate::tui::data::MockDataSource;

    fn snap_with_servers(n: usize) -> Snapshot {
        let mut snap = Snapshot {
            daemon: DaemonSnapshot {
                generated_at: crate::state_snapshot::now_iso(),
                version: env!("CATENARY_VERSION").to_string(),
                ..DaemonSnapshot::default()
            },
            ..Snapshot::default()
        };
        for i in 0..n {
            snap.servers.push(ServerEntry {
                id: format!("srv{i}@/p/root{}", i % 3),
                language: "rust".to_string(),
                server: format!("srv{i}"),
                scope_root: format!("/p/root{}", i % 3),
                state: "healthy".to_string(),
                state_since: crate::state_snapshot::now_iso(),
                ..ServerEntry::default()
            });
        }
        snap
    }

    /// A config that routes `language` to `server` — so a matching live instance
    /// is judged and any failure surfaces as a finding.
    fn config_with_server(server: &str, language: &str) -> Config {
        use crate::config::{LanguageConfig, ServerBinding, ServerDef};
        let mut config = Config::default();
        config.server.insert(
            server.to_string(),
            ServerDef {
                command: server.to_string(),
                ..ServerDef::default()
            },
        );
        config.language.insert(
            language.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server)]),
                ..Default::default()
            },
        );
        config
    }

    /// Build an app over a synthetic snapshot with an injected config and the
    /// hermetic snapshot seam (no config-file / install reads).
    fn app_with<'a>(
        theme: &'a Theme,
        icons: &'a IconSet,
        snap: Snapshot,
        config: Config,
    ) -> App<'a> {
        App::with_injected_config(
            theme,
            icons,
            PathBuf::from("/nonexistent"),
            config,
            Box::new(MockDataSource::new(snap)),
        )
        .expect("app")
    }

    #[test]
    fn healthy_fleet_collapses_to_one_line_per_root() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let app = app_with(&theme, &icons, snap_with_servers(21), Config::default());
        assert_eq!(app.root_rows.len(), 3, "one collapsed line per root");
    }

    #[test]
    fn expand_root_shows_servers_then_collapse_hides_them() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snap_with_servers(21), Config::default());
        assert_eq!(app.focus, Pane::RootTree);
        app.activate();
        assert!(app.root_rows.len() > 3, "expanding a root reveals servers");
        app.activate();
        assert_eq!(app.root_rows.len(), 3, "collapsing hides them again");
    }

    #[test]
    fn cursor_skips_inline_finding_rows() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut snap = snap_with_servers(1);
        snap.servers[0].state = "failed".to_string();
        // Configure srv0 so its failure surfaces as an inline finding row.
        let mut app = app_with(&theme, &icons, snap, config_with_server("srv0", "rust"));
        app.expanded_roots.insert("/p/root0".to_string());
        app.rebuild_rows();
        assert!(
            app.root_rows
                .iter()
                .any(|r| matches!(r, Row::InlineFinding { .. })),
            "the failed configured server produces an inline finding row",
        );
        for _ in 0..5 {
            app.cursor_down(1);
            let idx = app.root_cursor.index;
            assert!(
                app.root_rows[idx].selectable(),
                "cursor never lands on a finding row",
            );
        }
    }

    #[test]
    fn problems_only_filters_trees_to_broken_things() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snap_with_servers(21), Config::default());
        // Healthy fleet: no findings → problems-only empties the root tree.
        app.toggle_problems_only();
        assert!(app.root_rows.is_empty(), "no broken roots to show");
    }
}
