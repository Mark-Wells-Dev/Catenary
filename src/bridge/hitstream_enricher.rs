// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The ws43 hit-batch enricher: grep's LSP enrichment as a
//! [`BatchEnricher`](crate::hitstream::BatchEnricher).
//!
//! This is the enrichment-migration leg of ws43-02: the LSP enrichment the
//! daemon's grep executor performs (pool lookups, symbol-index population, the
//! `#scope` containment trail, the `#?` coverage marker) becomes the annotator
//! behind the `tool/hitstream` dispatch arm. The implementation is NOT a copy —
//! the executor and this enricher call the same shared core
//! ([`anchor_context`], [`nudge_observed_files`] in
//! [`grep_server`](super::grep_server)), so the two paths cannot drift while
//! they coexist. When the `catenary grep` CLI cutover completes, the executor
//! retires and this enricher is the only consumer.
//!
//! Per batch, in order:
//!
//! 1. **Pool readiness** — ensure servers exist for the batch's files and wait,
//!    bounded, for settle
//!    ([`QUERY_ENRICHMENT_BUDGET`](crate::lsp::manager::QUERY_ENRICHMENT_BUDGET)).
//!    The annotator's own per-batch budget
//!    ([`ANNOTATION_BATCH_BUDGET`](crate::hitstream::ANNOTATION_BATCH_BUDGET))
//!    wraps this whole future, so a cold pool can blow the first batch's budget
//!    into a pass-through verdict while the pool warms — later batches then
//!    enrich. Degrade-only, never fewer hits.
//! 2. **The WS31 observation nudge** — the batch's canonical hit paths are the
//!    queried/touched paths; they are statted fresh and fed to the root
//!    tracker's changed-set diff (add/update only: a hit set never proves
//!    absence, so the per-batch nudge never reaps). This keeps enrichment fresh
//!    for exactly the files this batch is about to anchor.
//! 3. **Anchors** — the shared [`anchor_context`] populates/refreshes the
//!    symbol index and classifies per-file coverage; each hit's containment
//!    trail maps onto the wire [`AnnotatedHit`] tri-state (`#trail` / top-level
//!    / `#?`).
//!
//! Paths are canonical by contract: canonicalization is CLI-side, at the walk
//! ingestion seam ([`crate::hitstream::canonicalize_hit_path`]), and this
//! enricher keys pool lookups, the nudge, and the symbol index on the canonical
//! paths the batches carry.
//!
//! The query auto-mount (`ensure_ephemeral_mounts`, with its ws43-05
//! sensitive-path gate) also rides the annotation call, but it needs the
//! router's dispatch context, so it lives in the router's `tool/hitstream` arm
//! wrapper, which mounts before delegating to this enricher.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use super::filesystem_manager::{FilesystemManager, mtime_nanos, stat_with_retry};
use super::grep_server::{Anchor, anchor_context, nudge_observed_files};
use crate::hitstream::frame::AnnotatedHit;
use crate::hitstream::{BatchEnricher, WireHit};
use crate::lsp::LspClientManager;
use crate::symbol_index::SymbolIndex;

/// Grep's LSP enrichment as a hit-batch annotator (ws43-02).
///
/// Holds the same shared infrastructure as the grep executor — the LSP pool,
/// the filesystem manager, and the symbol index — and runs the shared
/// enrichment core per batch. Built via
/// [`GrepServer::hitstream_enricher`](super::grep_server::GrepServer::hitstream_enricher).
pub struct GrepHitEnricher {
    /// Shared LSP server pool (lookups, readiness waits, the changed-set nudge).
    client_manager: Arc<LspClientManager>,
    /// Root resolution and file classification.
    fs_manager: Arc<FilesystemManager>,
    /// The `documentSymbol` index the `#scope` anchors are derived from.
    symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
}

impl GrepHitEnricher {
    /// Wraps the shared infrastructure as a batch enricher.
    #[must_use]
    pub(super) const fn new(
        client_manager: Arc<LspClientManager>,
        fs_manager: Arc<FilesystemManager>,
        symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    ) -> Self {
        Self {
            client_manager,
            fs_manager,
            symbol_index,
        }
    }
}

impl BatchEnricher for GrepHitEnricher {
    async fn enrich(&self, hits: Vec<WireHit>) -> Result<Vec<AnnotatedHit>> {
        // The batch's distinct files, in a deterministic order. Hit paths are
        // canonical by contract (CLI-side canonicalization at the walk seam).
        let paths: Vec<PathBuf> = hits
            .iter()
            .map(|h| h.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        // Pool readiness, bounded — a wedged/busy settle must never make the
        // batch go silent. Past the bound the hits serve unenriched (their `#?`
        // could-not-enrich anchor); the hits themselves are already complete.
        self.client_manager
            .ensure_and_wait_for_paths_bounded(&paths, crate::lsp::manager::QUERY_ENRICHMENT_BUDGET)
            .await;

        // The WS31 observation nudge: the batch's queried paths, statted fresh,
        // feed the changed-set diff so this batch's anchors are derived from
        // post-edit content. Add/update only (`reap_scopes: None`): a hit set
        // never proves a baseline entry is gone. A file whose stat misses (a
        // live-race deletion) is simply omitted — with reaping off, omission
        // can never fabricate a deletion, and its enrichment degrades on its
        // own terms.
        let observed: Vec<(PathBuf, i64)> = paths
            .iter()
            .filter_map(|p| stat_with_retry(p).map(|md| (p.clone(), mtime_nanos(&md))))
            .collect();
        nudge_observed_files(&self.client_manager, &self.fs_manager, &observed, None).await;

        // The shared anchor core: symbol population + per-file coverage, then a
        // local ancestry walk per hit. No lock guard is held across an await
        // (the ruled law) — `anchor_context` scopes its index guards internally.
        let anchors = anchor_context(
            self.symbol_index.as_ref(),
            &self.client_manager,
            &self.fs_manager,
            &paths,
            None,
        )
        .await;

        Ok(hits
            .into_iter()
            .map(|hit| {
                let anchor = anchors.anchor_for(&hit.path, hit.line.saturating_sub(1));
                match anchor {
                    Anchor::Scope(trail) => AnnotatedHit {
                        hit,
                        anchor: Some(trail),
                        enriched: true,
                    },
                    Anchor::TopLevel => AnnotatedHit {
                        hit,
                        anchor: None,
                        enriched: true,
                    },
                    Anchor::Unknown => AnnotatedHit {
                        hit,
                        anchor: None,
                        enriched: false,
                    },
                }
            })
            .collect())
    }
}
