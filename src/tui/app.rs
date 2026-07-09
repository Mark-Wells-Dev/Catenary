// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Grid application state: the snapshot + config it reads, the findings it
//! derives, the four panes' cursors, and the collapse/filter toggles.
//!
//! The App is a **pure `state.json` + config-file reader** (DESIGN non-goal: no
//! second data path, never the firehose). Findings recompute only when the
//! snapshot content changes; the per-tick reload re-reads the small snapshot
//! and re-clamps cursors, so a growing time-in-state surfaces without a rebuild.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::config::{Config, Mutation};
use crate::health::{FindingCode, servers::server_binary_installed};
use crate::install::{
    BlessedRecipe, CommandRunner, InstallPlan, ProcessRunner, TarballFetcher, UreqFetcher,
};
use crate::recipes::{BlessedManifest, InstallRecipe};
use crate::state_snapshot::Snapshot;

use super::action::{
    ActionState, InstallState, PendingRestart, binding_specs, find_migration_source,
};
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

    /// The open guided-mutation consent overlay, if any (tui-rework 05).
    pub pending_action: Option<ActionState>,
    /// The open guided-install consent overlay, if any (tui-rework 06).
    pub pending_install: Option<InstallState>,
    /// Applied mutations awaiting a daemon restart to take effect. They stay
    /// marked until the snapshot shows the daemon has come back under a new
    /// identity.
    pub pending_restarts: Vec<PendingRestart>,

    /// CI-internal pinned install recipes, keyed by canonical server name. The
    /// guided-install action reads these only through the blessing gate.
    recipes: BTreeMap<String, InstallRecipe>,
    /// The blessed-manifest: the only recipe-derived data a user surface may
    /// consult. Empty in production until CI conformance blesses a server.
    blessed: BlessedManifest,
    /// The command runner the guided install spawns argv through.
    runner: Box<dyn CommandRunner>,
    /// The tarball fetcher the npm verified-install path fetches through.
    fetcher: Box<dyn TarballFetcher>,

    /// The `generated_at` the findings were built from.
    last_generated_at: String,
    /// Set by a toggle/filter change to force a row rebuild next reload.
    needs_rebuild: bool,
    /// Whether to gather the environment-reading findings (config files,
    /// install health) alongside the hermetic snapshot seam. Always `true` in
    /// production; `false` under the fleet-scale tests, which inject a config
    /// and drive [`findings::gather_snapshot`] for a deterministic finding set.
    env_findings: bool,
    /// Injected render clock. `None` in production (reads `Utc::now()` each
    /// frame); a fixed instant under tests so a render is deterministic and two
    /// refreshes inside one quantization bucket are byte-identical (tui-rework
    /// 11, item 4).
    now_override: Option<DateTime<Utc>>,
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
            pending_action: None,
            pending_install: None,
            pending_restarts: Vec::new(),
            recipes: crate::recipes::default_recipes().unwrap_or_default(),
            blessed: crate::recipes::default_blessed_manifest().unwrap_or_default(),
            runner: Box::new(ProcessRunner),
            fetcher: Box::new(UreqFetcher),
            last_generated_at: String::new(),
            needs_rebuild: true,
            env_findings,
            now_override: None,
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

    /// Inject a synthetic recipe set, blessed manifest, and install backends —
    /// the seam the guided-install tests drive to exercise the (production-empty)
    /// blessing gate deterministically. Recomputes findings + rows afterward.
    #[cfg(test)]
    pub fn inject_install_env(
        &mut self,
        recipes: BTreeMap<String, InstallRecipe>,
        blessed: BlessedManifest,
        runner: Box<dyn CommandRunner>,
        fetcher: Box<dyn TarballFetcher>,
    ) {
        self.recipes = recipes;
        self.blessed = blessed;
        self.runner = runner;
        self.fetcher = fetcher;
        self.recompute_findings();
        self.rebuild_rows();
    }

    /// Whether the daemon is present (a snapshot has been generated).
    #[must_use]
    pub const fn daemon_present(&self) -> bool {
        !self.snapshot.daemon.generated_at.is_empty()
    }

    /// The clock the current frame renders every duration against: the injected
    /// instant under tests, otherwise the wall clock. Threading a single `now`
    /// through the render is what makes an idle board byte-identical between
    /// refreshes within a quantization bucket (tui-rework 11, item 4).
    #[must_use]
    pub fn render_now(&self) -> DateTime<Utc> {
        self.now_override.unwrap_or_else(Utc::now)
    }

    /// Pin the render clock to a fixed instant — the seam the calm-board tests
    /// drive to render a snapshot deterministically at a chosen `now`.
    #[cfg(test)]
    pub const fn inject_now(&mut self, now: DateTime<Utc>) {
        self.now_override = Some(now);
    }

    /// Re-read the snapshot; recompute findings + rows only when the snapshot
    /// content changed or a toggle marked the model dirty.
    pub fn reload(&mut self) {
        if let Ok(snap) = self.data.load() {
            self.snapshot = snap;
        }
        // Clear any pending-restart marker whose daemon has come back on the new
        // config (a fresh instance id / start time in the snapshot).
        self.pending_restarts
            .retain(|p| !p.cleared_by(&self.snapshot));
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
        self.enrich_blessed_suggestions();
        self.verdict = model::verdict(&self.findings);
    }

    /// Rewrite the fix-it text of each install-suggestion finding whose server is
    /// blessed (a matching-version manifest entry) to the exact pinned form. An
    /// unblessed suggestion keeps naming the binary only — Catenary never prints
    /// an unpinned install command. Text-only: severity and the verdict are
    /// untouched, so a suggestion still never counts as a problem.
    fn enrich_blessed_suggestions(&mut self) {
        let recipes = &self.recipes;
        let blessed = &self.blessed;
        for owned in &mut self.findings {
            if owned.finding.code != FindingCode::ServerInstallSuggestion {
                continue;
            }
            let Owner::Server(server) = &owned.owner else {
                continue;
            };
            if let Some(recipe) = recipes.get(server)
                && let Some(br) = BlessedRecipe::resolve(server, recipe, blessed)
            {
                owned.finding.fix_it = Some(br.pinned_fix_it());
            }
        }
    }

    /// Active workspace languages: the **activity-gated** set from the snapshot's
    /// language-activity ledger (tui-rework 09, item 5).
    ///
    /// A language is live only when tracked-session activity touched a file of
    /// it — presence in a dormant fixture directory no session opened lights
    /// nothing, so it can never raise a suggestion or Fatal. The daemon records
    /// the ledger; the TUI never walks the filesystem for this.
    fn active_languages(&self) -> HashSet<String> {
        let (active, _) =
            crate::health::servers::activity_inputs(&self.snapshot.activity_languages);
        active
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
                    .position(|r| matches!(r, Row::Server { entry, .. } if &entry.server == name))
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

    // ── Guided mutations (tui-rework 05) ─────────────────────────────

    /// The guided mutation the action key offers for the current context, if
    /// any: a problem row's fix, or the cursored server's config.
    #[must_use]
    pub fn available_mutation(&self) -> Option<Mutation> {
        match self.focus {
            Pane::Problems => {
                let row = self.problem_rows.get(self.problem_cursor.index)?;
                self.mutation_for_problem(row)
            }
            Pane::RootTree | Pane::Detail => match self.detail_entity()? {
                EntityKey::Server { name, .. } => self.mutation_for_server(&name),
                _ => None,
            },
            Pane::SessionTree => None,
        }
    }

    /// The mutation that executes a problem row's fix-it, if it has one.
    fn mutation_for_problem(&self, row: &ProblemRow) -> Option<Mutation> {
        match row.code {
            FindingCode::ConfigLegacyNamespace => {
                find_migration_source().map(|source| Mutation::MigrateNamespace { source })
            }
            FindingCode::ServerRoutedBroken => match &row.owner {
                Owner::Server(name) => self.set_path_mutation(name),
                _ => None,
            },
            _ => None,
        }
    }

    /// The mutation the detail pane offers for a cursored server: set its binary
    /// path when the executable is not installed, else toggle its diagnostics.
    fn mutation_for_server(&self, name: &str) -> Option<Mutation> {
        let def = self.config.as_ref()?.server.get(name)?;
        // The server key IS the executable (misc 162); `server_binary_installed`
        // is honest against the rust-analyzer rustup proxy shim.
        if server_binary_installed(name, def.program(name)) {
            self.toggle_server_mutation(name)
        } else {
            self.set_path_mutation(name)
        }
    }

    /// A "relocate the binary" mutation seeded with the server's current `path`
    /// override (empty when it spawns off `PATH` under its key).
    fn set_path_mutation(&self, name: &str) -> Option<Mutation> {
        let path = self
            .config
            .as_ref()?
            .server
            .get(name)?
            .path
            .clone()
            .unwrap_or_default();
        Some(Mutation::SetServerPath {
            server: name.to_string(),
            path,
        })
    }

    /// An enable/disable mutation flipping the server's `diagnostics` in the
    /// first language that binds it, carrying the full binding list so siblings
    /// survive.
    fn toggle_server_mutation(&self, name: &str) -> Option<Mutation> {
        let cfg = self.config.as_ref()?;
        let (language, lang) = cfg
            .language
            .iter()
            .find(|(_, lc)| lc.servers().iter().any(|b| b.name == name))?;
        let enabled = lang
            .servers()
            .iter()
            .find(|b| b.name == name)
            .is_none_or(|b| b.diagnostics);
        Some(Mutation::SetServerEnabled {
            language: language.clone(),
            server: name.to_string(),
            enabled: !enabled,
            bindings: binding_specs(lang),
        })
    }

    /// Open the consent overlay for the current context's action, if any. A
    /// blessed install-suggestion opens the guided-install overlay; otherwise a
    /// guided mutation opens its own.
    pub fn begin_action(&mut self) {
        if self.begin_install() {
            return;
        }
        let Some(mutation) = self.available_mutation() else {
            return;
        };
        let candidates = mutation.candidate_layers(Some(&self.project_root));
        let value = match &mutation {
            Mutation::SetServerPath { path, .. } => Some(path.clone()),
            _ => None,
        };
        self.pending_action = Some(ActionState::new(mutation, candidates, 0, value));
    }

    // ── Guided install (tui-rework 06) ───────────────────────────────

    /// The blessed install the action key offers for the current context, if
    /// any: a cursored install-suggestion row whose server has a blessed recipe.
    ///
    /// The blessing gate is structural — [`BlessedRecipe::resolve`] returns a
    /// value only when a manifest entry matches the recipe's pin — so an
    /// unblessed suggestion simply yields `None` here (no offer to construct).
    #[must_use]
    pub fn available_install(&self) -> Option<BlessedRecipe> {
        if self.focus != Pane::Problems {
            return None;
        }
        let row = self.problem_rows.get(self.problem_cursor.index)?;
        if row.code != FindingCode::ServerInstallSuggestion {
            return None;
        }
        let Owner::Server(server) = &row.owner else {
            return None;
        };
        let recipe = self.recipes.get(server)?;
        BlessedRecipe::resolve(server, recipe, &self.blessed)
    }

    /// Open the guided-install overlay for the cursored suggestion, if offerable.
    /// Returns whether an overlay was opened.
    fn begin_install(&mut self) -> bool {
        let Some(blessed) = self.available_install() else {
            return false;
        };
        let plan = InstallPlan::resolve(&blessed).map_err(|e| format!("{e:#}"));
        self.pending_install = Some(InstallState::new(blessed.server().to_owned(), plan));
        true
    }

    /// Run the pending install through the injected seams, store its outcome on
    /// the overlay, and — on success — re-derive health so the suggestion
    /// vanishes once the probe (a `$PATH` check) sees the now-installed binary.
    /// A first Enter runs it; once run, Enter is a no-op (the escape key
    /// dismisses). On failure the overlay carries the error.
    pub fn confirm_install(&mut self) {
        // Take the plan out without holding a borrow across execution.
        let plan = match self.pending_install.as_ref() {
            Some(state) if state.can_execute() => match state.plan() {
                Ok(plan) => plan.clone(),
                Err(_) => return,
            },
            _ => return,
        };
        let outcome = crate::install::execute(&plan, self.runner.as_ref(), self.fetcher.as_ref());
        let success = outcome.success;
        if let Some(state) = self.pending_install.as_mut() {
            state.set_outcome(outcome);
        }
        if success {
            self.rederive_health();
        }
    }

    /// Close the guided-install overlay (escape-equivalent).
    pub fn cancel_install(&mut self) {
        self.pending_install = None;
    }

    /// Recompute findings and rebuild rows so a completed install's effect (the
    /// suggestion clearing once the binary is on `$PATH`) surfaces immediately —
    /// re-running the availability check rather than assuming success.
    fn rederive_health(&mut self) {
        self.recompute_findings();
        self.needs_rebuild = true;
        self.rebuild_rows();
    }

    /// Cancel the open consent overlay without writing (escape-equivalent).
    pub fn cancel_action(&mut self) {
        self.pending_action = None;
    }

    /// Cycle the target layer in the open overlay.
    pub const fn action_cycle_layer(&mut self) {
        if let Some(a) = self.pending_action.as_mut() {
            a.cycle_layer();
        }
    }

    /// Type a character into the overlay's editable value.
    pub fn action_push_char(&mut self, c: char) {
        if let Some(a) = self.pending_action.as_mut() {
            a.push_char(c);
        }
    }

    /// Delete the last character of the overlay's editable value.
    pub fn action_backspace(&mut self) {
        if let Some(a) = self.pending_action.as_mut() {
            a.backspace();
        }
    }

    /// Apply the pending mutation to its chosen layer, mark it pending-restart,
    /// and re-read the config so the detail pane's provenance reflects the file
    /// just written. On failure the overlay stays open carrying the error.
    pub fn confirm_action(&mut self) {
        let Some(state) = self.pending_action.as_mut() else {
            return;
        };
        let Some(layer) = state.current_layer().cloned() else {
            state.error = Some("no writable config layer".to_string());
            return;
        };
        let mutation = state.effective_mutation();
        let path = match layer.resolve_path() {
            Ok(p) => p,
            Err(e) => {
                state.error = Some(format!("{e:#}"));
                return;
            }
        };
        if let Err(e) = mutation.apply(&path) {
            state.error = Some(format!("{e:#}"));
            return;
        }
        let summary = format!(
            "{} = {} @ {}",
            mutation.key_label(),
            mutation.value_label(),
            layer.label()
        );
        self.pending_action = None;
        self.pending_restarts
            .push(PendingRestart::new(summary, &self.snapshot));
        self.refresh_config();
    }

    /// Re-read the config from disk (in production) and recompute findings + rows
    /// so provenance and findings reflect the file just written.
    fn refresh_config(&mut self) {
        if self.env_findings {
            let (config, config_error) = load_config();
            self.config = config;
            self.config_error = config_error;
        }
        self.recompute_findings();
        self.needs_rebuild = true;
        self.rebuild_rows();
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
        Row::Server { entry, .. } => problem_servers.contains(&entry.server),
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
        // The key IS the executable (misc 162): a bare def spawns `server` on PATH.
        config
            .server
            .insert(server.to_string(), ServerDef::default());
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

    #[test]
    fn cursored_server_with_missing_binary_offers_set_path() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut snap = snap_with_servers(1);
        snap.servers[0].state = "failed".to_string();
        let mut app = app_with(&theme, &icons, snap, config_with_server("srv0", "rust"));
        app.expanded_roots.insert("/p/root0".to_string());
        app.rebuild_rows();
        let idx = app
            .root_rows
            .iter()
            .position(|r| matches!(r, Row::Server { entry, .. } if entry.server == "srv0"))
            .expect("server row present");
        app.root_cursor.index = idx;
        app.set_focus(Pane::RootTree);
        // `srv0` isn't on `$PATH`, so the action is "relocate the binary".
        assert!(
            matches!(
                app.available_mutation(),
                Some(crate::config::Mutation::SetServerPath { server, .. }) if server == "srv0"
            ),
            "a missing-binary server offers a set-path mutation",
        );
    }

    #[test]
    fn confirm_action_applies_write_and_records_pending_restart() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snap_with_servers(1), Config::default());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[server.foo]\ncommand = \"foo\"\n").expect("write fixture");
        let mutation = crate::config::Mutation::MigrateNamespace {
            source: path.clone(),
        };
        let candidates = mutation.candidate_layers(None);
        app.pending_action = Some(ActionState::new(mutation, candidates, 0, None));

        app.confirm_action();

        assert!(
            app.pending_action.is_none(),
            "overlay closes on a clean write"
        );
        assert_eq!(
            app.pending_restarts.len(),
            1,
            "the applied change is marked pending a daemon restart",
        );
        let out = std::fs::read_to_string(&path).expect("read back");
        assert!(out.contains("[lsp.server.foo]"), "migration applied: {out}");
    }

    #[test]
    fn pending_restart_clears_when_daemon_identity_changes() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snap_with_servers(1), Config::default());
        app.pending_restarts
            .push(PendingRestart::new("x".to_string(), &app.snapshot));
        // Swap the data source to a snapshot with a different daemon identity —
        // the daemon has restarted on the new config, so the marker clears.
        let mut restarted = snap_with_servers(1);
        restarted.daemon.instance_id = "daemon:new".to_string();
        restarted.daemon.started_at = "2026-07-07T13:00:00Z".to_string();
        app.data = Box::new(MockDataSource::new(restarted));
        app.reload();
        assert!(
            app.pending_restarts.is_empty(),
            "the marker clears once the daemon returns under a new identity",
        );
    }

    // ── Guided install (tui-rework 06) ───────────────────────────────

    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::health::{Finding, Severity};
    use crate::install::{CommandOutcome, InstallCommand};
    use crate::recipes::{BlessedEntry, Ecosystem, VerificationTier};

    /// A runner that records each program it is asked to run and always succeeds.
    struct RecordingRunner(Rc<RefCell<Vec<String>>>);

    impl CommandRunner for RecordingRunner {
        fn run(&self, command: &InstallCommand) -> Result<CommandOutcome> {
            self.0.borrow_mut().push(command.program().to_owned());
            Ok(CommandOutcome {
                success: true,
                code: Some(0),
                output: String::new(),
            })
        }
    }

    /// A fetcher never reached by the cargo-recipe tests.
    struct NoFetch;

    impl TarballFetcher for NoFetch {
        fn fetch(&self, _url: &str) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    fn cargo_recipe() -> InstallRecipe {
        InstallRecipe {
            ecosystem: Ecosystem::Cargo,
            package: "taplo-cli".to_string(),
            version: "0.10.0".to_string(),
            tier: VerificationTier::CargoLocked,
            draft: true,
            hash: None,
            note: None,
            conformance: true,
            runtime: None,
        }
    }

    fn recipes_map(server: &str, recipe: InstallRecipe) -> BTreeMap<String, InstallRecipe> {
        let mut m = BTreeMap::new();
        m.insert(server.to_string(), recipe);
        m
    }

    fn blessed_manifest(server: &str, version: &str) -> BlessedManifest {
        let mut m = BlessedManifest::default();
        m.blessed.insert(
            server.to_string(),
            BlessedEntry {
                version: version.to_string(),
                platform: "linux-x86_64".to_string(),
                date: "2026-07-07".to_string(),
                tier: None,
            },
        );
        m
    }

    fn suggestion_row(server: &str) -> ProblemRow {
        ProblemRow {
            code: FindingCode::ServerInstallSuggestion,
            severity: Severity::Suggestion,
            message: format!("{server}: not installed"),
            fix_it: Some(format!(
                "Install `{server}`. Catenary never runs installs for you."
            )),
            owner: Owner::Server(server.to_string()),
            is_suggestion: true,
        }
    }

    fn install_app<'a>(
        theme: &'a Theme,
        icons: &'a IconSet,
        blessed: BlessedManifest,
        calls: &Rc<RefCell<Vec<String>>>,
    ) -> App<'a> {
        let mut app = app_with(theme, icons, snap_with_servers(1), Config::default());
        app.inject_install_env(
            recipes_map("taplo", cargo_recipe()),
            blessed,
            Box::new(RecordingRunner(calls.clone())),
            Box::new(NoFetch),
        );
        app.problem_rows = vec![suggestion_row("taplo")];
        app.focus = Pane::Problems;
        app.problem_cursor.index = 0;
        app
    }

    #[test]
    fn blessed_suggestion_offers_install_unblessed_does_not() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let calls = Rc::new(RefCell::new(Vec::new()));

        // A matching-version manifest entry unlocks the offer.
        let app = install_app(&theme, &icons, blessed_manifest("taplo", "0.10.0"), &calls);
        assert!(
            app.available_install().is_some(),
            "a blessed suggestion offers an install",
        );

        // An empty manifest (production reality) offers nothing.
        let app = install_app(&theme, &icons, BlessedManifest::default(), &calls);
        assert!(
            app.available_install().is_none(),
            "an unblessed suggestion is structurally unofferable",
        );

        // A version-skewed entry does not match the recipe pin.
        let app = install_app(&theme, &icons, blessed_manifest("taplo", "0.9.0"), &calls);
        assert!(
            app.available_install().is_none(),
            "a version-skewed manifest entry does not unlock the offer",
        );
    }

    #[test]
    fn action_key_opens_install_overlay_and_confirm_runs_the_pinned_command() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let calls = Rc::new(RefCell::new(Vec::new()));
        let mut app = install_app(&theme, &icons, blessed_manifest("taplo", "0.10.0"), &calls);

        app.begin_action();
        assert!(
            app.pending_install.is_some(),
            "the action key opens the install overlay for a blessed suggestion",
        );
        assert!(
            app.pending_action.is_none(),
            "an install does not open a mutation overlay",
        );

        app.confirm_install();
        let state = app
            .pending_install
            .as_ref()
            .expect("the overlay stays open showing the outcome");
        let outcome = state.outcome().expect("the install ran");
        assert!(
            outcome.success,
            "the pinned cargo install succeeds: {:?}",
            outcome.log
        );
        assert!(
            calls.borrow().iter().any(|c| c.as_str() == "cargo"),
            "confirm ran `cargo install` via argv",
        );
    }

    #[test]
    fn enrich_blessed_suggestion_rewrites_fix_it_to_pinned_form() {
        let theme = Theme::new();
        let icons = IconSet::from_config(crate::config::IconConfig::default());
        let mut app = app_with(&theme, &icons, snap_with_servers(1), Config::default());
        app.recipes = recipes_map("taplo", cargo_recipe());

        // Blessed → the fix-it becomes the exact pinned form.
        app.blessed = blessed_manifest("taplo", "0.10.0");
        app.findings = vec![OwnedFinding {
            finding: Finding::new(
                FindingCode::ServerInstallSuggestion,
                Severity::Suggestion,
                "taplo: not installed",
            )
            .with_fix_it("Install `taplo-cli`. Catenary never runs installs for you."),
            owner: Owner::Server("taplo".to_string()),
        }];
        app.enrich_blessed_suggestions();
        let fix = app.findings[0]
            .finding
            .fix_it
            .as_deref()
            .expect("fix-it present");
        assert!(
            fix.contains("cargo install taplo-cli --version =0.10.0 --locked"),
            "blessed suggestion shows the pinned form: {fix}",
        );

        // Unblessed → the fix-it keeps naming the binary only.
        app.blessed = BlessedManifest::default();
        app.findings = vec![OwnedFinding {
            finding: Finding::new(
                FindingCode::ServerInstallSuggestion,
                Severity::Suggestion,
                "taplo: not installed",
            )
            .with_fix_it("Install `taplo-cli`. Catenary never runs installs for you."),
            owner: Owner::Server("taplo".to_string()),
        }];
        app.enrich_blessed_suggestions();
        let fix = app.findings[0]
            .finding
            .fix_it
            .as_deref()
            .expect("fix-it present");
        assert!(
            !fix.contains("Pinned:"),
            "an unblessed suggestion keeps the binary-only text: {fix}",
        );
    }
}
