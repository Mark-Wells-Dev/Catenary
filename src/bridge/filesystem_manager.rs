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

/// Files above this size are assumed binary without reading.
const BINARY_SIZE_THRESHOLD: u64 = 10 * 1024 * 1024; // 10 MB

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
    /// Binary file (contains null bytes or exceeds size threshold).
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
    /// Returns `true` when no path changed since the last walk.
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

/// Cross-tool filesystem classification cache.
///
/// Single authority for file metadata: binary detection, line count,
/// language ID, and shebang detection. Shared by `GrepServer` and
/// `GlobServer` through `Session`.
///
/// Also owns the workspace root list for longest-prefix root resolution
/// and the classification lookup tables built from config.
pub struct FilesystemManager {
    /// Cache keyed by `(file_path, owning_root)`. The root component
    /// ensures that root changes (add/remove) cause cache misses,
    /// preventing stale `language_id` from per-root classification.
    cache: std::sync::Mutex<HashMap<(PathBuf, Option<PathBuf>), CachedEntry>>,
    roots: std::sync::Mutex<Vec<PathBuf>>,
    classification: ClassificationTables,
    per_root_classification: std::sync::Mutex<HashMap<PathBuf, ClassificationTables>>,
    /// Per-root generation counter, bumped by
    /// [`bump_generations()`](Self::bump_generations) when files are
    /// modified. Used by [`SymbolIndex`] enrichment cache and
    /// [`ResultCache`] for invalidation.
    root_generations: std::sync::Mutex<HashMap<PathBuf, u64>>,
    /// Per-root last-seen mtimes for the LSP changed-set nudge (WS31 Consumer A).
    /// Inner key is the path **relative to the root** (the root prefix is the
    /// outer key, not repeated per entry). Tracks what the servers have been
    /// told — distinct from the Consumer-B cache floors, which track each cache
    /// entry's build mtime. Per-root inner lock so parallel-subagent worktrees
    /// don't contend; the outer lock only fetches/creates the inner `Arc<Mutex>`
    /// and is never held across the walk or an `.await`.
    last_seen: LastSeen,
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
            roots: std::sync::Mutex::new(Vec::new()),
            classification: ClassificationTables::default(),
            per_root_classification: std::sync::Mutex::new(HashMap::new()),
            root_generations: std::sync::Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
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
            roots: std::sync::Mutex::new(Vec::new()),
            classification,
            per_root_classification: std::sync::Mutex::new(HashMap::new()),
            root_generations: std::sync::Mutex::new(HashMap::new()),
            last_seen: Mutex::new(HashMap::new()),
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
        let kind = scan_file(path, metadata).map_or(FileKind::Binary, |scan| {
            // Per-root shebang → per-root path → global shebang → global path.
            let language_id = root
                .as_ref()
                .and_then(|r| {
                    let per_root = self
                        .per_root_classification
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    per_root.get(r).and_then(|tables| {
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
            let per_root = self
                .per_root_classification
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(tables) = per_root.get(&root)
                && let Some(lang) = tables.classify_path(path)
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

    /// Resolves the owning workspace root for a path.
    ///
    /// Returns the longest-prefix match against known roots, or `None` if
    /// the path is outside all known roots.
    #[must_use]
    pub fn resolve_root(&self, path: &Path) -> Option<PathBuf> {
        let Ok(roots) = self.roots.lock() else {
            return None;
        };
        resolve_root_in(&roots, path)
    }

    /// Returns a snapshot of the current workspace roots.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().map_or_else(|_| Vec::new(), |r| r.clone())
    }

    /// Updates the known workspace root set.
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        if let Ok(mut current) = self.roots.lock() {
            *current = roots;
        }
    }

    /// Sets per-root classification tables from a project config.
    ///
    /// Called by the manager during `spawn_all` and `sync_roots`.
    /// Replaces any existing per-root tables for the given root.
    pub fn set_root_classification(&self, root: PathBuf, tables: ClassificationTables) {
        if let Ok(mut per_root) = self.per_root_classification.lock() {
            per_root.insert(root, tables);
        }
    }

    /// Removes per-root classification tables for a root.
    ///
    /// Called when a root is removed.
    pub fn remove_root_classification(&self, root: &Path) {
        if let Ok(mut per_root) = self.per_root_classification.lock() {
            per_root.remove(root);
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
    /// modified. Used by the enrichment cache in [`SymbolIndex`] and
    /// [`ResultCache`] for staleness checks.
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
        self.root_generations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(root);
    }

    /// Reverts a set of just-merged changes in a root's baseline so the **next**
    /// walk re-emits them (WS31-review F4 — best-effort delivery recovery).
    ///
    /// [`nudge_changed_set`](crate::lsp::manager::LspClientManager::nudge_changed_set)
    /// advances the per-root baseline **once** (steps via
    /// [`diff_and_update`](Self::diff_and_update) /
    /// [`diff_update_and_reap`](Self::diff_update_and_reap)) *before* the
    /// per-server notify loop, and that baseline is **shared** across every server
    /// covering the root. So a `workspace/didChangeWatchedFiles` notify that fails
    /// for one covering server would otherwise lose those changes for it
    /// permanently: the next walk diffs against the already-advanced shared
    /// baseline and emits nothing (even across a respawn — the baseline is torn
    /// down only by [`remove_root_baseline`](Self::remove_root_baseline) on
    /// `roots rm`). Reverting the affected entries makes the next walk re-emit them
    /// to **all** covering servers; a duplicate `didChangeWatchedFiles` to a server
    /// that already received the change is harmless/idempotent.
    ///
    /// Per change kind:
    /// - [`ChangeKind::Created`] / [`ChangeKind::Changed`] — the entry was inserted
    ///   or its mtime advanced, so the entry is **removed**. The next walk finds it
    ///   absent from a populated baseline ⇒ re-emits it (as `Created`, or `Changed`
    ///   if it is the only baselined path on a fresh root — both are safe
    ///   re-notifications that no longer lose the change).
    /// - [`ChangeKind::Deleted`] — the reaping sweep already **removed** the entry,
    ///   and the file is gone from disk, so removal would not re-emit anything (the
    ///   next walk never observes a deleted file). Instead the entry is
    ///   **re-inserted** with the [`OBSERVED_STAT_MISS_MTIME`] sentinel so the next
    ///   **full** walk's reaping sweep — which reaps any baseline entry the walk did
    ///   not visit — re-emits the `Deleted`. **Limitation:** this only re-emits on a
    ///   *full* walk (`reap = true`); a subsequent scoped walk never reaps, so a
    ///   Deleted lost to a notify failure waits for the next full walk
    ///   (`grep`/`diagnostics`). If the file reappears before that walk, the
    ///   sentinel (`i64::MIN`, below every real mtime) makes it re-emit as `Changed`
    ///   rather than a spurious nothing — a safe over-notification.
    pub(crate) fn revert_baseline_changes(&self, root: &Path, changes: &[Change]) {
        let inner = {
            let outer = self
                .last_seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // No baseline for the root ⇒ nothing to revert (it was torn down, or
            // the root was never walked).
            let Some(inner) = outer.get(root).map(Arc::clone) else {
                return;
            };
            drop(outer);
            inner
        };

        let mut baseline = inner.lock().unwrap_or_else(PoisonError::into_inner);
        for change in changes {
            match change.kind {
                ChangeKind::Created | ChangeKind::Changed => {
                    baseline.remove(&change.rel);
                }
                ChangeKind::Deleted => {
                    baseline.insert(change.rel.clone(), OBSERVED_STAT_MISS_MTIME);
                }
            }
        }
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

/// Resolves the owning workspace root for a path from a roots slice.
///
/// Returns the longest-prefix match, or `None` if the path is outside
/// all roots. Used by methods that already hold the roots lock to avoid
/// re-locking.
fn resolve_root_in(roots: &[PathBuf], path: &Path) -> Option<PathBuf> {
    roots
        .iter()
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

/// Intermediate result from a single-pass file scan.
struct ScanResult {
    lines: usize,
    shebang_interpreter: Option<String>,
}

/// Scans a file for null bytes, counts lines, and extracts shebang in one pass.
///
/// Returns `Some(ScanResult)` for text files, `None` for binary files.
/// Files above the size threshold are assumed binary without reading.
fn scan_file(path: &Path, metadata: &std::fs::Metadata) -> Option<ScanResult> {
    if metadata.len() > BINARY_SIZE_THRESHOLD {
        return None;
    }

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
        if memchr::memchr(0, &buf[..n]).is_some() {
            return None; // Binary
        }

        if first_chunk {
            first_chunk = false;
            let first_line_end = memchr::memchr(b'\n', &buf[..n]).unwrap_or(n);
            shebang_interpreter = extract_shebang_interpreter(&buf[..first_line_end]);
        }

        lines += memchr::memchr_iter(b'\n', &buf[..n]).count();
    }
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
        mgr.set_roots(vec![root_a.clone(), root_b]);

        // Root A maps .pkg → pkgbuild.
        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root_a, tables);

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
        mgr.set_roots(vec![root_a.clone()]);

        let mut languages = HashMap::new();
        languages.insert(
            "custom".to_string(),
            lang_config_with_filenames(&["Taskfile"]),
        );
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root_a, tables);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/Taskfile")),
            Some("custom".to_string()),
        );
    }

    #[test]
    fn test_per_root_shebang_classification() {
        let root_a = tempfile::tempdir().expect("tempdir");
        let mgr = default_mgr();
        mgr.set_roots(vec![root_a.path().to_path_buf()]);

        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_shebangs(&["deno"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root_a.path().to_path_buf(), tables);

        // Extensionless file with deno shebang in root A → custom.
        let path = root_a.path().join("script");
        std::fs::write(&path, "#!/usr/bin/env deno\nconsole.log('hi')\n").expect("write");

        assert_eq!(mgr.language_id(&path), Some("custom".to_string()),);
    }

    #[test]
    fn test_per_root_precedence_over_global() {
        let root_a = PathBuf::from("/projects/a");

        let mgr = default_mgr();
        mgr.set_roots(vec![root_a.clone()]);

        // Root A maps .sh → custom-shell (global maps .sh → shellscript).
        let mut languages = HashMap::new();
        languages.insert("custom-shell".to_string(), lang_config_with_exts(&["sh"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root_a, tables);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/test.sh")),
            Some("custom-shell".to_string()),
        );
    }

    #[test]
    fn test_unrooted_file_uses_global() {
        let root_a = PathBuf::from("/projects/a");

        let mgr = default_mgr();
        mgr.set_roots(vec![root_a.clone()]);

        // Set per-root tables for root A.
        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_exts(&["xyz"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root_a, tables);

        // File outside all roots uses global classification only.
        assert_eq!(
            mgr.language_id(Path::new("/other/path/foo.rs")),
            Some("rust".to_string()),
        );
        // Per-root extension not visible for unrooted files.
        assert_eq!(mgr.language_id(Path::new("/other/path/foo.xyz")), None);
    }

    #[test]
    fn test_set_root_classification() {
        let root = PathBuf::from("/projects/a");
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![root.clone()]);

        // No per-root tables initially.
        assert_eq!(mgr.language_id(Path::new("/projects/a/foo.pkg")), None);

        // Set per-root tables.
        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root, tables);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/foo.pkg")),
            Some("pkgbuild".to_string()),
        );
    }

    #[test]
    fn test_remove_root_classification() {
        let root = PathBuf::from("/projects/a");
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![root.clone()]);

        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root.clone(), tables);

        assert_eq!(
            mgr.language_id(Path::new("/projects/a/foo.pkg")),
            Some("pkgbuild".to_string()),
        );

        // Remove per-root tables — falls back to global (None).
        mgr.remove_root_classification(&root);
        assert_eq!(mgr.language_id(Path::new("/projects/a/foo.pkg")), None);
    }

    #[test]
    fn test_detect_workspace_languages_per_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![root.path().to_path_buf()]);

        // Create a file with a custom extension.
        std::fs::write(root.path().join("build.pkg"), "content\n").expect("write");

        // Set per-root classification: .pkg → pkgbuild.
        let mut languages = HashMap::new();
        languages.insert("pkgbuild".to_string(), lang_config_with_exts(&["pkg"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root.path().to_path_buf(), tables);

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
        mgr.set_roots(vec![root.path().to_path_buf()]);

        // Per-root: deno → custom.
        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_shebangs(&["deno"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(root.path().to_path_buf(), tables);

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

    /// WS31-review C3 (F4) — land-with-fix unit on the evict-on-failure logic.
    ///
    /// `nudge_changed_set` advances the per-root baseline once (step 3), then
    /// fans the delta out to each covering server (step 4); the baseline is
    /// **shared** across all covering servers. A notify failure to one server
    /// would, absent recovery, lose those changes for it permanently — the next
    /// walk diffs against the already-advanced baseline and emits nothing. The fix
    /// reverts exactly the changes routed to the failed server via
    /// [`revert_baseline_changes`], so a re-diff against an **unchanged** FS
    /// re-emits them. This pins that re-emit contract directly (the integration
    /// path can't observe a forced notify failure without a new mockls capability
    /// — see the C3 ticket testability note — so this lands with the fix, the same
    /// precedent as L1/L4/N2/C1-F1).
    ///
    /// Covers all three [`ChangeKind`]s: a Created/Changed entry is removed so the
    /// next diff re-emits it; a Deleted entry (already dropped by the reap sweep)
    /// is re-inserted with the sentinel so the next **full** walk's reap sweep
    /// re-emits the deletion while the file stays gone.
    #[test]
    fn ws31_review_c3_notify_failure_reemits() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");

        // ── Created/Changed: revert removes the entry ⇒ next walk re-emits ──
        // First walk seeds the baseline (cold snapshot). The root key now exists,
        // so a later-walk absent path is a genuine Created.
        let _ = mgr.diff_update_and_reap(&root, &[(PathBuf::from("a.rs"), 100)]);
        // Second walk: a new file `b.rs` ⇒ Created, merged into the baseline.
        let observed = vec![(PathBuf::from("a.rs"), 100), (PathBuf::from("b.rs"), 200)];
        let set = mgr.diff_update_and_reap(&root, &observed);
        let created: Vec<&Change> = set
            .changes
            .iter()
            .filter(|c| c.kind == ChangeKind::Created)
            .collect();
        assert_eq!(created.len(), 1, "b.rs is the only Created");
        assert_eq!(created[0].rel, PathBuf::from("b.rs"));

        // Pre-revert sanity: a re-diff against the SAME observation is empty (the
        // baseline already absorbed b.rs — exactly the bug-38 no-repeat property
        // that loses the change if delivery failed).
        assert!(
            mgr.diff_update_and_reap(&root, &observed).is_empty(),
            "without revert, the advanced baseline silently swallows the change"
        );

        // Simulate the failed-notify recovery: revert the change routed to the
        // failed server. The next walk over the UNCHANGED FS must re-emit it.
        let reverted = vec![Change {
            rel: PathBuf::from("b.rs"),
            kind: ChangeKind::Created,
        }];
        mgr.revert_baseline_changes(&root, &reverted);
        let reemit = mgr.diff_update_and_reap(&root, &observed);
        let reemit_rels: Vec<&PathBuf> = reemit.changes.iter().map(|c| &c.rel).collect();
        assert_eq!(
            reemit_rels,
            vec![&PathBuf::from("b.rs")],
            "the reverted Created must re-emit on the next walk; a.rs must not"
        );

        // ── Deleted: revert re-inserts the entry ⇒ next full walk re-reaps ──
        // Drop b.rs from disk: a full walk reaps it (Deleted, baseline entry
        // removed).
        let reaped = mgr.diff_update_and_reap(&root, &[(PathBuf::from("a.rs"), 100)]);
        assert!(
            reaped
                .changes
                .iter()
                .any(|c| c.rel == Path::new("b.rs") && c.kind == ChangeKind::Deleted),
            "b.rs gone from disk ⇒ reaped as Deleted: {:?}",
            reaped.changes
        );
        // Without revert, the Deleted does not re-emit (entry already dropped).
        assert!(
            mgr.diff_update_and_reap(&root, &[(PathBuf::from("a.rs"), 100)])
                .is_empty(),
            "a reaped Deleted does not re-emit on its own"
        );

        // Simulate the failed-notify recovery for the Deleted change.
        let reverted_del = vec![Change {
            rel: PathBuf::from("b.rs"),
            kind: ChangeKind::Deleted,
        }];
        mgr.revert_baseline_changes(&root, &reverted_del);
        // Next full walk with b.rs still absent ⇒ Deleted re-emitted.
        let reemit_del = mgr.diff_update_and_reap(&root, &[(PathBuf::from("a.rs"), 100)]);
        assert!(
            reemit_del
                .changes
                .iter()
                .any(|c| c.rel == Path::new("b.rs") && c.kind == ChangeKind::Deleted),
            "the reverted Deleted must re-reap on the next full walk: {:?}",
            reemit_del.changes
        );
    }

    /// `revert_baseline_changes` is a no-op for an unknown root and for an empty
    /// change set — it must never spuriously re-emit unrelated baseline entries.
    #[test]
    fn revert_baseline_changes_is_scoped_and_safe() {
        let mgr = FilesystemManager::new();
        let root = PathBuf::from("/root");

        // No baseline yet ⇒ reverting against an unknown root is a no-op.
        mgr.revert_baseline_changes(
            &root,
            &[Change {
                rel: PathBuf::from("a.rs"),
                kind: ChangeKind::Created,
            }],
        );
        assert!(
            !mgr.has_baseline_for_test(&root),
            "reverting against an unknown root must not create a baseline"
        );

        // Seed two files, then revert only `a.rs`: `b.rs` must stay quiet.
        let observed = vec![(PathBuf::from("a.rs"), 100), (PathBuf::from("b.rs"), 100)];
        let _ = mgr.diff_and_update(&root, &observed);
        mgr.revert_baseline_changes(
            &root,
            &[Change {
                rel: PathBuf::from("a.rs"),
                kind: ChangeKind::Changed,
            }],
        );
        let reemit = mgr.diff_and_update(&root, &observed);
        let reemit_rels: Vec<&PathBuf> = reemit.changes.iter().map(|c| &c.rel).collect();
        assert_eq!(
            reemit_rels,
            vec![&PathBuf::from("a.rs")],
            "only the reverted entry re-emits; an empty revert leaves the rest quiet"
        );
    }

    /// C1/F1 — the shared per-file observation step (now used by `stat_walk`,
    /// the diagnostics surface that lacked the H1 retry/sentinel) must NEVER omit
    /// an enumerated present file on a stat miss: a residual miss yields the
    /// `OBSERVED_STAT_MISS_MTIME` sentinel, not an omission. An omission would
    /// drop the file from the observation set and a `reap=true` full walk would
    /// then false-reap the live file as `Deleted` (WS31-review F1/H1).
    ///
    /// Driven via the `#[cfg(test)]` injectable probe seam (mirrors R2/L4): a
    /// stateful probe deterministically fails the first call and succeeds later,
    /// so the test pins (a) the sentinel on a never-hit miss, (b) recovery within
    /// the full retry budget, and (c) retry-count sensitivity — a regression to
    /// a single attempt would surface the sentinel where recovery is expected.
    #[test]
    fn ws31_review_c1_diagnostics_stat_miss_not_reaped() {
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
