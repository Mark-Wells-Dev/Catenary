// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The ws43 hit-batch enricher: grep's LSP enrichment as a
//! [`BatchEnricher`](crate::hitstream::BatchEnricher).
//!
//! This is the enrichment-migration leg of ws43-02: the LSP enrichment the
//! retired grep executor performed (pool lookups, symbol-index population, the
//! `#scope` containment trail, the `#?` coverage marker) is the annotator
//! behind the `tool/hitstream` dispatch arm — since the CLI cutover, the
//! daemon's only grep surface. The implementation was never a copy: the
//! executor and this enricher called the same shared core
//! ([`anchor_context`], [`nudge_observed_files`] in
//! [`grep_server`](super::grep_server)), and this enricher is now its sole
//! grep consumer.
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
//! 2. **The WS31 observation nudge** — the batch's shipped observation slice
//!    (every file the CLI walk visited since the previous batch, walk-time
//!    mtimes) feeds the root tracker's changed-set diff before this batch's
//!    anchors are derived — the executor's nudge-then-anchor order, and on a
//!    cold root the first nudge is still the cold snapshot (first-walk
//!    `Changed`). Add/update only: a per-batch slice never proves absence, so
//!    it never reaps. A batch from an old CLI ships no observations; the nudge
//!    degrades to the batch's canonical hit paths, statted fresh. (Residual: a
//!    cold walk long enough to span several batches classifies files first
//!    seen in later batches as `Created` — the baseline warmed mid-walk. The
//!    retired executor's single post-walk nudge classified them `Changed`;
//!    either kind invalidates the server's view of the file.)
//! 3. **Anchors** — the shared [`anchor_context`] populates/refreshes the
//!    symbol index and classifies per-file coverage; each hit's containment
//!    trail maps onto the wire [`AnnotatedHit`] tri-state (`#trail` / top-level
//!    / `#?`).
//!
//! At the walk's end ([`BatchEnricher::observe_walk`]) the accumulated
//! observations — every batch's slice plus the terminator's tail — run the
//! executor's once-per-walk nudge with the pathless-walk reap rule, so a
//! deleted file a full walk no longer visits is reaped exactly as before.
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
/// Holds the same shared infrastructure as the retired grep executor — the LSP
/// pool, the filesystem manager, and the symbol index — and runs the shared
/// enrichment core per batch. Built per `tool/hitstream` connection via
/// [`GrepServer::hitstream_enricher`](super::grep_server::GrepServer::hitstream_enricher),
/// so the observation accumulator below is one walk's.
pub struct GrepHitEnricher {
    /// Shared LSP server pool (lookups, readiness waits, the changed-set nudge).
    client_manager: Arc<LspClientManager>,
    /// Root resolution and file classification.
    fs_manager: Arc<FilesystemManager>,
    /// The `documentSymbol` index the `#scope` anchors are derived from.
    symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    /// Every observation the batches shipped, accumulated for the walk-end
    /// reap diff (the executor's once-per-walk nudge needs the FULL visited
    /// set — reaping against a partial set would false-delete the rest).
    /// Bounded by the walk's visited-file count, the executor's own
    /// accumulation shape. The guard is never held across an await.
    seen: std::sync::Mutex<Vec<(PathBuf, i64)>>,
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
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl BatchEnricher for GrepHitEnricher {
    /// The walk-level observation nudge (ws43-02 reap parity): the accumulated
    /// batch observations plus the terminator's tail — the CLI walk's full
    /// visited-file set — fed once per walk to the shared
    /// [`nudge_observed_files`] with the executor's exact reap rule:
    /// `reap_scopes` is `Some` only for a pathless walk, and reaping still
    /// gates per-root on whole-root coverage inside the shared core. This is
    /// the once-after-the-walk nudge the retired executor ran; the per-batch
    /// nudges in [`Self::enrich`] stay add/update-only (a partial set never
    /// proves absence).
    async fn observe_walk(&self, observed: Vec<(PathBuf, i64)>, reap_scopes: Option<Vec<PathBuf>>) {
        let full: Vec<(PathBuf, i64)> = {
            let mut seen = self
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            seen.extend(observed);
            std::mem::take(&mut *seen)
            // Guard dropped here — never held across the await below.
        };
        nudge_observed_files(
            &self.client_manager,
            &self.fs_manager,
            &full,
            reap_scopes.as_deref(),
        )
        .await;
    }

    async fn enrich(
        &self,
        hits: Vec<WireHit>,
        observed: Vec<(PathBuf, i64)>,
    ) -> Result<Vec<AnnotatedHit>> {
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

        // The WS31 observation nudge, BEFORE this batch's anchors (the
        // executor's nudge-then-anchor order): the batch's shipped observation
        // slice — every file the walk visited since the previous batch, with
        // walk-time mtimes — feeds the changed-set diff so the anchors are
        // derived from post-edit content, and a cold root's first nudge is the
        // cold snapshot. Fallback for an old CLI that ships no observations:
        // the batch's hit paths, statted fresh (a stat that misses is simply
        // omitted — with reaping off, omission can never fabricate a
        // deletion). Add/update only (`reap_scopes: None`): a partial set
        // never proves a baseline entry is gone.
        let batch_observed: Vec<(PathBuf, i64)> = if observed.is_empty() {
            paths
                .iter()
                .filter_map(|p| stat_with_retry(p).map(|md| (p.clone(), mtime_nanos(&md))))
                .collect()
        } else {
            observed
        };
        nudge_observed_files(
            &self.client_manager,
            &self.fs_manager,
            &batch_observed,
            None,
        )
        .await;
        // Accumulate for the walk-end reap diff (guard scoped, no await held).
        {
            let mut seen = self
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            seen.extend(batch_observed);
        }

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
