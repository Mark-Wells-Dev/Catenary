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
use super::filesystem_manager::{FilesystemManager, Root};
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
/// An existing concrete **file** is searched/outlined unconditionally —
/// naming it is a direct request for that exact file, so the gitignore (and
/// hidden) gate does not apply (misc 110, ripgrep parity: ripgrep searches
/// files you name even when ignored). Existing **directories** are still
/// filtered against `.gitignore` unless `include_gitignored` is set: the gate
/// governs the recursive directory walk, not a named file. Gitignore is
/// repo-scoped (matching ripgrep/editors); outside a git repository nothing is
/// filtered.
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
        // Re-stat with a bounded retry before treating a literal path as a
        // glob — a transient stat miss (e.g. an atomic-rename write between the
        // CLI probe and here) must never silently zero a path that is present
        // on disk.
        if path_exists_with_retry(path) {
            // A named existing file bypasses the gitignore (and hidden) gate —
            // the user named that exact file, so it is searched unconditionally
            // (misc 110). The gate still governs directory walks: a named
            // gitignored directory is dropped here unless `include_gitignored`.
            // `is_file()` follows symlinks, so a symlink-to-file is a file and a
            // symlink-to-dir or broken symlink falls into the gated branch.
            if path.is_file() || include_gitignored || !is_gitignored(path, &mut visible) {
                resolved.push(path.clone());
            }
        } else if has_glob_metachar(&path.to_string_lossy()) {
            // Only metachar-bearing args expand as globs. A metachar-free path
            // that still does not resolve is a genuine "not found" — it is the
            // CLI's loud `path does not exist` (collected before dispatch), not
            // a glob that silently expands to an empty set.
            if let Ok(glob) = ResolvedGlob::new(&path.to_string_lossy()) {
                resolved.extend(glob.expand(include_gitignored, include_hidden));
            }
        }
    }
    resolved
}

/// Number of `symlink_metadata` attempts before treating a miss as genuine.
///
/// A transient stat miss races a sub-millisecond atomic-rename window
/// (write temp + `rename`); a few tight retries (no sleep) close the
/// in-workflow case without masking a path that is genuinely absent.
const STAT_RETRY_ATTEMPTS: u32 = 3;

/// Whether `path` resolves on disk, retrying a transient `symlink_metadata`
/// miss a bounded number of times.
///
/// Existence is probed via `symlink_metadata` so a broken symlink still counts
/// as present (matching the literal-first contract). The retry never sleeps —
/// the rename window is sub-millisecond — so a present path that lost a single
/// stat race is kept rather than silently treated as a missing glob.
fn path_exists_with_retry(path: &Path) -> bool {
    path_exists_with_retry_with(path, STAT_RETRY_ATTEMPTS, |p| p.symlink_metadata().is_ok())
}

/// Retry loop body for [`path_exists_with_retry`], with the per-attempt
/// existence probe injected.
///
/// The production helper calls this with the real `symlink_metadata` probe and
/// [`STAT_RETRY_ATTEMPTS`]; tests inject a stateful probe (e.g. miss on attempt
/// 1, hit thereafter) to prove the loop actually retries — a regression to a
/// single attempt would no longer recover a transient miss.
fn path_exists_with_retry_with(path: &Path, attempts: u32, probe: impl Fn(&Path) -> bool) -> bool {
    for attempt in 0..attempts {
        if probe(path) {
            return true;
        }
        // Yield between attempts (not after the last) so the scheduler can advance
        // the racing writer past its sub-µs atomic-rename window before the
        // re-stat. Cheap and `.await`-free (this is a sync helper). (walk-3)
        if attempt + 1 < attempts {
            std::thread::yield_now();
        }
    }
    false
}

/// Whether `s` contains a shell glob metacharacter (`* ? [ {`).
///
/// Mirrors the CLI's `contains_glob_metachar` classifier so a metachar-free
/// argument is treated as a literal path (and reported missing if absent)
/// rather than compiled into a glob that silently expands to nothing.
fn has_glob_metachar(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
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
    /// Daemon-owned live-state snapshot writer, shared from the primary
    /// session (`None` outside daemon mode). Action boundaries
    /// ([`Self::set_last_action`]) mark it dirty so the session board reflects
    /// the change; the writer pulls per-session `status` / `last_action` at
    /// flush time (observability ticket 05).
    pub(crate) snapshot: Option<Arc<crate::state_snapshot::SnapshotWriter>>,
    /// The session's most recent attributable action, surfaced on the snapshot
    /// session board. Set at edit / diagnostics / sed boundaries.
    last_action: std::sync::Mutex<Option<crate::state_snapshot::LastAction>>,
    /// When the daemon last saw a hook dispatch from this session (ISO 8601),
    /// surfaced on the snapshot session board. Bumped on **every**
    /// `get_or_create_router` call — i.e. every non-catenary tool the
    /// `PreToolUse` hook forwards (`Read`, `Edit`, `Bash`, …) — so it advances
    /// far more often than `last_action`. It is the recency / liveness signal
    /// the board has no death event for (ticket 05a).
    last_seen: std::sync::Mutex<String>,
    /// `true` while a `catenary diagnostics` run is in flight for this session
    /// — drives the board's `diagnostics` status (the editing accumulator has
    /// already drained by the time the run starts).
    diagnostics_in_flight: std::sync::atomic::AtomicBool,
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
        clippy::too_many_lines,
        reason = "session wiring threads shared daemon deps, the JSONL sink, and the snapshot sink"
    )]
    pub fn new(
        config: Config,
        roots: Vec<PathBuf>,
        logging: LoggingServer,
        instance_id: Arc<str>,
        runtime: Handle,
        notification_router: Arc<NotificationRouter>,
        snapshot: Option<Arc<crate::state_snapshot::SnapshotWriter>>,
    ) -> Self {
        let config = Arc::new(config);

        // JSONL firehose sink (replaces MessageDbSink); owned for flush-on-shutdown.
        // The reap policy bounds on-write growth (rotation + per-tool budget,
        // ticket 01).
        let jsonl_sink = JsonlSink::with_policy(
            &crate::paths::cache_dir(),
            instance_id.clone(),
            config.reap_policy(),
        );
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
        // Install config-complete roots: each `Root` loads its `.catenary.toml`
        // at birth, so the per-root toggle gate (`is_lsp_disabled` /
        // `is_diag_disabled`) sees the config the moment the root is resolvable —
        // no separate prime step, no spawn race (ticket 00a).
        fs_manager.set_roots_rich(
            roots
                .iter()
                .map(|p| Arc::new(Root::load(p.clone())))
                .collect(),
        );

        // Build symbol index (in-memory, populated lazily from documentSymbol).
        let symbol_index = SymbolIndex::new()
            .map(|idx| Arc::new(std::sync::Mutex::new(idx)))
            .map_err(|e| tracing::info!("symbol index unavailable: {e}"))
            .ok();

        // One shared display line budget feeds both search surfaces' overflow
        // valve (pipeable-output ticket 03); the per-tool char budgets are gone.
        let line_budget = config
            .tools
            .as_ref()
            .map_or(1000, crate::config::ToolsConfig::line_budget);
        let glob_config = config
            .tools
            .as_ref()
            .map_or_else(crate::config::GlobConfig::default, |t| t.glob.clone());

        let path_validator = Arc::new(RwLock::new(PathValidator::new(roots)));
        let mut client_manager =
            LspClientManager::new(config.clone(), logging.clone(), fs_manager.clone());
        if let Some(writer) = &snapshot {
            client_manager.set_snapshot(writer.clone());
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
            budget: line_budget,
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
            budget: line_budget,
            outline_threshold: glob_config.outline_threshold,
            outline_suppress,
        };
        Self {
            config,
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
            snapshot,
            last_action: std::sync::Mutex::new(None),
            last_seen: std::sync::Mutex::new(crate::state_snapshot::now_iso()),
            diagnostics_in_flight: std::sync::atomic::AtomicBool::new(false),
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
        let line_budget = primary
            .config
            .tools
            .as_ref()
            .map_or(1000, crate::config::ToolsConfig::line_budget);
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
            grep: GrepServer {
                client_manager: primary.client_manager.clone(),
                fs_manager: primary.fs_manager.clone(),
                symbol_index: primary.symbol_index.clone(),
                budget: line_budget,
            },
            glob: GlobServer {
                client_manager: primary.client_manager.clone(),
                fs_manager: primary.fs_manager.clone(),
                symbol_index: primary.symbol_index.clone(),
                budget: line_budget,
                outline_threshold: glob_config.outline_threshold,
                outline_suppress,
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
            snapshot: primary.snapshot.clone(),
            last_action: std::sync::Mutex::new(None),
            last_seen: std::sync::Mutex::new(crate::state_snapshot::now_iso()),
            diagnostics_in_flight: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Records the session's most recent action and marks the snapshot dirty.
    ///
    /// Surfaced on the snapshot session board's `last_action` field
    /// (observability ticket 05). Called at edit, diagnostics, and `sed`
    /// boundaries. The snapshot lock is taken only after the `last_action`
    /// guard is dropped, so this never inverts lock order against the flush
    /// path (which reads `last_action` while pulling the board).
    pub fn set_last_action(&self, summary: impl Into<String>) {
        let action = crate::state_snapshot::LastAction {
            summary: summary.into(),
            at: crate::state_snapshot::now_iso(),
        };
        {
            let mut guard = self
                .last_action
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(action);
        }
        self.touch_snapshot();
    }

    /// Returns the session's most recent action, if any.
    #[must_use]
    pub fn last_action(&self) -> Option<crate::state_snapshot::LastAction> {
        self.last_action
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Bumps `last_seen` to now and marks the snapshot dirty.
    ///
    /// Called on **every** session-bound hook dispatch (the
    /// `get_or_create_router` chokepoint), so it tracks recency — the only
    /// uniform liveness signal a hook session has, since the hook side carries
    /// no authoritative death event (ticket 05a). Distinct from
    /// [`Self::set_last_action`], which moves only on edit / diagnostics / sed.
    /// Like that method, the snapshot lock is taken only after the `last_seen`
    /// guard is dropped, so it never inverts lock order against the flush path.
    pub fn touch_last_seen(&self) {
        {
            let mut guard = self
                .last_seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = crate::state_snapshot::now_iso();
        }
        self.touch_snapshot();
    }

    /// Returns the session's most recent hook-dispatch time (ISO 8601).
    #[must_use]
    pub fn last_seen(&self) -> String {
        self.last_seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sets whether a `catenary diagnostics` run is in flight and marks the
    /// snapshot dirty so the board's status reflects the transition promptly.
    pub fn set_diagnostics_in_flight(&self, in_flight: bool) {
        self.diagnostics_in_flight
            .store(in_flight, std::sync::atomic::Ordering::Release);
        self.touch_snapshot();
    }

    /// Derives the session's board status from live editing state.
    ///
    /// `diagnostics` while a run is in flight; otherwise `editing` when an
    /// editing accumulator is active; otherwise `idle`. No transition tracking
    /// — read at snapshot-build time (observability ticket 05).
    #[must_use]
    pub fn status(&self) -> crate::state_snapshot::SessionStatus {
        use crate::state_snapshot::SessionStatus;
        if self
            .diagnostics_in_flight
            .load(std::sync::atomic::Ordering::Acquire)
        {
            SessionStatus::Diagnostics
        } else if self.editing.is_active() {
            SessionStatus::Editing
        } else {
            SessionStatus::Idle
        }
    }

    /// Marks the snapshot dirty (coalesced flush). No-op outside daemon mode.
    pub fn touch_snapshot(&self) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.touch();
        }
    }

    /// Records a curated milestone on the snapshot's activity ring. No-op
    /// outside daemon mode (observability ticket 08). Used by session /
    /// editing / diagnostics boundaries to promote a significant event into the
    /// dashboard's live glimpse without tailing the firehose.
    pub fn record_milestone(
        &self,
        kind: crate::state_snapshot::MilestoneKind,
        summary: impl Into<String>,
        scope: Option<String>,
    ) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_milestone(kind, summary, scope);
        }
    }

    /// Renders a path for a `last_action` summary: relative to its workspace
    /// root when resolvable (e.g. `src/db.rs`), else the bare file name, else
    /// the full path.
    #[must_use]
    pub fn display_path(&self, path: &Path) -> String {
        if let Some(root) = self.resolve_root(path)
            && let Ok(rel) = path.strip_prefix(&root)
        {
            return rel.display().to_string();
        }
        path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
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
    /// Both tiers require the file's language to be actually served: an
    /// in-root file (tiers 1–2) is covered when a server is *configured* for
    /// its language ([`has_configured_server`], independent of instance
    /// state — a cold per-root instance of a warm language still counts,
    /// granularity Decision 3); an out-of-root file (tier 3) is covered when
    /// its language has a server with a positive single-file cache entry
    /// ([`has_single_file_coverage`]). Files whose language is unknown, has no
    /// configured server (e.g. `.txt`, logs, data/scratch files), or has only
    /// an uncached / negative-cached single-file server return `false` — the
    /// editing gate should not impose friction on edits it cannot diagnose.
    ///
    /// A root with `disable_lsp` set (ticket 00) runs no language server, so its
    /// files have no LSP coverage regardless of configured servers.
    ///
    /// [`has_configured_server`]: crate::lsp::LspClientManager::has_configured_server
    /// [`has_single_file_coverage`]: crate::lsp::LspClientManager::has_single_file_coverage
    #[must_use]
    pub fn has_lsp_coverage(&self, path: &Path) -> bool {
        let lang = self.fs_manager.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
        if let Some(root) = self.fs_manager.resolve_root(path) {
            if self.client_manager.is_lsp_disabled(&root) {
                return false;
            }
            return lang.is_some_and(|id| self.client_manager.has_configured_server(&id));
        }
        lang.is_some_and(|id| self.client_manager.has_single_file_coverage(&id))
    }

    /// Whether *any* diagnostic feeder covers this file.
    ///
    /// `has_coverage = has_lsp_coverage || has_lint_coverage` (workstream 34
    /// ticket 00). The editing-boundary gate tracks/gates a file iff some
    /// feeder covers it. [`has_lint_coverage`](Self::has_lint_coverage) is
    /// stubbed `false` until the linter framework lands (ticket 01), so today
    /// this equals [`has_lsp_coverage`](Self::has_lsp_coverage).
    #[must_use]
    pub fn has_coverage(&self, path: &Path) -> bool {
        self.has_lsp_coverage(path) || self.has_lint_coverage(path)
    }

    /// Whether a standalone linter covers this file (workstream 34 ticket 01).
    ///
    /// Resolves the file to its owning root and matches the root-relative path
    /// against that root's effective `[linter.*]` patterns (user ∪ project),
    /// reusing `LspGlob`. Out-of-root files and `disable_lint` roots are never
    /// covered. With no `[linter.*]` configured (defaults ship in ticket 03)
    /// this is `false`, so the coverage gate is unchanged until a linter is set.
    #[must_use]
    pub fn has_lint_coverage(&self, path: &Path) -> bool {
        self.client_manager.lint_covers(path)
    }

    /// Whether the diagnostics surface is suppressed for the file's root
    /// (`disable_diag`, ticket 00).
    ///
    /// Out-of-root files have no owning root to consult and are never disabled.
    #[must_use]
    pub fn diag_disabled(&self, path: &Path) -> bool {
        self.fs_manager
            .resolve_root(path)
            .is_some_and(|root| self.client_manager.is_diag_disabled(&root))
    }

    /// The editing-boundary gate predicate (ticket 00).
    ///
    /// Track/gate a file for batched diagnostics iff some feeder covers it
    /// ([`has_coverage`](Self::has_coverage)) AND its root has not suppressed
    /// the diagnostics surface (`disable_diag`). `disable_diag` keeps LSP
    /// navigation (grep/glob) but turns the gate + output off, so a covered
    /// file in such a root flows free.
    #[must_use]
    pub fn covered_for_diagnostics(&self, path: &Path) -> bool {
        self.has_coverage(path) && !self.diag_disabled(path)
    }

    /// Drops cached symbols and bumps the enrichment generation for files
    /// written outside the diagnostics batch (currently `catenary sed
    /// --in-place`).
    ///
    /// This eager invalidate is the daemon's *granularity-independent* backstop
    /// for its own writes. The lazy `grep`/`glob` path (`ensure_symbols`)
    /// already re-populates when a file's recorded mtime is stale
    /// ([`SymbolIndex::symbols_outdated`](crate::symbol_index::SymbolIndex::symbols_outdated),
    /// bug #26), and the result-cache witness path checks mtime too — but both
    /// rely on the on-disk mtime *visibly advancing*, which a coarse-mtime or
    /// NFS/SMB/FUSE mount can defeat when `sed` rewrites a file within the
    /// filesystem's mtime resolution. Clearing the rows unconditionally and
    /// bumping the per-root generation does not depend on mtime: it forces a
    /// fresh `documentSymbol` on the next access (bug #23) and invalidates the
    /// enrichment cache outright. So this is *not* dead redundancy now that the
    /// lazy backstop exists — it strictly dominates that backstop for the
    /// daemon's own writes on hostile filesystems. Both effects are in-memory —
    /// no read-path cost. **Keep it.**
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
    /// `roots` are config-complete [`Root`]s (loaded at birth by the daemon's
    /// root tracker, ticket 00a). The manager consumes the rich roots (config +
    /// classification); the path validator gets the path-only view.
    ///
    /// # Errors
    ///
    /// Returns an error if root synchronization fails.
    pub async fn sync_roots(&self, roots: Vec<Arc<Root>>) -> Result<()> {
        // Path-only view for the validator (a path-only consumer).
        let paths: Vec<PathBuf> = roots.iter().map(|r| r.path().to_path_buf()).collect();

        // sync_roots updates FilesystemManager roots first (before any
        // async work), then reacts to the diff.
        let removed = self.client_manager.sync_roots(roots).await?;
        self.path_validator.write().await.update_roots(paths);

        // Evict the per-root `SymbolIndex` entries for every removed root —
        // the manager owns no handle to the index, so this is the only layer
        // where both the removed set and the index are visible. Without it the
        // daemon-lived cache outlives a root's tracked lifetime and serves
        // enrichment for a path `catenary roots ls` reports as untracked
        // (bug #36). The per-root baseline / `root_generations` teardown for the
        // same removed roots already happens inside `LspClientManager`.
        if let Some(index) = &self.symbol_index {
            let mut idx = index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for root in &removed {
                idx.evict_root(root);
            }
        }

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
    fn expand_search_paths_keeps_named_gitignored_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(root.join("ignored.rs"), "x").expect("write");
        std::fs::write(root.join("kept.rs"), "x").expect("write");

        let ignored = root.join("ignored.rs");
        let kept = root.join("kept.rs");

        // A named existing gitignored FILE is searched unconditionally — naming
        // it is a direct request for that exact file, so the gitignore gate does
        // not apply even without `--include-gitignored` (misc 110, ripgrep
        // parity).
        let resolved = expand_search_paths(&[ignored.clone(), kept.clone()], false, false);
        assert_eq!(resolved, vec![ignored.clone(), kept], "{resolved:?}");

        // The escape hatch is also a no-op for an already-kept named file.
        let with_ignored = expand_search_paths(std::slice::from_ref(&ignored), true, false);
        assert_eq!(with_ignored, vec![ignored], "{with_ignored:?}");
    }

    #[test]
    fn expand_search_paths_still_gates_named_gitignored_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        git_init(root);
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir_all(root.join("target")).expect("mkdir");
        std::fs::write(root.join("target/ignored.rs"), "x").expect("write");

        let dir = root.join("target");

        // A named gitignored DIRECTORY is still dropped — the gate governs the
        // recursive directory walk, not a named file. `--include-gitignored`
        // remains the opt-in for directory contents (directory-walk behavior
        // unchanged, misc 110).
        let gated = expand_search_paths(std::slice::from_ref(&dir), false, false);
        assert!(gated.is_empty(), "named gitignored dir is gated: {gated:?}");

        // The escape hatch lifts the directory gate.
        let with_ignored = expand_search_paths(std::slice::from_ref(&dir), true, false);
        assert_eq!(with_ignored, vec![dir], "{with_ignored:?}");
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

    #[test]
    fn path_exists_with_retry_succeeds_for_present_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("present.rs");
        std::fs::write(&file, "x").expect("write");
        assert!(
            path_exists_with_retry(&file),
            "a present file resolves through the bounded retry"
        );
    }

    #[test]
    fn path_exists_with_retry_fails_for_absent_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ghost = tmp.path().join("ghost.rs");
        assert!(
            !path_exists_with_retry(&ghost),
            "a genuinely absent path stays absent after the bounded retry"
        );
    }

    #[test]
    fn path_exists_with_retry_keeps_broken_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let link = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("missing-target"), &link).expect("symlink");
        assert!(
            path_exists_with_retry(&link),
            "a broken symlink is present (symlink_metadata succeeds)"
        );
    }

    #[test]
    fn ws31_review_r2_live_retry_recovers_transient_miss() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A stateful probe that misses on call 1 and hits on every later call —
        // the deterministic transient miss→hit a real atomic-rename race would
        // produce. With the full `STAT_RETRY_ATTEMPTS` budget the loop must
        // recover; with a single attempt it must NOT — so the guard is sensitive
        // to the retry count (a regression to `attempts == 1` fails here, where a
        // terminal present/absent test would still pass).
        const {
            assert!(
                STAT_RETRY_ATTEMPTS >= 2,
                "the retry guard assumes more than one attempt"
            );
        }

        let calls = AtomicUsize::new(0);
        let probe = |_: &Path| calls.fetch_add(1, Ordering::Relaxed) >= 1;
        let path = Path::new("/does/not/matter");

        assert!(
            path_exists_with_retry_with(path, STAT_RETRY_ATTEMPTS, probe),
            "the bounded retry must recover a miss that resolves on a later attempt"
        );

        // Same probe, fresh counter, single attempt: the first call misses and
        // there is no retry, so the loop reports absent — pinning the retry-count
        // sensitivity (a `STAT_RETRY_ATTEMPTS = 1` regression would surface here).
        let calls = AtomicUsize::new(0);
        let probe = |_: &Path| calls.fetch_add(1, Ordering::Relaxed) >= 1;
        assert!(
            !path_exists_with_retry_with(path, 1, probe),
            "a single attempt cannot recover a transient miss"
        );
    }

    #[test]
    fn has_glob_metachar_matches_cli_classifier() {
        assert!(has_glob_metachar("*.rs"));
        assert!(has_glob_metachar("a?b"));
        assert!(has_glob_metachar("[abc].rs"));
        assert!(has_glob_metachar("{a,b}.rs"));
        assert!(!has_glob_metachar("plain.rs"));
        assert!(!has_glob_metachar("src/bridge/session.rs"));
    }

    #[test]
    fn expand_search_paths_metachar_free_absent_is_not_glob_expanded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("real.rs"), "x").expect("write");

        // A metachar-free, non-existent literal must NOT be compiled into a
        // glob (which could silently expand to a non-empty set); it is the
        // CLI's loud `path does not exist`. Here it simply contributes nothing.
        let resolved = expand_search_paths(&[root.join("ghost.rs")], false, false);
        assert!(
            resolved.is_empty(),
            "metachar-free absent path is not glob-expanded: {resolved:?}"
        );
    }

    // ── has_lsp_coverage ───────────────────────────────────────────

    /// Builds a `Session` rooted at `root` with the embedded default
    /// classification + server bindings loaded, so coverage gating sees the
    /// real served/unserved split.
    fn session_with_root(handle: &Handle, root: PathBuf) -> Session {
        let instance_id: Arc<str> = "test-session".into();
        let notification_router = Arc::new(NotificationRouter::new(crate::logging::Severity::Warn));
        notification_router.register_session(&instance_id);
        Session::new(
            Config::default_with_classification(),
            vec![root],
            LoggingServer::new(),
            instance_id,
            handle.clone(),
            notification_router,
            None,
        )
    }

    #[test]
    fn has_lsp_coverage_gates_in_root_on_served_language() {
        // Bug 44: the in-root tier must require the file's language to be
        // actually served, not blanket-cover every in-root path.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        // Served in-root types stay covered (rust → rust-analyzer).
        assert!(
            session.has_lsp_coverage(&root.join("src/main.rs")),
            "in-root .rs (served) must have coverage"
        );

        // Non-served in-root types flow free (no configured server).
        assert!(
            !session.has_lsp_coverage(&root.join("notes.txt")),
            "in-root .txt (non-served) must not claim coverage"
        );
        assert!(
            !session.has_lsp_coverage(&root.join("run.log")),
            "in-root .log (non-served) must not claim coverage"
        );
    }

    #[test]
    fn has_lsp_coverage_gates_out_of_root_on_single_file_coverage() {
        // Bug 44 / Decision 3: the out-of-root tier (tier 3) gates on single-file
        // coverage, NOT the in-root configured-server check. The project-based
        // rust-analyzer is not a single-file server, so an out-of-root .rs is not
        // covered — even though the same .rs in-root IS. Pins the `resolve_root`
        // in-root/out-of-root branch so it cannot collapse to a single tier.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        // In-root .rs is covered (configured server) ...
        assert!(
            session.has_lsp_coverage(&root.join("src/main.rs")),
            "in-root .rs must have coverage"
        );
        // ... but the same language out of root is not (no single-file server).
        let outside = tmp.path().join("outside").join("lib.rs");
        assert!(
            !session.has_lsp_coverage(&outside),
            "out-of-root .rs must gate on single-file coverage (none for rust)"
        );
    }

    // ── disable_lsp / disable_diag (ticket 00) ─────────────────────

    #[test]
    fn has_coverage_equals_lsp_coverage_without_linters() {
        // has_lint_coverage is a false stub (ticket 01), so has_coverage tracks
        // has_lsp_coverage exactly today.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        let served = root.join("src/main.rs");
        let unserved = root.join("notes.txt");
        assert_eq!(
            session.has_coverage(&served),
            session.has_lsp_coverage(&served),
            "has_coverage == has_lsp_coverage while linters are stubbed"
        );
        assert!(session.has_coverage(&served));
        assert!(!session.has_coverage(&unserved));
    }

    #[test]
    fn disable_lsp_root_has_no_lsp_coverage() {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        std::fs::write(root.join(".catenary.toml"), "[lsp]\ndisable = true\n")
            .expect("write config");
        // No spawn_all: `Session::new` primes the project config at construction,
        // so the gate sees `[lsp] disable` immediately (ticket 00).
        let session = session_with_root(rt.handle(), root.clone());

        let served = root.join("src/main.rs");
        // A served language in a disable_lsp root has NO LSP coverage ...
        assert!(
            !session.has_lsp_coverage(&served),
            "disable_lsp root must not claim LSP coverage"
        );
        // ... so with the linter stub still false, the editing gate is inert.
        assert!(
            !session.covered_for_diagnostics(&served),
            "disable_lsp root with no linter leaves the gate inert"
        );
    }

    #[test]
    fn disable_diag_root_keeps_lsp_coverage_but_gate_off() {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        std::fs::write(
            root.join(".catenary.toml"),
            "[diagnostics]\ndisable = true\n",
        )
        .expect("write config");
        // No spawn_all: the config is primed at construction (ticket 00).
        let session = session_with_root(rt.handle(), root.clone());

        let served = root.join("src/main.rs");
        // disable_diag keeps LSP navigation/coverage ...
        assert!(
            session.has_lsp_coverage(&served),
            "disable_diag keeps LSP coverage (navigation intact)"
        );
        assert!(session.diag_disabled(&served), "root is disable_diag");
        // ... but turns the diagnostics gate off.
        assert!(
            !session.covered_for_diagnostics(&served),
            "disable_diag turns the editing gate off despite LSP coverage"
        );
    }

    // ── has_lint_coverage (ticket 01) ──────────────────────────────

    #[test]
    fn has_lint_coverage_matches_configured_linter() {
        // A root-level `[linter.*]` with a matching path glob covers a file even
        // when no language server backs it — the gate tracks lint-only files.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        std::fs::write(
            root.join(".catenary.toml"),
            "[linter.shellcheck]\ncommand = \"shellcheck\"\n\
             args = [\"-f\", \"json1\"]\npatterns = [\"**/*.sh\"]\n",
        )
        .expect("write config");
        let session = session_with_root(rt.handle(), root.clone());

        let script = root.join("scripts/deploy.sh");
        // A .sh file in the root is lint-covered ...
        assert!(
            session.has_lint_coverage(&script),
            "configured shellcheck linter covers a matching .sh"
        );
        assert!(
            session.has_coverage(&script),
            "lint coverage feeds has_coverage"
        );
        // ... and gated for diagnostics (no LSP server required).
        assert!(
            session.covered_for_diagnostics(&script),
            "a lint-covered file is gated for diagnostics"
        );
        // A non-matching file is not lint-covered.
        assert!(
            !session.has_lint_coverage(&root.join("notes.txt")),
            "a non-matching file is not lint-covered"
        );
    }

    #[test]
    fn has_lint_coverage_false_without_linters() {
        // With no `[linter.*]` configured (defaults ship in ticket 03), every
        // file is lint-uncovered — the gate is unchanged.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        let session = session_with_root(rt.handle(), root.clone());

        assert!(!session.has_lint_coverage(&root.join("scripts/deploy.sh")));
    }
}
