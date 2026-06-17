// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Symbol index for workspace-wide symbol extraction.
//!
//! Provides [`SymbolIndex`], an in-memory symbol cache populated from
//! `textDocument/documentSymbol` LSP responses. The index starts empty and
//! is filled lazily via [`SymbolIndex::populate_from_document_symbols()`].
//! Callers are responsible for requesting `documentSymbol` from the LSP
//! server and feeding the response to the index.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};

use crate::bridge::filesystem_manager::mtime_nanos;

/// A symbol extracted from the symbol index.
#[derive(Clone)]
pub struct Symbol {
    /// Symbol name.
    pub name: String,
    /// Kind string (e.g., `"function"`, `"struct"`).
    pub kind: String,
    /// 0-based start line of the definition.
    pub line: u32,
    /// 0-based end line of the definition (for structure spans).
    pub end_line: u32,
    /// Container name (enclosing definition's name).
    pub scope: Option<String>,
    /// Container kind (enclosing definition's kind string).
    pub scope_kind: Option<String>,
    /// Whether the symbol has a `Deprecated` tag.
    pub deprecated: bool,
}

/// Scope filter for symbol queries used by the `into` pipeline.
pub enum ScopeFilter<'a> {
    /// Top-level symbols only (scope IS NULL).
    TopLevel,
    /// Children of a specific scope name.
    ChildrenOf(&'a str),
    /// Symbols at any depth (no scope constraint).
    AnyDepth,
    /// Symbols within a line span (for `**` after a matched container).
    WithinSpan(u32, u32),
}

/// Display labels for kind brackets in output.
///
/// NOT a query gate — all enrichment queries are sent for every
/// tier 1 symbol regardless of category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrichmentCategory {
    /// Functions, methods, constructors, macros, etc.
    Callable,
    /// Structs, classes, enums, traits, interfaces, etc.
    Type,
    /// Everything else.
    Other,
}

static CALLABLE_KINDS: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    HashSet::from([
        "function",
        "method",
        "constructor",
        "macro",
        "subroutine",
        "command",
        "procedure",
    ])
});

static TYPE_KINDS: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    HashSet::from([
        "struct",
        "class",
        "enum",
        "trait",
        "interface",
        "union",
        "typedef",
        "type",
        "protocol",
    ])
});

/// Categorize a kind string into an [`EnrichmentCategory`].
///
/// Uses `HashSet` lookups against the callable and type kind tables.
#[must_use]
pub fn categorize(kind: &str) -> EnrichmentCategory {
    if CALLABLE_KINDS.contains(kind) {
        EnrichmentCategory::Callable
    } else if TYPE_KINDS.contains(kind) {
        EnrichmentCategory::Type
    } else {
        EnrichmentCategory::Other
    }
}

/// Title-case a kind string for display brackets.
///
/// Special case: `"implementation"` → `"Impl"`. All others: first char
/// uppercase, rest lowercase.
#[must_use]
pub fn format_symbol_kind(kind: &str) -> String {
    if kind == "implementation" {
        return "Impl".to_string();
    }
    let mut chars = kind.chars();
    chars.next().map_or_else(String::new, |first| {
        let mut s = first.to_uppercase().to_string();
        for ch in chars {
            s.extend(ch.to_lowercase());
        }
        s
    })
}

/// LSP abbreviation table for edge labels (calls, supertypes, subtypes).
///
/// Maps LSP `SymbolKind` numeric values to short display labels.
/// Unknown kinds return `"Sym"`.
#[must_use]
pub const fn lsp_kind_label(kind: u32) -> &'static str {
    match kind {
        1 => "File",
        2 => "Mod",
        3 => "Ns",
        4 => "Pkg",
        5 => "Class",
        6 => "Method",
        7 => "Prop",
        8 => "Field",
        9 => "Ctor",
        10 => "Enum",
        11 => "Iface",
        12 => "Fn",
        13 => "Var",
        14 => "Const",
        15 => "Str",
        16 => "Num",
        17 => "Bool",
        18 => "Array",
        19 => "Obj",
        20 => "Key",
        21 => "Null",
        22 => "Member",
        23 => "Struct",
        24 => "Event",
        25 => "Op",
        26 => "TypeParam",
        _ => "Sym",
    }
}

/// Converts an LSP `SymbolKind` numeric value to a kind string for storage.
///
/// These strings match the display label taxonomy used by [`format_symbol_kind`].
#[must_use]
pub const fn symbol_kind_to_string(kind: u32) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type_parameter",
        _ => "unknown",
    }
}

/// Cached enrichment result for a symbol position.
///
/// Wraps `SymbolEnrichment` with the root and generation counter at
/// cache time for staleness checking against [`FilesystemManager::root_generation`].
struct CachedEnrichment {
    /// The enrichment data.
    enrichment: SymbolEnrichment,
    /// Workspace root this position belongs to.
    root: PathBuf,
    /// Generation counter at cache time.
    generation: u64,
    /// `mtime_nanos` of the enriched position's source file at cache time,
    /// or `None` if it could not be stat-ed. Re-stat on read: a change
    /// (or a stat failure ⇒ file gone) misses, catching a host
    /// `Edit`/`Write` that does not bump a generation. Mirrors the outline
    /// cache's `FileEntry::mtime` and the result cache's witness mtimes.
    source_mtime: Option<i64>,
}

/// Enrichment data for a single symbol from LSP queries.
///
/// Shared between [`GrepServer`] (producer) and the enrichment cache.
#[derive(Clone)]
pub(crate) struct SymbolEnrichment {
    /// Reference lines grouped by file path (0-based line numbers).
    pub ref_lines: HashMap<String, HashSet<u32>>,
    /// Incoming call edges (callers of this symbol).
    pub incoming_calls: Vec<CallEdge>,
    /// Outgoing call edges (callees of this symbol).
    pub outgoing_calls: Vec<CallEdge>,
    /// Implementation locations: `(file_path, line_0)`.
    pub implementations: Vec<(String, u32)>,
    /// Supertype edges.
    pub supertypes: Vec<TypeEdge>,
    /// Subtype edges.
    pub subtypes: Vec<TypeEdge>,
}

/// A call hierarchy edge (caller or callee).
#[derive(Clone)]
pub(crate) struct CallEdge {
    /// Symbol name.
    pub name: String,
    /// LSP `SymbolKind` numeric value.
    pub kind: u32,
    /// Container name (enclosing scope).
    pub container: Option<String>,
    /// File path.
    pub file: String,
    /// 0-based line number.
    pub line: u32,
    /// Whether the symbol has a `Deprecated` tag.
    pub deprecated: bool,
}

/// A type hierarchy edge (supertype or subtype).
#[derive(Clone)]
pub(crate) struct TypeEdge {
    /// Symbol name.
    pub name: String,
    /// LSP `SymbolKind` numeric value.
    pub kind: u32,
    /// Container name from LSP `detail` field.
    pub container: Option<String>,
    /// File path.
    pub file: String,
    /// 0-based line number.
    pub line: u32,
    /// Whether the symbol has a `Deprecated` tag.
    pub deprecated: bool,
}

/// Workspace-wide symbol index held in memory.
///
/// Populated lazily from `textDocument/documentSymbol` LSP responses and
/// stored as per-file symbol lists. The symbol index is ephemeral — built
/// during a session, discarded on session end. No dependency on any
/// persistent store.
///
/// Also caches per-position enrichment results (references, call
/// hierarchy, implementations, type hierarchy) with per-root generation
/// counter invalidation.
pub struct SymbolIndex {
    /// Per-file symbol lists, each kept sorted by start `line`, plus the
    /// on-disk mtime each file was populated from.
    ///
    /// Wrapped in a [`RefCell`] so
    /// [`populate_from_document_symbols`](Self::populate_from_document_symbols)
    /// and [`invalidate`](Self::invalidate) keep their `&self` signature: every
    /// live caller holds the index behind a `Mutex`, which already serializes
    /// access, so the cell is never borrowed concurrently.
    files: RefCell<HashMap<PathBuf, FileEntry>>,
    /// Per-position enrichment cache: `(file, line, col)` → cached result.
    enrichment_cache: HashMap<(PathBuf, u32, u32), CachedEnrichment>,
}

/// A file's cached symbols and the on-disk mtime they were populated from.
struct FileEntry {
    /// Flattened symbols, sorted ascending by start `line`. At most one symbol
    /// is kept per start line, mirroring the old `PRIMARY KEY (file_path, line)`.
    symbols: Vec<Symbol>,
    /// On-disk `mtime_nanos` recorded at population time, or `None` when the
    /// path could not be stat-ed. Drives
    /// [`symbols_outdated`](SymbolIndex::symbols_outdated).
    mtime: Option<i64>,
}

impl SymbolIndex {
    /// Creates a new empty symbol index.
    ///
    /// Symbols are populated lazily via
    /// [`populate_from_document_symbols()`](Self::populate_from_document_symbols).
    ///
    /// # Errors
    ///
    /// Returns a `Result` for signature stability with callers that construct
    /// the index fallibly; construction is currently infallible.
    pub fn new() -> Result<Self> {
        Ok(Self {
            files: RefCell::new(HashMap::new()),
            enrichment_cache: HashMap::new(),
        })
    }

    /// Populates the index for a file from a `documentSymbol` LSP response.
    ///
    /// Walks the `DocumentSymbol` hierarchy (recursive children) and flattens
    /// it into per-file [`Symbol`] entries. Sets `scope`/`scope_kind` from the
    /// parent. Sets `deprecated` from `tags` containing `SymbolTag::Deprecated`
    /// (value 1). Replaces any existing symbols for the file. At most one symbol
    /// is kept per start line (mirrors the old `PRIMARY KEY (file_path, line)`
    /// with `INSERT OR IGNORE`: the first symbol seen at a given line wins).
    ///
    /// Records the file's current on-disk mtime alongside the symbols so a later
    /// external write (host `Edit`/`Write`, `git checkout`, formatter) that
    /// leaves the symbols untouched is detected as stale by
    /// [`symbols_outdated`](Self::symbols_outdated) (bug #26). Capturing it here
    /// means every populate path — `grep`/`glob` and the diagnostics batch —
    /// records it uniformly. A path that cannot be stat-ed (a synthetic test
    /// path) records no mtime, degrading to the prior absence-only behavior.
    ///
    /// The `symbols` parameter is the JSON array from the LSP response.
    ///
    /// # Errors
    ///
    /// Returns a `Result` for signature stability; the in-memory swap is
    /// infallible.
    pub fn populate_from_document_symbols(
        &self,
        file_path: &Path,
        symbols: &serde_json::Value,
    ) -> Result<()> {
        let mut flat: Vec<Symbol> = Vec::new();
        if let Some(arr) = symbols.as_array() {
            for sym in arr {
                flatten_document_symbol(sym, None, None, &mut flat);
            }
        }

        // Stat before storing. The recorded mtime is the version the server saw
        // (the caller opened the document from disk before requesting
        // `documentSymbol`), so a write landing after this point advances the
        // mtime and is caught on the next access.
        let recorded_mtime: Option<i64> =
            std::fs::metadata(file_path).ok().map(|m| mtime_nanos(&m));

        // One symbol per start line, kept ascending — the old store keyed rows
        // on `(file_path, line)` with `INSERT OR IGNORE`, so the first symbol
        // seen at a line won and rows read back ordered by line.
        let mut by_line: BTreeMap<u32, Symbol> = BTreeMap::new();
        for sym in flat {
            by_line.entry(sym.line).or_insert(sym);
        }

        self.files.borrow_mut().insert(
            file_path.to_path_buf(),
            FileEntry {
                symbols: by_line.into_values().collect(),
                mtime: recorded_mtime,
            },
        );
        Ok(())
    }

    /// Returns `true` if the file needs symbol population (no cached symbols).
    #[must_use]
    pub fn needs_population(&self, path: &Path) -> bool {
        !self.has_symbols_for(path)
    }

    /// Returns paths from the input that need symbol population (no cached symbols).
    ///
    /// Encapsulates the "has no symbols" check so callers filter with
    /// `idx.needs_symbols(files)` instead of inverting `has_symbols_for`.
    pub fn needs_symbols<'a>(&self, paths: &'a [PathBuf]) -> Vec<&'a PathBuf> {
        paths.iter().filter(|p| self.needs_population(p)).collect()
    }

    /// Returns `true` if the file has any cached symbols.
    #[must_use]
    pub fn has_symbols_for(&self, path: &Path) -> bool {
        self.files
            .borrow()
            .get(path)
            .is_some_and(|entry| !entry.symbols.is_empty())
    }

    /// Deletes all symbols (and the recorded mtime) for the file. Next access
    /// should re-populate.
    ///
    /// # Errors
    ///
    /// Returns a `Result` for signature stability; the in-memory swap is
    /// infallible.
    pub fn invalidate(&self, path: &Path) -> Result<()> {
        self.files.borrow_mut().remove(path);
        Ok(())
    }

    /// Drops all cached outlines and enrichment for files under `root` — a
    /// prefix sweep of both backing maps.
    ///
    /// Called when `root` leaves the tracked set (MCP disconnect,
    /// `catenary roots rm`, `SubagentStop`) so an untracked path can no longer
    /// serve enrichment from a dead session's cache (bug #36), and so caches
    /// for gone roots do not accumulate across sessions (a leak). Aligns the
    /// `SymbolIndex` lifetime with the tracked-root set.
    ///
    /// Takes `&mut self` because `enrichment_cache` is a plain field (unlike the
    /// `RefCell`-wrapped `files`); both are reached through the outer `Mutex`,
    /// which serializes access.
    pub fn evict_root(&mut self, root: &Path) {
        self.files.borrow_mut().retain(|p, _| !p.starts_with(root));
        self.enrichment_cache
            .retain(|(p, _, _), _| !p.starts_with(root));
    }

    /// Returns `true` when `path` has cached symbols whose recorded mtime is
    /// older than `current_mtime` — an external write the daemon never
    /// invalidated (host `Edit`/`Write`, `git checkout`, formatter; bug #26).
    ///
    /// Reports *staleness* of present symbols; *absence* is reported by
    /// [`needs_population`](Self::needs_population). A file with no recorded
    /// mtime (never populated, or populated from a path that could not be
    /// stat-ed) returns `false`: there is nothing to compare against, and
    /// absence already forces population. `current_mtime` is the file's
    /// current `mtime_nanos` (nanoseconds since epoch).
    #[must_use]
    pub fn symbols_outdated(&self, path: &Path, current_mtime: i64) -> bool {
        self.files
            .borrow()
            .get(path)
            .and_then(|entry| entry.mtime)
            .is_some_and(|recorded| current_mtime > recorded)
    }

    /// Query the index for symbols whose names match a regex pattern.
    ///
    /// If `files` is `Some` and non-empty, only symbols from those files are
    /// returned; otherwise the whole index is scanned. Results are unordered.
    ///
    /// # Errors
    ///
    /// Returns an error if `pattern` is not a valid regular expression.
    pub fn query(
        &self,
        pattern: &str,
        files: Option<&[PathBuf]>,
    ) -> Result<Vec<(PathBuf, Symbol)>> {
        let re = regex::Regex::new(pattern).context("invalid query regex")?;
        let store = self.files.borrow();
        let mut results = Vec::new();
        let mut collect = |path: &PathBuf, entry: &FileEntry| {
            for sym in &entry.symbols {
                if re.is_match(&sym.name) {
                    results.push((path.clone(), sym.clone()));
                }
            }
        };

        match files {
            Some(file_list) if !file_list.is_empty() => {
                for path in file_list {
                    if let Some(entry) = store.get(path) {
                        collect(path, entry);
                    }
                }
            }
            _ => {
                for (path, entry) in store.iter() {
                    collect(path, entry);
                }
            }
        }

        Ok(results)
    }

    /// Query depth-0 (outline) symbols for a batch of files.
    ///
    /// Returns top-level symbols (`scope` is `None`) grouped by file path,
    /// ordered by line number within each file. Files with no top-level
    /// symbols are omitted. Used by the glob tool for defensive maps.
    ///
    /// # Errors
    ///
    /// Returns a `Result` for signature stability; the in-memory scan is
    /// infallible.
    pub fn query_outline_batch(&self, files: &[&Path]) -> Result<HashMap<PathBuf, Vec<Symbol>>> {
        if files.is_empty() {
            return Ok(HashMap::new());
        }

        let store = self.files.borrow();
        let mut result: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        for &file in files {
            let Some(entry) = store.get(file) else {
                continue;
            };
            // `entry.symbols` is sorted by line; filtering preserves order.
            let outline: Vec<Symbol> = entry
                .symbols
                .iter()
                .filter(|sym| sym.scope.is_none())
                .cloned()
                .collect();
            if !outline.is_empty() {
                result.insert(file.to_path_buf(), outline);
            }
        }

        Ok(result)
    }

    /// Finds the innermost symbol enclosing a line in a file.
    ///
    /// Returns the tightest definition (smallest span) containing the given
    /// 0-based line, or `None` if no symbol covers it.
    ///
    /// # Errors
    ///
    /// Returns a `Result` for signature stability; the in-memory scan is
    /// infallible.
    pub fn find_enclosing(&self, file_path: &Path, line_0: u32) -> Result<Option<Symbol>> {
        let store = self.files.borrow();
        let result = store.get(file_path).and_then(|entry| {
            entry
                .symbols
                .iter()
                .filter(|sym| sym.line <= line_0 && sym.end_line >= line_0)
                .min_by_key(|sym| sym.end_line - sym.line)
                .cloned()
        });
        Ok(result)
    }

    /// Check whether a scope (container) has children in the given file.
    ///
    /// Returns `true` if any symbol in the index has `scope = scope_name`
    /// within the given file path.
    #[must_use]
    pub fn has_children(&self, file_path: &Path, scope_name: &str) -> bool {
        self.files.borrow().get(file_path).is_some_and(|entry| {
            entry
                .symbols
                .iter()
                .any(|sym| sym.scope.as_deref() == Some(scope_name))
        })
    }

    /// Query symbols filtered by scope, name glob, kind, and deprecated status.
    ///
    /// Used by the `into` pipeline for segment-by-segment symbol tree navigation.
    /// Results are grouped by file path and ordered by line number; files with
    /// no matching symbols are omitted.
    ///
    /// # Errors
    ///
    /// Returns an error if `name_glob` is not a valid glob pattern.
    pub fn query_scoped(
        &self,
        files: &[&Path],
        scope: &ScopeFilter<'_>,
        name_glob: &str,
        kind_filter: Option<&str>,
        deprecated_only: bool,
    ) -> Result<HashMap<PathBuf, Vec<Symbol>>> {
        if files.is_empty() {
            return Ok(HashMap::new());
        }

        // The old store used SQLite's `GLOB` operator; `globset` matches the
        // same `*`/`?`/`[…]` syntax against symbol names (which carry no `/`).
        let matcher = globset::Glob::new(name_glob)
            .context("invalid name glob")?
            .compile_matcher();

        let store = self.files.borrow();
        let mut result: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        for &file in files {
            let Some(entry) = store.get(file) else {
                continue;
            };
            // `entry.symbols` is sorted by line; filtering preserves order.
            let selected: Vec<Symbol> = entry
                .symbols
                .iter()
                .filter(|&sym| {
                    scope_matches(scope, sym)
                        && matcher.is_match(&sym.name)
                        && kind_filter.is_none_or(|kind| sym.kind == kind)
                        && (!deprecated_only || sym.deprecated)
                })
                .cloned()
                .collect();
            if !selected.is_empty() {
                result.insert(file.to_path_buf(), selected);
            }
        }

        Ok(result)
    }

    /// Returns a cached enrichment result if it is still fresh.
    ///
    /// Two gates, both must pass:
    /// 1. The per-root generation counter against the current value from
    ///    `FilesystemManager` — catches `sed`/diagnostics/explicit
    ///    invalidation that bumps a root generation.
    /// 2. The source file's `mtime_nanos`, re-stat-ed and compared (`!=`)
    ///    against the value recorded at cache time — catches a host
    ///    `Edit`/`Write` that bumps no generation, and a stat failure (file
    ///    removed). Mirrors [`ResultCache`]'s witness-mtime check.
    ///
    /// Returns `None` on miss or staleness (evicts the stale entry). Returns
    /// a clone because a stale hit requires mutable access to evict the entry.
    pub(crate) fn get_enrichment(
        &mut self,
        file: &Path,
        line: u32,
        col: u32,
        fs_manager: &super::bridge::filesystem_manager::FilesystemManager,
    ) -> Option<SymbolEnrichment> {
        let key = (file.to_path_buf(), line, col);
        let entry = self.enrichment_cache.get(&key)?;

        // Generation gate — catches sed/diagnostics/explicit invalidation.
        if entry.generation != fs_manager.root_generation(&entry.root) {
            self.enrichment_cache.remove(&key);
            return None;
        }

        // mtime floor — catches a host Edit/Write that bumps no generation.
        let current = std::fs::metadata(file).ok().map(|m| mtime_nanos(&m));
        if current != entry.source_mtime {
            self.enrichment_cache.remove(&key);
            return None;
        }

        Some(entry.enrichment.clone())
    }

    /// Stores an enrichment result in the cache.
    ///
    /// Records the current root generation and the source file's
    /// `source_mtime` (from `FilesystemManager` / a stat at the call site) so
    /// that future lookups can detect staleness via both gates.
    #[allow(
        clippy::too_many_arguments,
        reason = "position key (file/line/col) plus both staleness witnesses (root/generation/source_mtime) and the payload"
    )]
    pub(crate) fn cache_enrichment(
        &mut self,
        file: &Path,
        line: u32,
        col: u32,
        root: PathBuf,
        generation: u64,
        source_mtime: Option<i64>,
        enrichment: SymbolEnrichment,
    ) {
        let key = (file.to_path_buf(), line, col);
        self.enrichment_cache.insert(
            key,
            CachedEnrichment {
                enrichment,
                root,
                generation,
                source_mtime,
            },
        );
    }
}

/// Tests whether a symbol satisfies a [`ScopeFilter`].
fn scope_matches(scope: &ScopeFilter<'_>, sym: &Symbol) -> bool {
    match scope {
        ScopeFilter::TopLevel => sym.scope.is_none(),
        ScopeFilter::ChildrenOf(name) => sym.scope.as_deref() == Some(*name),
        ScopeFilter::AnyDepth => true,
        ScopeFilter::WithinSpan(lo, hi) => sym.line >= *lo && sym.line <= *hi,
    }
}

/// Recursively flattens a `DocumentSymbol` JSON node into [`Symbol`] entries.
fn flatten_document_symbol(
    node: &serde_json::Value,
    parent_name: Option<&str>,
    parent_kind: Option<&str>,
    out: &mut Vec<Symbol>,
) {
    let Some(name) = node.get("name").and_then(serde_json::Value::as_str) else {
        return;
    };
    let kind_num = node
        .get("kind")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let kind = symbol_kind_to_string(u32::try_from(kind_num).unwrap_or(0));

    let range = node.get("range");
    let start_line = range
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let end_line = range
        .and_then(|r| r.get("end"))
        .and_then(|e| e.get("line"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(start_line);

    let deprecated = node
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tags| tags.iter().any(|t| t.as_u64() == Some(1)));

    let line = u32::try_from(start_line).unwrap_or(u32::MAX);
    let end = u32::try_from(end_line).unwrap_or(line);

    out.push(Symbol {
        name: name.to_string(),
        kind: kind.to_string(),
        line,
        end_line: end,
        scope: parent_name.map(String::from),
        scope_kind: parent_kind.map(String::from),
        deprecated,
    });

    if let Some(children) = node.get("children").and_then(serde_json::Value::as_array) {
        for child in children {
            flatten_document_symbol(child, Some(name), Some(kind), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EnrichmentCategory, ScopeFilter, SymbolIndex, categorize, format_symbol_kind,
        lsp_kind_label, symbol_kind_to_string,
    };

    #[test]
    fn test_format_symbol_kind() {
        assert_eq!(format_symbol_kind("function"), "Function");
        assert_eq!(format_symbol_kind("implementation"), "Impl");
        assert_eq!(format_symbol_kind("struct"), "Struct");
        assert_eq!(format_symbol_kind("method"), "Method");
    }

    #[test]
    fn test_categorize() {
        assert_eq!(categorize("function"), EnrichmentCategory::Callable);
        assert_eq!(categorize("struct"), EnrichmentCategory::Type);
        assert_eq!(categorize("variable"), EnrichmentCategory::Other);
        assert_eq!(categorize("unknown"), EnrichmentCategory::Other);
    }

    #[test]
    fn test_lsp_kind_label() {
        assert_eq!(lsp_kind_label(1), "File");
        assert_eq!(lsp_kind_label(2), "Mod");
        assert_eq!(lsp_kind_label(3), "Ns");
        assert_eq!(lsp_kind_label(4), "Pkg");
        assert_eq!(lsp_kind_label(5), "Class");
        assert_eq!(lsp_kind_label(6), "Method");
        assert_eq!(lsp_kind_label(7), "Prop");
        assert_eq!(lsp_kind_label(8), "Field");
        assert_eq!(lsp_kind_label(9), "Ctor");
        assert_eq!(lsp_kind_label(10), "Enum");
        assert_eq!(lsp_kind_label(11), "Iface");
        assert_eq!(lsp_kind_label(12), "Fn");
        assert_eq!(lsp_kind_label(13), "Var");
        assert_eq!(lsp_kind_label(14), "Const");
        assert_eq!(lsp_kind_label(15), "Str");
        assert_eq!(lsp_kind_label(16), "Num");
        assert_eq!(lsp_kind_label(17), "Bool");
        assert_eq!(lsp_kind_label(18), "Array");
        assert_eq!(lsp_kind_label(19), "Obj");
        assert_eq!(lsp_kind_label(20), "Key");
        assert_eq!(lsp_kind_label(21), "Null");
        assert_eq!(lsp_kind_label(22), "Member");
        assert_eq!(lsp_kind_label(23), "Struct");
        assert_eq!(lsp_kind_label(24), "Event");
        assert_eq!(lsp_kind_label(25), "Op");
        assert_eq!(lsp_kind_label(26), "TypeParam");
        assert_eq!(lsp_kind_label(0), "Sym");
        assert_eq!(lsp_kind_label(27), "Sym");
        assert_eq!(lsp_kind_label(999), "Sym");
    }

    #[test]
    fn test_symbol_kind_to_string() {
        assert_eq!(symbol_kind_to_string(1), "file");
        assert_eq!(symbol_kind_to_string(2), "module");
        assert_eq!(symbol_kind_to_string(3), "namespace");
        assert_eq!(symbol_kind_to_string(4), "package");
        assert_eq!(symbol_kind_to_string(5), "class");
        assert_eq!(symbol_kind_to_string(6), "method");
        assert_eq!(symbol_kind_to_string(7), "property");
        assert_eq!(symbol_kind_to_string(8), "field");
        assert_eq!(symbol_kind_to_string(9), "constructor");
        assert_eq!(symbol_kind_to_string(10), "enum");
        assert_eq!(symbol_kind_to_string(11), "interface");
        assert_eq!(symbol_kind_to_string(12), "function");
        assert_eq!(symbol_kind_to_string(13), "variable");
        assert_eq!(symbol_kind_to_string(14), "constant");
        assert_eq!(symbol_kind_to_string(15), "string");
        assert_eq!(symbol_kind_to_string(16), "number");
        assert_eq!(symbol_kind_to_string(17), "boolean");
        assert_eq!(symbol_kind_to_string(18), "array");
        assert_eq!(symbol_kind_to_string(19), "object");
        assert_eq!(symbol_kind_to_string(20), "key");
        assert_eq!(symbol_kind_to_string(21), "null");
        assert_eq!(symbol_kind_to_string(22), "member");
        assert_eq!(symbol_kind_to_string(23), "struct");
        assert_eq!(symbol_kind_to_string(24), "event");
        assert_eq!(symbol_kind_to_string(25), "operator");
        assert_eq!(symbol_kind_to_string(26), "type_parameter");
        assert_eq!(symbol_kind_to_string(0), "unknown");
        assert_eq!(symbol_kind_to_string(27), "unknown");
        assert_eq!(symbol_kind_to_string(999), "unknown");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_populate_and_query() {
        let index = SymbolIndex::new().expect("create index");

        let symbols = serde_json::json!([
            {
                "name": "foo",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            },
            {
                "name": "Bar",
                "kind": 23,
                "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 10, "character": 1 } },
                "selectionRange": { "start": { "line": 4, "character": 7 }, "end": { "line": 4, "character": 10 } },
                "children": [
                    {
                        "name": "baz",
                        "kind": 6,
                        "range": { "start": { "line": 5, "character": 4 }, "end": { "line": 7, "character": 5 } },
                        "selectionRange": { "start": { "line": 5, "character": 7 }, "end": { "line": 5, "character": 10 } }
                    }
                ]
            }
        ]);

        let path = std::path::Path::new("/test/file.rs");
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");

        assert!(index.has_symbols_for(path));

        let results = index.query(".*", None).expect("query all");
        assert_eq!(results.len(), 3, "expected 3 symbols (foo, Bar, baz)");

        let names: Vec<&str> = results.iter().map(|(_, s)| s.name.as_str()).collect();
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"Bar"));
        assert!(names.contains(&"baz"));

        // Check scope
        let baz = results.iter().find(|(_, s)| s.name == "baz").expect("baz");
        assert_eq!(baz.1.scope.as_deref(), Some("Bar"));
        assert_eq!(baz.1.scope_kind.as_deref(), Some("struct"));
        assert_eq!(baz.1.kind, "method");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_deprecated_tag() {
        let index = SymbolIndex::new().expect("create index");

        let symbols = serde_json::json!([
            {
                "name": "old_fn",
                "kind": 12,
                "tags": [1],
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 9 } }
            },
            {
                "name": "new_fn",
                "kind": 12,
                "range": { "start": { "line": 4, "character": 0 }, "end": { "line": 6, "character": 1 } },
                "selectionRange": { "start": { "line": 4, "character": 3 }, "end": { "line": 4, "character": 9 } }
            }
        ]);

        let path = std::path::Path::new("/test/file.rs");
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");

        let results = index.query(".*", None).expect("query");
        let old = results
            .iter()
            .find(|(_, s)| s.name == "old_fn")
            .expect("old_fn");
        assert!(old.1.deprecated, "old_fn should be deprecated");

        let new = results
            .iter()
            .find(|(_, s)| s.name == "new_fn")
            .expect("new_fn");
        assert!(!new.1.deprecated, "new_fn should not be deprecated");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_invalidate() {
        let index = SymbolIndex::new().expect("create index");

        let symbols = serde_json::json!([
            {
                "name": "foo",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
            }
        ]);

        let path = std::path::Path::new("/test/file.rs");
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");
        assert!(index.has_symbols_for(path));

        index.invalidate(path).expect("invalidate");
        assert!(!index.has_symbols_for(path));

        // Re-populate
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("re-populate");
        assert!(index.has_symbols_for(path));
    }

    /// `evict_root` is a prefix sweep: it drops every outline AND enrichment
    /// entry under the removed root while leaving sibling roots untouched
    /// (bug #36). Exercises both backing maps directly.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn evict_root_prefix_sweep_drops_under_root_keeps_siblings() {
        let mut index = SymbolIndex::new().expect("create index");

        let symbols = serde_json::json!([{
            "name": "foo",
            "kind": 12,
            "range": { "start": { "line": 0 }, "end": { "line": 0 } },
            "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
        }]);

        let under = std::path::Path::new("/proj/a/file.rs");
        let sibling = std::path::Path::new("/proj/b/file.rs");
        index
            .populate_from_document_symbols(under, &symbols)
            .expect("populate under");
        index
            .populate_from_document_symbols(sibling, &symbols)
            .expect("populate sibling");

        // Seed the enrichment cache for one position under each path.
        let empty = || super::SymbolEnrichment {
            ref_lines: std::collections::HashMap::new(),
            incoming_calls: Vec::new(),
            outgoing_calls: Vec::new(),
            implementations: Vec::new(),
            supertypes: Vec::new(),
            subtypes: Vec::new(),
        };
        index.cache_enrichment(under, 0, 0, "/proj/a".into(), 0, None, empty());
        index.cache_enrichment(sibling, 0, 0, "/proj/b".into(), 0, None, empty());

        // Both maps carry an entry for each path before eviction.
        assert!(index.has_symbols_for(under));
        assert!(index.has_symbols_for(sibling));
        assert!(
            index
                .enrichment_cache
                .contains_key(&(under.to_path_buf(), 0, 0))
        );
        assert!(
            index
                .enrichment_cache
                .contains_key(&(sibling.to_path_buf(), 0, 0))
        );

        index.evict_root(std::path::Path::new("/proj/a"));

        // The under-root entries are gone from BOTH maps; the sibling survives.
        assert!(!index.has_symbols_for(under), "outline under root evicted");
        assert!(index.has_symbols_for(sibling), "sibling outline retained");
        assert!(
            !index
                .enrichment_cache
                .contains_key(&(under.to_path_buf(), 0, 0)),
            "enrichment under root evicted"
        );
        assert!(
            index
                .enrichment_cache
                .contains_key(&(sibling.to_path_buf(), 0, 0)),
            "sibling enrichment retained"
        );
    }

    /// Bug #26: populating a real file records its mtime, and a later write
    /// (newer mtime) is reported by `symbols_outdated` so `ensure_symbols`
    /// re-requests `documentSymbol`. Invalidation clears the recorded mtime.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn populate_records_mtime_and_symbols_outdated_detects_external_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("outline.rs");
        std::fs::write(&file, "fn alpha() {}\n").expect("write file");

        let index = SymbolIndex::new().expect("create index");
        let symbols = serde_json::json!([{
            "name": "alpha",
            "kind": 12,
            "range": { "start": { "line": 0 }, "end": { "line": 0 } },
            "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
        }]);
        index
            .populate_from_document_symbols(&file, &symbols)
            .expect("populate");
        assert!(index.has_symbols_for(&file));

        let recorded = crate::bridge::filesystem_manager::mtime_nanos(
            &std::fs::metadata(&file).expect("metadata"),
        );
        // Just populated — current rows are not outdated at the recorded mtime.
        assert!(
            !index.symbols_outdated(&file, recorded),
            "freshly populated symbols are current"
        );
        // A later external write (strictly newer mtime) is detected as stale.
        assert!(
            index.symbols_outdated(&file, recorded + 1),
            "a newer on-disk mtime marks the symbols outdated"
        );

        // Invalidation drops both the rows and the recorded mtime.
        index.invalidate(&file).expect("invalidate");
        assert!(index.needs_population(&file), "rows dropped");
        assert!(
            !index.symbols_outdated(&file, recorded + 1),
            "the recorded mtime is cleared on invalidate"
        );
    }

    /// A path that cannot be stat-ed (a synthetic test path) records no mtime,
    /// so `symbols_outdated` is always `false` — staleness degrades to the
    /// prior absence-only behavior rather than spuriously re-populating.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn symbols_outdated_false_without_recorded_mtime() {
        let index = SymbolIndex::new().expect("create index");
        let path = std::path::Path::new("/nonexistent/synthetic/file.rs");
        let symbols = serde_json::json!([{
            "name": "x",
            "kind": 12,
            "range": { "start": { "line": 0 }, "end": { "line": 0 } },
            "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
        }]);
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");

        assert!(index.has_symbols_for(path), "rows are stored regardless");
        assert!(
            !index.symbols_outdated(path, i64::MAX),
            "no recorded mtime → never reported outdated"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_has_children() {
        let index = SymbolIndex::new().expect("create index");

        let symbols = serde_json::json!([
            {
                "name": "Container",
                "kind": 23,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 16 } },
                "children": [
                    {
                        "name": "child",
                        "kind": 6,
                        "range": { "start": { "line": 1, "character": 4 }, "end": { "line": 3, "character": 5 } },
                        "selectionRange": { "start": { "line": 1, "character": 7 }, "end": { "line": 1, "character": 12 } }
                    }
                ]
            }
        ]);

        let path = std::path::Path::new("/test/file.rs");
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");

        assert!(index.has_children(path, "Container"));
        assert!(!index.has_children(path, "child"));
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn test_no_symbols_for_unknown_file() {
        let index = SymbolIndex::new().expect("create index");
        assert!(!index.has_symbols_for(std::path::Path::new("/unknown/file.rs")));
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn needs_symbols_returns_unpopulated_paths() {
        let index = SymbolIndex::new().expect("create index");

        let symbols = serde_json::json!([{
            "name": "foo",
            "kind": 12,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
            "selectionRange": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }
        }]);

        let populated = std::path::PathBuf::from("/test/populated.rs");
        let missing_a = std::path::PathBuf::from("/test/missing_a.rs");
        let missing_b = std::path::PathBuf::from("/test/missing_b.rs");

        index
            .populate_from_document_symbols(&populated, &symbols)
            .expect("populate");

        let all = vec![populated.clone(), missing_a.clone(), missing_b.clone()];
        let need = index.needs_symbols(&all);
        assert_eq!(need.len(), 2, "only unpopulated paths: {need:?}");
        assert!(need.contains(&&missing_a));
        assert!(need.contains(&&missing_b));

        // Populated path alone returns empty.
        assert!(
            index
                .needs_symbols(std::slice::from_ref(&populated))
                .is_empty()
        );

        // All unpopulated returns all.
        assert_eq!(index.needs_symbols(&[missing_a, missing_b]).len(), 2);

        // Single-path convenience: needs_population.
        assert!(
            !index.needs_population(&populated),
            "populated file should not need population"
        );
        assert!(
            index.needs_population(std::path::Path::new("/test/unknown.rs")),
            "unknown file should need population"
        );
    }

    /// Helper: builds a two-file index for multi-file query tests.
    ///
    /// File A (`/test/a.rs`): `alpha` (function, lines 0–5), `Beta` (struct, lines 10–20)
    ///   with child `gamma` (method, lines 12–18).
    /// File B (`/test/b.rs`): `delta` (function, lines 0–3).
    #[allow(clippy::expect_used, reason = "test helper")]
    fn two_file_index() -> SymbolIndex {
        let index = SymbolIndex::new().expect("create index");

        let syms_a = serde_json::json!([
            {
                "name": "alpha",
                "kind": 12,
                "range": { "start": { "line": 0 }, "end": { "line": 5 } },
                "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
            },
            {
                "name": "Beta",
                "kind": 23,
                "range": { "start": { "line": 10 }, "end": { "line": 20 } },
                "selectionRange": { "start": { "line": 10 }, "end": { "line": 10 } },
                "children": [{
                    "name": "gamma",
                    "kind": 6,
                    "range": { "start": { "line": 12 }, "end": { "line": 18 } },
                    "selectionRange": { "start": { "line": 12 }, "end": { "line": 12 } }
                }]
            }
        ]);
        let syms_b = serde_json::json!([
            {
                "name": "delta",
                "kind": 12,
                "range": { "start": { "line": 0 }, "end": { "line": 3 } },
                "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
            }
        ]);

        index
            .populate_from_document_symbols(std::path::Path::new("/test/a.rs"), &syms_a)
            .expect("populate a");
        index
            .populate_from_document_symbols(std::path::Path::new("/test/b.rs"), &syms_b)
            .expect("populate b");
        index
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_with_file_filter() {
        let index = two_file_index();
        let path_a = std::path::PathBuf::from("/test/a.rs");
        let path_b = std::path::PathBuf::from("/test/b.rs");

        // Filter to file A only — should return alpha, Beta, gamma but not delta.
        let filtered = index
            .query(".*", Some(std::slice::from_ref(&path_a)))
            .expect("query filtered");
        let names: Vec<&str> = filtered.iter().map(|(_, s)| s.name.as_str()).collect();
        assert_eq!(filtered.len(), 3, "file A has 3 symbols: {names:?}");
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"Beta"));
        assert!(names.contains(&"gamma"));
        assert!(
            !names.contains(&"delta"),
            "delta is in file B, should be excluded"
        );

        // Filter to file B only.
        let filtered_b = index
            .query(".*", Some(std::slice::from_ref(&path_b)))
            .expect("query filtered b");
        assert_eq!(filtered_b.len(), 1);
        assert_eq!(filtered_b[0].1.name, "delta");

        // Filtered to A should be fewer than all.
        let all = index.query(".*", None).expect("query all");
        assert!(filtered.len() < all.len());

        // Empty file list falls through to unfiltered branch.
        let empty_filter = index.query(".*", Some(&[])).expect("query empty filter");
        assert_eq!(empty_filter.len(), all.len());
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn find_enclosing_returns_tightest_span() {
        let index = two_file_index();
        let path = std::path::Path::new("/test/a.rs");

        // Line 15 is inside gamma (12–18) which is inside Beta (10–20).
        // find_enclosing should return gamma (tightest span).
        let enc = index
            .find_enclosing(path, 15)
            .expect("query")
            .expect("should find enclosing");
        assert_eq!(enc.name, "gamma");
        assert_eq!(enc.kind, "method");
        assert_eq!(enc.line, 12);
        assert_eq!(enc.end_line, 18);
        assert_eq!(enc.scope.as_deref(), Some("Beta"));
        assert!(!enc.deprecated);

        // Line 3 is inside alpha (0–5).
        let enc_alpha = index
            .find_enclosing(path, 3)
            .expect("query")
            .expect("should find alpha");
        assert_eq!(enc_alpha.name, "alpha");

        // Line 25 is outside all symbols.
        let none = index.find_enclosing(path, 25).expect("query");
        assert!(none.is_none(), "no symbol at line 25");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn find_enclosing_deprecated_flag() {
        let index = SymbolIndex::new().expect("create index");
        let symbols = serde_json::json!([{
            "name": "old_fn",
            "kind": 12,
            "tags": [1],
            "range": { "start": { "line": 0 }, "end": { "line": 5 } },
            "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
        }]);
        let path = std::path::Path::new("/test/dep.rs");
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");

        let enc = index
            .find_enclosing(path, 2)
            .expect("query")
            .expect("should find");
        assert!(enc.deprecated, "deprecated flag should be true");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_outline_batch_returns_top_level_only() {
        let index = two_file_index();
        let path_a = std::path::Path::new("/test/a.rs");
        let path_b = std::path::Path::new("/test/b.rs");

        let outlines = index
            .query_outline_batch(&[path_a, path_b])
            .expect("outline batch");

        // File A: alpha and Beta are top-level; gamma is nested (scope = "Beta").
        let a_syms = outlines.get(path_a).expect("file A present");
        let a_names: Vec<&str> = a_syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(a_syms.len(), 2, "top-level only: {a_names:?}");
        assert_eq!(a_syms[0].name, "alpha", "ordered by line");
        assert_eq!(a_syms[1].name, "Beta");
        assert!(a_syms[0].scope.is_none(), "alpha has no scope");

        // File B: delta is top-level.
        let b_syms = outlines.get(path_b).expect("file B present");
        assert_eq!(b_syms.len(), 1);
        assert_eq!(b_syms[0].name, "delta");

        // Empty input returns empty map.
        let empty = index.query_outline_batch(&[]).expect("empty");
        assert!(empty.is_empty());
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_scoped_children_of() {
        let index = two_file_index();
        let path_a = std::path::Path::new("/test/a.rs");

        // ChildrenOf("Beta") should return gamma.
        let result = index
            .query_scoped(
                &[path_a],
                &ScopeFilter::ChildrenOf("Beta"),
                "*",
                None,
                false,
            )
            .expect("children of");
        let syms = result.get(path_a).expect("file A");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "gamma");
        assert_eq!(syms[0].kind, "method");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_scoped_within_span() {
        let index = two_file_index();
        let path_a = std::path::Path::new("/test/a.rs");

        // WithinSpan(10, 20) covers Beta (line 10) and gamma (line 12).
        let result = index
            .query_scoped(
                &[path_a],
                &ScopeFilter::WithinSpan(10, 20),
                "*",
                None,
                false,
            )
            .expect("within span");
        let syms = result.get(path_a).expect("file A");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(syms.len(), 2, "Beta + gamma in span: {names:?}");
        assert_eq!(syms[0].name, "Beta");
        assert_eq!(syms[1].name, "gamma");

        // WithinSpan(0, 5) covers only alpha (line 0).
        let narrow = index
            .query_scoped(&[path_a], &ScopeFilter::WithinSpan(0, 5), "*", None, false)
            .expect("narrow span");
        let narrow_syms = narrow.get(path_a).expect("file A");
        assert_eq!(narrow_syms.len(), 1);
        assert_eq!(narrow_syms[0].name, "alpha");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_scoped_with_kind_filter() {
        let index = two_file_index();
        let path_a = std::path::Path::new("/test/a.rs");

        // AnyDepth + kind "function" should return alpha only (not Beta/gamma).
        let result = index
            .query_scoped(
                &[path_a],
                &ScopeFilter::AnyDepth,
                "*",
                Some("function"),
                false,
            )
            .expect("kind filter");
        let syms = result.get(path_a).expect("file A");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "alpha");
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_scoped_top_level() {
        let index = two_file_index();
        let path_a = std::path::Path::new("/test/a.rs");

        // TopLevel should return alpha and Beta but not gamma.
        let result = index
            .query_scoped(&[path_a], &ScopeFilter::TopLevel, "*", None, false)
            .expect("top level");
        let syms = result.get(path_a).expect("file A");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(syms.len(), 2, "top-level: {names:?}");
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"Beta"));
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_scoped_name_glob() {
        let index = two_file_index();
        let path_a = std::path::Path::new("/test/a.rs");

        // Name glob "al*" should match only alpha.
        let result = index
            .query_scoped(&[path_a], &ScopeFilter::AnyDepth, "al*", None, false)
            .expect("name glob");
        let syms = result.get(path_a).expect("file A");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "alpha");

        // ChildrenOf("Beta") with name glob "g*" should match gamma.
        let child_glob = index
            .query_scoped(
                &[path_a],
                &ScopeFilter::ChildrenOf("Beta"),
                "g*",
                None,
                false,
            )
            .expect("child glob");
        let child_syms = child_glob.get(path_a).expect("file A");
        assert_eq!(child_syms.len(), 1);
        assert_eq!(child_syms[0].name, "gamma");

        // ChildrenOf("Beta") with non-matching glob — empty.
        let no_match = index
            .query_scoped(
                &[path_a],
                &ScopeFilter::ChildrenOf("Beta"),
                "zzz*",
                None,
                false,
            )
            .expect("no match glob");
        assert!(
            !no_match.contains_key(path_a),
            "no symbols match zzz* under Beta"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_scoped_deprecated_only() {
        let index = SymbolIndex::new().expect("create index");
        let symbols = serde_json::json!([
            {
                "name": "old_fn",
                "kind": 12,
                "tags": [1],
                "range": { "start": { "line": 0 }, "end": { "line": 5 } },
                "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
            },
            {
                "name": "new_fn",
                "kind": 12,
                "range": { "start": { "line": 10 }, "end": { "line": 15 } },
                "selectionRange": { "start": { "line": 10 }, "end": { "line": 10 } }
            }
        ]);
        let path = std::path::Path::new("/test/dep.rs");
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");

        let result = index
            .query_scoped(&[path], &ScopeFilter::AnyDepth, "*", None, true)
            .expect("deprecated only");
        let syms = result.get(path).expect("file");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "old_fn");
        assert!(syms[0].deprecated);
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn query_scoped_empty_files() {
        let index = two_file_index();
        let result = index
            .query_scoped(&[], &ScopeFilter::AnyDepth, "*", None, false)
            .expect("empty files");
        assert!(result.is_empty());
    }

    /// Helper: builds a minimal `SymbolEnrichment` for cache tests.
    fn dummy_enrichment() -> super::SymbolEnrichment {
        super::SymbolEnrichment {
            ref_lines: std::collections::HashMap::from([(
                "/test/other.rs".to_string(),
                std::collections::HashSet::from([10, 20]),
            )]),
            incoming_calls: vec![super::CallEdge {
                name: "caller".to_string(),
                kind: 12,
                container: None,
                file: "/test/caller.rs".to_string(),
                line: 5,
                deprecated: false,
            }],
            outgoing_calls: Vec::new(),
            implementations: vec![("/test/impl.rs".to_string(), 42)],
            supertypes: Vec::new(),
            subtypes: Vec::new(),
        }
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn enrichment_cache_hit() {
        let mut index = SymbolIndex::new().expect("create index");
        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        let root = std::path::PathBuf::from("/workspace");
        let file = std::path::Path::new("/workspace/src/main.rs");

        // Cache an enrichment at generation 0. Synthetic path → source_mtime
        // None at cache time and on re-stat, so the floor matches (None == None).
        index.cache_enrichment(file, 10, 5, root, 0, None, dummy_enrichment());

        // Should hit — generation matches (both 0).
        let hit = index.get_enrichment(file, 10, 5, &fs);
        assert!(hit.is_some(), "expected cache hit");
        let enrichment = hit.expect("just checked");
        assert_eq!(enrichment.implementations.len(), 1);
        assert_eq!(enrichment.incoming_calls.len(), 1);
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn enrichment_cache_generation_miss() {
        let mut index = SymbolIndex::new().expect("create index");
        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        let root = std::path::PathBuf::from("/workspace");
        let file = std::path::Path::new("/workspace/src/main.rs");

        // Cache at generation 5 — but FilesystemManager returns 0 (no bumps).
        index.cache_enrichment(file, 10, 5, root, 5, None, dummy_enrichment());

        // Should miss — stale generation.
        let miss = index.get_enrichment(file, 10, 5, &fs);
        assert!(miss.is_none(), "expected cache miss on stale generation");

        // Entry should have been evicted.
        assert!(
            index.enrichment_cache.is_empty(),
            "stale entry should be evicted"
        );
    }

    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn enrichment_cache_root_scoped() {
        let dir_a = tempfile::tempdir().expect("tempdir a");
        let dir_b = tempfile::tempdir().expect("tempdir b");
        let file_a = dir_a.path().join("lib.rs");
        let file_b = dir_b.path().join("lib.rs");
        std::fs::write(&file_a, "fn a() {}\n").expect("write a");
        std::fs::write(&file_b, "fn b() {}\n").expect("write b");

        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        fs.set_roots(vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()]);

        let mut index = SymbolIndex::new().expect("create index");

        // Cache entries in both roots at generation 0, recording each file's
        // current mtime so the floor passes on read (the test mutates only
        // the generation, not the files).
        let mtime_a = std::fs::metadata(&file_a)
            .ok()
            .map(|m| crate::bridge::filesystem_manager::mtime_nanos(&m));
        let mtime_b = std::fs::metadata(&file_b)
            .ok()
            .map(|m| crate::bridge::filesystem_manager::mtime_nanos(&m));
        index.cache_enrichment(
            &file_a,
            1,
            0,
            dir_a.path().to_path_buf(),
            0,
            mtime_a,
            dummy_enrichment(),
        );
        index.cache_enrichment(
            &file_b,
            1,
            0,
            dir_b.path().to_path_buf(),
            0,
            mtime_b,
            dummy_enrichment(),
        );

        // Bump generation for root A only (simulates editing a file there).
        fs.bump_generations(std::slice::from_ref(&file_a));

        // Root A entry should be stale, root B should survive.
        assert!(
            index.get_enrichment(&file_a, 1, 0, &fs).is_none(),
            "root A should be stale after diff"
        );
        assert!(
            index.get_enrichment(&file_b, 1, 0, &fs).is_some(),
            "root B should survive"
        );
    }

    /// A host `Edit`/`Write` to the enriched position's source file advances its
    /// mtime but does not bump a root generation; the floor must still miss
    /// (bug-26-sibling enrichment-staleness gap) and evict the stale entry.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn enrichment_floor_misses_after_source_mtime_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("src.rs");
        std::fs::write(&file, "fn original() {}\n").expect("write");

        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let mut index = SymbolIndex::new().expect("create index");
        let source_mtime = std::fs::metadata(&file)
            .ok()
            .map(|m| crate::bridge::filesystem_manager::mtime_nanos(&m));
        index.cache_enrichment(
            &file,
            1,
            0,
            dir.path().to_path_buf(),
            0,
            source_mtime,
            dummy_enrichment(),
        );
        assert!(
            index.get_enrichment(&file, 1, 0, &fs).is_some(),
            "fresh cache should hit before any edit"
        );

        // Rewrite the source file with a strictly-newer mtime (no generation
        // bump — mirrors a host Edit/Write).
        std::fs::write(&file, "fn edited() {}\n").expect("rewrite");
        let f = std::fs::File::options()
            .write(true)
            .open(&file)
            .expect("open");
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
            .expect("set mtime");
        drop(f);

        assert!(
            index.get_enrichment(&file, 1, 0, &fs).is_none(),
            "an edit to the source file must miss"
        );
        assert!(
            index.enrichment_cache.is_empty(),
            "stale entry should be evicted"
        );
    }

    /// A removed source file (stat fails → `current = None != Some(stored)`)
    /// also misses.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn enrichment_floor_misses_when_source_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("src.rs");
        std::fs::write(&file, "fn original() {}\n").expect("write");

        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let mut index = SymbolIndex::new().expect("create index");
        let source_mtime = std::fs::metadata(&file)
            .ok()
            .map(|m| crate::bridge::filesystem_manager::mtime_nanos(&m));
        index.cache_enrichment(
            &file,
            1,
            0,
            dir.path().to_path_buf(),
            0,
            source_mtime,
            dummy_enrichment(),
        );

        std::fs::remove_file(&file).expect("remove");
        assert!(
            index.get_enrichment(&file, 1, 0, &fs).is_none(),
            "a removed source file must miss"
        );
    }

    /// Cache then immediately read: same generation and unchanged mtime ⇒ hit.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn enrichment_hit_when_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("src.rs");
        std::fs::write(&file, "fn alpha() {}\n").expect("write");

        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let mut index = SymbolIndex::new().expect("create index");
        let source_mtime = std::fs::metadata(&file)
            .ok()
            .map(|m| crate::bridge::filesystem_manager::mtime_nanos(&m));
        index.cache_enrichment(
            &file,
            1,
            0,
            dir.path().to_path_buf(),
            0,
            source_mtime,
            dummy_enrichment(),
        );

        let hit = index.get_enrichment(&file, 1, 0, &fs);
        assert!(hit.is_some(), "unchanged generation and mtime should hit");
        assert_eq!(hit.expect("just checked").implementations.len(), 1);
    }

    /// Regression guard for the existing generation gate: bumping the root
    /// generation (no mtime change) still misses.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn enrichment_generation_gate_still_applies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("src.rs");
        std::fs::write(&file, "fn alpha() {}\n").expect("write");

        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        fs.set_roots(vec![dir.path().to_path_buf()]);

        let mut index = SymbolIndex::new().expect("create index");
        let source_mtime = std::fs::metadata(&file)
            .ok()
            .map(|m| crate::bridge::filesystem_manager::mtime_nanos(&m));
        index.cache_enrichment(
            &file,
            1,
            0,
            dir.path().to_path_buf(),
            0,
            source_mtime,
            dummy_enrichment(),
        );

        // Bump the generation without touching the file (sed/diagnostics path).
        fs.bump_generation_for_test(dir.path());

        assert!(
            index.get_enrichment(&file, 1, 0, &fs).is_none(),
            "a generation bump must miss even with an unchanged mtime"
        );
        assert!(
            index.enrichment_cache.is_empty(),
            "stale entry should be evicted"
        );
    }

    /// WS31-review R5 (finding L2): an entry whose `source_mtime` was cached as
    /// `None` (stat failed at cache time) and whose file is STILL unstattable on
    /// read must MISS, not be served forever. The mtime floor compares
    /// `current != entry.source_mtime`; when both sides are `None` the `!=` is
    /// false, so the floor falls through and returns `Some(hit)` — the
    /// generation gate is the only remaining guard. The generation is pinned to
    /// the root's current value (0 for a never-bumped root) so the gen gate
    /// passes and the test isolates the both-`None` mtime floor.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    #[ignore = "RED: WS31-review R5; un-ignore in fix"]
    fn ws31_review_r5_unstattable_floor_entry_misses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        let fs = crate::bridge::filesystem_manager::FilesystemManager::new();
        fs.set_roots(vec![root.clone()]);

        let mut index = SymbolIndex::new().expect("create index");

        // A path that does NOT exist — its source mtime is genuinely None at
        // cache time and remains None on read (still unstattable).
        let absent = root.join("absent.rs");

        // Cache at generation 0 (matches the never-bumped root, neutralizing
        // the generation gate) with source_mtime = None.
        index.cache_enrichment(&absent, 1, 0, root, 0, None, dummy_enrichment());

        assert!(
            index.get_enrichment(&absent, 1, 0, &fs).is_none(),
            "a cached enrichment whose source mtime is None and whose file is still unstattable must MISS, not be served forever"
        );
        assert!(
            index.enrichment_cache.is_empty(),
            "the unstattable entry should be evicted"
        );
    }
}
