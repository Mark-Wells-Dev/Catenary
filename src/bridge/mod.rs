// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Diagnostics pipeline for PostToolUse hook requests.
pub mod diagnostics_server;
/// Cross-session per-root editing guardrail.
pub mod editing_guardrail;
/// In-memory editing state manager.
pub mod editing_manager;
/// Glob tool handler: unified file/directory/pattern browsing.
mod file_tools;
/// Single authority for file classification (binary, language ID, shebang).
pub mod filesystem_manager;
/// Grep's shared machinery: flags, skips, matcher/searcher construction, the
/// LSP enrichment core, and stdin (stream) mode.
mod grep_server;
/// Path utilities shared by bridge components.
mod handler;
/// The ws43 hit-batch enricher: grep's LSP enrichment as a `BatchEnricher`.
mod hitstream_enricher;
/// Application dispatch for hook requests.
mod hook_router;
/// Daemon-side adapter binding the shared linter core to the diagnostics batch.
pub mod linter;
/// Path validation for LSP-aware operations and config file protection.
pub mod path_security;
/// Shared container for tool servers and cross-tool infrastructure.
pub mod session;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::debug;

use crate::bridge::filesystem_manager::{FilesystemManager, mtime_nanos};
use crate::config::DispatchMethod;
use crate::lsp::LspClientManager;
use crate::lsp::server::LspServer;
use crate::symbol_index::SymbolIndex;

pub use diagnostics_server::DiagnosticsServer;
pub use editing_guardrail::EditingGuardrail;
pub use editing_manager::EditingManager;
pub use file_tools::{
    FileEnrichment, GlobPlan, anchor_base, build_glob_plan, canonicalize_pattern_base,
    count_glob_paths, render_glob_plan, resolve_walk_tier,
};
pub use grep_server::{GrepFlags, GrepSkips, SkipRecord, StreamOutcome, grep_stream};
pub use handler::expand_tilde;
pub use hitstream_enricher::HitstreamEnricher;
// The ws43 hitstream engine walks with the same matcher/searcher constructors
// stdin mode uses, so the surfaces' matching semantics cannot drift (ws43-02).
pub(crate) use grep_server::{build_matcher, build_searcher};
pub use hook_router::HookRouter;
pub use hook_router::is_edit_tool;
pub use path_security::PathValidator;

// The `(no LSP …)` scope label (`NO_LSP_LABEL`) retired with the ws43 cutovers:
// grep replaced it with the per-line `#?` could-not-enrich marker (ws43-02) and
// glob with the per-file `no outline` marker (ws43-03) — the CLI owns the
// render in both modes and cannot see the daemon's mounted roots, so
// degradation is disclosed per result, never as a scope header.

/// Compresses a path by replacing the `$HOME` prefix with `~`.
pub(crate) fn compress_home(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let home_path = Path::new(&home);
        if let Ok(rel) = path.strip_prefix(home_path) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

/// Reads source files once and serves any line by 0-based index.
///
/// The one-atom render model (decision 024) makes every `grep` and `glob`
/// result a `path:line  <source line>` atom — the verbatim text at that
/// location. Neither `Symbol` nor `Edge` carries the source text,
/// so both surfaces resolve it here: glob reads one file and indexes many
/// nodes; grep reads scattered edge-target lines across files. Each file is
/// read at most once and its newline-stripped lines are cached, so repeated
/// lookups (a hot file with many symbols, an edge cited several times) cost
/// one read.
///
/// A file that cannot be read, or a line index past the file's end, yields
/// `None`; callers fall back to the bare `path:line` form. Trailing `\r` is
/// stripped alongside `\n` so a CRLF file renders the same bytes as `rg`.
#[derive(Default)]
pub(super) struct SourceLines {
    /// Per-file cached lines (newline-stripped), or `None` when the file
    /// could not be read — memoizing the failure so a missing file is
    /// stat-ed once, not once per lookup.
    cache: HashMap<PathBuf, Option<Vec<String>>>,
}

impl SourceLines {
    /// Creates an empty reader cache.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Returns the source line at `line_0` (0-based) of `path`, verbatim and
    /// newline-stripped, or `None` if the file is unreadable or the line is
    /// out of range.
    ///
    /// Reads and caches the whole file on first touch.
    pub(super) fn line(&mut self, path: &Path, line_0: u32) -> Option<&str> {
        let entry = self
            .cache
            .entry(path.to_path_buf())
            .or_insert_with(|| Self::read_lines(path));
        entry
            .as_ref()
            .and_then(|lines| lines.get(line_0 as usize))
            .map(String::as_str)
    }

    /// Reads a file and splits it into newline-stripped lines.
    ///
    /// Returns `None` if the file cannot be read. A trailing `\r` is removed
    /// alongside `\n` so CRLF input renders identically to ripgrep.
    fn read_lines(path: &Path) -> Option<Vec<String>> {
        let content = std::fs::read_to_string(path).ok()?;
        Some(
            content
                .split('\n')
                .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
                .collect(),
        )
    }
}

/// File-size ceiling for outline enrichment, in bytes (2 MiB).
///
/// Above this, `ensure_symbols` skips the `documentSymbol` round-trip for the
/// file: it renders raw (grep/glob still return their matches, just without
/// enclosing labels or an outline) rather than round-tripping symbols.
///
/// Rationale (bug 136): a `documentSymbol` response is copied ~6× per round on
/// the daemon's data path (String → `Value` → the protocol tap's `to_string()` →
/// sink re-parse → record serialize), and glibc's per-thread arenas retain
/// those transient pages permanently. The dig's worst driver was a 7.66 MB JSON
/// data file whose response measured 82 MB — copied to ~490 MB of churn per
/// round. Useful outlines come from *source* files, which are orders of
/// magnitude smaller: a 2 MiB source file is already enormous by hand-authored
/// standards, so the gate spares the pathological data-file case while leaving
/// every realistic source file enriched.
const MAX_OUTLINE_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Whether a file of `file_bytes` is within the outline-enrichment size gate.
///
/// `true` up to and including [`MAX_OUTLINE_FILE_BYTES`]; `false` above it, in
/// which case `ensure_symbols` skips the `documentSymbol` round-trip and the
/// file renders raw. The boundary is inclusive at the ceiling.
const fn outline_gate_permits(file_bytes: u64) -> bool {
    file_bytes <= MAX_OUTLINE_FILE_BYTES
}

/// Ensures the symbol index is populated — and fresh — for the given files.
///
/// For each file, opens the document on the server, requests `documentSymbol`,
/// and feeds the response to the index when the cached symbols are either
/// absent (lazy first fill) or stale. Staleness is detected by comparing the
/// file's current on-disk mtime against the mtime recorded at population time:
/// a host `Edit`/`Write` (and any other external write the daemon has no signal
/// for — `git checkout`, formatters) leaves the rows in place, so without this
/// check `grep`/`glob` would serve a pre-edit outline and pre-edit enclosing
/// labels until a later `catenary diagnostics` pass happened to cover the
/// file (bug #26). One `stat` per file decides both cases; files that don't
/// exist on disk are skipped.
///
/// A genuinely-stale file additionally bumps its root's generation counter so
/// the per-position enrichment cache and the paged result cache re-derive,
/// mirroring the diagnostics invalidation path. First-time fills are
/// not bumped — there is no prior generation to invalidate, and bumping would
/// needlessly evict unrelated files in the same root.
///
/// `parent_id` is propagated to the LSP client so that `didOpen` and
/// `documentSymbol` traffic appears as children of the calling scope
/// in the TUI.
///
/// Since the ws43 cutovers the sole caller is the hitstream annotator's shared
/// enrichment core (`grep_server::anchor_context`), which serves both grep
/// anchors and glob outlines from the index this populates.
pub(super) async fn ensure_symbols(
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    client_manager: &LspClientManager,
    fs_manager: &FilesystemManager,
    files: &[PathBuf],
    parent_id: Option<&str>,
) {
    let Some(idx_arc) = symbol_index else {
        return;
    };

    // One stat per file classifies it as a first-time fill or a stale refresh.
    let mut to_populate: Vec<PathBuf> = Vec::new();
    let mut stale: Vec<PathBuf> = Vec::new();
    {
        let Ok(idx) = idx_arc.lock() else { return };
        for path in files {
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            // Size-gate the outline round-trip (bug 136 lean 1): a file above
            // the ceiling renders raw — no symbol enrichment — rather than
            // round-tripping a `documentSymbol` response that the allocator
            // retains as churn. Honest skip: grep/glob still return the file's
            // matches, just without enclosing labels or an outline.
            if !outline_gate_permits(meta.len()) {
                debug!(
                    path = %path.display(),
                    file_bytes = meta.len(),
                    max_bytes = MAX_OUTLINE_FILE_BYTES,
                    "skipping outline enrichment: file exceeds outline size gate"
                );
                continue;
            }
            if idx.needs_population(path) {
                to_populate.push(path.clone());
            } else if idx.symbols_outdated(path, mtime_nanos(&meta)) {
                stale.push(path.clone());
                to_populate.push(path.clone());
            }
        }
    }

    if to_populate.is_empty() {
        return;
    }

    // Drop the enrichment/result caches for files whose rows are being replaced
    // because the file changed on disk (not for first-time fills).
    if !stale.is_empty() {
        fs_manager.bump_generations(&stale);
    }

    for path in &to_populate {
        let servers = client_manager
            .get_servers(
                path,
                LspServer::supports_document_symbols,
                Some(DispatchMethod::DocumentSymbol),
            )
            .await;
        let Some(server) = servers.first() else {
            continue;
        };
        // Query-cycle open through the held-open change gate (no owner —
        // queries never hold): a doc the batch holds open is neither reopened
        // here nor ever closed by this path (diagnostics-debt 01).
        let Ok((uri, _)) = client_manager
            .open_document_on(path, server, parent_id.map(str::to_string), None)
            .await
        else {
            continue;
        };
        let Ok(response) = server.lock().await.document_symbols(&uri).await else {
            continue;
        };
        // A delivered symbol response is served work — strike-ledger credit
        // (misc 167).
        let served_key = server.lock().await.server().key();
        if let Some(key) = &served_key {
            client_manager.record_server_service(key);
        }
        if let Ok(idx) = idx_arc.lock() {
            let _ = idx.populate_from_document_symbols(path, &response);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_OUTLINE_FILE_BYTES, outline_gate_permits};

    #[test]
    fn outline_gate_ceiling_is_two_mib() {
        assert_eq!(MAX_OUTLINE_FILE_BYTES, 2 * 1024 * 1024);
    }

    #[test]
    fn outline_gate_boundary_is_inclusive_at_the_ceiling() {
        // Well under the gate: enriched.
        assert!(outline_gate_permits(0));
        assert!(outline_gate_permits(64 * 1024));
        assert!(outline_gate_permits(MAX_OUTLINE_FILE_BYTES - 1));

        // Exactly the ceiling: still permitted (inclusive boundary).
        assert!(
            outline_gate_permits(MAX_OUTLINE_FILE_BYTES),
            "a file of exactly the ceiling is enriched"
        );

        // One byte over: gated (skipped, renders raw).
        assert!(
            !outline_gate_permits(MAX_OUTLINE_FILE_BYTES + 1),
            "one byte over the ceiling is gated"
        );

        // The dig's specimen (7.66 MB) is firmly gated.
        assert!(
            !outline_gate_permits(7_660_000),
            "the 7.66 MB data-file specimen is gated (bug 136)"
        );
    }
}
