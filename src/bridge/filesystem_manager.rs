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

use ignore::WalkBuilder;

use crate::lsp::glob::{FileChange, FileChangeType};

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
    /// Per-root generation counter, bumped by [`diff()`](Self::diff) when
    /// changes are detected in a root. Used by [`SymbolIndex`] enrichment
    /// cache for invalidation.
    root_generations: std::sync::Mutex<HashMap<PathBuf, u64>>,
}

/// Cache entry storing classification results keyed by mtime.
///
/// `kind` is `None` for seed-only entries (stat-only, no classification).
/// [`FilesystemManager::classify`] overwrites these on first access.
struct CachedEntry {
    mtime: u64,
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
        let size = metadata.len();
        let root = self.resolve_root(path);
        let cache_key = (path.to_path_buf(), root.clone());

        // Check cache — skip unclassified (seed-only) entries.
        if let Ok(cache) = self.cache.lock()
            && let Some(entry) = cache.get(&cache_key)
            && entry.mtime == mtime
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
                    mtime,
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

    /// Populates the cache with `(path, mtime)` for every file and directory
    /// in the known roots. Stat-only — no content read, no classification.
    /// Respects `.gitignore` via the `ignore` crate.
    ///
    /// Called once at session start during `LspClientManager` init. Subsequent
    /// root additions use `add_root` which already walks the new root.
    pub fn seed(&self) {
        let roots = {
            let Ok(roots) = self.roots.lock() else {
                return;
            };
            roots.clone()
        };

        let mut entries = HashMap::new();
        for root in &roots {
            if !root.exists() {
                continue;
            }
            let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();
            for entry in walker.flatten() {
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                let path = entry.into_path();
                if let Ok(meta) = std::fs::metadata(&path) {
                    let resolved = resolve_root_in(&roots, &path);
                    let kind = if is_dir { Some(FileKind::Folder) } else { None };
                    entries.insert(
                        (path, resolved),
                        CachedEntry {
                            mtime: mtime_secs(&meta),
                            kind,
                        },
                    );
                }
            }
        }

        if let Ok(mut cache) = self.cache.lock() {
            *cache = entries;
        }
    }

    /// Diffs current disk state against the cache.
    ///
    /// Returns creates, changes, and deletes since last diff (or since seed
    /// for the first call). Updates the cache to reflect current disk state.
    ///
    /// The cache lock is held for the full duration (walk + compare + update).
    /// This is acceptable because `diff()` runs synchronously at tool
    /// boundaries, the critical section is stat-bound, and no other code path
    /// contends on the cache lock for long.
    pub fn diff(&self) -> Vec<FileChange> {
        let Ok(roots) = self.roots.lock() else {
            return Vec::new();
        };
        let Ok(mut cache) = self.cache.lock() else {
            return Vec::new();
        };

        // Walk all roots, collecting current (path, root, mtime, is_dir).
        let mut current: HashMap<(PathBuf, Option<PathBuf>), (u64, bool)> = HashMap::new();
        for root in roots.iter() {
            if !root.exists() {
                continue;
            }
            let walker = WalkBuilder::new(root).git_ignore(true).hidden(true).build();
            for entry in walker.flatten() {
                let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
                let path = entry.into_path();
                if let Ok(meta) = std::fs::metadata(&path) {
                    let resolved = resolve_root_in(&roots, &path);
                    current.insert((path, resolved), (mtime_secs(&meta), is_dir));
                }
            }
        }
        drop(roots);

        let mut changes = Vec::new();

        // Detect created and changed, updating cache inline.
        for (key, (mtime, is_dir)) in &current {
            match cache.get(key) {
                None => {
                    changes.push(FileChange {
                        path: key.0.clone(),
                        change_type: FileChangeType::Created,
                    });
                    let kind = if *is_dir {
                        Some(FileKind::Folder)
                    } else {
                        None
                    };
                    cache.insert(
                        key.clone(),
                        CachedEntry {
                            mtime: *mtime,
                            kind,
                        },
                    );
                }
                Some(entry) if entry.mtime != *mtime => {
                    changes.push(FileChange {
                        path: key.0.clone(),
                        change_type: FileChangeType::Changed,
                    });
                    if let Some(entry) = cache.get_mut(key) {
                        entry.mtime = *mtime;
                        entry.kind = if *is_dir {
                            Some(FileKind::Folder)
                        } else {
                            None
                        };
                    }
                }
                _ => {}
            }
        }

        // Detect deleted.
        let deleted: Vec<(PathBuf, Option<PathBuf>)> = cache
            .keys()
            .filter(|k| !current.contains_key(*k))
            .cloned()
            .collect();
        for key in &deleted {
            changes.push(FileChange {
                path: key.0.clone(),
                change_type: FileChangeType::Deleted,
            });
            cache.remove(key);
        }

        // Bump generation counter for each root that has changes.
        if !changes.is_empty() {
            let roots_snapshot = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mut changed_roots = HashSet::new();
            for change in &changes {
                if let Some(root) = resolve_root_in(&roots_snapshot, &change.path) {
                    changed_roots.insert(root);
                }
            }
            if let Ok(mut gens) = self.root_generations.lock() {
                for root in changed_roots {
                    *gens.entry(root).or_insert(0) += 1;
                }
            }
        }

        changes
    }

    /// Refreshes cache entries for specific paths.
    ///
    /// Re-stats each path and updates its mtime in the cache. If a path no
    /// longer exists, removes it from the cache.
    ///
    /// Used by `done_editing` to prevent the next [`diff`](Self::diff) from
    /// reporting edited files as `Changed`.
    pub fn mark_current(&self, paths: &[PathBuf]) {
        // Resolve roots before locking cache to maintain lock ordering
        // (roots → cache), consistent with diff() which holds both.
        let keys: Vec<(PathBuf, Option<PathBuf>)> = paths
            .iter()
            .map(|p| (p.clone(), self.resolve_root(p)))
            .collect();

        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        for key in keys {
            match std::fs::metadata(&key.0) {
                Ok(meta) => {
                    let mtime = mtime_secs(&meta);
                    if let Some(entry) = cache.get_mut(&key) {
                        entry.mtime = mtime;
                    } else {
                        cache.insert(key, CachedEntry { mtime, kind: None });
                    }
                }
                Err(_) => {
                    cache.remove(&key);
                }
            }
        }
    }

    /// Returns the current generation counter for a root.
    ///
    /// The generation starts at 0 and is bumped by [`diff()`](Self::diff)
    /// each time changes are detected in the root. Used by the enrichment
    /// cache in [`SymbolIndex`] for staleness checks.
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

    // --- Seed / Diff / Mark current ---

    /// Helper: set a file's mtime to a specific epoch second.
    fn set_mtime(path: &Path, epoch_secs: u64) {
        let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs);
        let times = std::fs::FileTimes::new().set_modified(time);
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for set_mtime");
        file.set_times(times).expect("set_times");
    }

    #[test]
    fn seed_populates_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");
        std::fs::write(dir.path().join("b.rs"), "fn b() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        let changes = mgr.diff();
        assert!(changes.is_empty(), "diff after seed should be empty");
    }

    #[test]
    fn diff_detects_created_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("existing.rs"), "fn e() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::write(dir.path().join("new.rs"), "fn n() {}\n").expect("write");

        // Creating a file may also change the parent directory's mtime,
        // so avoid asserting an exact count.
        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("new.rs") && c.change_type == FileChangeType::Created),
            "expected Created for new.rs, got: {changes:?}",
        );
    }

    #[test]
    fn diff_detects_changed_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn original() {}\n").expect("write");
        set_mtime(&path, 1_000_000);

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Change content and bump mtime.
        std::fs::write(&path, "fn modified() {}\n").expect("write");
        set_mtime(&path, 2_000_000);

        let changes = mgr.diff();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].path.ends_with("code.rs"));
        assert_eq!(changes[0].change_type, FileChangeType::Changed);
    }

    #[test]
    fn diff_detects_deleted_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gone.rs");
        std::fs::write(&path, "fn gone() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::remove_file(&path).expect("remove");

        // Deleting a file may also change the parent directory's mtime,
        // so avoid asserting an exact count.
        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("gone.rs") && c.change_type == FileChangeType::Deleted),
            "expected Deleted for gone.rs, got: {changes:?}",
        );
    }

    #[test]
    fn diff_detects_created_directory() {
        let dir = tempfile::tempdir().expect("tempdir");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::create_dir(dir.path().join("subdir")).expect("mkdir");

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("subdir") && c.change_type == FileChangeType::Created),
            "expected Created for new directory, got: {changes:?}",
        );
    }

    #[test]
    fn diff_detects_deleted_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("subdir")).expect("mkdir");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::remove_dir(dir.path().join("subdir")).expect("rmdir");

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("subdir") && c.change_type == FileChangeType::Deleted),
            "expected Deleted for removed directory, got: {changes:?}",
        );
    }

    #[test]
    fn diff_updates_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("file.rs");
        std::fs::write(&path, "fn f() {}\n").expect("write");
        set_mtime(&path, 1_000_000);

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Trigger a change.
        std::fs::write(&path, "fn changed() {}\n").expect("write");
        set_mtime(&path, 2_000_000);

        let first = mgr.diff();
        assert_eq!(first.len(), 1);

        // Second diff should be empty — cache was updated.
        let second = mgr.diff();
        assert!(
            second.is_empty(),
            "second diff should be empty after cache update"
        );
    }

    #[test]
    fn diff_multiple_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let modify_path = dir.path().join("modify.rs");
        let delete_path = dir.path().join("delete.rs");
        std::fs::write(&modify_path, "fn m() {}\n").expect("write");
        set_mtime(&modify_path, 1_000_000);
        std::fs::write(&delete_path, "fn d() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Create, modify, and delete in one pass.
        std::fs::write(dir.path().join("create.rs"), "fn c() {}\n").expect("write");
        std::fs::write(&modify_path, "fn modified() {}\n").expect("write");
        set_mtime(&modify_path, 2_000_000);
        std::fs::remove_file(&delete_path).expect("remove");

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("create.rs") && c.change_type == FileChangeType::Created),
            "missing Created, got: {changes:?}",
        );
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("modify.rs") && c.change_type == FileChangeType::Changed),
            "missing Changed, got: {changes:?}",
        );
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("delete.rs") && c.change_type == FileChangeType::Deleted),
            "missing Deleted, got: {changes:?}",
        );
    }

    #[test]
    fn mark_current_refreshes_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edited.rs");
        std::fs::write(&path, "fn e() {}\n").expect("write");
        set_mtime(&path, 1_000_000);

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Simulate an edit — change content and mtime.
        std::fs::write(&path, "fn edited() {}\n").expect("write");
        set_mtime(&path, 2_000_000);

        // mark_current refreshes the cache entry.
        mgr.mark_current(std::slice::from_ref(&path));

        // diff should see no changes for this file.
        let changes = mgr.diff();
        assert!(
            !changes.iter().any(|c| c.path == path),
            "mark_current should have prevented diff from reporting this file",
        );
    }

    #[test]
    fn mark_current_removes_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("doomed.rs");
        std::fs::write(&path, "fn d() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        std::fs::remove_file(&path).expect("remove");
        mgr.mark_current(std::slice::from_ref(&path));

        // File should be gone from cache — diff should not report it.
        let changes = mgr.diff();
        assert!(
            !changes.iter().any(|c| c.path == path),
            "mark_current should have removed deleted file from cache",
        );
    }

    #[test]
    fn seed_respects_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Initialize a git repo so .gitignore is respected.
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir .git");
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").expect("write gitignore");
        std::fs::create_dir(dir.path().join("ignored")).expect("mkdir ignored");
        std::fs::write(dir.path().join("ignored/secret.rs"), "fn s() {}\n").expect("write");
        std::fs::write(dir.path().join("visible.rs"), "fn v() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        let cache = mgr.cache.lock().expect("lock");
        let has_secret = cache
            .keys()
            .any(|(p, _)| p.to_string_lossy().contains("secret.rs"));
        let has_visible = cache
            .keys()
            .any(|(p, _)| p.to_string_lossy().contains("visible.rs"));
        drop(cache);
        assert!(!has_secret, "gitignored file should not be in cache");
        assert!(has_visible, "visible file should be in cache");
    }

    #[test]
    fn diff_respects_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir .git");
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").expect("write gitignore");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir.path().to_path_buf()]);
        mgr.seed();

        // Create files in both ignored and visible locations.
        std::fs::create_dir(dir.path().join("ignored")).expect("mkdir ignored");
        std::fs::write(dir.path().join("ignored/new.rs"), "fn n() {}\n").expect("write");
        std::fs::write(dir.path().join("visible.rs"), "fn v() {}\n").expect("write");

        let changes = mgr.diff();
        assert!(
            !changes
                .iter()
                .any(|c| c.path.to_string_lossy().contains("ignored")),
            "gitignored file should not appear in diff, got: {changes:?}",
        );
        assert!(
            changes.iter().any(|c| c.path.ends_with("visible.rs")),
            "visible file should appear in diff, got: {changes:?}",
        );
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
    fn test_seed_with_per_root_classification() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");

        let mgr = default_mgr();
        mgr.set_roots(vec![dir.path().to_path_buf()]);

        // Set per-root classification.
        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_exts(&["xyz"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(dir.path().to_path_buf(), tables);

        // Seed + diff should be clean.
        mgr.seed();
        let changes = mgr.diff();
        assert!(changes.is_empty(), "diff after seed should be empty");
    }

    #[test]
    fn test_diff_with_per_root_classification() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");

        let mgr = default_mgr();
        mgr.set_roots(vec![dir.path().to_path_buf()]);

        let mut languages = HashMap::new();
        languages.insert("custom".to_string(), lang_config_with_exts(&["xyz"]));
        let tables = ClassificationTables::from_project_config(&languages);
        mgr.set_root_classification(dir.path().to_path_buf(), tables);

        mgr.seed();

        // Create a file with the per-root extension.
        std::fs::write(dir.path().join("new.xyz"), "content\n").expect("write");
        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("new.xyz") && c.change_type == FileChangeType::Created),
            "new file should appear in diff, got: {changes:?}",
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
    fn add_root_then_diff_reports_new_files() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        std::fs::write(dir_a.path().join("a.rs"), "fn a() {}\n").expect("write");
        std::fs::write(dir_b.path().join("b.rs"), "fn b() {}\n").expect("write");

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir_a.path().to_path_buf()]);
        mgr.seed();

        // Add a second root.
        mgr.set_roots(vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()]);

        let changes = mgr.diff();
        assert!(
            changes
                .iter()
                .any(|c| c.path.ends_with("b.rs") && c.change_type == FileChangeType::Created),
            "new root's files should appear as Created, got: {changes:?}",
        );
        // Existing root should have no changes.
        assert!(
            !changes.iter().any(|c| c.path.ends_with("a.rs")),
            "existing root files should not appear, got: {changes:?}",
        );
    }

    #[test]
    fn diff_bumps_generation_for_changed_roots() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let file_a = dir_a.path().join("a.rs");
        let file_b = dir_b.path().join("b.rs");
        std::fs::write(&file_a, "fn a() {}\n").expect("write a");
        std::fs::write(&file_b, "fn b() {}\n").expect("write b");
        set_mtime(&file_a, 1_000_000);
        set_mtime(&file_b, 1_000_000);

        let mgr = FilesystemManager::new();
        mgr.set_roots(vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()]);
        mgr.seed();

        // Both roots start at generation 0.
        assert_eq!(mgr.root_generation(dir_a.path()), 0);
        assert_eq!(mgr.root_generation(dir_b.path()), 0);

        // No changes yet — diff should not bump.
        let changes = mgr.diff();
        assert!(changes.is_empty());
        assert_eq!(mgr.root_generation(dir_a.path()), 0);
        assert_eq!(mgr.root_generation(dir_b.path()), 0);

        // Modify file in root A only.
        std::fs::write(&file_a, "fn a_modified() {}\n").expect("write a modified");
        set_mtime(&file_a, 2_000_000);
        let changes = mgr.diff();
        assert!(!changes.is_empty(), "should detect change");
        assert_eq!(
            mgr.root_generation(dir_a.path()),
            1,
            "root A generation should bump"
        );
        assert_eq!(
            mgr.root_generation(dir_b.path()),
            0,
            "root B generation should not bump"
        );

        // Another change in root A.
        std::fs::write(&file_a, "fn a_again() {}\n").expect("write a again");
        set_mtime(&file_a, 3_000_000);
        let _ = mgr.diff();
        assert_eq!(mgr.root_generation(dir_a.path()), 2);

        // Change in root B.
        std::fs::write(&file_b, "fn b_modified() {}\n").expect("write b modified");
        set_mtime(&file_b, 2_000_000);
        let _ = mgr.diff();
        assert_eq!(mgr.root_generation(dir_b.path()), 1);
    }
}
