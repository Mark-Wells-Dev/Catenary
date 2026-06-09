// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared application container for tool servers and cross-tool infrastructure.
//!
//! `Session` creates and owns all internal servers and shared dependencies.
//! Protocol boundaries (`LspBridgeHandler`, `HookServer`) hold `Arc<Session>`
//! and access any dependency through it.

use anyhow::Result;
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::RwLock;

use super::diagnostics_server::DiagnosticsServer;
use super::editing_guardrail::EditingGuardrail;
use super::editing_manager::EditingManager;
use super::file_tools::GlobServer;
use super::filesystem_manager::FilesystemManager;
use super::grep_server::GrepServer;
use super::handler::expand_tilde;
use super::path_security::PathValidator;
use crate::config::Config;
use crate::logging::LoggingServer;
use crate::logging::jsonl_sink::JsonlSink;
use crate::logging::notification_router::NotificationRouter;
use crate::lsp::LspClientManager;
use crate::lsp::glob::LspGlob;
use crate::symbol_index::SymbolIndex;

/// A resolved glob pattern that handles tilde expansion and absolute paths.
///
/// For relative patterns (e.g. `src/**/*.rs`), matches against paths relative
/// to workspace roots. For absolute patterns (e.g. `~/other-project/*.rs`),
/// extracts the non-glob base directory as a search root and matches against
/// full paths.
pub struct ResolvedGlob {
    glob: LspGlob,
    match_full_path: bool,
    override_root: Option<PathBuf>,
}

impl ResolvedGlob {
    /// Resolves a glob pattern, expanding tilde and detecting absolute patterns.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern is not a valid glob.
    pub fn new(pattern: &str) -> Result<Self> {
        let expanded = expand_tilde(pattern);
        let glob = LspGlob::new(&expanded)?;

        if Path::new(&expanded).is_absolute() {
            let base = Self::base_dir(&expanded);
            Ok(Self {
                glob,
                match_full_path: true,
                override_root: Some(base),
            })
        } else {
            Ok(Self {
                glob,
                match_full_path: false,
                override_root: None,
            })
        }
    }

    /// Tests whether a file path matches this glob.
    ///
    /// For absolute patterns, matches against the full path.
    /// For relative patterns, strips the root prefix first.
    #[must_use]
    pub fn is_match(&self, path: &Path, root: &Path) -> bool {
        if self.match_full_path {
            self.glob.is_match(path)
        } else {
            let rel = path.strip_prefix(root).unwrap_or(path);
            self.glob.is_match(rel)
        }
    }

    /// Returns the override search root for absolute patterns.
    #[must_use]
    pub fn override_root(&self) -> Option<&Path> {
        self.override_root.as_deref()
    }

    /// Returns `true` if the pattern explicitly targets hidden files.
    ///
    /// A pattern is "explicit" when any path segment starts with `.`
    /// (excluding the trivial `.` and `..` navigation components).
    /// Examples: `.gitignore`, `.github/*.yml`, `.git*`.
    ///
    /// When this returns `true`, callers should force `include_hidden`
    /// so that the directory walker does not skip the targeted entries.
    #[must_use]
    pub fn targets_hidden(pattern: &str) -> bool {
        pattern
            .split('/')
            .any(|seg| seg.starts_with('.') && seg != "." && seg != "..")
    }

    /// Extracts the longest directory prefix without glob metacharacters.
    fn base_dir(pattern: &str) -> PathBuf {
        let mut base = PathBuf::new();
        for component in Path::new(pattern).components() {
            let s = component.as_os_str().to_string_lossy();
            if s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{') {
                break;
            }
            base.push(component);
        }
        if base.as_os_str().is_empty() {
            PathBuf::from("/")
        } else {
            base
        }
    }

    /// Expands this glob into the concrete paths it matches on disk.
    ///
    /// Walks the pattern's non-glob base directory with the gitignore-aware
    /// [`ignore`] walker, so within a git repository gitignored and (by
    /// default) hidden entries are skipped — a blind `**/*.rs` from a project
    /// root would otherwise descend into `target/` and hang.
    /// `include_gitignored` / `include_hidden` lift those filters. Gitignore is
    /// repo-scoped (matching ripgrep and editors): outside a git repository no
    /// `.gitignore` rules apply. Results are sorted for deterministic output.
    ///
    /// Only meaningful for absolute patterns (the only form the daemon
    /// receives — the CLI resolves every path argument against `cwd` before
    /// dispatch). Relative patterns carry no base directory and yield an empty
    /// list.
    #[must_use]
    pub fn expand(&self, include_gitignored: bool, include_hidden: bool) -> Vec<PathBuf> {
        let Some(base) = self.override_root.as_deref() else {
            return Vec::new();
        };
        let mut matches: Vec<PathBuf> = WalkBuilder::new(base)
            .git_ignore(!include_gitignored)
            .hidden(!include_hidden)
            .build()
            .flatten()
            .map(ignore::DirEntry::into_path)
            .filter(|path| self.is_match(path, base))
            .collect();
        matches.sort();
        matches
    }
}

/// Resolves search path arguments into the concrete paths to scope a query.
///
/// Mirrors the CLI's literal-first contract on the daemon side: a path that
/// exists on disk (file, directory, or symlink — including a broken one) is
/// kept; a non-existent path is treated as a glob pattern and expanded via
/// [`ResolvedGlob::expand`]. A non-existent path with no glob metacharacters
/// compiles to a literal glob whose base directory does not exist, so it
/// expands to nothing — the CLI reports those as `path does not exist` before
/// they ever reach here.
///
/// Existing concrete paths are still filtered against `.gitignore` unless
/// `include_gitignored` is set: a shell-expanded `target/*.rs` and a
/// daemon-expanded `'target/*.rs'` must yield the same set, so a gitignored
/// path is dropped no matter how it arrived. Gitignore is repo-scoped
/// (matching ripgrep/editors); outside a git repository nothing is filtered.
///
/// Paths are expected to be absolute (the CLI resolves them against `cwd`
/// before dispatch). An empty input yields an empty result; callers
/// distinguish "no path arguments" (search `cwd`) from "arguments that matched
/// nothing" (empty result) before calling this.
#[must_use]
pub fn expand_search_paths(
    paths: &[PathBuf],
    include_gitignored: bool,
    include_hidden: bool,
) -> Vec<PathBuf> {
    let mut resolved = Vec::new();
    // Per-parent cache of gitignore-visible entries, so a batch of
    // shell-expanded siblings only walks their directory once.
    let mut visible: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    for path in paths {
        if path.symlink_metadata().is_ok() {
            if include_gitignored || !is_gitignored(path, &mut visible) {
                resolved.push(path.clone());
            }
        } else if let Ok(glob) = ResolvedGlob::new(&path.to_string_lossy()) {
            resolved.extend(glob.expand(include_gitignored, include_hidden));
        }
    }
    resolved
}

/// Whether a single `path` is excluded by `.gitignore`, repo-scoped.
///
/// Standalone convenience over [`is_gitignored`] for callers checking one path
/// (e.g. `catenary sed`'s explicit-file drop reporting) with no batch to
/// amortize the per-parent cache over.
#[must_use]
pub(crate) fn path_is_gitignored(path: &Path) -> bool {
    let mut cache = HashMap::new();
    is_gitignored(path, &mut cache)
}

/// Whether `path` is excluded by `.gitignore`, repo-scoped like ripgrep.
///
/// Outside a git repository nothing is gitignored, so the directory walk is
/// skipped entirely (cheap `.git` probe up the tree). Inside a repo, a
/// depth-1 walk of the parent applies the full ignore hierarchy; `path` is
/// gitignored iff it is absent from the visible set. `cache` memoizes that
/// set per parent directory.
fn is_gitignored(path: &Path, cache: &mut HashMap<PathBuf, HashSet<PathBuf>>) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if !in_git_repo(parent) {
        return false;
    }
    let entries = cache
        .entry(parent.to_path_buf())
        .or_insert_with(|| visible_entries(parent));
    !entries.contains(path)
}

/// Walks up from `dir` (inclusive) looking for a `.git` entry (a directory
/// for a normal checkout, a file for worktrees/submodules).
fn in_git_repo(dir: &Path) -> bool {
    let mut current = Some(dir);
    while let Some(d) = current {
        if d.join(".git").exists() {
            return true;
        }
        current = d.parent();
    }
    false
}

/// The gitignore-visible entries directly under `dir` (depth-1 walk).
///
/// Hidden filtering is left off — an explicitly named hidden file should not
/// be dropped here; only `.gitignore` governs this filter.
fn visible_entries(dir: &Path) -> HashSet<PathBuf> {
    WalkBuilder::new(dir)
        .max_depth(Some(1))
        .git_ignore(true)
        .hidden(false)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .collect()
}

/// Shared application container for tool servers and cross-tool infrastructure.
///
/// Creates and owns all internal servers and shared dependencies.
/// [`super::hook_router::HookRouter`] holds an `Arc<Session>` and
/// handles hook dispatch. CLI tool commands access grep/glob through
/// the IPC socket.
pub struct Session {
    /// Session-wide configuration (shared with `LspClientManager`).
    pub config: Arc<Config>,
    /// Monotonic config version — bumped when merged command config changes
    /// (e.g., root addition expands the allowlist). Read by `HookRouter`
    /// for debounce invalidation.
    pub config_version: std::sync::atomic::AtomicU64,
    /// Grep tool server.
    pub grep: GrepServer,
    /// Glob tool server.
    pub glob: GlobServer,
    /// Diagnostics pipeline for `PostToolUse` hook requests.
    pub diagnostics: Arc<DiagnosticsServer>,
    /// In-memory editing state (`start_editing`/`done_editing` lifecycle).
    pub editing: EditingManager,
    /// Cross-session per-root editing guardrail (daemon mode only).
    ///
    /// `None` in single-session mode. When present, `start_editing`
    /// checks this guardrail before entering editing mode, and
    /// `done_editing` / session cleanup release all held locks.
    pub editing_guardrail: Option<Arc<EditingGuardrail>>,
    /// LSP client manager (also owns document manager).
    pub(super) client_manager: Arc<LspClientManager>,
    /// File classification and root resolution.
    fs_manager: Arc<FilesystemManager>,
    /// Path validation for LSP-aware operations.
    path_validator: Arc<RwLock<PathValidator>>,
    /// Multi-sink tracing dispatcher.
    pub logging: LoggingServer,
    /// Per-session notification router.
    ///
    /// Routes notifications to per-session queues based on `session_id`
    /// from the tracing span hierarchy. Drained at stationary hook points.
    pub notification_router: Arc<NotificationRouter>,
    /// Symbol index populated from `documentSymbol` responses (shared with grep).
    pub symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    /// Catenary instance ID (unique per process invocation).
    pub instance_id: Arc<str>,
    /// Tokio runtime handle for blocking dispatch.
    pub runtime: Handle,
    /// JSONL firehose sink, owned by the primary daemon session so clean
    /// shutdown can flush + join its writer thread. `None` for per-connection
    /// sessions, which share the already-activated `LoggingServer`.
    jsonl_sink: Option<Arc<JsonlSink>>,
}

impl Session {
    /// Creates a new `Session`, constructing all internal dependencies.
    ///
    /// Constructs the logging sinks and activates the `LoggingServer`,
    /// draining any bootstrap-buffered events. After this call, all
    /// `tracing` events flow through the logging pipeline.
    ///
    /// The `notification_router` is registered as a tracing sink. It
    /// routes notifications to per-session queues based on `session_id`
    /// from the tracing span hierarchy.
    #[must_use]
    #[allow(
        clippy::too_many_arguments,
        reason = "session wiring threads shared daemon deps plus the snapshot sink"
    )]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        logging: LoggingServer,
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
        instance_id: Arc<str>,
        runtime: Handle,
        notification_router: Arc<NotificationRouter>,
        snapshot: Option<Arc<crate::state_snapshot::SnapshotWriter>>,
    ) -> Self {
        let config = Arc::new(config);

        // JSONL firehose sink (replaces MessageDbSink); owned for flush-on-shutdown.
        let jsonl_sink = JsonlSink::new(&crate::db::cache_dir(), instance_id.clone());
        let desktop_enabled = config
            .notifications
            .as_ref()
            .and_then(|n| n.desktop)
            .unwrap_or(true);
        let desktop_sink = crate::notify::DesktopNotificationSink::with_enabled(desktop_enabled);

        // Activate — drains bootstrap buffer, enables direct dispatch. The
        // snapshot writer (daemon mode) joins as an alert-ring sink.
        let mut sinks: Vec<Arc<dyn crate::logging::Sink>> = vec![
            notification_router.clone(),
            jsonl_sink.clone(),
            desktop_sink,
        ];
        if let Some(writer) = &snapshot {
            sinks.push(writer.clone());
        }
        logging.activate(sinks);

        let classification = super::filesystem_manager::ClassificationTables::from_config(&config);
        let fs_manager = Arc::new(FilesystemManager::with_classification(classification));
        fs_manager.set_roots(roots.clone());

        // Build symbol index (in-memory, populated lazily from documentSymbol).
        let symbol_index = SymbolIndex::new()
            .map(|idx| Arc::new(std::sync::Mutex::new(idx)))
            .map_err(|e| tracing::info!("symbol index unavailable: {e}"))
            .ok();

        let grep_budget = config
            .tools
            .as_ref()
            .map_or(4000, |t| t.grep.budget as usize);
        let glob_budget = config
            .tools
            .as_ref()
            .map_or(2000, |t| t.glob.budget as usize);
        let glob_config = config
            .tools
            .as_ref()
            .map_or_else(crate::config::GlobConfig::default, |t| t.glob.clone());

        let path_validator = Arc::new(RwLock::new(PathValidator::new(roots)));
        let mut client_manager =
            LspClientManager::new(config.clone(), logging.clone(), fs_manager.clone());
        client_manager.set_db(conn, instance_id.clone());
        if let Some(writer) = snapshot {
            client_manager.set_snapshot(writer);
        }
        let client_manager = Arc::new(client_manager);
        let diagnostics = Arc::new(DiagnosticsServer::new(
            client_manager.clone(),
            path_validator.clone(),
            fs_manager.clone(),
            symbol_index.clone(),
        ));

        let grep = GrepServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            symbol_index: symbol_index.clone(),
            budget: grep_budget,
            cache: std::sync::Mutex::new(super::result_cache::ResultCache::new(grep_budget)),
        };
        let outline_suppress: Vec<globset::GlobMatcher> = glob_config
            .outline_suppress
            .iter()
            .filter_map(|pat| {
                let effective = if pat.contains('/') {
                    pat.clone()
                } else {
                    format!("**/{pat}")
                };
                globset::Glob::new(&effective)
                    .ok()
                    .map(|g| g.compile_matcher())
            })
            .collect();
        let glob = GlobServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            symbol_index: symbol_index.clone(),
            budget: glob_budget,
            outline_threshold: glob_config.outline_threshold,
            outline_suppress,
            cache: std::sync::Mutex::new(super::result_cache::ResultCache::new(glob_budget)),
        };
        Self {
            config,
            config_version: std::sync::atomic::AtomicU64::new(0),
            grep,
            glob,
            diagnostics,
            editing: EditingManager::new(),
            editing_guardrail: None,
            client_manager,
            fs_manager,
            path_validator,
            logging,
            notification_router,
            symbol_index,
            instance_id,
            runtime,
            jsonl_sink: Some(jsonl_sink),
        }
    }

    /// Creates a per-session `Session` for daemon mode.
    ///
    /// Shares heavy resources (`LspClientManager`, `FilesystemManager`,
    /// `SymbolIndex`, config, logging) with the daemon's primary session.
    /// Creates fresh per-session state: editing manager and editing
    /// guardrail.
    ///
    /// The shared [`NotificationRouter`] handles per-session routing via
    /// the `session_id` tracing span — no per-session sink registration
    /// is needed.
    #[must_use]
    pub fn new_for_daemon(
        primary: &Self,
        session_id: Arc<str>,
        editing_guardrail: Option<Arc<EditingGuardrail>>,
    ) -> Self {
        let grep_budget = primary
            .config
            .tools
            .as_ref()
            .map_or(4000, |t| t.grep.budget as usize);
        let glob_budget = primary
            .config
            .tools
            .as_ref()
            .map_or(2000, |t| t.glob.budget as usize);
        let glob_config = primary
            .config
            .tools
            .as_ref()
            .map_or_else(crate::config::GlobConfig::default, |t| t.glob.clone());

        let outline_suppress: Vec<globset::GlobMatcher> = glob_config
            .outline_suppress
            .iter()
            .filter_map(|pat| {
                let effective = if pat.contains('/') {
                    pat.clone()
                } else {
                    format!("**/{pat}")
                };
                globset::Glob::new(&effective)
                    .ok()
                    .map(|g| g.compile_matcher())
            })
            .collect();

        Self {
            config: primary.config.clone(),
            config_version: std::sync::atomic::AtomicU64::new(0),
            grep: GrepServer {
                client_manager: primary.client_manager.clone(),
                fs_manager: primary.fs_manager.clone(),
                symbol_index: primary.symbol_index.clone(),
                budget: grep_budget,
                cache: std::sync::Mutex::new(super::result_cache::ResultCache::new(grep_budget)),
            },
            glob: GlobServer {
                client_manager: primary.client_manager.clone(),
                fs_manager: primary.fs_manager.clone(),
                symbol_index: primary.symbol_index.clone(),
                budget: glob_budget,
                outline_threshold: glob_config.outline_threshold,
                outline_suppress,
                cache: std::sync::Mutex::new(super::result_cache::ResultCache::new(glob_budget)),
            },
            diagnostics: primary.diagnostics.clone(),
            editing: EditingManager::new(),
            editing_guardrail,
            client_manager: primary.client_manager.clone(),
            fs_manager: primary.fs_manager.clone(),
            path_validator: primary.path_validator.clone(),
            logging: primary.logging.clone(),
            notification_router: primary.notification_router.clone(),
            symbol_index: primary.symbol_index.clone(),
            instance_id: session_id,
            runtime: primary.runtime.clone(),
            jsonl_sink: None,
        }
    }

    /// Builds the merged command filter from user config + all project configs.
    ///
    /// Returns `None` when no `[commands]` section is configured. The merged
    /// result reflects the current workspace roots and project configs —
    /// adding a root expands the allow surface.
    #[must_use]
    pub fn merged_commands(&self) -> Option<crate::config::ResolvedCommands> {
        let base = self.config.resolved_commands.as_ref()?;
        let roots = self.client_manager.roots();
        let project_commands = self.client_manager.project_commands();
        Some(base.merge_project_commands(&roots, &project_commands))
    }

    /// Returns `true` if the path is within any known workspace root.
    ///
    /// Simple prefix check against known roots — no canonicalization or
    /// symlink resolution. Used for hook scope gating where approximate
    /// checking is sufficient.
    #[must_use]
    pub fn is_within_roots(&self, path: &Path) -> bool {
        self.fs_manager.resolve_root(path).is_some()
    }

    /// Returns the workspace root containing the given path, if any.
    ///
    /// Longest-prefix match against known roots. Used by the editing
    /// guardrail to lock the specific root being edited rather than
    /// all session roots.
    #[must_use]
    pub fn resolve_root(&self, path: &Path) -> Option<PathBuf> {
        self.fs_manager.resolve_root(path)
    }

    /// Returns `true` if the path has known LSP coverage for diagnostics.
    ///
    /// A file has coverage if it is within a workspace root (tiers 1–2)
    /// OR its language has a server with a positive single-file cache
    /// entry (tier 3). Files with uncached or negative-cached languages
    /// return `false` — the editing gate should not impose friction
    /// until we know the server works in single-file mode.
    #[must_use]
    pub fn has_lsp_coverage(&self, path: &Path) -> bool {
        if self.fs_manager.resolve_root(path).is_some() {
            return true;
        }
        let lang = self.fs_manager.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
        lang.is_some_and(|id| self.client_manager.has_single_file_coverage(&id))
    }

    /// Drops cached symbols and bumps the enrichment generation for files
    /// written outside the diagnostics batch (currently `catenary sed
    /// --in-place`).
    ///
    /// `SymbolIndex` re-populates only files with no rows
    /// ([`SymbolIndex::needs_population`](crate::symbol_index::SymbolIndex::needs_population)),
    /// so a write that leaves the rows in place makes `grep`/`glob` enrichment
    /// serve pre-write enclosing-symbol labels and ranges until a later access
    /// finds an empty table (bug #23). Deleting the rows forces a fresh
    /// `documentSymbol` on the next access; bumping the per-root generation
    /// invalidates the enrichment cache. Both are in-memory — no read-path cost.
    pub fn invalidate_symbols(&self, files: &[PathBuf]) {
        if files.is_empty() {
            return;
        }
        if let Some(index) = &self.symbol_index {
            let idx = index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for file in files {
                let _ = idx.invalidate(file);
            }
        }
        self.fs_manager.bump_generations(files);
    }

    /// Returns the shared `LspClientManager`.
    ///
    /// Used by the daemon's `SessionManager` to wire MCP lifecycle
    /// callbacks (`on_roots_changed`) directly to the shared
    /// infrastructure without routing through a `Session`.
    #[must_use]
    pub(crate) const fn lsp_client_manager(&self) -> &Arc<LspClientManager> {
        &self.client_manager
    }

    /// Returns the current workspace roots.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.client_manager.roots()
    }

    /// Spawns LSP servers for languages detected in the workspace.
    pub async fn spawn_all(&self) {
        self.client_manager.spawn_all().await;
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Updates path validation, notifies LSP servers of folder changes,
    /// and spawns servers for any newly detected languages.
    ///
    /// # Errors
    ///
    /// Returns an error if root synchronization fails.
    pub async fn sync_roots(&self, roots: Vec<PathBuf>) -> Result<()> {
        // sync_roots updates FilesystemManager roots first (before any
        // async work), then reacts to the diff.
        self.client_manager.sync_roots(roots.clone()).await?;
        self.path_validator.write().await.update_roots(roots);

        // Root changes may expand the merged command allowlist —
        // bump config version so the next denial shows a fresh full dump.
        self.config_version
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);

        // Fire-and-forget: spawn_all is pre-warming, not a gate.
        // Tool calls that need a server will trigger spawning on demand.
        let cm = self.client_manager.clone();
        tokio::spawn(async move { cm.spawn_all().await });
        Ok(())
    }

    /// Shuts down all active LSP servers gracefully.
    pub async fn shutdown(&self) {
        self.client_manager.shutdown_all().await;
    }

    /// Flush the JSONL firehose and stop its writer thread on clean shutdown.
    ///
    /// Drains the queued lines and joins the writer so the firehose tail lands
    /// on disk before the daemon exits. Only the primary daemon session owns the
    /// sink; per-connection sessions hold `None` and this is a no-op. Call after
    /// [`Session::shutdown`] so LSP-shutdown telemetry is captured too.
    pub fn flush_telemetry(&self) {
        if let Some(sink) = &self.jsonl_sink {
            sink.shutdown();
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    // ── expand_tilde ──────────────────────────────────────────────

    #[test]
    fn expand_tilde_home_prefix() {
        let home = std::env::var("HOME").expect("HOME must be set");
        assert_eq!(expand_tilde("~/foo/bar"), format!("{home}/foo/bar"));
    }

    #[test]
    fn expand_tilde_bare() {
        let home = std::env::var("HOME").expect("HOME must be set");
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn expand_tilde_no_op_for_absolute() {
        assert_eq!(expand_tilde("/usr/bin"), "/usr/bin");
    }

    #[test]
    fn expand_tilde_no_op_for_relative() {
        assert_eq!(expand_tilde("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn expand_tilde_no_op_for_mid_tilde() {
        assert_eq!(expand_tilde("foo/~/bar"), "foo/~/bar");
    }

    // ── ResolvedGlob::targets_hidden ───────────────────────────────

    #[test]
    fn targets_hidden_dotfile() {
        assert!(ResolvedGlob::targets_hidden(".gitignore"));
    }

    #[test]
    fn targets_hidden_dotdir_glob() {
        assert!(ResolvedGlob::targets_hidden(".github/*.yml"));
    }

    #[test]
    fn targets_hidden_dot_prefix_glob() {
        assert!(ResolvedGlob::targets_hidden(".git*"));
    }

    #[test]
    fn targets_hidden_nested_dotdir() {
        assert!(ResolvedGlob::targets_hidden("src/.hidden/foo.rs"));
    }

    #[test]
    fn targets_hidden_dotfile_toml() {
        assert!(ResolvedGlob::targets_hidden(".catenary.toml"));
    }

    #[test]
    fn targets_hidden_broad_doublestar() {
        assert!(!ResolvedGlob::targets_hidden("**/*.rs"));
    }

    #[test]
    fn targets_hidden_broad_src() {
        assert!(!ResolvedGlob::targets_hidden("src/**/*"));
    }

    #[test]
    fn targets_hidden_broad_star() {
        assert!(!ResolvedGlob::targets_hidden("**/*"));
    }

    #[test]
    fn targets_hidden_dotdot_is_not_hidden() {
        assert!(!ResolvedGlob::targets_hidden("../src/*.rs"));
    }

    #[test]
    fn targets_hidden_single_dot_is_not_hidden() {
        assert!(!ResolvedGlob::targets_hidden("./src/*.rs"));
    }

    // ── glob expansion ─────────────────────────────────────────────

    #[test]
    fn expand_matches_recursively_with_doublestar() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
        std::fs::write(root.join("a/b/deep.rs"), "x").expect("write");
        std::fs::write(root.join("top.rs"), "x").expect("write");
        std::fs::write(root.join("a/note.txt"), "x").expect("write");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = ResolvedGlob::new(&pattern).expect("compile glob");
        let matches = glob.expand(false, false);

        assert!(matches.contains(&root.join("a/b/deep.rs")), "{matches:?}");
        assert!(matches.contains(&root.join("top.rs")), "{matches:?}");
        assert!(
            !matches.iter().any(|p| p.ends_with("note.txt")),
            "non-matching extension excluded: {matches:?}"
        );
    }

    /// Initializes a git repo at `dir` so gitignore rules apply (gitignore is
    /// repo-scoped: outside a repo no `.gitignore` is honored).
    fn git_init(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .expect("git init");
    }

    #[test]
    fn expand_is_gitignore_aware() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::write(root.join("target/ignored.rs"), "x").expect("write");
        std::fs::write(root.join("kept.rs"), "x").expect("write");

        let pattern = format!("{}/**/*.rs", root.display());
        let glob = ResolvedGlob::new(&pattern).expect("compile glob");

        let matches = glob.expand(false, false);
        assert!(matches.contains(&root.join("kept.rs")), "{matches:?}");
        assert!(
            !matches.iter().any(|p| p.ends_with("ignored.rs")),
            "gitignored target/ pruned: {matches:?}"
        );

        // The escape hatch lifts the filter.
        let with_ignored = glob.expand(true, false);
        assert!(
            with_ignored.iter().any(|p| p.ends_with("ignored.rs")),
            "include_gitignored surfaces target/: {with_ignored:?}"
        );
    }

    #[test]
    fn expand_search_paths_drops_gitignored_concrete() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(root.join("ignored.rs"), "x").expect("write");
        std::fs::write(root.join("kept.rs"), "x").expect("write");

        let ignored = root.join("ignored.rs");
        let kept = root.join("kept.rs");

        // A shell-expanded gitignored path is dropped just like an internally
        // expanded one — convergence regardless of who expanded.
        let resolved = expand_search_paths(&[ignored.clone(), kept.clone()], false, false);
        assert_eq!(resolved, vec![kept], "{resolved:?}");

        // The escape hatch keeps it.
        let with_ignored = expand_search_paths(std::slice::from_ref(&ignored), true, false);
        assert_eq!(with_ignored, vec![ignored], "{with_ignored:?}");
    }

    #[test]
    fn expand_single_star_does_not_cross_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("sub")).expect("mkdir");
        std::fs::write(root.join("flat.rs"), "x").expect("write");
        std::fs::write(root.join("sub/nested.rs"), "x").expect("write");

        let pattern = format!("{}/*.rs", root.display());
        let glob = ResolvedGlob::new(&pattern).expect("compile glob");
        let matches = glob.expand(false, false);

        assert!(matches.contains(&root.join("flat.rs")), "{matches:?}");
        assert!(
            !matches.contains(&root.join("sub/nested.rs")),
            "single star stays within one segment: {matches:?}"
        );
    }

    #[test]
    fn expand_search_paths_keeps_existing_and_expands_patterns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("real.rs"), "x").expect("write");
        std::fs::write(root.join("other.rs"), "x").expect("write");

        let existing = root.join("real.rs");
        // An existing path passes through; a non-glob, non-existent path
        // expands to nothing (the CLI reports those as `path does not exist`).
        let resolved =
            expand_search_paths(&[existing.clone(), root.join("ghost.rs")], false, false);
        assert_eq!(resolved, vec![existing], "{resolved:?}");

        // A pattern expands to its matches.
        let expanded = expand_search_paths(&[root.join("*.rs")], false, false);
        assert!(expanded.contains(&root.join("real.rs")), "{expanded:?}");
        assert!(expanded.contains(&root.join("other.rs")), "{expanded:?}");
    }
}
