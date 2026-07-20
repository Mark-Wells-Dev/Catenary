// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Symbol index for workspace-wide symbol extraction.
//!
//! Provides [`SymbolIndex`], an in-memory symbol cache populated from
//! `textDocument/documentSymbol` LSP responses. The index starts empty and
//! is filled lazily via [`SymbolIndex::populate_from_document_symbols()`].
//! Callers are responsible for requesting `documentSymbol` from the LSP
//! server and feeding the response to the index.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};

use crate::bridge::filesystem_manager::mtime_nanos;

/// Non-cryptographic 64-bit content hash of `bytes` via std's [`DefaultHasher`].
///
/// Staleness detection, not security: a same-second external write that leaves
/// the mtime unchanged still changes the bytes, so a differing hash catches the
/// slip the mtime backstop misses (bug #26). Zero-dep by design — the
/// page-cache-warm read dominates the cost, so the hash algorithm is off the
/// hot path and an xxh3-class dependency buys nothing measurable here.
///
/// Shared with the held-open document change gate
/// ([`crate::lsp::LspClient::plan_document_sync`], diagnostics-debt 01) — one
/// staleness idiom, not two.
pub(crate) fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// A symbol extracted from the symbol index.
#[derive(Clone)]
pub struct Symbol {
    /// Symbol name.
    pub name: String,
    /// Kind string (e.g., `"function"`, `"struct"`).
    pub kind: String,
    /// 0-based line of the symbol's name (`selectionRange.start`) — the
    /// declaration line `grep` matches, not `range.start` (which includes
    /// leading doc comments and attributes).
    pub line: u32,
    /// 0-based end line of the definition (`range.end`, for structure spans).
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

/// Workspace-wide symbol index held in memory.
///
/// Populated lazily from `textDocument/documentSymbol` LSP responses and
/// stored as per-file symbol lists. The symbol index is ephemeral — built
/// during a session, discarded on session end. No dependency on any
/// persistent store.
pub struct SymbolIndex {
    /// Per-file symbol lists, each kept sorted by start `line`, plus the
    /// on-disk mtime and content hash each file was populated from.
    ///
    /// Wrapped in a [`RefCell`] so
    /// [`populate_from_document_symbols`](Self::populate_from_document_symbols)
    /// and [`invalidate`](Self::invalidate) keep their `&self` signature: every
    /// live caller holds the index behind a `Mutex`, which already serializes
    /// access, so the cell is never borrowed concurrently.
    files: RefCell<HashMap<PathBuf, FileEntry>>,
    /// Test-only tally of content hashes computed by
    /// [`symbols_outdated`](Self::symbols_outdated) — one per same-mtime hit
    /// that falls through to the content check. Per-index (not a global static)
    /// so the scope test can assert a query touching N files hashes exactly N
    /// without racing parallel tests. A [`Cell`] mirrors the `&self` interior
    /// mutability of `files`.
    #[cfg(test)]
    hash_count: Cell<usize>,
}

/// A file's cached symbols and the on-disk mtime and content hash they were
/// populated from.
struct FileEntry {
    /// Flattened symbols, sorted ascending by start `line`. At most one symbol
    /// is kept per start line, mirroring the old `PRIMARY KEY (file_path, line)`.
    symbols: Vec<Symbol>,
    /// On-disk `mtime_nanos` recorded at population time, or `None` when the
    /// path could not be stat-ed. The cheap first leg of
    /// [`symbols_outdated`](SymbolIndex::symbols_outdated): a moved mtime is
    /// stale outright, no hash needed.
    mtime: Option<i64>,
    /// 64-bit content hash of the file bytes recorded at population time, or
    /// `None` when the path could not be read. The second leg of
    /// [`symbols_outdated`](SymbolIndex::symbols_outdated): when the mtime is
    /// unchanged, a differing hash catches a same-second external write the
    /// mtime backstop misses (bug #26).
    hash: Option<u64>,
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
            #[cfg(test)]
            hash_count: Cell::new(0),
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
    /// Records the file's current on-disk mtime *and* content hash alongside the
    /// symbols so a later external write (host `Edit`/`Write`, `git checkout`,
    /// formatter) that leaves the symbols untouched is detected as stale by
    /// [`symbols_outdated`](Self::symbols_outdated) (bug #26). The hash closes
    /// the same-second slip the mtime alone misses. Capturing both here means
    /// every populate path — `grep`/`glob` and the diagnostics batch — records
    /// them uniformly. A path that cannot be stat-ed or read (a synthetic test
    /// path) records no mtime/hash, degrading to the prior absence-only
    /// behavior.
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
        // Stat and hash before storing. Both describe the version the server saw
        // (the caller opened the document from disk before requesting
        // `documentSymbol`), so a write landing after this point either advances
        // the mtime (cheap-path stale) or, landing within the same second,
        // changes the content hash (bug #26 same-second slip) and is caught on
        // the next access.
        let recorded_mtime: Option<i64> =
            std::fs::metadata(file_path).ok().map(|m| mtime_nanos(&m));
        let recorded_hash: Option<u64> = std::fs::read(file_path)
            .ok()
            .map(|bytes| hash_bytes(&bytes));

        self.files.borrow_mut().insert(
            file_path.to_path_buf(),
            FileEntry {
                symbols: flatten_symbols(symbols),
                mtime: recorded_mtime,
                hash: recorded_hash,
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

    /// Drops all cached outlines for files under `root` — a prefix sweep of the
    /// per-file symbol map.
    ///
    /// Called when `root` leaves the tracked set (MCP disconnect,
    /// `catenary unpin`, `SubagentStop`) so an untracked path can no longer
    /// serve a dead session's cached outline (bug #36), and so caches for gone
    /// roots do not accumulate across sessions (a leak). Aligns the
    /// `SymbolIndex` lifetime with the tracked-root set.
    pub fn evict_root(&self, root: &Path) {
        self.files.borrow_mut().retain(|p, _| !p.starts_with(root));
    }

    /// Returns `true` when `path` has cached symbols that no longer match the
    /// file on disk — an external write the daemon never invalidated (host
    /// `Edit`/`Write`, `git checkout`, formatter; bug #26).
    ///
    /// Staleness is `(mtime, content hash)`, checked cheapest-first:
    /// - **mtime moved** (`current_mtime > recorded`) → stale outright, no hash
    ///   (the cheap direction stays cheap).
    /// - **mtime unchanged** → read the bytes and hash them; a differing hash
    ///   catches a same-second write the mtime backstop misses. An equal hash
    ///   short-circuits — no repopulation.
    ///
    /// The hash runs **only** on this per-file serve/hit path, which sees only
    /// the query's fan-out files — the index is never swept. `current_mtime` is
    /// the file's current `mtime_nanos` (nanoseconds since epoch), already
    /// stat-ed by the caller; the bytes are read here, lazily, only when the
    /// mtime is unchanged.
    ///
    /// Reports *staleness* of present symbols; *absence* is reported by
    /// [`needs_population`](Self::needs_population). A file with no recorded
    /// mtime (never populated, or populated from a path that could not be
    /// stat-ed) returns `false`: there is nothing to compare against, and
    /// absence already forces population. When the mtime is unchanged but no
    /// hash was recorded, or the bytes cannot be read now, staleness degrades to
    /// `false` — the same conservative "nothing to compare, absence handles it"
    /// stance.
    #[must_use]
    pub fn symbols_outdated(&self, path: &Path, current_mtime: i64) -> bool {
        let files = self.files.borrow();
        let Some(entry) = files.get(path) else {
            return false;
        };
        let Some(recorded_mtime) = entry.mtime else {
            return false;
        };
        // Cheap leg: a moved mtime is stale outright, no hash needed.
        if current_mtime > recorded_mtime {
            return true;
        }
        // Same-mtime leg: hash the current bytes to catch the same-second slip.
        // No recorded hash → nothing to compare against, so not stale.
        let Some(recorded_hash) = entry.hash else {
            return false;
        };
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        #[cfg(test)]
        self.hash_count.set(self.hash_count.get() + 1);
        hash_bytes(&bytes) != recorded_hash
    }

    /// Test-only reader for the per-index content-hash tally — the number of
    /// hashes computed by [`symbols_outdated`](Self::symbols_outdated).
    #[cfg(test)]
    const fn hash_count(&self) -> usize {
        self.hash_count.get()
    }

    /// Test-only reset of the per-index content-hash tally to zero.
    #[cfg(test)]
    fn reset_hash_count(&self) {
        self.hash_count.set(0);
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

/// Flattens a `documentSymbol` LSP response (the JSON array) into per-file
/// [`Symbol`] entries: the recursive `DocumentSymbol` hierarchy walked with
/// parent scopes recorded, then deduplicated to one symbol per start line,
/// kept ascending — exactly the shape
/// [`SymbolIndex::populate_from_document_symbols`] stores (which delegates
/// here). `pub(crate)` so the file-grade sweep tier (brackets 04) parses a
/// rootless singleton's response through the same flatten the index uses —
/// the two tiers' symbol semantics cannot drift.
pub(crate) fn flatten_symbols(symbols: &serde_json::Value) -> Vec<Symbol> {
    let mut flat: Vec<Symbol> = Vec::new();
    if let Some(arr) = symbols.as_array() {
        for sym in arr {
            flatten_document_symbol(sym, None, None, &mut flat);
        }
    }
    // One symbol per start line, kept ascending — the old store keyed rows
    // on `(file_path, line)` with `INSERT OR IGNORE`, so the first symbol
    // seen at a line won and rows read back ordered by line.
    let mut by_line: BTreeMap<u32, Symbol> = BTreeMap::new();
    for sym in flat {
        by_line.entry(sym.line).or_insert(sym);
    }
    by_line.into_values().collect()
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
    let range_start_line = range
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let end_line = range
        .and_then(|r| r.get("end"))
        .and_then(|e| e.get("line"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(range_start_line);

    // The symbol's line is the NAME line (`selectionRange.start`), not
    // `range.start`. rust-analyzer's `range` spans the whole item INCLUDING
    // leading doc comments and attributes, but `grep` matches the declaration
    // line where the name appears — and the classifier keys its definition
    // lookup on that match line. Keying by `range.start` would miss every
    // doc-commented or attributed symbol (i.e. every public API in a codebase
    // that mandates doc comments), silently dropping enrichment (bug 48). Fall
    // back to `range.start` when `selectionRange` is absent. `end_line` stays
    // `range.end` so structure spans still cover the whole item.
    let name_line = node
        .get("selectionRange")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(range_start_line);

    let deprecated = node
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tags| tags.iter().any(|t| t.as_u64() == Some(1)));

    let line = u32::try_from(name_line).unwrap_or(u32::MAX);
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
    fn symbol_line_is_name_line_not_range_start() {
        // Repro for bug 48 (silent enrichment degradation on code symbols).
        //
        // rust-analyzer's `DocumentSymbol.range` spans the ENTIRE item,
        // including leading doc comments and attributes; `selectionRange` is the
        // name. This codebase mandates doc comments on public APIs, so virtually
        // every public symbol has `range.start.line` ABOVE the declaration line.
        //
        // `catenary grep` matches the declaration line (where the name appears)
        // and classifies a hit as an enrichable definition only when the symbol
        // index holds a symbol at that exact line (grep_server `def_lookup`). If
        // the index keys by `range.start.line` (the doc-comment line) the lookup
        // misses, the definition is demoted to a plain reference, and NO
        // enrichment is produced — silently. So the index must key by the name
        // line.
        //
        //   line 0:  /// doc comment          <- range.start.line
        //   line 1:  #[derive(Clone)]
        //   line 2:  pub fn format_deny() {   <- selectionRange.start.line (name)
        //
        // NOTE: every existing fixture sets range.start.line ==
        // selectionRange.start.line, which is exactly why this bug went
        // unnoticed.
        let index = SymbolIndex::new().expect("create index");
        let symbols = serde_json::json!([
            {
                "name": "format_deny",
                "kind": 12,
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 4, "character": 1 } },
                "selectionRange": { "start": { "line": 2, "character": 7 }, "end": { "line": 2, "character": 18 } }
            }
        ]);
        let path = std::path::Path::new("/test/hooks.rs");
        index
            .populate_from_document_symbols(path, &symbols)
            .expect("populate");

        let results = index.query("format_deny", None).expect("query");
        let (_, sym) = results.first().expect("format_deny must be indexed");
        assert_eq!(
            sym.line, 2,
            "symbol must be keyed by selectionRange (name on line 2), not \
             range.start (doc-comment line 0); a range.start key makes grep's \
             def_lookup miss the declaration line and silently drop enrichment \
             (bug 48)"
        );
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

    /// `evict_root` is a prefix sweep: it drops every cached outline under the
    /// removed root while leaving sibling roots untouched (bug #36).
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn evict_root_prefix_sweep_drops_under_root_keeps_siblings() {
        let index = SymbolIndex::new().expect("create index");

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

        // Both paths carry an outline before eviction.
        assert!(index.has_symbols_for(under));
        assert!(index.has_symbols_for(sibling));

        index.evict_root(std::path::Path::new("/proj/a"));

        // The under-root outline is gone; the sibling survives.
        assert!(!index.has_symbols_for(under), "outline under root evicted");
        assert!(index.has_symbols_for(sibling), "sibling outline retained");
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

    /// Misc 190 — the same-second slip pin. A file is populated, then rewritten
    /// with DIFFERENT content while its mtime is forced back equal to the
    /// populated version (`filetime`, standing in for a same-second host write
    /// the mtime backstop can't see). The mtime leg reports "unchanged", so
    /// `symbols_outdated` falls through to the content-hash leg — which sees the
    /// differing bytes and reports the symbols stale. This is the slip the
    /// bug-26 mtime-only backstop missed.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn same_mtime_different_content_is_outdated_via_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("slip.rs");
        std::fs::write(&file, "fn original() {}\n").expect("write original");

        let index = SymbolIndex::new().expect("create index");
        let symbols = serde_json::json!([{
            "name": "original",
            "kind": 12,
            "range": { "start": { "line": 0 }, "end": { "line": 0 } },
            "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
        }]);
        index
            .populate_from_document_symbols(&file, &symbols)
            .expect("populate");

        // The mtime the population recorded (and the version we pin back to).
        let recorded_mtime = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(&file).expect("metadata"),
        );

        // Rewrite with different content, then force the mtime back to the
        // recorded value — the same-second slip the mtime backstop misses.
        std::fs::write(&file, "fn rewritten() {}\n").expect("rewrite");
        filetime::set_file_mtime(&file, recorded_mtime).expect("pin mtime back");

        let current_mtime = crate::bridge::filesystem_manager::mtime_nanos(
            &std::fs::metadata(&file).expect("metadata after rewrite"),
        );

        index.reset_hash_count();
        assert!(
            index.symbols_outdated(&file, current_mtime),
            "same mtime but different content → stale via the content-hash leg"
        );
        assert_eq!(
            index.hash_count(),
            1,
            "the same-mtime path hashes exactly once"
        );
    }

    /// Misc 190 — an unchanged re-serve does not repopulate. The file is
    /// untouched, so the mtime leg reports "unchanged", the hash leg recomputes
    /// an equal hash, and `symbols_outdated` short-circuits to `false`. One hash
    /// is computed (the same-mtime fall-through), and it reports current.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn unchanged_reserve_short_circuits_on_equal_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("stable.rs");
        std::fs::write(&file, "fn stable() {}\n").expect("write file");

        let index = SymbolIndex::new().expect("create index");
        let symbols = serde_json::json!([{
            "name": "stable",
            "kind": 12,
            "range": { "start": { "line": 0 }, "end": { "line": 0 } },
            "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
        }]);
        index
            .populate_from_document_symbols(&file, &symbols)
            .expect("populate");

        let current_mtime = crate::bridge::filesystem_manager::mtime_nanos(
            &std::fs::metadata(&file).expect("metadata"),
        );

        index.reset_hash_count();
        assert!(
            !index.symbols_outdated(&file, current_mtime),
            "unchanged file at the same mtime → equal hash → not stale"
        );
        assert_eq!(
            index.hash_count(),
            1,
            "the equal-hash short-circuit still performs exactly one hash"
        );
    }

    /// Misc 190 — scope pin. A query touching N same-mtime files hashes exactly
    /// N times: the check runs once per fan-out file and never sweeps the index.
    /// Here the index holds a spare file the "query" does not touch; checking
    /// the N queried files leaves the spare unhashed.
    #[allow(clippy::expect_used, reason = "test assertions")]
    #[test]
    fn scoped_check_hashes_exactly_n_fanout_files() {
        // The fan-out width: N files a query names, plus one spare it does not.
        const N: usize = 4;

        let dir = tempfile::tempdir().expect("tempdir");
        let symbols = serde_json::json!([{
            "name": "s",
            "kind": 12,
            "range": { "start": { "line": 0 }, "end": { "line": 0 } },
            "selectionRange": { "start": { "line": 0 }, "end": { "line": 0 } }
        }]);

        let index = SymbolIndex::new().expect("create index");

        // Populate N fan-out files plus one spare the query never names.
        let mut fanout = Vec::new();
        for i in 0..N {
            let file = dir.path().join(format!("f{i}.rs"));
            std::fs::write(&file, format!("fn f{i}() {{}}\n")).expect("write fanout");
            index
                .populate_from_document_symbols(&file, &symbols)
                .expect("populate fanout");
            fanout.push(file);
        }
        let spare = dir.path().join("spare.rs");
        std::fs::write(&spare, "fn spare() {}\n").expect("write spare");
        index
            .populate_from_document_symbols(&spare, &symbols)
            .expect("populate spare");

        // Serve path: check staleness for exactly the N fan-out files (all at
        // their recorded, unchanged mtime, so each falls through to the hash).
        index.reset_hash_count();
        for file in &fanout {
            let current_mtime = crate::bridge::filesystem_manager::mtime_nanos(
                &std::fs::metadata(file).expect("metadata"),
            );
            assert!(
                !index.symbols_outdated(file, current_mtime),
                "unchanged fan-out file is not stale"
            );
        }
        assert_eq!(
            index.hash_count(),
            N,
            "a query touching N files hashes exactly N — the spare is never swept"
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
}
