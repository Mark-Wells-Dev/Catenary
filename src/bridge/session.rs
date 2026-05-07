// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shared application container for tool servers and cross-tool infrastructure.
//!
//! `Session` creates and owns all internal servers and shared dependencies.
//! Protocol boundaries (`LspBridgeHandler`, `HookServer`) hold `Arc<Session>`
//! and access any dependency through it.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::RwLock;

use super::cwd_stash::CwdStash;
use super::diagnostics_server::DiagnosticsServer;
use super::editing_guardrail::EditingGuardrail;
use super::editing_manager::EditingManager;
use super::file_tools::GlobServer;
use super::filesystem_manager::FilesystemManager;
use super::grep_server::GrepServer;
use super::handler::expand_tilde;
use super::path_security::PathValidator;
use crate::config::Config;
use crate::config::SeverityConfig;
use crate::logging::LoggingServer;
use crate::logging::notification_queue::NotificationQueueSink;
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
}

/// Shared application container for tool servers and cross-tool infrastructure.
///
/// Creates and owns all internal servers and shared dependencies.
/// [`super::handler::LspBridgeHandler`] holds an `Arc<Session>` and handles
/// protocol boundary concerns (health checks, readiness, dispatch routing).
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
    /// Pending host-CLI cwd for grep/glob relative-pattern resolution.
    pub cwd_stash: CwdStash,
    /// LSP client manager (also owns document manager).
    pub(super) client_manager: Arc<LspClientManager>,
    /// File classification and root resolution.
    fs_manager: Arc<FilesystemManager>,
    /// Path validation for LSP-aware operations.
    path_validator: Arc<RwLock<PathValidator>>,
    /// Multi-sink tracing dispatcher.
    pub logging: LoggingServer,
    /// Notification queue for draining into `systemMessage`.
    ///
    /// Used in single-session mode. In daemon mode, the
    /// [`crate::logging::notification_router::NotificationRouter`] handles
    /// per-session routing instead — this field is still present but empty.
    pub notifications: Arc<NotificationQueueSink>,
    /// Per-session notification router (daemon mode only).
    ///
    /// When set, `HookRouter::drain_notifications` drains from the router
    /// instead of `self.notifications`. `None` in single-session mode.
    pub notification_router: Option<Arc<crate::logging::notification_router::NotificationRouter>>,
    /// Symbol index populated from `documentSymbol` responses (shared with grep).
    pub symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    /// Catenary instance ID (unique per process invocation).
    pub instance_id: Arc<str>,
    /// Tokio runtime handle for blocking dispatch.
    pub runtime: Handle,
    /// Set by `HookRouter` on `PreAgent` dispatch, cleared by `McpServer`
    /// run loop. Triggers a `roots/list` poll at the next turn boundary.
    pub roots_refresh_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Transcript file path, stashed by `HookRouter` on `PreAgent` dispatch.
    /// Read by [`scan_transcript`] for `/add-dir` root detection.
    pub transcript_path: std::sync::Mutex<Option<PathBuf>>,
    /// Byte offset for incremental transcript scanning.
    pub transcript_offset: std::sync::atomic::AtomicU64,
    /// Cumulative set of roots discovered from the transcript.
    /// Written by the eager scan (`PreAgent`), read by `on_roots_changed`
    /// to prevent `fetch_roots` from overwriting them.
    pub transcript_roots: std::sync::Mutex<Vec<PathBuf>>,
}

impl Session {
    /// Creates a new `Session`, constructing all internal dependencies.
    ///
    /// Constructs the logging sinks and activates the `LoggingServer`,
    /// draining any bootstrap-buffered events. After this call, all
    /// `tracing` events flow through the logging pipeline.
    ///
    /// When `notification_router` is `Some` (daemon mode), the router is
    /// registered as the notification sink instead of a per-session queue.
    /// The router routes events to per-session queues based on `session_id`
    /// from the tracing span hierarchy.
    #[must_use]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        logging: LoggingServer,
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
        instance_id: Arc<str>,
        runtime: Handle,
        notification_router: Option<Arc<crate::logging::notification_router::NotificationRouter>>,
    ) -> Self {
        let config = Arc::new(config);

        // Construct logging sinks.
        let threshold = config
            .notifications
            .as_ref()
            .map_or_else(SeverityConfig::default, |n| n.threshold)
            .into();
        let notifications = NotificationQueueSink::new(threshold);
        let message_db = crate::logging::message_db::MessageDbSink::new(conn, instance_id.clone());

        // Activate — drains bootstrap buffer, enables direct dispatch.
        // In daemon mode, the notification router replaces the per-session
        // queue as the notification sink.
        if let Some(ref router) = notification_router {
            logging.activate(vec![router.clone(), message_db]);
        } else {
            logging.activate(vec![notifications.clone(), message_db]);
        }

        let classification = super::filesystem_manager::ClassificationTables::from_config(&config);
        let fs_manager = Arc::new(FilesystemManager::with_classification(classification));
        fs_manager.set_roots(roots.clone());
        fs_manager.seed();

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
        let client_manager = Arc::new(LspClientManager::new(
            config.clone(),
            logging.clone(),
            fs_manager.clone(),
        ));
        let diagnostics = Arc::new(DiagnosticsServer::new(
            client_manager.clone(),
            path_validator.clone(),
            fs_manager.clone(),
        ));

        let grep = GrepServer {
            client_manager: client_manager.clone(),
            fs_manager: fs_manager.clone(),
            symbol_index: symbol_index.clone(),
            budget: grep_budget,
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
        };
        Self {
            config,
            config_version: std::sync::atomic::AtomicU64::new(0),
            grep,
            glob,
            diagnostics,
            editing: EditingManager::new(),
            editing_guardrail: None,
            cwd_stash: CwdStash::new(),
            client_manager,
            fs_manager,
            path_validator,
            logging,
            notifications,
            notification_router,
            symbol_index,
            instance_id,
            runtime,
            roots_refresh_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            transcript_path: std::sync::Mutex::new(None),
            transcript_offset: std::sync::atomic::AtomicU64::new(0),
            transcript_roots: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Creates a per-session `Session` for daemon mode.
    ///
    /// Shares heavy resources (`LspClientManager`, `FilesystemManager`,
    /// `SymbolIndex`, config, logging) with the daemon's primary session.
    /// Creates fresh per-session state: editing manager, CWD stash,
    /// transcript state, and roots-refresh flag.
    ///
    /// The per-session notification queue is not registered as a tracing
    /// sink — the shared [`crate::logging::notification_router::NotificationRouter`]
    /// handles per-session routing via the `session_id` tracing span.
    #[must_use]
    pub fn new_for_daemon(
        primary: &Self,
        session_id: Arc<str>,
        editing_guardrail: Option<Arc<EditingGuardrail>>,
    ) -> Self {
        let threshold = primary
            .config
            .notifications
            .as_ref()
            .map_or_else(SeverityConfig::default, |n| n.threshold)
            .into();
        let notifications =
            crate::logging::notification_queue::NotificationQueueSink::new(threshold);

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
            },
            glob: GlobServer {
                client_manager: primary.client_manager.clone(),
                fs_manager: primary.fs_manager.clone(),
                symbol_index: primary.symbol_index.clone(),
                budget: glob_budget,
                outline_threshold: glob_config.outline_threshold,
                outline_suppress,
            },
            diagnostics: primary.diagnostics.clone(),
            editing: EditingManager::new(),
            editing_guardrail,
            cwd_stash: CwdStash::new(),
            client_manager: primary.client_manager.clone(),
            fs_manager: primary.fs_manager.clone(),
            path_validator: primary.path_validator.clone(),
            logging: primary.logging.clone(),
            notifications,
            notification_router: primary.notification_router.clone(),
            symbol_index: primary.symbol_index.clone(),
            instance_id: session_id,
            runtime: primary.runtime.clone(),
            roots_refresh_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            transcript_path: std::sync::Mutex::new(None),
            transcript_offset: std::sync::atomic::AtomicU64::new(0),
            transcript_roots: std::sync::Mutex::new(Vec::new()),
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

    /// Diffs the filesystem and notifies servers with matching file watcher
    /// registrations. Delegates to [`LspClientManager::notify_file_changes`].
    pub async fn notify_file_changes(&self) {
        self.client_manager.notify_file_changes().await;
    }

    /// Spawns LSP servers for languages detected in the workspace.
    pub async fn spawn_all(&self) {
        self.client_manager.spawn_all().await;
    }

    /// Merges stored transcript roots into the given root set.
    ///
    /// Appends any transcript-discovered roots that aren't already present.
    /// Used by the `on_roots_changed` callback to prevent `fetch_roots`
    /// from overwriting transcript-discovered roots with a `roots/list`
    /// response that omits `/add-dir` roots.
    pub fn merge_transcript_roots(&self, paths: &mut Vec<PathBuf>) {
        if let Ok(stored) = self.transcript_roots.lock() {
            for root in stored.iter() {
                if !paths.contains(root) {
                    paths.push(root.clone());
                }
            }
        }
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
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::path::Path;

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

    // ── ResolvedGlob::base_dir ────────────────────────────────────

    #[test]
    fn base_dir_strips_at_star() {
        let base = ResolvedGlob::base_dir("/home/user/projects/*");
        assert_eq!(base, Path::new("/home/user/projects"));
    }

    #[test]
    fn base_dir_strips_at_double_star() {
        let base = ResolvedGlob::base_dir("/home/user/**/*.rs");
        assert_eq!(base, Path::new("/home/user"));
    }

    #[test]
    fn base_dir_strips_at_question_mark() {
        let base = ResolvedGlob::base_dir("/tmp/foo?/bar");
        assert_eq!(base, Path::new("/tmp"));
    }

    #[test]
    fn base_dir_strips_at_bracket() {
        let base = ResolvedGlob::base_dir("/tmp/[abc]/bar");
        assert_eq!(base, Path::new("/tmp"));
    }

    #[test]
    fn base_dir_no_metachar_returns_full_path() {
        let base = ResolvedGlob::base_dir("/home/user/projects/src");
        assert_eq!(base, Path::new("/home/user/projects/src"));
    }

    #[test]
    fn base_dir_only_metachar_returns_root() {
        let base = ResolvedGlob::base_dir("*");
        assert_eq!(base, Path::new("/"));
    }

    // ── ResolvedGlob::new ─────────────────────────────────────────

    #[test]
    fn resolved_glob_relative_pattern() {
        let rg = ResolvedGlob::new("src/**/*.rs").expect("valid glob");
        assert!(rg.override_root().is_none());
        assert!(!rg.match_full_path);
    }

    #[test]
    fn resolved_glob_absolute_pattern() {
        let rg = ResolvedGlob::new("/tmp/project/*.rs").expect("valid glob");
        assert_eq!(rg.override_root(), Some(Path::new("/tmp/project")));
        assert!(rg.match_full_path);
    }

    #[test]
    fn resolved_glob_tilde_becomes_absolute() {
        let rg = ResolvedGlob::new("~/projects/*.rs").expect("valid glob");
        assert!(rg.override_root().is_some());
        assert!(rg.match_full_path);
    }

    #[test]
    fn resolved_glob_invalid_pattern() {
        assert!(ResolvedGlob::new("[invalid").is_err());
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

    // ── ResolvedGlob::is_match ────────────────────────────────────

    #[test]
    fn is_match_relative_strips_root() {
        let rg = ResolvedGlob::new("src/**/*.rs").expect("valid glob");
        let root = Path::new("/workspace");

        assert!(rg.is_match(Path::new("/workspace/src/lib.rs"), root));
        assert!(rg.is_match(Path::new("/workspace/src/deep/mod.rs"), root));
        assert!(!rg.is_match(Path::new("/workspace/tests/foo.rs"), root));
    }

    #[test]
    fn is_match_relative_star_no_cross_directory() {
        let rg = ResolvedGlob::new("src/*.rs").expect("valid glob");
        let root = Path::new("/workspace");

        assert!(rg.is_match(Path::new("/workspace/src/lib.rs"), root));
        assert!(!rg.is_match(Path::new("/workspace/src/deep/mod.rs"), root));
    }

    #[test]
    fn is_match_absolute_uses_full_path() {
        let rg = ResolvedGlob::new("/tmp/project/*.rs").expect("valid glob");
        let root = Path::new("/tmp/project");

        assert!(rg.is_match(Path::new("/tmp/project/main.rs"), root));
        // `*` does not cross directory boundaries (shell-like)
        assert!(!rg.is_match(Path::new("/tmp/project/sub/lib.rs"), root));
        assert!(!rg.is_match(Path::new("/other/main.rs"), root));
    }

    #[test]
    fn is_match_absolute_double_star() {
        let rg = ResolvedGlob::new("/tmp/project/**/*.rs").expect("valid glob");
        let root = Path::new("/tmp/project");

        assert!(rg.is_match(Path::new("/tmp/project/main.rs"), root));
        assert!(rg.is_match(Path::new("/tmp/project/sub/lib.rs"), root));
        assert!(!rg.is_match(Path::new("/other/main.rs"), root));
    }

    #[test]
    fn is_match_relative_wrong_root_still_tries() {
        let rg = ResolvedGlob::new("*.txt").expect("valid glob");
        // When strip_prefix fails, falls back to matching the full path.
        // A bare filename matches *.txt.
        assert!(rg.is_match(Path::new("notes.txt"), Path::new("/nonexistent")));
    }

    // ── merge_transcript_roots ────────────────────────────────────────

    fn make_session() -> (tempfile::TempDir, tokio::runtime::Runtime, Session) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&db_path).expect("open test DB");
        let conn = Arc::new(std::sync::Mutex::new(conn));
        let logging = crate::logging::LoggingServer::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let instance_id: Arc<str> = "test".into();
        let session = Session::new(
            Config::default(),
            vec![],
            logging,
            conn,
            instance_id,
            runtime.handle().clone(),
            None,
        );
        (dir, runtime, session)
    }

    #[test]
    fn merge_transcript_roots_adds_missing() {
        let (_dir, _rt, session) = make_session();
        let root_a = PathBuf::from("/tmp/a");
        let root_b = PathBuf::from("/tmp/b");

        // Pre-populate transcript roots with root_b.
        session
            .transcript_roots
            .lock()
            .expect("lock")
            .push(root_b.clone());

        // MCP roots only has root_a.
        let mut paths = vec![root_a.clone()];
        session.merge_transcript_roots(&mut paths);

        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], root_a);
        assert_eq!(paths[1], root_b);
    }

    #[test]
    fn merge_transcript_roots_deduplicates() {
        let (_dir, _rt, session) = make_session();
        let root = PathBuf::from("/tmp/shared");

        // Transcript roots has the same root as MCP roots.
        session
            .transcript_roots
            .lock()
            .expect("lock")
            .push(root.clone());

        let mut paths = vec![root.clone()];
        session.merge_transcript_roots(&mut paths);

        // No duplicate added.
        assert_eq!(paths, vec![root]);
    }

    #[test]
    fn merge_transcript_roots_empty_stored() {
        let (_dir, _rt, session) = make_session();

        let mut paths = vec![PathBuf::from("/tmp/mcp_root")];
        session.merge_transcript_roots(&mut paths);

        // No change when transcript_roots is empty.
        assert_eq!(paths.len(), 1);
    }
}
