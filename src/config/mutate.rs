// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Guided config mutations: format-preserving writes that turn a health
//! finding's fix-it into an executable action (tui-rework 05).
//!
//! The [`Mutation`] set is deliberately small — enable/disable a server, set a
//! server's binary path, apply a config-migration rename, disable a linter rule.
//! Free-form config editing stays a non-goal: the file remains the single source
//! of truth, and each mutation touches exactly one key (or one structural
//! rename), never re-serializing the parsed config. Writes go through
//! [`toml_edit`] so a hand-formatted config keeps its comments and layout
//! everywhere but the edited key.
//!
//! ## Layer routing is structural
//!
//! Which file a mutation may write is a **property of the key**, not a runtime
//! check performed after the fact. [`Mutation::top_section`] names the TOML
//! section a mutation authors; [`section_is_project_scoped`] is the engine's own
//! resolution rule (`[lsp]` / `[linter]` / `[diagnostics]` may live in a project
//! `.catenary.toml`; everything else — command enforcement, notifications — is a
//! daemon-global, user-level decision). [`Mutation::candidate_layers`] derives
//! the offered layers from that classifier, so a project write to an enforcement
//! key is never *constructed*: the TUI is structurally unable to author config
//! the engine would ignore (DESIGN resolved call 3).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, InlineTable, Item, RawString, Table, Value};

/// The config file a mutation writes to.
///
/// Distinct from a raw path so a preview can name the layer honestly and so the
/// project variant carries the root whose `.catenary.toml` it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLayer {
    /// The user config (`~/.config/catenary/config.toml`).
    User,
    /// A workspace root's project config (`<root>/.catenary.toml`).
    Project(PathBuf),
    /// A specific source file — a migration rewrites the file the legacy content
    /// lives in, not a chosen layer.
    File(PathBuf),
}

impl ConfigLayer {
    /// A short human label for a consent preview.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::User => "user config.toml".to_string(),
            Self::Project(_) => "project .catenary.toml".to_string(),
            Self::File(p) => p
                .file_name()
                .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into()),
        }
    }

    /// The concrete file this layer resolves to.
    ///
    /// # Errors
    ///
    /// Currently infallible — [`crate::paths::config_dir`] always resolves
    /// (env override → platform dir → durable fallback), so the old
    /// no-config-directory failure is unreachable. The `Result` is kept for
    /// call-site stability (`?` chains) and for future layers that may fail.
    pub fn resolve_path(&self) -> Result<PathBuf> {
        match self {
            Self::User => Ok(crate::paths::config_dir()
                .join("catenary")
                .join("config.toml")),
            Self::Project(root) => Ok(root.join(".catenary.toml")),
            Self::File(path) => Ok(path.clone()),
        }
    }
}

/// One server binding as it should appear in a `[lsp.language.*].servers` array
/// — the minimal shape [`Mutation::SetServerEnabled`] writes.
///
/// The enable/disable mutation rewrites the *whole* servers array from the
/// resolved bindings so a sibling server is never dropped; each binding round-
/// trips through this spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingSpec {
    /// The referenced `[lsp.server.*]` name.
    pub name: String,
    /// Whether this binding delivers diagnostics.
    pub diagnostics: bool,
    /// LSP methods suppressed for this binding (dispatch-method names).
    pub disabled_methods: Vec<String>,
}

impl BindingSpec {
    /// Render this binding as a TOML value: a bare string when it carries no
    /// per-binding overrides, else an inline table `{ name, diagnostics?,
    /// disabled_methods? }` — mirroring the config's own dual binding form.
    fn to_value(&self) -> Value {
        if self.diagnostics && self.disabled_methods.is_empty() {
            return Value::from(self.name.as_str());
        }
        let mut table = InlineTable::new();
        table.insert("name", Value::from(self.name.as_str()));
        if !self.diagnostics {
            table.insert("diagnostics", Value::from(false));
        }
        if !self.disabled_methods.is_empty() {
            let methods: Value = self
                .disabled_methods
                .iter()
                .map(|m| Value::from(m.as_str()))
                .collect();
            table.insert("disabled_methods", methods);
        }
        Value::from(table)
    }
}

/// A guided, single-key config mutation.
///
/// Each variant carries exactly the data it writes. Construct one, offer its
/// [`candidate_layers`](Self::candidate_layers), preview it, then
/// [`apply`](Self::apply) — no variant edits more than one key or performs more
/// than one structural rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    /// Relocate a server's executable (`[lsp.server.<server>].path`).
    ///
    /// The server key IS the executable now (misc 162); `path` is the optional
    /// absolute-path override for a binary that is not on `PATH` under its key.
    SetServerPath {
        /// The `[lsp.server.*]` name.
        server: String,
        /// The absolute path to write.
        path: String,
    },
    /// Enable or disable a server for one language binding by rewriting
    /// `[lsp.language.<language>].servers` with `<server>`'s `diagnostics` flag
    /// toggled. `bindings` is the full resolved servers list for the language so
    /// siblings survive the override.
    SetServerEnabled {
        /// The `[lsp.language.*]` key.
        language: String,
        /// The server whose `diagnostics` flag flips.
        server: String,
        /// Whether the server should deliver diagnostics after the write.
        enabled: bool,
        /// The full resolved binding list for the language (written verbatim
        /// but for `server`'s toggled flag).
        bindings: Vec<BindingSpec>,
    },
    /// Disable or re-enable a linter rule (`[linter.rule.<rule>].disable`).
    SetLinterDisabled {
        /// The `[linter.rule.*]` name.
        rule: String,
        /// Whether the rule should be disabled after the write.
        disabled: bool,
    },
    /// Execute the misc-120 config-migration guidance on a source file: hoist the
    /// pre-namespacing top-level `[server.*]` / `[language.*]` tables under
    /// `[lsp.*]` and the legacy `[linter.<name>]` definition tables under
    /// `[linter.rule.*]`, preserving their contents and comments.
    MigrateNamespace {
        /// The config source file to rewrite.
        source: PathBuf,
    },
}

/// Whether a top-level TOML section may be authored at project scope.
///
/// This mirrors the engine's own resolution rule: `[lsp]` / `[linter]` /
/// `[diagnostics]` may live in a project `.catenary.toml`; everything else
/// (command enforcement, notifications, icons, …) resolves at user scope,
/// because the command filter is daemon-global across sessions (DESIGN resolved
/// call 3).
///
/// `[commands]` is deliberately absent: only its `build` key is project-honored,
/// and no v1 mutation authors `[commands]`, so classifying the whole section as
/// user-only keeps an enforcement mutation structurally off the project layer.
#[must_use]
pub fn section_is_project_scoped(section: &str) -> bool {
    matches!(section, "lsp" | "linter" | "diagnostics")
}

impl Mutation {
    /// The top-level TOML section this mutation authors, or `None` for a
    /// file-fixed rewrite (a migration targets the file the legacy content lives
    /// in, not a section).
    #[must_use]
    pub const fn top_section(&self) -> Option<&'static str> {
        match self {
            Self::SetServerPath { .. } | Self::SetServerEnabled { .. } => Some("lsp"),
            Self::SetLinterDisabled { .. } => Some("linter"),
            Self::MigrateNamespace { .. } => None,
        }
    }

    /// The layers this mutation may target, derived structurally from its
    /// [`top_section`](Self::top_section).
    ///
    /// A project layer is offered only when a `project_root` is given *and* the
    /// section is project-scoped ([`section_is_project_scoped`]); a migration is
    /// pinned to its source file. An enforcement-section mutation therefore never
    /// yields a project candidate — the routing is a property of the key, not a
    /// post-write validation.
    #[must_use]
    pub fn candidate_layers(&self, project_root: Option<&Path>) -> Vec<ConfigLayer> {
        if let Self::MigrateNamespace { source } = self {
            return vec![ConfigLayer::File(source.clone())];
        }
        let mut layers = vec![ConfigLayer::User];
        if let (Some(section), Some(root)) = (self.top_section(), project_root)
            && section_is_project_scoped(section)
        {
            layers.push(ConfigLayer::Project(root.to_path_buf()));
        }
        layers
    }

    /// The TOML key path this mutation writes, for a consent preview
    /// (e.g. `[lsp.server.rust-analyzer] command`).
    #[must_use]
    pub fn key_label(&self) -> String {
        match self {
            Self::SetServerPath { server, .. } => {
                format!("[lsp.server.{server}] path")
            }
            Self::SetServerEnabled { language, .. } => {
                format!("[lsp.language.{language}] servers")
            }
            Self::SetLinterDisabled { rule, .. } => {
                format!("[linter.rule.{rule}] disable")
            }
            Self::MigrateNamespace { .. } => "namespace rename".to_string(),
        }
    }

    /// The value this mutation writes, rendered for a consent preview.
    #[must_use]
    pub fn value_label(&self) -> String {
        match self {
            Self::SetServerPath { path, .. } => format!("\"{path}\""),
            Self::SetServerEnabled {
                server, enabled, ..
            } => {
                let state = if *enabled { "enabled" } else { "disabled" };
                format!("{server} → {state}")
            }
            Self::SetLinterDisabled { disabled, .. } => disabled.to_string(),
            Self::MigrateNamespace { .. } => {
                "[server.*]→[lsp.server.*], [language.*]→[lsp.language.*]".to_string()
            }
        }
    }

    /// Apply this mutation to `path`, preserving formatting outside the edited
    /// key. Creates the file (and parent directory) if absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed as TOML,
    /// the config shape is incompatible with the edit, or the write fails.
    pub fn apply(&self, path: &Path) -> Result<()> {
        let mut doc = read_document(path)?;
        match self {
            Self::SetServerPath { server, path } => {
                let table = ensure_table_path(doc.as_table_mut(), &["lsp", "server", server])?;
                table.insert("path", Item::Value(Value::from(path.as_str())));
            }
            Self::SetServerEnabled {
                language,
                server,
                enabled,
                bindings,
            } => {
                apply_server_enabled(&mut doc, language, server, *enabled, bindings)?;
            }
            Self::SetLinterDisabled { rule, disabled } => {
                let table = ensure_table_path(doc.as_table_mut(), &["linter", "rule", rule])?;
                table.insert("disable", Item::Value(Value::from(*disabled)));
            }
            Self::MigrateNamespace { .. } => migrate_namespace(&mut doc)?,
        }
        write_document(path, &doc)
    }
}

/// Adds a canonical root to the persisted pin list (`[roots] pinned`), preserving
/// the config's comments and layout everywhere but the edited array (misc 175).
///
/// `canonical` is the daemon's already-canonicalized root path — the same
/// spelling the [`crate::config::Config::pinned_roots`] boot restore matches
/// against the tracker. The entry is written home-compressed (a `$HOME`-prefixed
/// path renders with `~` to match the file's idiom); an entry whose expanded,
/// canonical form already equals `canonical` is a no-op, so a repeat pin never
/// duplicates a line. Creates the file (and `[roots]` table) when absent.
///
/// Returns `true` when a new entry was written, `false` when the root was already
/// pinned (the write is skipped and the file is left byte-identical).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed as TOML, the
/// `[roots]`/`pinned` shape is incompatible with the edit, or the write fails.
pub fn pin_config(path: &Path, canonical: &Path) -> Result<bool> {
    let mut doc = read_document(path)?;
    let array = ensure_pinned_array(&mut doc)?;
    if pinned_array_contains(array, canonical) {
        return Ok(false);
    }
    push_pin_entry(array, &crate::paths::compress_home(canonical));
    write_document(path, &doc)?;
    Ok(true)
}

/// Append `entry` to a pin array, preserving a hand-authored **multiline** layout.
///
/// `toml_edit`'s bare [`toml_edit::Array::push`] appends inline, collapsing a
/// one-entry-per-line array onto the previous line and shoving the new value in
/// front of the array's trailing decor. When the array already spans multiple
/// lines, this pushes with a decor prefix of `\n<indent>` — matching the existing
/// entries' indentation — so the new entry lands on its own line, its own-line
/// comment prefixes stay bound to their entries, and the file is not reflowed. An
/// inline (single-line) array keeps the plain inline push.
///
/// Comment placement follows `toml_edit`'s model: a comment on its own line above
/// an entry is that entry's *prefix* and is preserved exactly; a rare inline
/// comment after the previous last element lives in the array *trailing* (between
/// the final comma and `]`) and stays there — never lost, never rendered as
/// invalid TOML, though it then sits after the appended line.
fn push_pin_entry(array: &mut toml_edit::Array, entry: &str) {
    if let Some(indent) = multiline_indent(array) {
        let mut value = Value::from(entry);
        value.decor_mut().set_prefix(format!("\n{indent}"));
        array.push_formatted(value);
    } else {
        array.push(entry);
    }
}

/// The per-entry indentation of a multiline pin array, or `None` when the array
/// renders inline.
///
/// A pin array is multiline when an existing entry's decor prefix carries a
/// newline (a one-per-line hand-authored array) or, for an empty array, when its
/// trailing whitespace does (a `pinned = [\n]` skeleton). The indent is the
/// whitespace after that newline — reused verbatim so a 2-space or 4-space file
/// keeps its own gutter — falling back to two spaces when a newline is present
/// but the indent could not be read (an empty or comment-only run).
fn multiline_indent(array: &toml_edit::Array) -> Option<String> {
    fn indent_after_newline(raw: Option<&str>) -> Option<String> {
        let s = raw?;
        if !s.contains('\n') {
            return None;
        }
        let after = s.rsplit('\n').next().unwrap_or("");
        // Reuse a clean whitespace gutter verbatim; otherwise a sensible default.
        Some(if after.trim().is_empty() && !after.is_empty() {
            after.to_string()
        } else {
            "  ".to_string()
        })
    }

    // A populated array: read the first entry's prefix.
    if let Some(first) = array.get(0)
        && let Some(indent) =
            indent_after_newline(first.decor().prefix().and_then(RawString::as_str))
    {
        return Some(indent);
    }
    // An empty but multiline skeleton (`pinned = [\n]`): read the trailing.
    if array.is_empty() {
        return indent_after_newline(array.trailing().as_str());
    }
    None
}

/// Removes a canonical root from the persisted pin list (`[roots] pinned`),
/// preserving the config's comments and layout everywhere but the edited array
/// (misc 175).
///
/// Every entry whose expanded, canonical form equals `canonical` is dropped
/// (idiom-agnostic: a `~`-prefixed and an absolute spelling of the same root both
/// match, and a stray duplicate is cleared in one pass). An `unpin` of a
/// never-pinned path — or a config with no `[roots] pinned` array at all — is a
/// no-op that leaves the file untouched.
///
/// Returns `true` when at least one entry was removed, `false` when nothing
/// matched (the file is left byte-identical).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed as TOML, the
/// existing `pinned` value is not an array, or the write fails.
pub fn unpin_config(path: &Path, canonical: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut doc = read_document(path)?;
    let Some(array) = pinned_array_mut(&mut doc)? else {
        return Ok(false);
    };
    let before = array.len();
    array.retain(|entry| {
        entry
            .as_str()
            .is_none_or(|s| !pinned_entry_matches(s, canonical))
    });
    if array.len() == before {
        return Ok(false);
    }
    write_document(path, &doc)?;
    Ok(true)
}

/// Navigate/create the `[roots] pinned` array, returning it mutable.
///
/// Creates the `[roots]` table and an empty `pinned` array when absent. Errors if
/// `[roots]` or `pinned` exists as an incompatible non-table / non-array.
fn ensure_pinned_array(doc: &mut DocumentMut) -> Result<&mut toml_edit::Array> {
    let roots = ensure_table_path(doc.as_table_mut(), &["roots"])?;
    if !roots.contains_key("pinned") {
        roots.insert("pinned", Item::Value(Value::Array(toml_edit::Array::new())));
    }
    roots
        .get_mut("pinned")
        .and_then(Item::as_array_mut)
        .context("config key `roots.pinned` is not an array")
}

/// Borrow the existing `[roots] pinned` array mutably, or `None` when `[roots]`
/// or `pinned` is absent (an `unpin` on a config that never pinned anything).
///
/// Unlike [`ensure_pinned_array`] this creates nothing — an `unpin` must never
/// author a `[roots]` table just to find it empty. Errors if `pinned` exists as a
/// non-array.
fn pinned_array_mut(doc: &mut DocumentMut) -> Result<Option<&mut toml_edit::Array>> {
    let Some(roots) = doc
        .as_table_mut()
        .get_mut("roots")
        .and_then(Item::as_table_mut)
    else {
        return Ok(None);
    };
    let Some(item) = roots.get_mut("pinned") else {
        return Ok(None);
    };
    item.as_array_mut()
        .map(Some)
        .context("config key `roots.pinned` is not an array")
}

/// Whether any string entry in `array` canonically resolves to `canonical`.
fn pinned_array_contains(array: &toml_edit::Array, canonical: &Path) -> bool {
    array
        .iter()
        .filter_map(Value::as_str)
        .any(|s| pinned_entry_matches(s, canonical))
}

/// Whether the raw pin entry `raw` (as authored, possibly `~`-prefixed) resolves
/// to the canonical root `canonical`.
///
/// Expands a leading `~`, then canonicalizes so a symlinked or non-normalized
/// spelling still matches the tracker's canonical form; falls back to the
/// lexically-absolutized spelling when the path does not exist on disk (a
/// transiently absent mount — the entry stays comparable so `unpin` can still
/// remove it).
fn pinned_entry_matches(raw: &str, canonical: &Path) -> bool {
    let expanded = crate::bridge::expand_tilde(raw);
    let expanded = Path::new(&expanded);
    let resolved = expanded.canonicalize().unwrap_or_else(|_| {
        std::path::absolute(expanded).unwrap_or_else(|_| expanded.to_path_buf())
    });
    resolved == canonical
}

/// Rewrite `[lsp.language.<language>].servers` from `bindings`, with `server`'s
/// `diagnostics` flag set to `enabled`. Errors if `server` is not among
/// `bindings` (the caller supplies the resolved list).
fn apply_server_enabled(
    doc: &mut DocumentMut,
    language: &str,
    server: &str,
    enabled: bool,
    bindings: &[BindingSpec],
) -> Result<()> {
    if !bindings.iter().any(|b| b.name == server) {
        bail!("server '{server}' is not bound to language '{language}'");
    }
    let mut array = toml_edit::Array::new();
    for binding in bindings {
        let mut binding = binding.clone();
        if binding.name == server {
            binding.diagnostics = enabled;
        }
        array.push(binding.to_value());
    }
    let table = ensure_table_path(doc.as_table_mut(), &["lsp", "language", language])?;
    table.insert("servers", Item::Value(Value::Array(array)));
    Ok(())
}

/// Hoist pre-namespacing tables into their `[lsp.*]` / `[linter.rule.*]` homes.
fn migrate_namespace(doc: &mut DocumentMut) -> Result<()> {
    let root = doc.as_table_mut();
    hoist_top_table(root, "server", &["lsp", "server"])?;
    hoist_top_table(root, "language", &["lsp", "language"])?;
    hoist_legacy_linters(root)?;
    Ok(())
}

/// Move the top-level `[<old>.*]` table (if present) so its sub-tables live under
/// the dotted `dest` path, preserving each sub-table's contents and comments.
fn hoist_top_table(root: &mut Table, old: &str, dest: &[&str]) -> Result<()> {
    let Some(item) = root.remove(old) else {
        return Ok(());
    };
    let table = item
        .into_table()
        .map_err(|_| anyhow::anyhow!("[{old}] is not a table"))?;
    let target = ensure_table_path(root, dest)?;
    // The destination holds sub-tables (`[lsp.server.<name>]`), so keep it an
    // implicit parent — no bare `[lsp.server]` header of its own.
    target.set_implicit(true);
    for (key, value) in table {
        target.insert(key.as_str(), value);
    }
    Ok(())
}

/// Move legacy top-level `[linter.<name>]` definition tables under
/// `[linter.rule.*]`, leaving the `[linter]` feeder toggles (`disable`) and any
/// already-namespaced `[linter.rule.*]` in place.
fn hoist_legacy_linters(root: &mut Table) -> Result<()> {
    let Some(linter) = root.get_mut("linter").and_then(Item::as_table_mut) else {
        return Ok(());
    };
    let legacy: Vec<String> = linter
        .iter()
        .filter(|(name, item)| *name != "rule" && *name != "disable" && item.is_table())
        .map(|(name, _)| name.to_string())
        .collect();
    if legacy.is_empty() {
        return Ok(());
    }
    let mut moved: Vec<(String, Item)> = Vec::new();
    for name in legacy {
        if let Some(item) = linter.remove(&name) {
            moved.push((name, item));
        }
    }
    let rule = ensure_table(linter, "rule", true)?;
    for (name, item) in moved {
        rule.insert(&name, item);
    }
    Ok(())
}

/// Navigate/create the dotted table path `segments`, returning the leaf table.
///
/// Every segment but the last is created implicit (no `[header]` of its own);
/// the leaf is explicit so it renders as a `[a.b.c]` header when it gains a
/// key-value.
fn ensure_table_path<'a>(root: &'a mut Table, segments: &[&str]) -> Result<&'a mut Table> {
    let Some((leaf, parents)) = segments.split_last() else {
        return Ok(root);
    };
    let mut cursor = root;
    for seg in parents {
        cursor = ensure_table(cursor, seg, true)?;
    }
    ensure_table(cursor, leaf, false)
}

/// Return `key` as a mutable sub-table of `parent`, inserting an empty one (with
/// the given implicit flag) when absent. Errors when `key` exists as a non-table.
fn ensure_table<'a>(parent: &'a mut Table, key: &str, implicit: bool) -> Result<&'a mut Table> {
    if !parent.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(implicit);
        parent.insert(key, Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .with_context(|| format!("config key `{key}` is not a table"))
}

/// Read `path` as an editable TOML document, or an empty document when the file
/// does not exist yet.
fn read_document(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    contents
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse config file: {}", path.display()))
}

/// Write `doc` to `path`, creating the parent directory if needed.
///
/// The write is **atomic**: the rendered document lands in a sibling temp file
/// which is then renamed over `path` (an atomic replace within one directory on
/// POSIX). A crash or a concurrent writer therefore leaves either the old file
/// or the new one intact, never a truncated or interleaved one — closing the
/// torn-`write(2)` race that could strand the config in the empty/corrupt state
/// the create-fresh branch of [`read_document`] then rebuilds from scratch (bug
/// 109's section-destruction vector). The temp name carries the process id so
/// two writers targeting the same file never share a staging path.
fn write_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory: {}", parent.display()))?;
    }
    let rendered = doc.to_string();
    // Stage a sibling temp file (`<path>.tmp.<pid>`) in the target's own
    // directory, so the rename is a same-filesystem atomic replace (a cross-device
    // rename would fail). Appending to the full path keeps the temp beside the
    // target regardless of the filename's shape (extension or not); the pid keeps
    // two writers targeting one file on distinct staging paths.
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &rendered)
        .with_context(|| format!("failed to write config file: {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // The original file is untouched (the rename never began); drop the
        // stage so a failed write leaves no litter.
        let _ = std::fs::remove_file(&tmp);
        return Err(e)
            .with_context(|| format!("failed to replace config file: {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    #[test]
    fn set_server_path_updates_existing_key_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "config.toml",
            "# my servers\n[lsp.server.rust-analyzer]\npath = \"/old/rust-analyzer\"  # inline\n",
        );
        Mutation::SetServerPath {
            server: "rust-analyzer".to_string(),
            path: "/opt/ra/rust-analyzer".to_string(),
        }
        .apply(&path)
        .expect("apply");
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(out.contains("path = \"/opt/ra/rust-analyzer\""));
        assert!(
            out.contains("# my servers"),
            "header comment survives: {out}"
        );
    }

    #[test]
    fn set_server_path_creates_namespaced_header_not_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Mutation::SetServerPath {
            server: "gopls".to_string(),
            path: "/opt/go/bin/gopls".to_string(),
        }
        .apply(&path)
        .expect("apply");
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(
            out.contains("[lsp.server.gopls]"),
            "renders a table header, not an inline table: {out}"
        );
        // Round-trips through the real loader.
        let doc: toml::Value = toml::from_str(&out).expect("valid toml");
        assert_eq!(
            doc["lsp"]["server"]["gopls"]["path"].as_str(),
            Some("/opt/go/bin/gopls")
        );
    }

    #[test]
    fn hand_formatted_config_survives_mutation_intact_outside_edited_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "\
# Catenary config — hand tuned.
log_retention_days = 14   # keep two weeks

[notifications]
desktop = true   # keep the interrupt

# rust toolchain
[lsp.server.rust-analyzer]
path = \"/old/rust-analyzer\"
args = [\"--log-file\", \"/tmp/ra.log\"]   # debug
";
        let path = write(dir.path(), "config.toml", original);
        Mutation::SetServerPath {
            server: "rust-analyzer".to_string(),
            path: "/usr/local/bin/rust-analyzer".to_string(),
        }
        .apply(&path)
        .expect("apply");
        let out = std::fs::read_to_string(&path).expect("read");
        // Everything but the one edited value is byte-identical.
        for line in [
            "# Catenary config — hand tuned.",
            "log_retention_days = 14   # keep two weeks",
            "desktop = true   # keep the interrupt",
            "# rust toolchain",
            "args = [\"--log-file\", \"/tmp/ra.log\"]   # debug",
        ] {
            assert!(out.contains(line), "preserved line missing: {line}\n{out}");
        }
        assert!(out.contains("path = \"/usr/local/bin/rust-analyzer\""));
    }

    #[test]
    fn disable_linter_rule_writes_disable_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        Mutation::SetLinterDisabled {
            rule: "shellcheck".to_string(),
            disabled: true,
        }
        .apply(&path)
        .expect("apply");
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(out.contains("[linter.rule.shellcheck]"), "{out}");
        assert!(out.contains("disable = true"), "{out}");
    }

    #[test]
    fn disable_linter_rule_toggles_in_place_preserving_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            "config.toml",
            "[linter.rule.shellcheck]\ncommand = \"shellcheck\"\npatterns = [\"**/*.sh\"]\n",
        );
        Mutation::SetLinterDisabled {
            rule: "shellcheck".to_string(),
            disabled: true,
        }
        .apply(&path)
        .expect("apply");
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(
            out.contains("command = \"shellcheck\""),
            "command survives: {out}"
        );
        assert!(out.contains("disable = true"), "{out}");
    }

    #[test]
    fn enable_disable_server_rewrites_binding_without_dropping_siblings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let bindings = vec![
            BindingSpec {
                name: "rust-analyzer".to_string(),
                diagnostics: true,
                disabled_methods: Vec::new(),
            },
            BindingSpec {
                name: "somesim".to_string(),
                diagnostics: true,
                disabled_methods: Vec::new(),
            },
        ];
        Mutation::SetServerEnabled {
            language: "rust".to_string(),
            server: "rust-analyzer".to_string(),
            enabled: false,
            bindings,
        }
        .apply(&path)
        .expect("apply");
        let out = std::fs::read_to_string(&path).expect("read");
        let doc: toml::Value = toml::from_str(&out).expect("valid toml");
        let servers = doc["lsp"]["language"]["rust"]["servers"]
            .as_array()
            .expect("servers array");
        // Sibling survives as a bare string; the toggled one is an inline table.
        assert_eq!(servers.len(), 2, "both bindings present: {out}");
        assert!(out.contains("somesim"), "sibling preserved: {out}");
        assert!(out.contains("diagnostics = false"), "toggle written: {out}");
    }

    #[test]
    fn enable_server_not_bound_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let err = Mutation::SetServerEnabled {
            language: "rust".to_string(),
            server: "ghost".to_string(),
            enabled: true,
            bindings: vec![BindingSpec {
                name: "rust-analyzer".to_string(),
                diagnostics: true,
                disabled_methods: Vec::new(),
            }],
        }
        .apply(&path);
        assert!(err.is_err(), "a server not in the binding list is rejected");
    }

    #[test]
    fn migrate_namespace_hoists_and_preserves_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "\
# pre-namespacing config
[server.rust-analyzer]
command = \"rust-analyzer\"   # the binary

[language.rust]
extensions = [\"rs\"]

[linter.shellcheck]
command = \"shellcheck\"
";
        let path = write(dir.path(), "config.toml", original);
        Mutation::MigrateNamespace {
            source: path.clone(),
        }
        .apply(&path)
        .expect("apply");
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(
            out.contains("[lsp.server.rust-analyzer]"),
            "server hoisted: {out}"
        );
        assert!(
            out.contains("[lsp.language.rust]"),
            "language hoisted: {out}"
        );
        assert!(
            out.contains("[linter.rule.shellcheck]"),
            "linter hoisted: {out}"
        );
        assert!(!out.contains("\n[server."), "old server header gone: {out}");
        assert!(
            out.contains("# the binary"),
            "inner comment survives: {out}"
        );
        // The result parses under the real loader shape.
        let doc: toml::Value = toml::from_str(&out).expect("valid toml");
        assert_eq!(
            doc["lsp"]["server"]["rust-analyzer"]["command"].as_str(),
            Some("rust-analyzer")
        );
    }

    #[test]
    fn candidate_layers_offers_project_for_lsp_and_linter() {
        let root = PathBuf::from("/p/root");
        let cmd = Mutation::SetServerPath {
            server: "x".to_string(),
            path: "/opt/x".to_string(),
        };
        let layers = cmd.candidate_layers(Some(&root));
        assert_eq!(layers.len(), 2, "user + project");
        assert!(layers.contains(&ConfigLayer::User));
        assert!(layers.contains(&ConfigLayer::Project(root)));
    }

    #[test]
    fn candidate_layers_omits_project_without_root() {
        let cmd = Mutation::SetLinterDisabled {
            rule: "x".to_string(),
            disabled: true,
        };
        assert_eq!(cmd.candidate_layers(None), vec![ConfigLayer::User]);
    }

    #[test]
    fn migration_is_pinned_to_its_source_file() {
        let source = PathBuf::from("/home/u/.config/catenary/config.toml");
        let m = Mutation::MigrateNamespace {
            source: source.clone(),
        };
        assert_eq!(
            m.candidate_layers(Some(&PathBuf::from("/p/root"))),
            vec![ConfigLayer::File(source)],
        );
    }

    #[test]
    fn enforcement_sections_are_structurally_user_only() {
        // The classifier the candidate-layer routing is built on: an enforcement
        // section (or any non-definition section) never earns a project layer.
        assert!(section_is_project_scoped("lsp"));
        assert!(section_is_project_scoped("linter"));
        assert!(section_is_project_scoped("diagnostics"));
        assert!(!section_is_project_scoped("commands"));
        assert!(!section_is_project_scoped("notifications"));
    }

    // ── persisted pins (misc 175) ────────────────────────────────────

    #[test]
    fn pin_creates_roots_pinned_array_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let root = PathBuf::from("/srv/project");
        assert!(pin_config(&path, &root).expect("pin"));
        let out = std::fs::read_to_string(&path).expect("read");
        // A `[roots]` header with the pinned array, and the absolute path
        // verbatim (not under $HOME).
        assert!(out.contains("[roots]"), "roots header written: {out}");
        assert!(out.contains("\"/srv/project\""), "entry written: {out}");
        // Round-trips through the real loader shape.
        let doc: toml::Value = toml::from_str(&out).expect("valid toml");
        let pinned = doc["roots"]["pinned"].as_array().expect("pinned array");
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].as_str(), Some("/srv/project"));
    }

    #[test]
    fn pin_renders_home_prefixed_path_with_tilde() {
        // The written entry uses `~` for a $HOME-prefixed root, matching the
        // file's idiom. `compress_home` reads $HOME; construct the root under it
        // so the assertion is deterministic without mutating the environment.
        let home = std::env::var("HOME").expect("HOME set in test env");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let root = PathBuf::from(&home).join("Projects").join("Widget");
        assert!(pin_config(&path, &root).expect("pin"));
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(
            out.contains("\"~/Projects/Widget\""),
            "home-prefixed path rendered with ~: {out}"
        );
    }

    #[test]
    fn pin_is_idempotent_no_duplicate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let root = PathBuf::from("/srv/project");
        assert!(pin_config(&path, &root).expect("first pin adds"));
        assert!(
            !pin_config(&path, &root).expect("second pin is a no-op"),
            "a repeat pin reports no change"
        );
        let out = std::fs::read_to_string(&path).expect("read");
        let doc: toml::Value = toml::from_str(&out).expect("valid toml");
        assert_eq!(
            doc["roots"]["pinned"].as_array().expect("pinned").len(),
            1,
            "no duplicate entry: {out}"
        );
    }

    #[test]
    fn pin_unpin_round_trip_preserves_hand_authored_comments() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Own-line comments (the common hand-authored form) bind to their entry as
        // a prefix and survive exactly; the pin appends on its own line without
        // reflowing the file.
        let original = "\
# Catenary config — hand tuned.
log_retention_days = 14   # keep two weeks

[roots]
# my long-lived projects
pinned = [
  # the main repo
  \"/srv/alpha\",
]

# rust toolchain
[lsp.server.rust-analyzer]
path = \"/usr/bin/rust-analyzer\"
";
        let path = write(dir.path(), "config.toml", original);
        let beta = PathBuf::from("/srv/beta");
        assert!(pin_config(&path, &beta).expect("pin beta"));
        let after_pin = std::fs::read_to_string(&path).expect("read");
        // Every comment and the alpha entry survive byte-for-byte; the file still
        // parses and now carries beta on its own line.
        for line in [
            "# Catenary config — hand tuned.",
            "log_retention_days = 14   # keep two weeks",
            "# my long-lived projects",
            "  # the main repo",
            "  \"/srv/alpha\",",
            "# rust toolchain",
            "path = \"/usr/bin/rust-analyzer\"",
        ] {
            assert!(
                after_pin.contains(line),
                "comment/line survives pin: {line:?}\n{after_pin}"
            );
        }
        assert!(after_pin.contains("/srv/beta"), "beta added: {after_pin}");
        // Beta landed on its own line with the file's own indent — no reflow.
        assert!(
            after_pin.contains("  \"/srv/beta\""),
            "beta on its own indented line: {after_pin}"
        );
        // The result is valid TOML with both entries.
        let doc: toml::Value = toml::from_str(&after_pin).expect("valid toml after pin");
        assert_eq!(doc["roots"]["pinned"].as_array().expect("pinned").len(), 2);

        // Unpin beta: the added entry goes, every comment stays.
        assert!(unpin_config(&path, &beta).expect("unpin beta"));
        let after_unpin = std::fs::read_to_string(&path).expect("read");
        assert!(
            !after_unpin.contains("/srv/beta"),
            "beta removed: {after_unpin}"
        );
        for line in [
            "# Catenary config — hand tuned.",
            "# my long-lived projects",
            "  # the main repo",
            "  \"/srv/alpha\",",
            "# rust toolchain",
        ] {
            assert!(
                after_unpin.contains(line),
                "comment/line survives unpin: {line:?}\n{after_unpin}"
            );
        }
    }

    #[test]
    fn unpin_matches_canonically_across_spellings() {
        // A `~`-spelled entry and the absolute spelling of the same real dir are
        // the same root — `unpin` on either removes it (canonical comparison).
        let real = tempfile::tempdir().expect("tempdir");
        let canonical = real.path().canonicalize().expect("canonicalize");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        // Author the entry as the absolute path; unpin by the same canonical.
        std::fs::write(
            &path,
            format!("[roots]\npinned = [\n  \"{}\",\n]\n", canonical.display()),
        )
        .expect("write");
        assert!(
            unpin_config(&path, &canonical).expect("unpin"),
            "canonical entry removed"
        );
        let out = std::fs::read_to_string(&path).expect("read");
        let doc: toml::Value = toml::from_str(&out).expect("valid toml");
        assert_eq!(
            doc["roots"]["pinned"].as_array().expect("pinned").len(),
            0,
            "entry gone: {out}"
        );
    }

    #[test]
    fn unpin_of_absent_entry_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No config file at all.
        let missing = dir.path().join("config.toml");
        assert!(
            !unpin_config(&missing, &PathBuf::from("/srv/gone")).expect("unpin missing file"),
            "unpin on a missing config is a benign no-op"
        );
        assert!(!missing.exists(), "unpin never creates the config");

        // A config with a `[roots] pinned` array that lacks the target.
        let path = write(
            dir.path(),
            "present.toml",
            "[roots]\npinned = [\n  \"/srv/keep\",\n]\n",
        );
        assert!(
            !unpin_config(&path, &PathBuf::from("/srv/never-pinned")).expect("unpin absent entry"),
            "unpin of a never-pinned path reports no change"
        );
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(out.contains("/srv/keep"), "existing entry untouched: {out}");
    }

    #[test]
    fn unpin_with_no_roots_section_is_a_noop() {
        // An `unpin` must never author a `[roots]` table just to find it empty.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "config.toml", "log_retention_days = 7\n");
        assert!(
            !unpin_config(&path, &PathBuf::from("/srv/x")).expect("unpin"),
            "no change"
        );
        let out = std::fs::read_to_string(&path).expect("read");
        assert!(!out.contains("[roots]"), "no roots table created: {out}");
    }

    // ── bug 109: never discard untouched sections; never truncate ────────

    #[test]
    fn pin_preserves_unrelated_sections_it_did_not_touch() {
        // The section-destruction guard (bug 109): a pin appends to `[roots]
        // pinned` and must leave every unrelated section byte-for-byte — in
        // particular the `[roots.companions]` subtable and the `[commands]`
        // enforcement block, the two the poison incident destroyed.
        let dir = tempfile::tempdir().expect("tempdir");
        let original = "\
# Catenary config — hand tuned.

[roots.companions]
\"~/Projects/Widget\" = [\"~/Projects/WidgetDocs\"]

[roots]
pinned = [
  \"/srv/alpha\",
]

[commands]
allow = [\"git\", \"gh\"]
pipeline = [\"grep\", \"wc\"]
";
        let path = write(dir.path(), "config.toml", original);
        assert!(
            pin_config(&path, &PathBuf::from("/srv/beta")).expect("pin beta"),
            "beta is a new entry"
        );
        let out = std::fs::read_to_string(&path).expect("read");
        // The companions subtable and the commands block survive intact.
        for line in [
            "# Catenary config — hand tuned.",
            "[roots.companions]",
            "\"~/Projects/Widget\" = [\"~/Projects/WidgetDocs\"]",
            "[commands]",
            "allow = [\"git\", \"gh\"]",
            "pipeline = [\"grep\", \"wc\"]",
            "  \"/srv/alpha\",",
        ] {
            assert!(
                out.contains(line),
                "unrelated section survives the pin: {line:?}\n{out}"
            );
        }
        // The whole thing still parses under the real loader with both entries.
        let doc: toml::Value = toml::from_str(&out).expect("valid toml after pin");
        assert_eq!(
            doc["roots"]["pinned"].as_array().expect("pinned").len(),
            2,
            "alpha + beta: {out}"
        );
        assert!(
            doc["roots"]["companions"].is_table(),
            "companions table intact: {out}"
        );
        assert!(
            doc["commands"]["allow"].is_array(),
            "commands intact: {out}"
        );
    }

    #[test]
    fn pin_on_unparseable_config_refuses_and_never_truncates() {
        // The truncation guard (bug 109): a config that does not parse must make
        // the mutation FAIL, leaving the file byte-identical — never a
        // create-fresh replace that discards the operator's content. This pins
        // that `read_document` refuses (returns `Err`) instead of falling back to
        // an empty document.
        let dir = tempfile::tempdir().expect("tempdir");
        // Invalid TOML: an unterminated array. Also carries a would-be-lost
        // section so a silent truncation is unmistakable.
        let broken = "[commands]\nallow = [\"git\"\n\n[roots]\npinned = [\n";
        let path = write(dir.path(), "config.toml", broken);
        let before = std::fs::read_to_string(&path).expect("read before");

        let err = pin_config(&path, &PathBuf::from("/srv/x"));
        assert!(
            err.is_err(),
            "a pin against an unparseable config is refused, not applied"
        );

        let after = std::fs::read_to_string(&path).expect("read after");
        assert_eq!(
            before, after,
            "the unparseable file is left byte-identical — the mutation never truncated it"
        );
    }

    #[test]
    fn write_is_atomic_leaving_no_stage_behind() {
        // The atomic-write leg (bug 109): a successful mutation leaves exactly the
        // target file in its directory — the staging temp is renamed away, never
        // stranded. A leftover `*.tmp.*` would signal a torn write.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        assert!(pin_config(&path, &PathBuf::from("/srv/only")).expect("pin"));
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            entries,
            vec!["config.toml".to_string()],
            "only the target file remains, no staging temp: {entries:?}"
        );
    }
}
