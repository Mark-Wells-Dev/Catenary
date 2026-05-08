// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Symbol index for workspace-wide symbol extraction.
//!
//! Provides [`SymbolIndex`], a SQLite-backed symbol cache populated from
//! `textDocument/documentSymbol` LSP responses. The index starts empty and
//! is filled lazily via [`SymbolIndex::populate_from_document_symbols()`].
//! Callers are responsible for requesting `documentSymbol` from the LSP
//! server and feeding the response to the index.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use rusqlite::Connection;

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

/// Workspace-wide symbol index backed by in-memory `SQLite`.
///
/// Populated lazily from `textDocument/documentSymbol` LSP responses.
/// The symbol index is ephemeral — built during a session, discarded
/// on session end. No dependency on the persistent session database.
///
/// Also caches per-position enrichment results (references, call
/// hierarchy, implementations, type hierarchy) with per-root generation
/// counter invalidation.
pub struct SymbolIndex {
    /// In-memory connection for symbol reads and writes.
    conn: Connection,
    /// Per-position enrichment cache: `(file, line, col)` → cached result.
    enrichment_cache: HashMap<(PathBuf, u32, u32), CachedEnrichment>,
}

impl SymbolIndex {
    /// Creates a new empty symbol index.
    ///
    /// The in-memory database is created with the symbols table schema.
    /// Symbols are populated lazily via [`populate_from_document_symbols()`](Self::populate_from_document_symbols).
    ///
    /// # Errors
    ///
    /// Returns an error if the in-memory database cannot be created.
    pub fn new() -> Result<Self> {
        let conn = Connection::open_in_memory().context("failed to open in-memory database")?;
        conn.execute_batch(
            "CREATE TABLE symbols (
                file_path   TEXT NOT NULL,
                name        TEXT NOT NULL,
                kind        TEXT NOT NULL,
                line        INTEGER NOT NULL,
                end_line    INTEGER NOT NULL,
                scope       TEXT,
                scope_kind  TEXT,
                deprecated  INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (file_path, line)
            );
            CREATE INDEX idx_symbols_name ON symbols(name);
            CREATE INDEX idx_symbols_scope ON symbols(file_path, scope);",
        )
        .context("failed to create in-memory tables")?;

        conn.create_scalar_function(
            "regexp",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let pattern = ctx.get_raw(0).as_str()?;
                let text = ctx.get_raw(1).as_str()?;
                let re = regex::Regex::new(pattern)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                Ok(re.is_match(text))
            },
        )
        .context("failed to register REGEXP function")?;

        Ok(Self {
            conn,
            enrichment_cache: HashMap::new(),
        })
    }

    /// Populates the index for a file from a `documentSymbol` LSP response.
    ///
    /// Walks the `DocumentSymbol` hierarchy (recursive children), flattens
    /// into rows. Sets `scope`/`scope_kind` from the parent. Sets
    /// `deprecated` from `tags` containing `SymbolTag::Deprecated` (value 1).
    /// Replaces existing symbols for the file (delete + insert in transaction).
    ///
    /// The `symbols` parameter is the JSON array from the LSP response.
    ///
    /// # Errors
    ///
    /// Returns an error if the database transaction fails.
    pub fn populate_from_document_symbols(
        &self,
        file_path: &Path,
        symbols: &serde_json::Value,
    ) -> Result<()> {
        let path_str = file_path.to_string_lossy();
        let mut flat: Vec<Symbol> = Vec::new();

        if let Some(arr) = symbols.as_array() {
            for sym in arr {
                flatten_document_symbol(sym, None, None, &mut flat);
            }
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .context("begin transaction")?;

        tx.execute(
            "DELETE FROM symbols WHERE file_path = ?1",
            rusqlite::params![path_str.as_ref() as &str],
        )
        .context("failed to delete old symbols")?;

        for sym in &flat {
            tx.execute(
                "INSERT OR IGNORE INTO symbols \
                 (file_path, name, kind, line, end_line, scope, scope_kind, deprecated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    path_str.as_ref() as &str,
                    sym.name,
                    sym.kind,
                    sym.line,
                    sym.end_line,
                    sym.scope,
                    sym.scope_kind,
                    sym.deprecated,
                ],
            )
            .with_context(|| format!("failed to insert symbol {} in {}", sym.name, path_str))?;
        }

        tx.commit().context("commit transaction")?;
        Ok(())
    }

    /// Returns `true` if the file has any rows in the `symbols` table.
    #[must_use]
    pub fn has_symbols_for(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM symbols WHERE file_path = ?1)",
                rusqlite::params![path_str.as_ref() as &str],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }

    /// Deletes all symbols for the file. Next access should re-populate.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub fn invalidate(&self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();
        self.conn
            .execute(
                "DELETE FROM symbols WHERE file_path = ?1",
                rusqlite::params![path_str.as_ref() as &str],
            )
            .context("failed to invalidate symbols")?;
        Ok(())
    }

    /// Query the index for symbols whose names match a regex pattern.
    ///
    /// If `files` is `Some`, only symbols from those files are returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the regex is invalid or the query fails.
    pub fn query(
        &self,
        pattern: &str,
        files: Option<&[PathBuf]>,
    ) -> Result<Vec<(PathBuf, Symbol)>> {
        let mut results = Vec::new();

        match files {
            Some(file_list) if !file_list.is_empty() => {
                let placeholders: String = (0..file_list.len())
                    .map(|i| format!("?{}", i + 2))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT file_path, name, kind, line, end_line, scope, scope_kind, deprecated \
                     FROM symbols WHERE name REGEXP ?1 AND file_path IN ({placeholders})"
                );
                let mut stmt = self.conn.prepare(&sql).context("failed to prepare query")?;

                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    Vec::with_capacity(1 + file_list.len());
                params.push(Box::new(pattern.to_string()));
                for f in file_list {
                    params.push(Box::new(f.to_string_lossy().to_string()));
                }
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(AsRef::as_ref).collect();

                let rows = stmt
                    .query_map(param_refs.as_slice(), Self::row_to_symbol)
                    .context("failed to execute query")?;
                for row in rows {
                    results.push(row.context("failed to read symbol row")?);
                }
            }
            _ => {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT file_path, name, kind, line, end_line, scope, scope_kind, deprecated \
                         FROM symbols WHERE name REGEXP ?1",
                    )
                    .context("failed to prepare query")?;
                let rows = stmt
                    .query_map([pattern], Self::row_to_symbol)
                    .context("failed to execute query")?;
                for row in rows {
                    results.push(row.context("failed to read symbol row")?);
                }
            }
        }

        Ok(results
            .into_iter()
            .map(|(p, sym)| (PathBuf::from(p), sym))
            .collect())
    }

    /// Query depth-0 (outline) symbols for a batch of files.
    ///
    /// Returns symbols with `scope IS NULL` grouped by file path,
    /// ordered by line number within each file. Used by the glob tool
    /// for defensive maps.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn query_outline_batch(&self, files: &[&Path]) -> Result<HashMap<PathBuf, Vec<Symbol>>> {
        if files.is_empty() {
            return Ok(HashMap::new());
        }

        let placeholders: String = (0..files.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT file_path, name, kind, line, end_line, scope, scope_kind, deprecated \
             FROM symbols \
             WHERE file_path IN ({placeholders}) AND scope IS NULL \
             ORDER BY file_path, line"
        );

        let mut stmt = self.conn.prepare(&sql).context("prepare outline batch")?;
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::with_capacity(files.len());
        for f in files {
            params.push(Box::new(f.to_string_lossy().to_string()));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(AsRef::as_ref).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), Self::row_to_symbol)
            .context("execute outline batch")?;

        let mut result: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        for row in rows {
            let (path_str, sym) = row.context("read outline row")?;
            result.entry(PathBuf::from(path_str)).or_default().push(sym);
        }

        Ok(result)
    }

    /// Finds the innermost symbol enclosing a line in a file.
    ///
    /// Returns the tightest definition (smallest span) containing the given
    /// 0-based line.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn find_enclosing(&self, file_path: &Path, line_0: u32) -> Result<Option<Symbol>> {
        let path_str = file_path.to_string_lossy();
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, line, end_line, scope, scope_kind, deprecated \
             FROM symbols \
             WHERE file_path = ?1 AND line <= ?2 AND end_line >= ?2 \
             ORDER BY (end_line - line) ASC \
             LIMIT 1",
        )?;

        let result = stmt
            .query_row(
                rusqlite::params![path_str.as_ref() as &str, line_0],
                |row| {
                    Ok(Symbol {
                        name: row.get(0)?,
                        kind: row.get(1)?,
                        line: row.get(2)?,
                        end_line: row.get(3)?,
                        scope: row.get(4)?,
                        scope_kind: row.get(5)?,
                        deprecated: row.get::<_, i32>(6).unwrap_or(0) != 0,
                    })
                },
            )
            .ok();

        Ok(result)
    }

    /// Map a database row to a `(file_path, Symbol)` pair.
    ///
    /// Expected column order:
    /// `file_path, name, kind, line, end_line, scope, scope_kind, deprecated`
    fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, Symbol)> {
        Ok((
            row.get(0)?,
            Symbol {
                name: row.get(1)?,
                kind: row.get(2)?,
                line: row.get(3)?,
                end_line: row.get(4)?,
                scope: row.get(5)?,
                scope_kind: row.get(6)?,
                deprecated: row.get::<_, i32>(7).unwrap_or(0) != 0,
            },
        ))
    }

    /// Check whether a scope (container) has children in the given file.
    ///
    /// Returns `true` if any symbol in the index has `scope = scope_name`
    /// within the given file path.
    #[must_use]
    pub fn has_children(&self, file_path: &Path, scope_name: &str) -> bool {
        let path_str = file_path.to_string_lossy();
        self.conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM symbols WHERE file_path = ?1 AND scope = ?2)",
                rusqlite::params![path_str.as_ref() as &str, scope_name],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }

    /// Query symbols filtered by scope, name glob, kind, and deprecated status.
    ///
    /// Used by the `into` pipeline for segment-by-segment symbol tree navigation.
    /// Results are grouped by file path and ordered by line number.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
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

        let placeholders: String = (0..files.len())
            .map(|i| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let mut conditions = vec![format!("file_path IN ({placeholders})")];

        let scope_extra: usize = match scope {
            ScopeFilter::TopLevel | ScopeFilter::AnyDepth => 0,
            ScopeFilter::ChildrenOf(_) => 1,
            ScopeFilter::WithinSpan(_, _) => 2,
        };

        match scope {
            ScopeFilter::TopLevel => conditions.push("scope IS NULL".to_string()),
            ScopeFilter::ChildrenOf(_) => {
                conditions.push(format!("scope = ?{}", files.len() + 1));
            }
            ScopeFilter::AnyDepth => {}
            ScopeFilter::WithinSpan(_, _) => {
                let base = files.len() + 1;
                conditions.push(format!("line >= ?{base}"));
                conditions.push(format!("line <= ?{}", base + 1));
            }
        }

        conditions.push(format!("name GLOB ?{}", files.len() + scope_extra + 1));

        if let Some(_kind) = kind_filter {
            conditions.push(format!("kind = ?{}", files.len() + scope_extra + 2));
        }

        if deprecated_only {
            conditions.push("deprecated = 1".to_string());
        }

        let sql = format!(
            "SELECT file_path, name, kind, line, end_line, scope, scope_kind, deprecated \
             FROM symbols WHERE {} ORDER BY file_path, line",
            conditions.join(" AND ")
        );

        let mut stmt = self.conn.prepare(&sql).context("prepare scoped query")?;

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        for f in files {
            params.push(Box::new(f.to_string_lossy().to_string()));
        }
        match scope {
            ScopeFilter::ChildrenOf(name) => {
                params.push(Box::new(name.to_string()));
            }
            ScopeFilter::WithinSpan(start, end) => {
                params.push(Box::new(*start));
                params.push(Box::new(*end));
            }
            _ => {}
        }
        params.push(Box::new(name_glob.to_string()));
        if let Some(kind) = kind_filter {
            params.push(Box::new(kind.to_string()));
        }

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(AsRef::as_ref).collect();

        let rows = stmt
            .query_map(param_refs.as_slice(), Self::row_to_symbol)
            .context("execute scoped query")?;

        let mut result: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        for row in rows {
            let (path_str, sym) = row.context("read scoped query row")?;
            result.entry(PathBuf::from(path_str)).or_default().push(sym);
        }

        Ok(result)
    }

    /// Returns a cached enrichment result if the generation still matches.
    ///
    /// Checks the per-root generation counter against the current value from
    /// `FilesystemManager`. Returns `None` on miss or stale generation
    /// (evicts the stale entry). Returns a clone because a stale hit requires
    /// mutable access to evict the entry.
    pub(crate) fn get_enrichment(
        &mut self,
        file: &Path,
        line: u32,
        col: u32,
        fs_manager: &super::bridge::filesystem_manager::FilesystemManager,
    ) -> Option<SymbolEnrichment> {
        let key = (file.to_path_buf(), line, col);
        let entry = self.enrichment_cache.get(&key)?;
        let current_gen = fs_manager.root_generation(&entry.root);
        if entry.generation == current_gen {
            Some(entry.enrichment.clone())
        } else {
            self.enrichment_cache.remove(&key);
            None
        }
    }

    /// Stores an enrichment result in the cache.
    ///
    /// Records the current root generation from `FilesystemManager` so that
    /// future lookups can detect staleness.
    pub(crate) fn cache_enrichment(
        &mut self,
        file: &Path,
        line: u32,
        col: u32,
        root: PathBuf,
        generation: u64,
        enrichment: SymbolEnrichment,
    ) {
        let key = (file.to_path_buf(), line, col);
        self.enrichment_cache.insert(
            key,
            CachedEnrichment {
                enrichment,
                root,
                generation,
            },
        );
    }
}

/// Recursively flattens a `DocumentSymbol` JSON node into [`Symbol`] rows.
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
    use super::{EnrichmentCategory, SymbolIndex, categorize, format_symbol_kind, lsp_kind_label};

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
        assert_eq!(lsp_kind_label(12), "Fn");
        assert_eq!(lsp_kind_label(11), "Iface");
        assert_eq!(lsp_kind_label(2), "Mod");
        assert_eq!(lsp_kind_label(999), "Sym");
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

        // Cache an enrichment at generation 0.
        index.cache_enrichment(file, 10, 5, root, 0, dummy_enrichment());

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
        index.cache_enrichment(file, 10, 5, root, 5, dummy_enrichment());

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
        fs.seed();

        let mut index = SymbolIndex::new().expect("create index");

        // Cache entries in both roots at generation 0.
        index.cache_enrichment(
            &file_a,
            1,
            0,
            dir_a.path().to_path_buf(),
            0,
            dummy_enrichment(),
        );
        index.cache_enrichment(
            &file_b,
            1,
            0,
            dir_b.path().to_path_buf(),
            0,
            dummy_enrichment(),
        );

        // Modify file in root A — bumps generation for root A only.
        std::fs::write(&file_a, "fn a_changed() {}\n").expect("write a changed");
        let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000);
        let times = std::fs::FileTimes::new().set_modified(time);
        std::fs::File::options()
            .write(true)
            .open(&file_a)
            .expect("open for set_mtime")
            .set_times(times)
            .expect("set_times");
        let _ = fs.diff();

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
}
