// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Single authority for file classification.
//!
//! [`FilesystemManager`] centralises binary detection, line counting, and
//! language identification (extension, filename, and shebang) behind one
//! cache keyed by path + mtime. Replaces the former `FilesystemCache`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignore::WalkBuilder;

use crate::config::{LinterConfig, ProjectConfig};
use crate::source::Source;

/// File classification result.
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// File modification time (seconds since epoch).
    pub mtime: u64,
    /// File size in bytes.
    pub size: u64,
    /// Owning workspace root (longest-prefix match), or `None` if outside
    /// all known roots. Resolved live on every [`FilesystemManager::classify`]
    /// call — not cached.
    pub root: Option<PathBuf>,
    /// File kind (binary or text with metadata).
    pub kind: FileKind,
}

impl FileInfo {
    /// Returns the LSP language identifier, if detectable.
    #[must_use]
    pub fn language_id(&self) -> Option<&str> {
        match &self.kind {
            FileKind::Text { language_id, .. } => language_id.as_deref(),
            FileKind::Binary | FileKind::Folder => None,
        }
    }
}

/// File classification: binary, text, or folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileKind {
    /// Binary file (a NUL byte appeared before any text byte-order mark).
    Binary,
    /// Text file with line count and optional language ID.
    Text {
        /// Number of lines (newline-delimited).
        lines: usize,
        /// LSP language identifier, if detectable. `None` for files with
        /// no known extension, filename, or shebang.
        language_id: Option<String>,
    },
    /// Directory entry. Used by [`FilesystemManager::seed`] and
    /// [`FilesystemManager::diff`] for tracking directory creation and
    /// deletion.
    Folder,
}

/// Why the classifier treats a file as unsearchable binary.
///
/// Classification is purely content-based: a file is binary when a NUL byte
/// appears before any text byte-order mark (misc 140, decision 029 — the former
/// size cap that also skipped large *text* files unread is gone, bug 62). The
/// reason is kept as an enum so the grep skip-honesty surface (misc 135) has one
/// place to label it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySkip {
    /// A NUL byte was found while scanning — genuinely binary content.
    Binary,
}

impl BinarySkip {
    /// Human-readable reason label for the grep skip-honesty surface (misc 135).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Binary => "binary",
        }
    }
}

/// Semantic change kind for a baseline diff entry (WS31 Consumer A).
///
/// Drives **both** the per-server watch-**kind** mask filter and the wire
/// `FileChangeType` sent in `workspace/didChangeWatchedFiles` (Created ⇒ 1,
/// Changed ⇒ 2). The two now agree: a path routed as `Created` carries
/// `FileChangeType` 1 and is gated by the watcher's `Create` bit; a path routed
/// as `Changed` carries 2 and is gated by the `Change` bit. Per the LSP spec,
/// `didChangeWatchedFiles` is Catenary's channel for filesystem-observed
/// changes and its payload is meant to carry the real Created/Changed/Deleted
/// distinction (`didCreateFiles` is a *different*, editor-initiated notification
/// Catenary does not use). See decision 018 — filesystem-coherence changed-set.
///
/// The first walk for a root is the cold snapshot: those files pre-exist and the
/// server already indexed them at startup, so they are `Changed`, not `Created`
/// (nothing was created relative to the server's knowledge). Only a path that
/// appears on a *later* walk — absent from a baseline that already existed — is a
/// genuine `Created`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    /// The path was absent from a baseline that already existed (a genuine
    /// creation on a later walk). Gated by the watcher's `Create` kind bit and
    /// emitted as `FileChangeType` 1.
    Created,
    /// The path's mtime advanced, or it was observed on the first walk (the cold
    /// snapshot of pre-existing, already-indexed files). Gated by the watcher's
    /// `Change` kind bit and emitted as `FileChangeType` 2.
    Changed,
    /// A baseline entry that a **full** walk did not visit — the file is gone
    /// from disk. Gated by the watcher's `Delete` kind bit and emitted as
    /// `FileChangeType` 3. Reaped only on a full walk
    /// ([`diff_update_and_reap`](FilesystemManager::diff_update_and_reap)); a
    /// scoped walk cannot assert a baseline entry outside its pattern is gone.
    Deleted,
}

/// One diffed change from a coherence walk: a root-relative path plus its
/// semantic [`ChangeKind`]. Produced by
/// [`FilesystemManager::diff_and_update`] and routed per server.
#[derive(Debug, Clone)]
pub(crate) struct Change {
    /// Path relative to the owning root.
    pub rel: PathBuf,
    /// Whether the path was created (absent) or changed (mtime advanced).
    pub kind: ChangeKind,
}

/// The delta a single coherence walk produces for one root: the per-server
/// router fans these out filtered by each server's registered globs + kind
/// mask. Empty when nothing changed since the last walk (the bug-38 no-repeat
/// property).
#[derive(Debug, Clone, Default)]
pub(crate) struct ChangeSet {
    /// Diffed changes, each a root-relative path + semantic kind.
    pub changes: Vec<Change>,
}

impl ChangeSet {
    /// Returns `true` when no path changed since the last observation.
    ///
    /// Test-only since bug 146 stage 3: the delivery path no longer asks. An
    /// empty round still drains every server's frontier, because "nothing
    /// changed just now" says nothing about what a given server has been told.
    #[cfg(test)]
    pub const fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Per-root last-seen baseline: an outer map from root to a per-root inner
/// `Arc<Mutex<…>>` of `relative-path → mtime`. The per-root inner lock keeps
/// parallel-subagent worktrees from contending on a single global lock.
type LastSeen = Mutex<HashMap<PathBuf, Arc<Mutex<HashMap<PathBuf, i64>>>>>;

/// Pre-built classification lookup tables derived from merged config.
///
/// Built once from `Config` and stored in [`FilesystemManager`].
/// Classification precedence: shebang > filename > extension.
///
/// Also used for per-root overrides from `.catenary.toml` project
/// configs via [`from_project_config`](Self::from_project_config).
#[derive(Debug, Default)]
pub struct ClassificationTables {
    /// File extension (without dot) → language ID.
    extensions: HashMap<String, String>,
    /// Exact filename → language ID.
    filenames: HashMap<String, String>,
    /// Interpreter basename → language ID.
    shebangs: HashMap<String, String>,
}

impl ClassificationTables {
    /// Builds classification tables from a merged config.
    ///
    /// Iterates language entries in sorted order for deterministic
    /// first-insert-wins behavior when multiple languages claim the
    /// same extension, filename, or shebang.
    #[must_use]
    pub fn from_config(config: &crate::config::Config) -> Self {
        let mut tables = Self::default();

        let mut keys: Vec<&str> = config.language.keys().map(String::as_str).collect();
        keys.sort_unstable();

        for lang_id in keys {
            let Some(lc) = config.language.get(lang_id) else {
                continue;
            };
            if let Some(ref exts) = lc.extensions {
                for ext in exts {
                    tables
                        .extensions
                        .entry(ext.clone())
                        .or_insert_with(|| lang_id.to_string());
                }
            }
            if let Some(ref fnames) = lc.filenames {
                for fname in fnames {
                    tables
                        .filenames
                        .entry(fname.clone())
                        .or_insert_with(|| lang_id.to_string());
                }
            }
            if let Some(ref shebangs) = lc.shebangs {
                for shebang in shebangs {
                    tables
                        .shebangs
                        .entry(shebang.clone())
                        .or_insert_with(|| lang_id.to_string());
                }
            }
        }

        tables
    }

    /// Builds classification tables from a project config's language entries.
    ///
    /// Only includes entries that have classification fields set.
    /// Entries with only `servers` (no `extensions`/`filenames`/`shebangs`)
    /// are skipped — they don't affect classification.
    #[must_use]
    pub fn from_project_config(languages: &HashMap<String, crate::config::LanguageConfig>) -> Self {
        let mut tables = Self::default();

        let mut keys: Vec<&str> = languages.keys().map(String::as_str).collect();
        keys.sort_unstable();

        for lang_id in keys {
            let Some(lc) = languages.get(lang_id) else {
                continue;
            };
            if !lc.has_classification() {
                continue;
            }
            if let Some(ref exts) = lc.extensions {
                for ext in exts {
                    tables
                        .extensions
                        .entry(ext.clone())
                        .or_insert_with(|| lang_id.to_string());
                }
            }
            if let Some(ref fnames) = lc.filenames {
                for fname in fnames {
                    tables
                        .filenames
                        .entry(fname.clone())
                        .or_insert_with(|| lang_id.to_string());
                }
            }
            if let Some(ref shebangs) = lc.shebangs {
                for shebang in shebangs {
                    tables
                        .shebangs
                        .entry(shebang.clone())
                        .or_insert_with(|| lang_id.to_string());
                }
            }
        }

        tables
    }

    /// Returns `true` if any classification entries exist.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty() && self.filenames.is_empty() && self.shebangs.is_empty()
    }

    /// Looks up language ID by filename (exact match).
    fn lookup_filename(&self, filename: &str) -> Option<&str> {
        self.filenames.get(filename).map(String::as_str)
    }

    /// Looks up language ID by file extension (without dot).
    fn lookup_extension(&self, ext: &str) -> Option<&str> {
        self.extensions.get(ext).map(String::as_str)
    }

    /// Looks up language ID by interpreter basename.
    fn lookup_shebang(&self, interpreter: &str) -> Option<&str> {
        self.shebangs.get(interpreter).map(String::as_str)
    }

    /// Resolves language ID for a path without I/O (filename + extension only).
    fn classify_path(&self, path: &Path) -> Option<String> {
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && let Some(lang) = self.lookup_filename(name)
        {
            return Some(lang.to_string());
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && let Some(lang) = self.lookup_extension(ext)
        {
            return Some(lang.to_string());
        }
        None
    }

    /// Returns duplicate extensions across languages (for doctor warnings).
    #[must_use]
    pub fn find_duplicate_extensions(
        config: &crate::config::Config,
    ) -> Vec<(String, String, String)> {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        let mut duplicates = Vec::new();

        let mut keys: Vec<&str> = config.language.keys().map(String::as_str).collect();
        keys.sort_unstable();

        for lang_id in keys {
            if let Some(lc) = config.language.get(lang_id)
                && let Some(ref exts) = lc.extensions
            {
                for ext in exts {
                    if let Some(&first_lang) = seen.get(ext.as_str()) {
                        if first_lang != lang_id {
                            duplicates.push((
                                ext.clone(),
                                first_lang.to_string(),
                                lang_id.to_string(),
                            ));
                        }
                    } else {
                        seen.insert(ext.as_str(), lang_id);
                    }
                }
            }
        }

        duplicates
    }
}

/// A workspace root folded into one value: its path, its loaded
/// `.catenary.toml` project config, and the [`ClassificationTables`] derived
/// from that config's `[lsp.language.*]` section.
///
/// This replaces the formerly-parallel `(path → ProjectConfig)` side-table
/// (`LspClientManager.project_configs`) and the per-root classification map.
/// A `Root` is **config-complete the moment it exists**: it is born when a
/// path's tracker refcount goes 0→1 (loading `.catenary.toml` then) and reaped
/// at 1→0. Because the config travels with the path on a single map under a
/// single lock, a consumer that resolves a root can never observe it without
/// its config — the `disable_lsp` spawn race is structurally impossible, so no
/// "prime configs before `set_roots`" reorder is needed (workstream 34 ticket
/// 00a).
///
/// The hot, mutable runtime caches (`root_generations`, the changed-set
/// `last_seen` baseline, the LSP instance map) are deliberately **not** folded
/// here: they have independent locking and a lifetime tied to
/// spawn/invalidation, not config. Co-locating them behind one `Root` lock
/// would regress concurrency.
#[derive(Debug)]
pub struct Root {
    path: PathBuf,
    config: ProjectConfig,
    classification: ClassificationTables,
}

impl Root {
    /// Builds a root from an explicit path + config, deriving its
    /// classification tables from the config's `[lsp.language.*]` section.
    #[must_use]
    pub fn new(path: PathBuf, config: ProjectConfig) -> Self {
        let classification = ClassificationTables::from_project_config(&config.language);
        Self {
            path,
            config,
            classification,
        }
    }

    /// Builds a config-complete root by loading `.catenary.toml` at `path`.
    ///
    /// A missing config yields the default [`ProjectConfig`]; a malformed one
    /// logs a `warn!` and also falls back to default — a broken project config
    /// must never block a root from being tracked.
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let config = match crate::config::load_project_config(&path) {
            Ok(Some(pc)) => {
                tracing::info!(
                    source = Source::ConfigParse.as_str(),
                    root = %path.display(),
                    "Loaded project config from {}",
                    path.join(".catenary.toml").display(),
                );
                pc
            }
            Ok(None) => ProjectConfig::default(),
            Err(e) => {
                tracing::warn!(
                    source = Source::ConfigParse.as_str(),
                    root = %path.display(),
                    "Failed to load project config from {}: {e}",
                    path.join(".catenary.toml").display(),
                );
                ProjectConfig::default()
            }
        };
        Self::new(path, config)
    }

    /// Builds a root carrying the default (empty) project config.
    ///
    /// Used for path-only consumers (tests and query scoping) that never read
    /// per-root config — the classification tables are empty, so file
    /// classification falls through to the global tables.
    #[must_use]
    pub fn bare(path: PathBuf) -> Self {
        Self::new(path, ProjectConfig::default())
    }

    /// The root's path (its identity).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The root's loaded project config.
    #[must_use]
    pub const fn config(&self) -> &ProjectConfig {
        &self.config
    }

    /// The classification tables derived from the root's `[lsp.language.*]`.
    #[must_use]
    pub const fn classification(&self) -> &ClassificationTables {
        &self.classification
    }
}

/// Cross-tool filesystem classification cache.
///
/// Single authority for file metadata: binary detection, line count,
/// language ID, and shebang detection. Shared by the search pipelines and
/// the hitstream annotator through `Session`.
///
/// Also owns the workspace root list for longest-prefix root resolution
/// and the classification lookup tables built from config.
pub struct FilesystemManager {
    /// Cache keyed by `(file_path, owning_root)`. The root component
    /// ensures that root changes (add/remove) cause cache misses,
    /// preventing stale `language_id` from per-root classification.
    cache: std::sync::Mutex<HashMap<(PathBuf, Option<PathBuf>), CachedEntry>>,
    /// Workspace roots keyed by path. Each [`Root`] carries its loaded
    /// `.catenary.toml` config and the classification tables derived from it,
    /// so resolving a root and reading its config/classification hit one map
    /// under one lock — a resolvable root is always config-complete (workstream
    /// 34 ticket 00a). Folds the former `roots: Vec<PathBuf>` and
    /// `per_root_classification` maps.
    roots: std::sync::Mutex<HashMap<PathBuf, Arc<Root>>>,
    classification: ClassificationTables,
    /// Per-root generation counter, bumped by
    /// [`bump_generations()`](Self::bump_generations) when files are
    /// modified. Used by the [`SymbolIndex`] enrichment cache for
    /// invalidation.
    root_generations: std::sync::Mutex<HashMap<PathBuf, u64>>,
    /// Per-root last-seen mtimes for the LSP changed-set nudge (WS31 Consumer A).
    /// Inner key is the path **relative to the root** (the root prefix is the
    /// outer key, not repeated per entry). Tracks what the servers have been
    /// told — distinct from the Consumer-B cache floors, which track each cache
    /// entry's build mtime. Per-root inner lock so parallel-subagent worktrees
    /// don't contend; the outer lock only fetches/creates the inner `Arc<Mutex>`
    /// and is never held across the walk or an `.await`.
    last_seen: LastSeen,
    /// Per-root delivery journal — the per-server frontiers (bug 146 stage 3).
    /// Observation advances the shared baseline once; delivery is drained per
    /// server from here, so a server is told each change exactly once and a
    /// change that came and went between two of its consultations is never
    /// told at all. Same lock discipline as `last_seen`.
    journals: Mutex<HashMap<PathBuf, Arc<Mutex<RootJournal>>>>,
}

/// One root's delivery journal (bug 146 stage 3).
///
/// The baseline answers *what does disk look like now*; this answers *what has
/// each server been told*. Splitting them is what makes delivery per-server:
/// observation from any source (the probe, hit-file stats, the open-document
/// sweep, the diagnose walk) appends here once, and each server drains its own
/// frontier when it is about to be consulted.
#[derive(Debug, Default)]
struct RootJournal {
    /// Monotonic generation, bumped once per recorded change set.
    generation: u64,
    /// Recorded changes with the generation that carried them, oldest first.
    /// Pruned to what the furthest-behind live server still needs.
    entries: Vec<(u64, Change)>,
    /// Per-server delivery frontier: the last generation this server has been
    /// told about, keyed by **instance** identity (`language/server/scope`).
    /// It must be the instance, not the server name: one server can have
    /// several instances covering a root — a parent-scoped one and a
    /// subdirectory-scoped one — and each has its own idea of what it has been
    /// told.
    frontiers: HashMap<String, u64>,
}

/// Cache entry storing classification results keyed by mtime.
///
/// `mtime` is nanosecond-resolution ([`mtime_nanos`]) so a same-second content
/// edit (a host `Edit`/`Write` immediately followed by `glob`) is detected as a
/// change rather than served stale — without this the line count, binary flag,
/// and language id could lag a sub-second edit (same family as bug #26).
///
/// `kind` is `None` for seed-only entries (stat-only, no classification).
/// [`FilesystemManager::classify`] overwrites these on first access.
struct CachedEntry {
    mtime: i64,
    kind: Option<FileKind>,
}

impl Default for FilesystemManager {
    fn default() -> Self {
        Self {
            cache: std::sync::Mutex::new(HashMap::new()),
            roots: std::sync::Mutex::new(HashMap::new()),
            classification: ClassificationTables::default(),
            root_generations: std::sync::Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
            journals: Mutex::new(HashMap::new()),
        }
    }
}

impl FilesystemManager {
    /// Creates an empty manager with no classification tables.
    ///
    /// Use [`with_classification`](Self::with_classification) when
    /// language detection from config is needed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a manager with pre-built classification tables.
    #[must_use]
    pub fn with_classification(classification: ClassificationTables) -> Self {
        Self {
            cache: std::sync::Mutex::new(HashMap::new()),
            roots: std::sync::Mutex::new(HashMap::new()),
            classification,
            root_generations: std::sync::Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
            journals: Mutex::new(HashMap::new()),
        }
    }

    /// Classifies a file, using the cache when possible.
    ///
    /// Returns a [`FileInfo`] with binary/text classification, line count,
    /// and language ID. Cache is keyed by `(path, owning_root)` + mtime.
    /// On mtime change the entry is re-scanned.
    ///
    /// Classification precedence: shebang > filename > extension.
    pub fn classify(&self, path: &Path, metadata: &std::fs::Metadata) -> FileInfo {
        let mtime = mtime_secs(metadata);
        // Cache key uses nanosecond resolution so a same-second edit invalidates.
        let mtime_ns = mtime_nanos(metadata);
        let size = metadata.len();
        let root = self.resolve_root(path);
        let cache_key = (path.to_path_buf(), root.clone());

        // Check cache — skip unclassified (seed-only) entries.
        if let Ok(cache) = self.cache.lock()
            && let Some(entry) = cache.get(&cache_key)
            && entry.mtime == mtime_ns
            && let Some(ref kind) = entry.kind
        {
            return FileInfo {
                mtime,
                size,
                root,
                kind: kind.clone(),
            };
        }

        // Scan file for binary/text + line count + shebang.
        // Shebang is checked first here, but in practice it only matters
        // for extensionless scripts — `language_id()` short-circuits on
        // filename/extension before reaching `classify()`.
        let kind = scan_file(path).map_or(FileKind::Binary, |scan| {
            // Per-root shebang → per-root path → global shebang → global path.
            let language_id = root
                .as_ref()
                .and_then(|r| {
                    let roots = self
                        .roots
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    roots.get(r).and_then(|root| {
                        let tables = root.classification();
                        scan.shebang_interpreter
                            .as_deref()
                            .and_then(|interp| tables.lookup_shebang(interp))
                            .map(str::to_string)
                            .or_else(|| tables.classify_path(path))
                    })
                })
                .or_else(|| {
                    scan.shebang_interpreter
                        .as_deref()
                        .and_then(|interp| self.classification.lookup_shebang(interp))
                        .map(str::to_string)
                        .or_else(|| self.classification.classify_path(path))
                });
            FileKind::Text {
                lines: scan.lines,
                language_id,
            }
        });

        // Update cache
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(
                cache_key,
                CachedEntry {
                    mtime: mtime_ns,
                    kind: Some(kind.clone()),
                },
            );
        }

        FileInfo {
            mtime,
            size,
            root,
            kind,
        }
    }

    /// Returns `true` if the file is binary, using the cache when possible.
    pub fn is_binary(&self, path: &Path, metadata: &std::fs::Metadata) -> bool {
        matches!(self.classify(path, metadata).kind, FileKind::Binary)
    }

    /// Why a file is treated as unsearchable binary, or `None` when it is
    /// searchable text.
    ///
    /// A thin reason-carrying wrapper over [`is_binary`](Self::is_binary): the
    /// verdict is purely content-based (a NUL byte before any text BOM), so the
    /// only reason is [`BinarySkip::Binary`]. The former size-cap heuristic —
    /// which skipped large *text* files without reading a byte — is gone
    /// (misc 140, decision 029; the bug-62 mechanism), so a large pure-UTF-8 file
    /// is searchable text like any other (misc 135 keeps the honest skip line for
    /// genuinely binary content).
    pub fn binary_skip_reason(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> Option<BinarySkip> {
        if self.is_binary(path, metadata) {
            return Some(BinarySkip::Binary);
        }
        None
    }

    /// Returns the line count if the file is text, or `None` if binary or folder.
    pub fn line_count(&self, path: &Path, metadata: &std::fs::Metadata) -> Option<usize> {
        match self.classify(path, metadata).kind {
            FileKind::Binary | FileKind::Folder => None,
            FileKind::Text { lines, .. } => Some(lines),
        }
    }

    /// Returns the LSP language identifier for a file path, or `None` if unknown.
    ///
    /// Checks per-root classification tables first (if the file is in a
    /// known root), then falls back to global tables. Within each table
    /// set: filename/extension first (no I/O), then shebang detection.
    pub fn language_id(&self, path: &Path) -> Option<String> {
        // Per-root fast path: filename/extension (no I/O).
        if let Some(root) = self.resolve_root(path) {
            let roots = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(r) = roots.get(&root)
                && let Some(lang) = r.classification().classify_path(path)
            {
                return Some(lang);
            }
        }
        // Global fast path: filename/extension (no I/O).
        if let Some(lang) = self.classification.classify_path(path) {
            return Some(lang);
        }
        // Slow path: shebang detection for extensionless files.
        // Per-root shebang is checked inside `classify`.
        let metadata = std::fs::metadata(path).ok()?;
        self.classify(path, &metadata)
            .language_id()
            .map(str::to_string)
    }

    /// Returns the raw shebang interpreter basename for a file, or `None`.
    ///
    /// Resolves `#!/usr/bin/env bash` and `#!/bin/bash` alike to `bash`, reusing
    /// the same single-pass scan as classification (binary files and files
    /// without a `#!` line yield `None`). Unlike [`Self::language_id`], this
    /// returns the raw interpreter rather than a resolved language, so linter
    /// routing can match it against a linter's declared shebang list.
    #[must_use]
    pub fn shebang_interpreter(&self, path: &Path) -> Option<String> {
        scan_file(path).and_then(|scan| scan.shebang_interpreter)
    }

    /// Whether `linter` routes to `file` (whose root-relative path is `rel`).
    ///
    /// A file routes when its root-relative path matches one of the linter's
    /// path globs, **or** — for a linter that declares `shebangs` — when the
    /// file's `#!` interpreter is in that list. The shebang read is lazy: it is
    /// consulted only when the path globs miss and the linter declares shebangs,
    /// so a `.sh` path-glob match never touches the file. This is the routing
    /// predicate behind both the editing-boundary coverage gate
    /// ([`LspClientManager::lint_covers`]) and the diagnostics-batch fan-out
    /// ([`LinterFeeder`]).
    ///
    /// [`LspClientManager::lint_covers`]: crate::lsp::LspClientManager::lint_covers
    /// [`LinterFeeder`]: crate::bridge::linter::LinterFeeder
    #[must_use]
    pub fn linter_routes(&self, linter: &LinterConfig, file: &Path, rel: &Path) -> bool {
        if linter.matches(rel) {
            return true;
        }
        if linter.shebangs.is_empty() {
            return false;
        }
        self.shebang_interpreter(file)
            .is_some_and(|interp| linter.matches_shebang(&interp))
    }

    /// Resolves the owning workspace root for a path.
    ///
    /// Returns the longest-prefix match against known roots, or `None` if
    /// the path is outside all known roots.
    #[must_use]
    pub fn resolve_root(&self, path: &Path) -> Option<PathBuf> {
        let Ok(roots) = self.roots.lock() else {
            return None;
        };
        resolve_root_in_map(&roots, path)
    }

    /// Returns a sorted snapshot of the current workspace root paths.
    ///
    /// Sorted for deterministic order (the underlying map is unordered, and the
    /// daemon already feeds roots from an unordered set union, so callers must
    /// not rely on insertion order).
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = self
            .roots
            .lock()
            .map_or_else(|_| Vec::new(), |r| r.keys().cloned().collect());
        roots.sort();
        roots
    }

    /// Returns the [`Root`] for an exact path, if tracked.
    ///
    /// Returns a cheap `Arc` clone — the lock is released before the caller
    /// inspects the config/classification.
    #[must_use]
    pub fn root(&self, path: &Path) -> Option<Arc<Root>> {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
    }

    /// Returns a snapshot of all tracked [`Root`]s (cheap `Arc` clones).
    ///
    /// Used by config-bearing consumers (the manager's `project_commands`,
    /// orphan-server warnings) that need every root's config, not just its path.
    #[must_use]
    pub fn root_views(&self) -> Vec<Arc<Root>> {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// Updates the known workspace root set from bare paths.
    ///
    /// Builds [`Root::bare`] entries (default config, empty per-root
    /// classification) — for path-only callers and tests that never read
    /// per-root config. Production callers that carry config use
    /// [`set_roots_rich`](Self::set_roots_rich).
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        let map = roots
            .into_iter()
            .map(|p| (p.clone(), Arc::new(Root::bare(p))))
            .collect();
        if let Ok(mut current) = self.roots.lock() {
            *current = map;
        }
    }

    /// Replaces the workspace root set with config-complete [`Root`]s.
    ///
    /// The single chokepoint through which the daemon installs the root set:
    /// each `Root` already carries its loaded config + classification, so the
    /// path becomes resolvable and config-readable in one atomic map swap.
    /// Removed roots (absent from `roots`) drop out of the map, taking their
    /// per-root classification with them.
    pub fn set_roots_rich(&self, roots: Vec<Arc<Root>>) {
        let map = roots
            .into_iter()
            .map(|r| (r.path().to_path_buf(), r))
            .collect();
        if let Ok(mut current) = self.roots.lock() {
            *current = map;
        }
    }

    /// Scans workspace roots and returns the set of language keys that have
    /// matching files present among `configured_keys`.
    ///
    /// Respects `.gitignore` and skips hidden files. Uses filename/extension
    /// detection first, then full classification (including shebang) for
    /// files without a recognised extension. Falls back to the raw file
    /// extension for custom languages. Exits early once all configured
    /// languages have been detected.
    #[allow(clippy::implicit_hasher, reason = "All callers use the default hasher")]
    pub fn detect_workspace_languages(
        &self,
        roots: &[PathBuf],
        configured_keys: &HashSet<&str>,
    ) -> HashSet<String> {
        let mut detected = HashSet::new();

        for root in roots {
            if !root.exists() {
                continue;
            }

            let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();

            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }

                let path = entry.path();

                // Fast path: per-root then global filename/extension (no I/O).
                // Slow path: full classification (shebang detection).
                let lang = self.language_id(path);

                if let Some(ref lang) = lang {
                    if configured_keys.contains(lang.as_str()) {
                        detected.insert(lang.clone());
                    }
                } else if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && configured_keys.contains(ext)
                {
                    detected.insert(ext.to_string());
                }

                if detected.len() == configured_keys.len() {
                    return detected;
                }
            }
        }

        detected
    }

    /// Bumps the generation counter for each root that owns at least
    /// one of the given paths.
    ///
    /// Called after `process_files_batched` to invalidate the enrichment
    /// cache and result cache for affected roots.
    pub fn bump_generations(&self, paths: &[PathBuf]) {
        let mut affected_roots = HashSet::new();
        for path in paths {
            if let Some(root) = self.resolve_root(path) {
                affected_roots.insert(root);
            }
        }
        if let Ok(mut gens) = self.root_generations.lock() {
            for root in affected_roots {
                *gens.entry(root).or_insert(0) += 1;
            }
        }
    }

    /// Returns the current generation counter for a root.
    ///
    /// The generation starts at 0 and is bumped by
    /// [`bump_generations()`](Self::bump_generations) when files are
    /// modified. Used by the enrichment cache in [`SymbolIndex`] for
    /// staleness checks.
    #[must_use]
    pub fn root_generation(&self, root: &Path) -> u64 {
        self.root_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .copied()
            .unwrap_or(0)
    }

    /// Bumps the generation counter for a root (test-only).
    #[cfg(test)]
    pub fn bump_generation_for_test(&self, root: &Path) {
        if let Ok(mut gens) = self.root_generations.lock() {
            *gens.entry(root.to_path_buf()).or_insert(0) += 1;
        }
    }

    // ── Changed-set baseline (WS31 Consumer A) ────────────────────────

    /// Diffs a coherence walk's observations against the per-root baseline and
    /// merges the new mtimes in, returning the [`ChangeSet`] to route per server.
    ///
    /// `observed` is the set of `(relative-path, mtime)` pairs the walk visited
    /// for files matching some server's registered watch glob. Classification
    /// keys off whether this is the **first walk** for the root — the per-root
    /// key being absent from `last_seen` *before this diff*:
    ///
    /// - **first walk** (root key created here): every observed path is the cold
    ///   snapshot of pre-existing, already-indexed files ⇒ [`ChangeKind::Changed`]
    ///   (nothing was "created" relative to the server's startup knowledge; the
    ///   wire `FileChangeType` 2 and a `Change`-only watcher correctly receives
    ///   it while a `Create`-only watcher does not). Per the LSP spec,
    ///   `didChangeWatchedFiles` carries the true `FileChangeType`;
    ///   `didCreateFiles` is a different, editor-initiated notification we do not
    ///   use;
    /// - **populated baseline, absent** ⇒ [`ChangeKind::Created`] (a genuine
    ///   creation on a later walk; wire `FileChangeType` 1), record its mtime;
    /// - **present, mtime advanced** ⇒ [`ChangeKind::Changed`], update;
    /// - **present, unchanged** ⇒ nothing.
    ///
    /// The first walk *is* the snapshot — no separate seed. The first-walk marker
    /// also handles an initially-empty repo: the first walk creates the (possibly
    /// empty) key, so a file appearing on a later walk finds the key present ⇒ is
    /// a genuine `Created`. Deletion reaping (a baseline entry the walk did not
    /// visit) is a full-walk property specified with the gate in ticket 04; this
    /// records and updates only.
    ///
    /// **Lock discipline:** the outer `last_seen` lock is held only to test the
    /// first-walk marker (`contains_key`) and fetch-or-create the per-root
    /// `Arc<Mutex<…>>`; the inner per-root lock is held only for the short merge
    /// critical section. Neither is held across the walk (the walk runs in the
    /// caller, before this is called) nor across an `.await`.
    pub(crate) fn diff_and_update(&self, root: &Path, observed: &[(PathBuf, i64)]) -> ChangeSet {
        // Test the first-walk marker and fetch-or-create the per-root inner map
        // under the outer lock, then immediately release the outer lock — only
        // the inner lock is held for the merge. The marker is the root key being
        // absent *before* this get-or-insert: first walk ⇒ cold snapshot.
        let (inner, first_walk) = {
            let mut outer = self
                .last_seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let first_walk = !outer.contains_key(root);
            let inner = Arc::clone(
                outer
                    .entry(root.to_path_buf())
                    .or_insert_with(|| Arc::new(Mutex::new(HashMap::new()))),
            );
            drop(outer);
            (inner, first_walk)
        };

        let mut baseline = inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changes = Vec::new();
        for (rel, mtime) in observed {
            match baseline.get(rel) {
                None => {
                    baseline.insert(rel.clone(), *mtime);
                    // First walk ⇒ cold snapshot of already-indexed files ⇒
                    // Changed; absent on a populated baseline ⇒ genuine Created.
                    let kind = if first_walk {
                        ChangeKind::Changed
                    } else {
                        ChangeKind::Created
                    };
                    changes.push(Change {
                        rel: rel.clone(),
                        kind,
                    });
                }
                Some(&prev) if *mtime > prev => {
                    baseline.insert(rel.clone(), *mtime);
                    changes.push(Change {
                        rel: rel.clone(),
                        kind: ChangeKind::Changed,
                    });
                }
                Some(_) => {}
            }
        }
        drop(baseline);
        ChangeSet { changes }
    }

    /// Like [`diff_and_update`](Self::diff_and_update), but additionally **reaps
    /// deletions** — a full-walk-only property (WS31 ticket 04).
    ///
    /// `observed` is the **complete** set of `(relative-path, mtime)` pairs a
    /// *full* walk visited for watched files under `root`. After merging the
    /// Created/Changed deltas (identical to `diff_and_update`), any baseline
    /// entry whose relative path is **not** in `observed` is a file the walk
    /// did not see — it is gone from disk ⇒ a [`ChangeKind::Deleted`] change
    /// (wire `FileChangeType` 3, gated per server by the `Delete` watch-kind
    /// bit) — and it is dropped from the baseline so a later walk does not
    /// re-emit it.
    ///
    /// A scoped walk MUST NOT call this: it cannot assert that a baseline entry
    /// outside its pattern is gone (the file may simply be out of the scoped
    /// walk's breadth). Scoped callers use the non-reaping
    /// [`diff_and_update`](Self::diff_and_update).
    ///
    /// **Lock discipline:** identical to `diff_and_update` — the outer
    /// `last_seen` lock is held only to test the first-walk marker and
    /// fetch-or-create the per-root inner `Arc<Mutex<…>>`; the inner per-root
    /// lock is held only for the merge **and** the deletion sweep, with no
    /// `.await` and no walk inside it (the walk runs in the caller, before
    /// this is called).
    pub(crate) fn diff_update_and_reap(
        &self,
        root: &Path,
        observed: &[(PathBuf, i64)],
    ) -> ChangeSet {
        let (inner, first_walk) = {
            let mut outer = self
                .last_seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let first_walk = !outer.contains_key(root);
            let inner = Arc::clone(
                outer
                    .entry(root.to_path_buf())
                    .or_insert_with(|| Arc::new(Mutex::new(HashMap::new()))),
            );
            drop(outer);
            (inner, first_walk)
        };

        let mut baseline = inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changes = Vec::new();
        let observed_rels: HashSet<&Path> = observed.iter().map(|(rel, _)| rel.as_path()).collect();
        for (rel, mtime) in observed {
            match baseline.get(rel) {
                None => {
                    baseline.insert(rel.clone(), *mtime);
                    let kind = if first_walk {
                        ChangeKind::Changed
                    } else {
                        ChangeKind::Created
                    };
                    changes.push(Change {
                        rel: rel.clone(),
                        kind,
                    });
                }
                Some(&prev) if *mtime > prev => {
                    baseline.insert(rel.clone(), *mtime);
                    changes.push(Change {
                        rel: rel.clone(),
                        kind: ChangeKind::Changed,
                    });
                }
                Some(_) => {}
            }
        }

        // Deletion sweep: any baseline entry the full walk did not visit is gone
        // from disk ⇒ Deleted. On the first walk the baseline is created empty
        // (or is being populated above), so there is nothing to reap. Drop reaped
        // entries from the baseline so a later walk does not re-emit them.
        if !first_walk {
            let deleted: Vec<PathBuf> = baseline
                .keys()
                .filter(|rel| !observed_rels.contains(rel.as_path()))
                .cloned()
                .collect();
            for rel in deleted {
                baseline.remove(&rel);
                changes.push(Change {
                    rel,
                    kind: ChangeKind::Deleted,
                });
            }
        }
        drop(baseline);
        ChangeSet { changes }
    }

    // ── Per-server delivery frontiers (bug 146 stage 3) ───────────────

    /// Fetches (creating if needed) a root's delivery journal.
    fn journal_for(&self, root: &Path) -> Arc<Mutex<RootJournal>> {
        let mut outer = self.journals.lock().unwrap_or_else(PoisonError::into_inner);
        let journal = Arc::clone(outer.entry(root.to_path_buf()).or_default());
        drop(outer);
        journal
    }

    /// Records one observation round's changes on `root`'s delivery journal and
    /// returns the generation that was current **before** them — the floor a
    /// server not yet on the journal starts from.
    ///
    /// The floor matters: a server first seen at this nudge is told this
    /// round's changes and nothing older. It has just been consulted for the
    /// first time in this root's life (or has just come back), and the history
    /// before that is not news to it — its own startup index covered it.
    ///
    /// An empty change set bumps nothing: generations count *deliveries owed*,
    /// not nudges, which is exactly why nudges 2..N of a query burst deliver
    /// nothing at all.
    pub(crate) fn journal_changes(&self, root: &Path, changes: &[Change]) -> u64 {
        let journal = self.journal_for(root);
        let mut journal = journal.lock().unwrap_or_else(PoisonError::into_inner);
        let floor = journal.generation;
        if changes.is_empty() {
            return floor;
        }
        let generation = floor.saturating_add(1);
        journal.generation = generation;
        for change in changes {
            journal.entries.push((generation, change.clone()));
        }
        floor
    }

    /// Drains `server`'s frontier on `root`: the **net** diff of everything
    /// journalled since it was last told, with its frontier advanced to the
    /// current generation.
    ///
    /// Net, not replayed — the coalescing is the point. Per path, the first and
    /// last kinds since the frontier decide one delivery:
    ///
    /// | first → last | delivered | why |
    /// |---|---|---|
    /// | Created → Deleted | *nothing* | it came and went; this server never knew it existed |
    /// | Created → anything | `Created` | it is new to this server, whatever happened after |
    /// | Deleted → Deleted | `Deleted` | it is gone |
    /// | Deleted → Created/Changed | `Changed` | it exists again, with content this server has not seen |
    /// | Changed → Deleted | `Deleted` | it is gone |
    /// | Changed → anything | `Changed` | new content |
    ///
    /// The `Created → Deleted` row is where the delete/create flap dies
    /// structurally: no ordering luck, no suppression heuristic — a server
    /// simply is not told about a file whose whole life fell between two of its
    /// consultations.
    ///
    /// `floor` seeds a server that has no frontier yet (see
    /// [`journal_changes`](Self::journal_changes)).
    pub(crate) fn drain_frontier(&self, root: &Path, server: &str, floor: u64) -> Vec<Change> {
        let journal = self.journal_for(root);
        let mut journal = journal.lock().unwrap_or_else(PoisonError::into_inner);
        let from = *journal.frontiers.entry(server.to_string()).or_insert(floor);
        let generation = journal.generation;
        if from >= generation {
            return Vec::new();
        }

        // First and last kind per path, in journal order.
        let mut order: Vec<PathBuf> = Vec::new();
        let mut seen: HashMap<PathBuf, (ChangeKind, ChangeKind)> = HashMap::new();
        for (entry_gen, change) in &journal.entries {
            if *entry_gen <= from {
                continue;
            }
            seen.entry(change.rel.clone())
                .and_modify(|(_, last)| *last = change.kind)
                .or_insert_with(|| {
                    order.push(change.rel.clone());
                    (change.kind, change.kind)
                });
        }
        journal.frontiers.insert(server.to_string(), generation);
        drop(journal);

        order
            .into_iter()
            .filter_map(|rel| {
                let (first, last) = *seen.get(&rel)?;
                let kind = match (first, last) {
                    // Whole life between two consultations: say nothing.
                    (ChangeKind::Created, ChangeKind::Deleted) => return None,
                    (ChangeKind::Created, _) => ChangeKind::Created,
                    (_, ChangeKind::Deleted) => ChangeKind::Deleted,
                    // Back from the dead, or plain new content: either way it
                    // exists and carries content this server has not seen.
                    (ChangeKind::Deleted | ChangeKind::Changed, _) => ChangeKind::Changed,
                };
                Some(Change { rel, kind })
            })
            .collect()
    }

    /// Rewinds `server`'s frontier to `generation` — the delivery-failure
    /// recovery (bug 146 stage 3).
    ///
    /// A dropped notify is one server's problem, so it is one server's rewind:
    /// its next drain re-derives exactly what it missed, and no other server is
    /// re-told anything. (Before frontiers this had to be done by reverting the
    /// **shared** baseline, which re-emitted to every covering server.)
    pub(crate) fn rewind_frontier(&self, root: &Path, server: &str, generation: u64) {
        let journal = self.journal_for(root);
        let mut journal = journal.lock().unwrap_or_else(PoisonError::into_inner);
        journal
            .frontiers
            .entry(server.to_string())
            .and_modify(|f| *f = (*f).min(generation))
            .or_insert(generation);
    }

    /// Retires frontiers for servers no longer covering `root` and prunes
    /// journal entries every live frontier has passed.
    ///
    /// The journal is bounded by what the furthest-behind live server still
    /// needs — in the steady state (every covering server drained this round)
    /// that is nothing, so it empties each nudge. A server that goes away stops
    /// holding history: if it comes back it is a first-seen server again, told
    /// the current round and nothing older.
    pub(crate) fn retain_frontiers(&self, root: &Path, live: &HashSet<String>) {
        let journal = self.journal_for(root);
        let mut journal = journal.lock().unwrap_or_else(PoisonError::into_inner);
        journal.frontiers.retain(|name, _| live.contains(name));
        let keep_from = journal
            .frontiers
            .values()
            .copied()
            .min()
            .unwrap_or(u64::MAX);
        journal
            .entries
            .retain(|(entry_gen, _)| *entry_gen > keep_from);
    }

    /// A server's current delivery frontier on `root` (test observable).
    #[cfg(test)]
    pub(crate) fn frontier_of(&self, root: &Path, server: &str) -> u64 {
        let journal = self.journal_for(root);
        let journal = journal.lock().unwrap_or_else(PoisonError::into_inner);
        journal.frontiers.get(server).copied().unwrap_or(0)
    }

    /// Snapshots a root's baselined relative paths — the probe's candidate
    /// domain (bug 146).
    ///
    /// The supplemental watch probe is the deletion authority for watched
    /// patterns, and it can only condemn what it *looked at*. Two of its legs
    /// need the baseline to know where to look: the marker leg probes each
    /// known directory for each registered marker name, and the condemnation
    /// test asks whether a probed-and-absent path was ever baselined. Both
    /// read this snapshot; the lock is held only for the clone, never across
    /// the stats that follow (the probe's I/O runs outside every baseline
    /// lock).
    ///
    /// Empty for a root with no baseline yet — a cold root has nothing to
    /// condemn.
    pub(crate) fn baseline_paths(&self, root: &Path) -> Vec<PathBuf> {
        let inner = {
            let outer = self
                .last_seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(inner) = outer.get(root).map(Arc::clone) else {
                return Vec::new();
            };
            drop(outer);
            inner
        };
        let baseline = inner.lock().unwrap_or_else(PoisonError::into_inner);
        baseline.keys().cloned().collect()
    }

    /// Condemns paths the caller **looked at and did not find** (bug 146):
    /// each one still present in the baseline is dropped from it and returned
    /// as a [`ChangeKind::Deleted`] change.
    ///
    /// This is deletion by *targeted observation* rather than by a walk's
    /// claimed coverage. The caller — the supplemental watch probe — states
    /// its own coverage by construction: it condemns a path only after a stat
    /// of that exact path missed, so reap authority can never exceed
    /// observation coverage the way a filtered walk's did. A path absent from
    /// the baseline yields nothing (there is no deletion to announce for a
    /// file no server was ever told about).
    ///
    /// **Lock discipline:** identical to the diff paths — the outer lock is
    /// held only to fetch the per-root `Arc<Mutex<…>>`, the inner only for the
    /// removals. No I/O and no `.await` inside either: the stats that decided
    /// these paths ran in the caller, before this is called.
    pub(crate) fn condemn_absent(&self, root: &Path, absent: &[PathBuf]) -> Vec<Change> {
        if absent.is_empty() {
            return Vec::new();
        }
        let inner = {
            let outer = self
                .last_seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(inner) = outer.get(root).map(Arc::clone) else {
                return Vec::new();
            };
            drop(outer);
            inner
        };
        let mut baseline = inner.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changes = Vec::new();
        for rel in absent {
            if baseline.remove(rel).is_some() {
                changes.push(Change {
                    rel: rel.clone(),
                    kind: ChangeKind::Deleted,
                });
            }
        }
        drop(baseline);
        changes
    }

    /// Drops a root's changed-set baseline and generation counter.
    ///
    /// Called from the `sync_roots` `to_remove` cleanup when a root leaves the
    /// tracked set. Without this, removed-root entries accumulate (a memory
    /// leak) and a path later re-mounted by a different project would diff
    /// against a stale baseline. Re-adding the root starts fresh: the next walk
    /// is a cold-start full-candidate snapshot. Also drops the long-standing
    /// `root_generations` leak (inserted/bumped but never removed on root drop).
    pub fn remove_root_baseline(&self, root: &Path) {
        self.last_seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(root);
        // The delivery journal goes with it: a re-mounted root starts cold for
        // every server, exactly as its baseline does.
        self.journals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(root);
        self.root_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(root);
    }

    /// Returns `true` if a changed-set baseline exists for the root (test-only).
    #[cfg(test)]
    pub(crate) fn has_baseline_for_test(&self, root: &Path) -> bool {
        self.last_seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(root)
    }

    /// Returns `true` if a generation counter entry exists for the root
    /// (test-only).
    #[cfg(test)]
    pub(crate) fn has_generation_for_test(&self, root: &Path) -> bool {
        self.root_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(root)
    }
}

/// Formats a file size in human-readable form.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    reason = "display-only rounding is acceptable"
)]
pub fn format_file_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Resolves the owning workspace root for a path from the roots map.
///
/// Returns the longest-prefix match against the map keys, or `None` if the path
/// is outside all roots. Used by methods that already hold the roots lock to
/// avoid re-locking.
fn resolve_root_in_map(roots: &HashMap<PathBuf, Arc<Root>>, path: &Path) -> Option<PathBuf> {
    roots
        .keys()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())
        .cloned()
}

/// Extracts mtime as seconds since epoch (cross-platform).
fn mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Extracts mtime as nanoseconds since epoch, or `0` if unavailable.
///
/// Higher resolution than [`mtime_secs`]: the symbol-index staleness backstop
/// (bug #26) compares the populated mtime against the file's current mtime, and
/// a host `Edit`/`Write` immediately followed by `grep`/`glob` can land within
/// one wall-clock second. Nanosecond precision detects that change; on a
/// second-resolution filesystem the sub-second part is zero and it degrades to
/// the same granularity as [`mtime_secs`]. Saturates at `i64::MAX` (year 2262)
/// to fit a `SQLite` `INTEGER`.
pub(crate) fn mtime_nanos(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Number of fresh-stat attempts before treating a stat miss as genuine.
///
/// A transient `stat` miss races a sub-millisecond atomic-rename window (write
/// temp + `rename`), so a few tight retries (no sleep) close the in-workflow
/// case; a residual under a saturating concurrent-writer hammer is the
/// documented concurrent-writer non-goal.
pub(crate) const STAT_RETRY_ATTEMPTS: u32 = 3;

/// Sentinel mtime for an enumerated present file whose observation stat missed.
///
/// Recorded in the changed-set observation set for a file a walk *enumerated as
/// present* (passed its `is_file` decision) but whose fresh `metadata()` failed
/// even after [`STAT_RETRY_ATTEMPTS`] retries. Keeping the file in the
/// observation set means the reaping sweep never treats it as deleted
/// (WS31-review H1); `i64::MIN` is below every real [`mtime_nanos`], so the diff
/// never spuriously routes it as `Changed` against an existing baseline entry.
pub(crate) const OBSERVED_STAT_MISS_MTIME: i64 = i64::MIN;

/// Fetches `path`'s metadata, retrying a transient stat miss.
///
/// A changed-set observation push must not drop an enumerated present file just
/// because its *fresh* `metadata()` raced an atomic rename (or briefly returned
/// `EACCES`). The retry is bounded and sleepless: the rename window is
/// sub-millisecond. A residual miss after the last attempt is handled by the
/// caller with the [`OBSERVED_STAT_MISS_MTIME`] sentinel — never by omitting the
/// file from the observation set (WS31-review H1).
pub(crate) fn stat_with_retry(path: &Path) -> Option<std::fs::Metadata> {
    observe_metadata_with(path, STAT_RETRY_ATTEMPTS, |p| p.metadata().ok())
}

/// The single per-file changed-set observation step shared by every walk
/// surface (grep, diagnostics, glob): produce the file's `mtime` with a bounded
/// retry, falling back to [`OBSERVED_STAT_MISS_MTIME`] on a residual miss so the
/// enumerated-present file is **never omitted** from the observation set.
///
/// Centralizing this here keeps the three hand-rolled walk drivers from drifting
/// again (WS31-review F1): the walker shape differs (parallel `WalkState` vs
/// sequential `for entry`), but the per-entry "stat-with-retry, sentinel on
/// miss, never omit" contract is identical and lives in one place.
pub(crate) fn observe_mtime(path: &Path) -> i64 {
    observe_mtime_with(path, STAT_RETRY_ATTEMPTS, |p| p.metadata().ok())
}

/// Retry loop for [`observe_mtime`], with the per-attempt metadata probe
/// injected.
///
/// Production calls this via [`observe_mtime`] with the real `metadata()` probe
/// and [`STAT_RETRY_ATTEMPTS`]; tests inject a stateful probe (miss on attempt
/// 1, hit thereafter) to prove the loop actually retries, and a never-hit probe
/// to prove the [`OBSERVED_STAT_MISS_MTIME`] sentinel is emitted (never an
/// omission) — a regression to a single attempt, or to omitting the file, would
/// surface here (WS31-review F1/H1).
fn observe_mtime_with(
    path: &Path,
    attempts: u32,
    probe: impl Fn(&Path) -> Option<std::fs::Metadata>,
) -> i64 {
    observe_metadata_with(path, attempts, probe)
        .as_ref()
        .map_or(OBSERVED_STAT_MISS_MTIME, mtime_nanos)
}

/// Retry loop for [`stat_with_retry`], with the per-attempt metadata probe
/// injected.
fn observe_metadata_with(
    path: &Path,
    attempts: u32,
    probe: impl Fn(&Path) -> Option<std::fs::Metadata>,
) -> Option<std::fs::Metadata> {
    for attempt in 0..attempts {
        if let Some(md) = probe(path) {
            return Some(md);
        }
        // Yield between attempts (not after the last) so the scheduler can advance
        // the racing writer past its sub-µs atomic-rename window before the
        // re-stat. Cheap and `.await`-free (this is a sync helper). (walk-3)
        if attempt + 1 < attempts {
            std::thread::yield_now();
        }
    }
    None
}

// ─── content-byte instrumentation (bug 78 pathology tier / misc 159) ──
//
// A test-only tally of file content bytes read by [`scan_file`], so the
// bug-78 pathology tier can assert `--count` reads **zero** content bytes
// (the free win) as a work-based fact rather than a wall-clock guess.
#[cfg(test)]
static SCAN_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Content bytes read by [`scan_file`] since the last [`scan_bytes_reset`].
#[cfg(test)]
pub(crate) fn scan_bytes() -> usize {
    SCAN_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}
/// Zeroes the content-byte tally before a measured run.
#[cfg(test)]
pub(crate) fn scan_bytes_reset() {
    SCAN_BYTES.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// Intermediate result from a single-pass file scan.
struct ScanResult {
    lines: usize,
    shebang_interpreter: Option<String>,
}

/// Scans a file for its binary/text verdict, line count, and shebang in one pass.
///
/// Returns `Some(ScanResult)` for text files, `None` for binary files. The
/// verdict is content-based at **any size** (misc 140, decision 029): the file is
/// read in 8 KB chunks and declared binary at the first NUL byte — unless a text
/// byte-order mark opens it. A UTF-16/UTF-32 BOM marks NUL-dense text, so a BOM at
/// offset 0 suppresses the NUL verdict and the scan counts lines to EOF; a NUL
/// *before* any BOM is binary (rg's "binary file matches" contract stays declined,
/// misc 135). There is no size cap: a large pure-UTF-8 file is text and is counted
/// in full (the bug-62 fix — the former size cap misclassified such files unread).
fn scan_file(path: &Path) -> Option<ScanResult> {
    let Ok(file) = std::fs::File::open(path) else {
        return Some(ScanResult {
            lines: 0,
            shebang_interpreter: None,
        });
    };

    let mut reader = std::io::BufReader::new(file);
    let mut buf = [0u8; 8192];
    let mut lines = 0;
    let mut shebang_interpreter = None;
    let mut first_chunk = true;
    // A text byte-order mark at offset 0 marks NUL-dense UTF-16/UTF-32 text, so
    // the NUL verdict is suppressed for the rest of the scan (misc 140).
    let mut bom_text = false;

    loop {
        let Ok(n) = reader.read(&mut buf) else {
            return Some(ScanResult {
                lines,
                shebang_interpreter,
            });
        };
        if n == 0 {
            return Some(ScanResult {
                lines,
                shebang_interpreter,
            });
        }
        #[cfg(test)]
        SCAN_BYTES.fetch_add(n, std::sync::atomic::Ordering::Relaxed);

        // BOM detection precedes the NUL verdict: a UTF-16 BOM file is text even
        // though it is NUL-dense. Shebang extraction shares this first-chunk pass.
        if first_chunk {
            first_chunk = false;
            bom_text = starts_with_text_bom(&buf[..n]);
            let first_line_end = memchr::memchr(b'\n', &buf[..n]).unwrap_or(n);
            shebang_interpreter = extract_shebang_interpreter(&buf[..first_line_end]);
        }

        if !bom_text && memchr::memchr(0, &buf[..n]).is_some() {
            return None; // Binary: a NUL before any text BOM.
        }

        lines += memchr::memchr_iter(b'\n', &buf[..n]).count();
    }
}

/// Returns `true` when `head` opens with a UTF-8 or UTF-16 byte-order mark.
///
/// UTF-16 (and UTF-32 LE, which shares the `FF FE` prefix) text is NUL-dense, so
/// a quit-at-first-NUL scan would misclassify it as binary; a BOM at offset 0
/// marks it as text (misc 140). A NUL that appears *before* any BOM — including a
/// UTF-32 BE file, whose `00 00 FE FF` opens with NULs — stays binary, per the
/// ticket's NUL-before-BOM rule.
fn starts_with_text_bom(head: &[u8]) -> bool {
    head.starts_with(&[0xEF, 0xBB, 0xBF]) // UTF-8
        || head.starts_with(&[0xFF, 0xFE]) // UTF-16 LE (also UTF-32 LE prefix)
        || head.starts_with(&[0xFE, 0xFF]) // UTF-16 BE
}

/// Extracts the interpreter basename from a shebang line.
///
/// Returns the raw interpreter name without resolving it to a language ID.
/// Language resolution is done by the classification tables.
///
/// Handles both direct paths (`#!/bin/bash`) and `env` indirection
/// (`#!/usr/bin/env bash`). Flags after the interpreter are ignored.
fn extract_shebang_interpreter(first_line: &[u8]) -> Option<String> {
    let line = first_line.strip_prefix(b"#!")?;
    let line = line.trim_ascii_start();
    let line_str = std::str::from_utf8(line).ok()?;

    let mut parts = line_str.split_whitespace();
    let command = parts.next()?;

    // If command is /usr/bin/env (or similar), the interpreter is the next
    // non-flag argument.
    let interpreter = if command.ends_with("/env") {
        parts.find(|p| !p.starts_with('-'))?
    } else {
        command
    };

    let basename = interpreter.rsplit('/').next()?;
    Some(basename.to_string())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::io::Write;

    // --- Classification (migrated from FilesystemCache) ---

    /// A content edit that lands in the same wall-clock second as the cached
    /// classification must still invalidate the cache — the key is nanosecond
    /// resolution (same family as bug #26, where second-resolution would serve a
    /// stale line count after a fast host edit).
    #[test]
    fn classify_cache_invalidates_on_same_second_edit() {
        use std::time::{Duration, UNIX_EPOCH};

        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f.rs");
        let fs = FilesystemManager::new();

        // One line; pin mtime to a fixed instant.
        std::fs::write(&file, "one\n").expect("write");
        let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_mtime(&file, base);
        let lc1 = fs.line_count(&file, &std::fs::metadata(&file).expect("meta"));
        assert_eq!(lc1, Some(1), "initial line count");

        // Two lines; pin mtime to the SAME whole second (+1ms) — second-resolution
        // would treat this as unchanged and serve the stale count.
        std::fs::write(&file, "one\ntwo\n").expect("rewrite");
        set_mtime(&file, base + Duration::from_millis(1));
        let lc2 = fs.line_count(&file, &std::fs::metadata(&file).expect("meta"));
        assert_eq!(
            lc2,
            Some(2),
            "same-second edit must invalidate the classify cache"
        );
    }

    /// Sets a file's mtime to an explicit instant (test helper).
    fn set_mtime(path: &std::path::Path, t: std::time::SystemTime) {
        let f = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        f.set_modified(t).expect("set mtime");
    }

    #[test]
    fn classify_binary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binary.bin");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x89, 0x50, 0x4E, 0x47, 0x00, 0x0A])
            .expect("write");
        drop(f);

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(mgr.classify(&path, &metadata).kind, FileKind::Binary);
    }

    #[test]
    fn classify_text_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "Hello, world!\nLine two.\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 2,
                language_id: None,
            }
        );
    }

    #[test]
    fn classify_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 0,
                language_id: None,
            }
        );
    }

    #[test]
    fn line_count_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");

        // First call: scan + cache
        assert_eq!(mgr.line_count(&path, &metadata), Some(3));
        // Second call: cache hit (line count is now cached)
        assert_eq!(mgr.line_count(&path, &metadata), Some(3));
    }

    #[test]
    fn line_count_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("image.png");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x89, 0x50, 0x4E, 0x47, 0x00]).expect("write");
        drop(f);

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(mgr.line_count(&path, &metadata), None);
    }

    #[test]
    fn cache_populated_by_classify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cached.bin");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(&[0x00, 0x01, 0x02]).expect("write");
        drop(f);

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");

        assert!(mgr.is_binary(&path, &metadata));
        assert!(mgr.is_binary(&path, &metadata));

        let len = mgr.cache.lock().expect("lock").len();
        assert_eq!(len, 1);
    }

    // --- Default config classification ---

    /// Builds a `FilesystemManager` with tables from the default config.
    fn default_mgr() -> FilesystemManager {
        let config = crate::config::Config::default_with_classification();
        FilesystemManager::with_classification(ClassificationTables::from_config(&config))
    }

    #[test]
    fn test_default_config_loads() {
        let config = crate::config::Config::default_with_classification();
        let errors = config.validate();
        assert!(
            errors.is_empty(),
            "default config should validate: {errors:?}"
        );
    }

    #[test]
    fn test_classification_from_config() {
        let mgr = default_mgr();
        assert_eq!(
            mgr.classification.classify_path(Path::new("test.rs")),
            Some("rust".to_string()),
        );
        assert_eq!(
            mgr.classification.classify_path(Path::new("test.py")),
            Some("python".to_string()),
        );
        assert_eq!(
            mgr.classification.classify_path(Path::new("test.unknown")),
            None,
        );
        assert_eq!(
            mgr.classification.classify_path(Path::new("noextension")),
            None,
        );
    }

    #[test]
    fn test_filename_classification_from_config() {
        let mgr = default_mgr();
        assert_eq!(
            mgr.classification.classify_path(Path::new("Dockerfile")),
            Some("dockerfile".to_string()),
        );
        assert_eq!(
            mgr.classification.classify_path(Path::new("Makefile")),
            Some("makefile".to_string()),
        );
        assert_eq!(
            mgr.classification.classify_path(Path::new("PKGBUILD")),
            Some("shellscript".to_string()),
        );
        assert_eq!(
            mgr.classification.classify_path(Path::new("Justfile")),
            Some("just".to_string()),
        );
    }

    #[test]
    fn test_shebang_classification_from_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("my_script");
        std::fs::write(&path, "#!/bin/bash\necho hello\n").expect("write");

        let mgr = default_mgr();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 2,
                language_id: Some("shellscript".to_string()),
            }
        );
    }

    #[test]
    fn test_classification_precedence() {
        // shebang > filename > extension: a file with ruby shebang and
        // .py extension should be classified as ruby (shebang wins).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("script.py");
        std::fs::write(&path, "#!/usr/bin/env ruby\nprint('hello')\n").expect("write");

        let mgr = default_mgr();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 2,
                language_id: Some("ruby".to_string()),
            }
        );
    }

    // --- Shebang interpreter extraction ---

    #[test]
    fn shebang_direct_path() {
        assert_eq!(
            extract_shebang_interpreter(b"#!/bin/bash"),
            Some("bash".to_string()),
        );
    }

    #[test]
    fn shebang_env() {
        assert_eq!(
            extract_shebang_interpreter(b"#!/usr/bin/env python3"),
            Some("python3".to_string()),
        );
    }

    #[test]
    fn shebang_with_flags() {
        assert_eq!(
            extract_shebang_interpreter(b"#!/bin/bash -e"),
            Some("bash".to_string()),
        );
    }

    #[test]
    fn shebang_space_after_hash_bang() {
        assert_eq!(
            extract_shebang_interpreter(b"#! /bin/bash"),
            Some("bash".to_string()),
        );
    }

    #[test]
    fn shebang_env_with_flags() {
        assert_eq!(
            extract_shebang_interpreter(b"#!/usr/bin/env -S python3"),
            Some("python3".to_string()),
        );
    }

    #[test]
    fn shebang_unknown_interpreter() {
        assert_eq!(
            extract_shebang_interpreter(b"#!/usr/bin/env something_unknown"),
            Some("something_unknown".to_string()),
        );
    }

    #[test]
    fn no_shebang() {
        assert_eq!(extract_shebang_interpreter(b"hello world"), None);
    }

    // --- Integration: classify + shebang ---

    #[test]
    fn classify_extensionless_without_shebang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("data_file");
        std::fs::write(&path, "just some text\n").expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 1,
                language_id: None,
            }
        );
    }

    #[test]
    fn classify_binary_skips_shebang() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fake_script");
        let mut content = b"#!/bin/bash\n".to_vec();
        content.push(0x00);
        content.extend_from_slice(b"echo hello\n");
        std::fs::write(&path, &content).expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(mgr.classify(&path, &metadata).kind, FileKind::Binary);
    }

    // --- Content classification without a size cap (misc 140, bug 62) ---

    /// A pure-UTF-8 file well over the retired 10 MB cap classifies as text with
    /// a full streaming line count, not binary-by-size. This is the bug-62 fix:
    /// the former cap misclassified a large text file (a 15.7 MB minified bundle)
    /// as binary without reading a byte.
    #[test]
    fn classify_large_pure_utf8_is_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bundle.js");
        // 20 bytes/line * 600_000 = 12 MB, comfortably over the retired cap.
        let lines = 600_000;
        std::fs::write(&path, "the quick brown fox\n".repeat(lines)).expect("write");
        assert!(
            std::fs::metadata(&path).expect("metadata").len() > 10 * 1024 * 1024,
            "fixture must exceed the retired 10 MB cap"
        );

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert!(
            !mgr.is_binary(&path, &metadata),
            "large UTF-8 is not binary"
        );
        assert_eq!(
            mgr.line_count(&path, &metadata),
            Some(lines),
            "line count streams the whole file at any size (no early return)"
        );
        assert!(
            mgr.binary_skip_reason(&path, &metadata).is_none(),
            "a large pure-UTF-8 file is never a skip"
        );
    }

    /// A UTF-16LE-BOM file is NUL-dense but is text: the BOM check precedes the
    /// NUL verdict (misc 140).
    #[test]
    fn classify_utf16le_bom_is_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("utf16le.txt");
        let mut content = vec![0xFF, 0xFE]; // UTF-16 LE BOM
        for ch in "hi\nyo\n".chars() {
            content.push(ch as u8);
            content.push(0x00);
        }
        std::fs::write(&path, &content).expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 2,
                language_id: None,
            },
            "a UTF-16LE BOM file is text, not binary"
        );
    }

    /// A UTF-16BE-BOM file (NUL as the high byte of every code unit) is also text.
    #[test]
    fn classify_utf16be_bom_is_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("utf16be.txt");
        let mut content = vec![0xFE, 0xFF]; // UTF-16 BE BOM
        for ch in "hi\nyo\n".chars() {
            content.push(0x00);
            content.push(ch as u8);
        }
        std::fs::write(&path, &content).expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert!(
            matches!(mgr.classify(&path, &metadata).kind, FileKind::Text { .. }),
            "a UTF-16BE BOM file is text, not binary"
        );
    }

    /// A UTF-8-BOM prefix does not derail classification (no NULs, still text).
    #[test]
    fn classify_utf8_bom_is_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("utf8bom.txt");
        let mut content = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        content.extend_from_slice(b"hello world\n");
        std::fs::write(&path, &content).expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(
            mgr.classify(&path, &metadata).kind,
            FileKind::Text {
                lines: 1,
                language_id: None,
            }
        );
    }

    /// A NUL *before* any BOM is binary — including a `00 00 FE FF` head (which a
    /// UTF-32BE reader would take as a BOM): the ticket's NUL-before-BOM rule.
    #[test]
    fn classify_nul_before_bom_is_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nul_first.bin");
        std::fs::write(&path, [0x00, 0x00, 0xFE, 0xFF, b'h', b'i']).expect("write");

        let mgr = FilesystemManager::new();
        let metadata = std::fs::metadata(&path).expect("metadata");
        assert_eq!(mgr.classify(&path, &metadata).kind, FileKind::Binary);
    }

    // --- format_file_size ---

    #[test]
    fn format_file_size_units() {
        assert_eq!(format_file_size(0), "0 B");
        assert_eq!(format_file_size(512), "512 B");
        assert_eq!(format_file_size(1024), "1 KB");
        assert_eq!(format_file_size(1_048_576), "1.0 MB");
        assert_eq!(format_file_size(1_073_741_824), "1.0 GB");
        assert_eq!(format_file_size(5_368_709_120), "5.0 GB");
    }

    // --- Root resolution ---

    #[test]
    fn resolve_root_single_match() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/src/main.rs")),
            Some(PathBuf::from("/home/user/project"))
        );
    }

    #[test]
    fn resolve_root_outside_all_roots() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(mgr.resolve_root(Path::new("/other/path/file.rs")), None);
    }

    #[test]
    fn resolve_root_longest_prefix_wins() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![
            PathBuf::from("/home/user/project"),
            PathBuf::from("/home/user/project/subdir"),
        ]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/subdir/foo.rs")),
            Some(PathBuf::from("/home/user/project/subdir"))
        );
    }

    #[test]
    fn resolve_root_no_roots() {
        let mgr = FilesystemManager::new();
        assert_eq!(mgr.resolve_root(Path::new("/any/path/file.rs")), None);
    }

    #[test]
    fn set_roots_updates_resolution() {
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![PathBuf::from("/home/user/project")]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/src/main.rs")),
            Some(PathBuf::from("/home/user/project"))
        );

        mgr.set_roots(vec![PathBuf::from("/other/root")]);
        assert_eq!(
            mgr.resolve_root(Path::new("/home/user/project/src/main.rs")),
            None
        );
        assert_eq!(
            mgr.resolve_root(Path::new("/other/root/file.rs")),
            Some(PathBuf::from("/other/root"))
        );
    }

    #[test]
    fn classify_populates_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        let metadata = std::fs::metadata(&path).expect("metadata");
        let info = mgr.classify(&path, &metadata);
        assert_eq!(info.root, Some(dir.path().to_path_buf()));
    }

    #[test]
    fn classify_root_none_when_outside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        // No roots set
        let metadata = std::fs::metadata(&path).expect("metadata");
        let info = mgr.classify(&path, &metadata);
        assert_eq!(info.root, None);
    }

    // --- Linter routing (path glob + shebang) ---

    #[test]
    fn shebang_interpreter_reads_env_and_direct_forms() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_form = dir.path().join("a");
        std::fs::write(&env_form, "#!/usr/bin/env bash\necho hi\n").expect("write");
        let direct_form = dir.path().join("b");
        std::fs::write(&direct_form, "#!/bin/bash -e\necho hi\n").expect("write");
        let no_shebang = dir.path().join("c");
        std::fs::write(&no_shebang, "echo hi\n").expect("write");

        let mgr = FilesystemManager::new();
        assert_eq!(mgr.shebang_interpreter(&env_form).as_deref(), Some("bash"));
        assert_eq!(
            mgr.shebang_interpreter(&direct_form).as_deref(),
            Some("bash")
        );
        assert_eq!(mgr.shebang_interpreter(&no_shebang), None);
        assert_eq!(
            mgr.shebang_interpreter(dir.path().join("missing").as_path()),
            None
        );
    }

    #[test]
    fn linter_routes_matches_path_glob_and_shebang() {
        use crate::config::Config;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        // `.sh` routes by path glob; an extensionless bash script routes by
        // shebang alone (no `.sh` to match); a python script routes to neither.
        let sh = root.join("build.sh");
        std::fs::write(&sh, "echo hi\n").expect("write .sh");
        let extensionless = root.join("deploy");
        std::fs::write(&extensionless, "#!/usr/bin/env bash\necho hi\n").expect("write script");
        let py = root.join("run");
        std::fs::write(&py, "#!/usr/bin/env python3\nprint('hi')\n").expect("write py");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![root]);

        // The shipped default shellcheck: `**/*.sh` + shebangs [sh, bash, dash, ksh].
        let config = Config::load_from_sources(&[]).expect("load defaults");
        let shellcheck = config.linter.get("shellcheck").expect("default shellcheck");

        assert!(mgr.linter_routes(shellcheck, &sh, Path::new("build.sh")));
        assert!(
            mgr.linter_routes(shellcheck, &extensionless, Path::new("deploy")),
            "an extensionless bash script routes to shellcheck by shebang",
        );
        assert!(
            !mgr.linter_routes(shellcheck, &py, Path::new("run")),
            "a python shebang does not route to shellcheck",
        );
    }

    #[test]
    fn linter_routes_without_shebangs_is_path_glob_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let script = root.join("deploy");
        std::fs::write(&script, "#!/usr/bin/env bash\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![root.clone()]);

        // A yamllint-shaped linter declares no shebangs → an extensionless shell
        // script never routes, and a path-glob match short-circuits the file read.
        let yamllint =
            crate::config::LinterConfig::new("yamllint", vec![], vec!["**/*.yaml".to_string()])
                .expect("compile");
        assert!(!mgr.linter_routes(&yamllint, &script, Path::new("deploy")));
        assert!(mgr.linter_routes(&yamllint, &root.join("a.yaml"), Path::new("a.yaml")));
    }

    // --- Per-root classification ---

    /// Builds a `LanguageConfig` with classification fields for testing.
    fn lang_config_with_exts(exts: &[&str]) -> crate::config::LanguageConfig {
        crate::config::LanguageConfig {
            extensions: Some(exts.iter().map(|s| (*s).to_string()).collect()),
            ..Default::default()
        }
    }

    fn lang_config_with_filenames(names: &[&str]) -> crate::config::LanguageConfig {
        crate::config::LanguageConfig {
            filenames: Some(names.iter().map(|s| (*s).to_string()).collect()),
            ..Default::default()
        }
    }

    fn lang_config_with_shebangs(interps: &[&str]) -> crate::config::LanguageConfig {
        crate::config::LanguageConfig {
            shebangs: Some(interps.iter().map(|s| (*s).to_string()).collect()),
            ..Default::default()
        }
    }

    /// Builds an `Arc<Root>` carrying a project config whose `[lsp.language.*]`
    /// section is `languages` — the modern way to attach per-root
    /// classification (the classification tables are derived on the `Root`).
    fn root_with_langs(
        path: PathBuf,
        languages: HashMap<String, crate::config::LanguageConfig>,
    ) -> Arc<Root> {
        Arc::new(Root::new(
            path,
            ProjectConfig {
                language: languages,
                ..ProjectConfig::default()
            },
        ))
    }

    #[test]
    fn test_from_project_config_basic() {
        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        let tables = ClassificationTables::from_project_config(&languages);
        assert_eq!(
            tables.classify_path(Path::new("foo.pkg")),
            Some("pkgbuild".to_string()),
        );
        assert!(!tables.is_empty());
    }

    #[test]
    fn test_from_project_config_skips_no_classification() {
        let mut languages = HashMap::new();
        // Entry with only servers, no classification fields.
        languages.insert("rust".to_string(), crate::config::LanguageConfig::default());
        let tables = ClassificationTables::from_project_config(&languages);
        assert!(tables.is_empty());
    }

    #[test]
    fn test_per_root_classification_override() {
        let root_a = PathBuf::from("/projects/a");
        let root_b = PathBuf::from("/projects/b");

        let mgr = default_mgr();

        // Root A maps .pkg → pkgbuild; root B is bare.
        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        mgr.set_roots_rich(vec![
            root_with_langs(root_a, languages),
            Arc::new(Root::bare(root_b)),
        ]);

        // File in root A: .pkg → pkgbuild.
        assert_eq!(
            mgr.language_id(Path::new("/projects/a/foo.pkg")),
            Some("pkgbuild".to_string()),
        );
        // File in root B: .pkg → no match (not globally mapped).
        assert_eq!(mgr.language_id(Path::new("/projects/b/foo.pkg")), None);
    }

    #[test]
    fn test_per_root_classification_fallback() {
        let root_a = PathBuf::from("/projects/a");

        let mgr = default_mgr();
        mgr.set_roots(vec![root_a]);

        // Root A has no per-root tables.
        // .rs → rust from global tables.
        assert_eq!(
            mgr.language_id(Path::new("/projects/a/foo.rs")),
            Some("rust".to_string()),
        );
    }

    #[test]
    fn test_per_root_filename_classification() {
        let root_a = PathBuf::from("/projects/a");

        let mgr = default_mgr();

        let mut languages = HashMap::new();
        languages.insert(
            "custom".to_string(),
            lang_config_with_filenames(&["Taskfile"]),
        );
        mgr.set_roots_rich(vec![root_with_langs(root_a, languages)]);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/Taskfile")),
            Some("custom".to_string()),
        );
    }

    #[test]
    fn test_per_root_shebang_classification() {
        let root_a = tempfile::tempdir().expect("tempdir");
        let mgr = default_mgr();

        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_shebangs(&["deno"]));
        mgr.set_roots_rich(vec![root_with_langs(
            root_a.path().to_path_buf(),
            languages,
        )]);

        // Extensionless file with deno shebang in root A → custom.
        let path = root_a.path().join("script");
        std::fs::write(&path, "#!/usr/bin/env deno\nconsole.log('hi')\n").expect("write");

        assert_eq!(mgr.language_id(&path), Some("custom".to_string()),);
    }

    #[test]
    fn test_per_root_precedence_over_global() {
        let root_a = PathBuf::from("/projects/a");

        let mgr = default_mgr();

        // Root A maps .sh → custom-shell (global maps .sh → shellscript).
        let mut languages = HashMap::new();
        languages.insert("custom-shell".to_string(), lang_config_with_exts(&["sh"]));
        mgr.set_roots_rich(vec![root_with_langs(root_a, languages)]);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/test.sh")),
            Some("custom-shell".to_string()),
        );
    }

    #[test]
    fn test_unrooted_file_uses_global() {
        let root_a = PathBuf::from("/projects/a");

        let mgr = default_mgr();

        // Set per-root tables for root A.
        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_exts(&["xyz"]));
        mgr.set_roots_rich(vec![root_with_langs(root_a, languages)]);

        // File outside all roots uses global classification only.
        assert_eq!(
            mgr.language_id(Path::new("/other/path/foo.rs")),
            Some("rust".to_string()),
        );
        // Per-root extension not visible for unrooted files.
        assert_eq!(mgr.language_id(Path::new("/other/path/foo.xyz")), None);
    }

    #[test]
    fn test_set_roots_rich_attaches_classification() {
        let root = PathBuf::from("/projects/a");
        let mgr = FilesystemManager::new();

        // A bare root has no per-root tables.
        mgr.set_roots(vec![root.clone()]);
        assert_eq!(mgr.language_id(Path::new("/projects/a/foo.pkg")), None);

        // A config-bearing root carries its derived classification.
        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        mgr.set_roots_rich(vec![root_with_langs(root, languages)]);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/foo.pkg")),
            Some("pkgbuild".to_string()),
        );
    }

    #[test]
    fn test_dropped_root_drops_classification() {
        let root = PathBuf::from("/projects/a");
        let mgr = FilesystemManager::new();

        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        mgr.set_roots_rich(vec![root_with_langs(root, languages)]);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/foo.pkg")),
            Some("pkgbuild".to_string()),
        );

        // Replacing the set without the root drops its per-root classification —
        // falls back to global (None).
        mgr.set_roots_rich(vec![]);
        assert_eq!(mgr.language_id(Path::new("/projects/a/foo.pkg")), None);
    }

    #[test]
    fn test_detect_workspace_languages_per_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let mgr = FilesystemManager::new();

        // Create a file with a custom extension.
        std::fs::write(root.path().join("build.pkg"), "content\n").expect("write");

        // Per-root classification: .pkg → pkgbuild.
        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        mgr.set_roots_rich(vec![root_with_langs(root.path().to_path_buf(), languages)]);

        let configured: HashSet<&str> = std::iter::once("pkgbuild").collect();
        let detected = mgr.detect_workspace_languages(&[root.path().to_path_buf()], &configured);

        assert!(
            detected.contains("pkgbuild"),
            "per-root classification should be picked up by detection, got: {detected:?}",
        );
    }

    #[test]
    fn test_classify_uses_per_root_shebang() {
        let root = tempfile::tempdir().expect("tempdir");
        let mgr = default_mgr();

        // Per-root: deno → custom.
        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_shebangs(&["deno"]));
        mgr.set_roots_rich(vec![root_with_langs(root.path().to_path_buf(), languages)]);

        let path = root.path().join("script");
        std::fs::write(&path, "#!/usr/bin/env deno\nconsole.log('hi')\n").expect("write");

        let metadata = std::fs::metadata(&path).expect("metadata");
        let info = mgr.classify(&path, &metadata);
        assert_eq!(
            info.kind,
            FileKind::Text {
                lines: 2,
                language_id: Some("custom".to_string()),
            }
        );
    }

    #[test]
    fn bump_generations_for_affected_roots() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let file_a = dir_a.path().join("a.rs");
        let file_b = dir_b.path().join("b.rs");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()]);

        // Both roots start at generation 0.
        assert_eq!(mgr.root_generation(dir_a.path()), 0);
        assert_eq!(mgr.root_generation(dir_b.path()), 0);

        // Bump for file in root A only.
        mgr.bump_generations(std::slice::from_ref(&file_a));
        assert_eq!(mgr.root_generation(dir_a.path()), 1);
        assert_eq!(mgr.root_generation(dir_b.path()), 0);

        // Bump for files in both roots.
        mgr.bump_generations(&[file_a, file_b]);
        assert_eq!(mgr.root_generation(dir_a.path()), 2);
        assert_eq!(mgr.root_generation(dir_b.path()), 1);
    }

    // ── Changed-set baseline (diff_and_update / remove_root_baseline) ──

    /// The first diff against an empty baseline yields every observed path as a
    /// `Changed` candidate — the cold snapshot of pre-existing, already-indexed
    /// files (nothing was "created" relative to the server's startup knowledge).
    #[test]
    fn diff_and_update_first_walk_is_full_candidate_set() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let observed = vec![
            (PathBuf::from("a.rs"), 100),
            (PathBuf::from("src/b.rs"), 200),
        ];
        let set = mgr.diff_and_update(&root, &observed);
        assert_eq!(set.changes.len(), 2, "cold baseline ⇒ all observed");
        assert!(
            set.changes.iter().all(|c| c.kind == ChangeKind::Changed),
            "first-walk cold snapshot ⇒ Changed (not Created)"
        );
    }

    /// A file absent from a baseline that *already existed* (a later walk, not the
    /// first) is a genuine `Created`: seed via a first walk, then walk again with
    /// a brand-new rel path present.
    #[test]
    fn diff_and_update_new_file_after_seed_is_created() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");

        // First walk seeds the baseline (the root key now exists).
        let _ = mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 100)]);

        // Second walk: a NEW path absent from the populated baseline ⇒ Created.
        let set = mgr.diff_and_update(
            &root,
            &[(PathBuf::from("a.rs"), 100), (PathBuf::from("b.rs"), 200)],
        );
        assert_eq!(set.changes.len(), 1, "only the new path is a change");
        assert_eq!(set.changes[0].rel, PathBuf::from("b.rs"));
        assert_eq!(
            set.changes[0].kind,
            ChangeKind::Created,
            "absent on a populated baseline ⇒ genuine Created"
        );
    }

    /// A second diff with no mtime change yields an empty set (bug-38 no-repeat).
    #[test]
    fn diff_and_update_second_walk_no_change_is_empty() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let observed = vec![(PathBuf::from("a.rs"), 100)];
        let _ = mgr.diff_and_update(&root, &observed);
        let set = mgr.diff_and_update(&root, &observed);
        assert!(set.is_empty(), "unchanged mtime ⇒ nothing");
    }

    /// An advanced mtime on a previously-seen path yields a `Changed` (not
    /// `Created`) candidate; a regressed/equal mtime yields nothing.
    #[test]
    fn diff_and_update_advanced_mtime_is_changed() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let _ = mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 100)]);

        let set = mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 150)]);
        assert_eq!(set.changes.len(), 1);
        assert_eq!(set.changes[0].kind, ChangeKind::Changed);

        // Equal mtime ⇒ nothing; a stale (lower) mtime ⇒ nothing.
        assert!(
            mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 150)])
                .is_empty()
        );
        assert!(
            mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 120)])
                .is_empty()
        );
    }

    /// `remove_root_baseline` drops both the `last_seen` baseline and the
    /// `root_generations` entry; re-seeding the root yields a fresh first-walk
    /// full set.
    #[test]
    fn remove_root_baseline_drops_both_and_resets() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");

        let _ = mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 100)]);
        mgr.bump_generation_for_test(&root);
        assert!(mgr.has_baseline_for_test(&root));
        assert!(mgr.has_generation_for_test(&root));

        mgr.remove_root_baseline(&root);
        assert!(!mgr.has_baseline_for_test(&root), "last_seen entry dropped");
        assert!(
            !mgr.has_generation_for_test(&root),
            "root_generations entry dropped"
        );

        // Re-seed ⇒ fresh cold-start full set (a first walk again ⇒ Changed).
        let set = mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 100)]);
        assert_eq!(set.changes.len(), 1);
        assert_eq!(set.changes[0].kind, ChangeKind::Changed);
    }

    /// A full-walk reap drops a baseline entry the walk did not visit and emits
    /// it as `Deleted`. On the first walk there is nothing to reap.
    #[test]
    fn diff_update_and_reap_emits_deleted_for_unvisited_baseline_entry() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");

        // First walk seeds the baseline with two files (cold snapshot, no reap).
        let first = mgr.diff_update_and_reap(
            &root,
            &[(PathBuf::from("a.rs"), 100), (PathBuf::from("b.rs"), 100)],
        );
        assert!(
            first.changes.iter().all(|c| c.kind == ChangeKind::Changed),
            "first walk is the cold snapshot ⇒ all Changed, none Deleted"
        );

        // Second walk visits only a.rs ⇒ b.rs is gone ⇒ Deleted, and dropped.
        let second = mgr.diff_update_and_reap(&root, &[(PathBuf::from("a.rs"), 100)]);
        let deleted: Vec<&PathBuf> = second
            .changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Deleted)
            .map(|c| &c.rel)
            .collect();
        assert_eq!(
            deleted,
            vec![&PathBuf::from("b.rs")],
            "unvisited baseline entry b.rs must be reaped as Deleted"
        );

        // Third walk (a.rs still present, b.rs already reaped) ⇒ nothing.
        let third = mgr.diff_update_and_reap(&root, &[(PathBuf::from("a.rs"), 100)]);
        assert!(
            third.is_empty(),
            "a reaped entry must not re-emit on a later walk: {:?}",
            third.changes
        );
    }

    /// The non-reaping `diff_and_update` (the scoped glob path) never emits a
    /// `Deleted`, even when a previously-seen baseline entry is absent.
    #[test]
    fn diff_and_update_never_reaps() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");

        let _ = mgr.diff_and_update(
            &root,
            &[(PathBuf::from("a.rs"), 100), (PathBuf::from("b.rs"), 100)],
        );
        // Second walk omits b.rs — a scoped walk cannot assert it's gone.
        let set = mgr.diff_and_update(&root, &[(PathBuf::from("a.rs"), 100)]);
        assert!(
            set.changes.iter().all(|c| c.kind != ChangeKind::Deleted),
            "diff_and_update (scoped) must never reap: {:?}",
            set.changes
        );
    }

    /// Baselines are per-root: identical relative paths under different roots
    /// diff independently.
    #[test]
    fn diff_and_update_is_per_root() {
        let mgr = FilesystemManager::new();
        let root_a = PathBuf::from("/a");
        let root_b = PathBuf::from("/b");
        let observed = vec![(PathBuf::from("x.rs"), 100)];

        assert_eq!(mgr.diff_and_update(&root_a, &observed).changes.len(), 1);
        // Same rel path, different root ⇒ still a fresh candidate.
        assert_eq!(mgr.diff_and_update(&root_b, &observed).changes.len(), 1);
        // Re-walking root A with the same mtime ⇒ nothing.
        assert!(mgr.diff_and_update(&root_a, &observed).is_empty());
    }

    // ── Per-server delivery frontiers (bug 146 stage 3) ───────────────

    /// Records one round of changes and returns the pre-round generation.
    fn journal(mgr: &FilesystemManager, root: &Path, changes: &[Change]) -> u64 {
        mgr.journal_changes(root, changes)
    }

    /// One change, spelled compactly.
    fn change(rel: &str, kind: ChangeKind) -> Change {
        Change {
            rel: PathBuf::from(rel),
            kind,
        }
    }

    /// Delivery is per server: each drains its own frontier, so a change is
    /// delivered to a server exactly once no matter how many other servers have
    /// already taken it, and a server already current takes nothing.
    #[test]
    fn each_server_drains_its_own_frontier_exactly_once() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let floor = journal(&mgr, &root, &[change("a.rs", ChangeKind::Changed)]);

        let first = mgr.drain_frontier(&root, "alpha", floor);
        assert_eq!(first.len(), 1, "alpha is told once");
        assert!(
            mgr.drain_frontier(&root, "alpha", floor).is_empty(),
            "and never again — a drained frontier is current"
        );
        let other = mgr.drain_frontier(&root, "beta", floor);
        assert_eq!(
            other.len(),
            1,
            "beta's delivery is independent of alpha's: {other:?}"
        );
    }

    /// A file created and deleted between two of a server's consultations is
    /// never mentioned to it. The flap dies in the coalescing — no ordering
    /// luck, no suppression heuristic.
    #[test]
    fn created_then_deleted_between_consultations_delivers_nothing() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let floor = journal(&mgr, &root, &[change("tmp.rs", ChangeKind::Created)]);
        let _ = journal(&mgr, &root, &[change("tmp.rs", ChangeKind::Deleted)]);

        assert!(
            mgr.drain_frontier(&root, "alpha", floor).is_empty(),
            "a file whose whole life fell between two consultations is not news"
        );
    }

    /// The rest of the coalescing table: several journalled kinds collapse to
    /// the one delivery that describes the file's net move.
    #[test]
    fn net_diff_collapses_a_paths_history_to_one_delivery() {
        let cases = [
            (
                vec![ChangeKind::Created, ChangeKind::Changed],
                Some(ChangeKind::Created),
            ),
            (
                vec![ChangeKind::Changed, ChangeKind::Changed],
                Some(ChangeKind::Changed),
            ),
            (
                vec![ChangeKind::Changed, ChangeKind::Deleted],
                Some(ChangeKind::Deleted),
            ),
            // Back from the dead: the server knew a version, was told nothing,
            // and the file now exists with content it has not seen.
            (
                vec![ChangeKind::Deleted, ChangeKind::Created],
                Some(ChangeKind::Changed),
            ),
            (vec![ChangeKind::Created, ChangeKind::Deleted], None),
        ];
        for (history, expected) in cases {
            let mgr = FilesystemManager::new();
            let root = PathBuf::from("/root");
            let mut floor = None;
            for kind in &history {
                let before = journal(&mgr, &root, &[change("f.rs", *kind)]);
                floor.get_or_insert(before);
            }
            let drained = mgr.drain_frontier(&root, "alpha", floor.unwrap_or(0));
            assert_eq!(
                drained.first().map(|c| c.kind),
                expected,
                "history {history:?} must collapse to {expected:?}, got {drained:?}"
            );
        }
    }

    /// A dropped notify rewinds only the failing server: its next drain
    /// re-derives what it missed, and no other server is re-told anything.
    #[test]
    fn a_rewound_frontier_redelivers_only_to_that_server() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let floor = journal(&mgr, &root, &[change("a.rs", ChangeKind::Changed)]);

        assert_eq!(mgr.drain_frontier(&root, "alpha", floor).len(), 1);
        assert_eq!(mgr.drain_frontier(&root, "beta", floor).len(), 1);

        // alpha's notify failed.
        mgr.rewind_frontier(&root, "alpha", floor);
        assert_eq!(
            mgr.drain_frontier(&root, "alpha", floor).len(),
            1,
            "the failed server re-derives its missed delivery"
        );
        assert!(
            mgr.drain_frontier(&root, "beta", floor).is_empty(),
            "a healthy server is not splashed by another's failure"
        );
    }

    /// A server first seen at this round starts at the round's floor: it is
    /// told this round and nothing older (its own startup index covered that).
    #[test]
    fn a_first_seen_server_starts_at_the_current_round() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let _ = journal(&mgr, &root, &[change("old.rs", ChangeKind::Changed)]);
        let floor = journal(&mgr, &root, &[change("new.rs", ChangeKind::Changed)]);

        let drained = mgr.drain_frontier(&root, "latecomer", floor);
        assert_eq!(drained.len(), 1, "only this round: {drained:?}");
        assert_eq!(drained[0].rel, PathBuf::from("new.rs"));
    }

    /// The journal is bounded by what the furthest-behind live server needs:
    /// once every live server has drained, it empties. A server that stops
    /// covering the root stops holding history.
    #[test]
    fn retain_frontiers_prunes_history_and_retires_departed_servers() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let floor = journal(&mgr, &root, &[change("a.rs", ChangeKind::Changed)]);
        let _ = mgr.drain_frontier(&root, "alpha", floor);
        let _ = mgr.drain_frontier(&root, "gone", floor);

        let live: HashSet<String> = std::iter::once("alpha".to_string()).collect();
        mgr.retain_frontiers(&root, &live);
        assert_eq!(
            mgr.frontier_of(&root, "gone"),
            0,
            "departed frontier retired"
        );

        // A fresh round: alpha (still live) drains it; the departed server, if
        // it returns, is a first-seen server again.
        let floor = journal(&mgr, &root, &[change("b.rs", ChangeKind::Changed)]);
        assert_eq!(mgr.drain_frontier(&root, "alpha", floor).len(), 1);
    }

    /// An observation round with no changes bumps no generation, so a server
    /// that is already current is asked for nothing — nudges 2..N of a query
    /// burst induce no delivery at all.
    #[test]
    fn an_empty_round_delivers_nothing_and_advances_no_generation() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");
        let floor = journal(&mgr, &root, &[change("a.rs", ChangeKind::Changed)]);
        let _ = mgr.drain_frontier(&root, "alpha", floor);

        let next_floor = journal(&mgr, &root, &[]);
        assert_eq!(next_floor, floor + 1, "no changes ⇒ no new generation");
        assert!(
            mgr.drain_frontier(&root, "alpha", next_floor).is_empty(),
            "an already-current server takes nothing from an empty round"
        );
    }

    /// C1/F1 (helper isolation unit) — the shared per-file observation step
    /// `observe_mtime_with` (used by both the grep walker and `stat_walk`) must
    /// NEVER omit an enumerated present file on a stat miss: a residual miss
    /// yields the `OBSERVED_STAT_MISS_MTIME` sentinel, not an omission. This unit
    /// pins the helper's contract in ISOLATION; the diagnostics-surface
    /// behavioral guard that the omission would false-reap a present file through
    /// `stat_walk` → `reap=true` lives in
    /// `tests/ws31_review.rs::ws31_review_d_diagnostics_stat_miss_not_reaped`.
    ///
    /// Driven via the `#[cfg(test)]` injectable probe seam (mirrors R2/L4): a
    /// stateful probe deterministically fails the first call and succeeds later,
    /// so the test pins (a) the sentinel on a never-hit miss, (b) recovery within
    /// the full retry budget, and (c) retry-count sensitivity — a regression to
    /// a single attempt would surface the sentinel where recovery is expected.
    #[test]
    fn observe_mtime_with_emits_sentinel_never_omits() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        const {
            assert!(
                STAT_RETRY_ATTEMPTS >= 2,
                "the retry guard assumes more than one attempt"
            );
        }

        let path = Path::new("/does/not/matter");

        // (a) A probe that NEVER returns metadata ⇒ the sentinel, never omitted.
        let sentinel = observe_mtime_with(path, STAT_RETRY_ATTEMPTS, |_| None);
        assert_eq!(
            sentinel, OBSERVED_STAT_MISS_MTIME,
            "an enumerated present file whose stat keeps missing must record the \
             OBSERVED_STAT_MISS_MTIME sentinel (so the reap sweep never deletes \
             it), never be omitted"
        );

        // (b) A probe that misses on call 1 and hits thereafter ⇒ the full
        // budget recovers the real mtime (a non-sentinel value).
        let calls = AtomicUsize::new(0);
        let probe = |p: &Path| {
            if calls.fetch_add(1, Ordering::Relaxed) >= 1 {
                std::fs::metadata(p).ok().or_else(|| {
                    // The path does not exist; synthesize a hit by statting a real
                    // file (this crate's Cargo.toml) so we get a genuine mtime.
                    std::fs::metadata(env!("CARGO_MANIFEST_DIR")).ok()
                })
            } else {
                None
            }
        };
        let recovered = observe_mtime_with(path, STAT_RETRY_ATTEMPTS, probe);
        assert_ne!(
            recovered, OBSERVED_STAT_MISS_MTIME,
            "the bounded retry must recover a miss that resolves on a later \
             attempt (a real mtime, not the sentinel)"
        );

        // (c) Same fail-first probe, single attempt: no retry ⇒ the sentinel,
        // pinning retry-count sensitivity (a STAT_RETRY_ATTEMPTS = 1 regression
        // would surface here).
        let calls = AtomicUsize::new(0);
        let probe = |p: &Path| {
            if calls.fetch_add(1, Ordering::Relaxed) >= 1 {
                std::fs::metadata(env!("CARGO_MANIFEST_DIR")).ok()
            } else {
                let _ = p;
                None
            }
        };
        let single = observe_mtime_with(path, 1, probe);
        assert_eq!(
            single, OBSERVED_STAT_MISS_MTIME,
            "a single attempt cannot recover a transient miss ⇒ sentinel"
        );
    }
}
