// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The ws43 hit-batch enricher: the daemon's LSP enrichment as a
//! [`BatchEnricher`](crate::hitstream::BatchEnricher).
//!
//! This is the enrichment-migration leg of the ws43 cutovers: the LSP
//! enrichment the retired query executors performed is the annotator behind
//! the `tool/hitstream` dispatch arm — since the grep (ws43-02) and glob
//! (ws43-03) cutovers, the daemon's only search surface. The batch's requested
//! [`EnrichmentWeight`] selects the shape:
//!
//! - **No weight (grep):** pool lookups, symbol-index population, the `#scope`
//!   containment trail, the `#?` coverage marker — the retired grep executor's
//!   enrichment, via the shared core ([`anchor_context`],
//!   [`nudge_observed_files`] in [`grep_server`](super::grep_server)).
//! - **A weight (glob, ws43-03):** each hit's file answered with its outline
//!   body at the requested weight — [`EnrichmentWeight::Listing`] renders the
//!   file's top-level symbols only (the ruled default for listing shapes),
//!   [`EnrichmentWeight::Outline`] the fully-expanded types-and-callables tree
//!   the retired glob executor rendered — plus the per-file
//!   coverage/suppression classification the CLI turns into the `no outline`
//!   marker and the `[symbols available]` flag.
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
//!    anchors/outlines are derived — the executor's nudge-then-anchor order,
//!    and on a cold root the first nudge is still the cold snapshot
//!    (first-walk `Changed`). Add/update only: a per-batch slice never proves
//!    absence, so it never reaps. A batch from an old CLI ships no
//!    observations; the nudge degrades to the batch's canonical hit paths,
//!    statted fresh. (Residual: a cold walk long enough to span several
//!    batches classifies files first seen in later batches as `Created` — the
//!    baseline warmed mid-walk. The retired executor's single post-walk nudge
//!    classified them `Changed`; either kind invalidates the server's view of
//!    the file.)
//! 3. **Anchors or outlines** — the shared [`anchor_context`]
//!    populates/refreshes the symbol index and classifies per-file coverage;
//!    each hit maps onto the wire [`AnnotatedHit`]: the tri-state anchor
//!    (`#trail` / top-level / `#?`) for grep, or the outline body /
//!    suppression flag / could-not-enrich state for glob.
//!
//! At the walk's end ([`BatchEnricher::observe_walk`]) the accumulated
//! observations — every batch's slice plus the terminator's tail — run the
//! executor's once-per-walk nudge with the pathless-walk reap rule, so a
//! deleted file a full grep walk no longer visits is reaped exactly as before.
//! Glob ships no reap scopes (a scoped walk never proves absence), so its
//! terminator skips the walk-level nudge — its per-batch nudges are the whole
//! story, exactly the retired executor's scoped-nudge semantics.
//!
//! Paths are canonical by contract: canonicalization is CLI-side, at the walk
//! ingestion seam ([`crate::hitstream::canonicalize_hit_path`] for grep, the
//! pattern-base canonicalization for glob), and this enricher keys pool
//! lookups, the nudge, and the symbol index on the canonical paths the batches
//! carry.
//!
//! The query auto-mount (`ensure_ephemeral_mounts`, with its ws43-05
//! sensitive-path gate) also rides the annotation call, but it needs the
//! router's dispatch context, so it lives in the router's `tool/hitstream` arm
//! wrapper, which mounts before delegating to this enricher.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use super::SourceLines;
use super::file_tools::{is_outline_suppressed, render_full_outline, render_symbol_line};
use super::filesystem_manager::{FilesystemManager, mtime_nanos, stat_with_retry};
use super::grep_server::{Anchor, AnchorContext, anchor_context, nudge_observed_files};
use crate::hitstream::frame::AnnotatedHit;
use crate::hitstream::{BatchEnricher, EnrichmentWeight, WireHit};
use crate::lsp::LspClientManager;
use crate::symbol_index::SymbolIndex;

/// The daemon's LSP enrichment as a hit-batch annotator (ws43-02/-03).
///
/// Holds the same shared infrastructure as the retired query executors — the
/// LSP pool, the filesystem manager, the symbol index, and glob's
/// outline-suppression matchers — and runs the shared enrichment core per
/// batch. Built per `tool/hitstream` connection via
/// [`Session::hitstream_enricher`](super::session::Session::hitstream_enricher),
/// so the observation accumulator below is one walk's.
pub struct HitstreamEnricher {
    /// Shared LSP server pool (lookups, readiness waits, the changed-set nudge).
    client_manager: Arc<LspClientManager>,
    /// Root resolution and file classification.
    fs_manager: Arc<FilesystemManager>,
    /// The `documentSymbol` index the `#scope` anchors and outlines are
    /// derived from.
    symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
    /// Glob patterns whose outlines are suppressed from automatic display (an
    /// explicit user opt-out, `[tools.glob] outline_suppress`). Symbols remain
    /// available; the hit is flagged `suppressed` instead of outlined.
    outline_suppress: Vec<globset::GlobMatcher>,
    /// Every observation the batches shipped, accumulated for the walk-end
    /// reap diff (the executor's once-per-walk nudge needs the FULL visited
    /// set — reaping against a partial set would false-delete the rest).
    /// Bounded by the walk's visited-file count, the executor's own
    /// accumulation shape. The guard is never held across an await.
    seen: std::sync::Mutex<Vec<(PathBuf, i64)>>,
}

impl HitstreamEnricher {
    /// Wraps the shared infrastructure as a batch enricher.
    #[must_use]
    pub(super) const fn new(
        client_manager: Arc<LspClientManager>,
        fs_manager: Arc<FilesystemManager>,
        symbol_index: Option<Arc<std::sync::Mutex<SymbolIndex>>>,
        outline_suppress: Vec<globset::GlobMatcher>,
    ) -> Self {
        Self {
            client_manager,
            fs_manager,
            symbol_index,
            outline_suppress,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The shared per-batch preamble: pool readiness (bounded), the WS31
    /// observation nudge (before anchors/outlines — the executor's order), and
    /// the walk-end accumulation. Returns the batch's distinct canonical
    /// paths in a deterministic order.
    async fn prepare_batch(&self, hits: &[WireHit], observed: Vec<(PathBuf, i64)>) -> Vec<PathBuf> {
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
        // / `no outline` could-not-enrich state); the hits themselves are
        // already complete.
        self.client_manager
            .ensure_and_wait_for_paths_bounded(&paths, crate::lsp::manager::QUERY_ENRICHMENT_BUDGET)
            .await;

        // The WS31 observation nudge, BEFORE this batch's anchors/outlines
        // (the executor's nudge-then-anchor order): the batch's shipped
        // observation slice feeds the changed-set diff so the enrichment is
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

        paths
    }

    /// The glob leg (ws43-03): answers each hit's file with its outline body
    /// at the requested weight, plus the coverage/suppression classification.
    fn annotate_outlines(
        &self,
        hits: Vec<WireHit>,
        context: &AnchorContext,
        weight: EnrichmentWeight,
    ) -> Vec<AnnotatedHit> {
        let mut sources = SourceLines::new();
        // Per-file memo: several hits for one file (not typical for glob, but
        // legal on the wire) render the body once.
        let mut bodies: std::collections::HashMap<PathBuf, Option<String>> =
            std::collections::HashMap::new();

        hits.into_iter()
            .map(|hit| {
                if context.is_uncovered(&hit.path) {
                    // Could not enrich (no covering server): the CLI renders
                    // the `no outline` marker — the same spelling the retired
                    // executor used for an uncovered file.
                    return AnnotatedHit::passthrough(hit);
                }
                if is_outline_suppressed(&hit.path, &self.outline_suppress, &self.fs_manager) {
                    return AnnotatedHit {
                        hit,
                        anchor: None,
                        enriched: true,
                        outline: None,
                        suppressed: true,
                    };
                }
                let body = bodies
                    .entry(hit.path.clone())
                    .or_insert_with_key(|path| {
                        let syms = context.symbols_for(path)?;
                        let mut out = String::new();
                        match weight {
                            // Listing weight: top-level symbols only, no
                            // nested tree — the ruled default for listing
                            // shapes.
                            EnrichmentWeight::Listing => {
                                for sym in syms.iter().filter(|s| s.scope.is_none()) {
                                    let source = sources.line(path, sym.line);
                                    render_symbol_line(&mut out, sym, "", source);
                                }
                            }
                            // The full picture: the fully-expanded
                            // types-and-callables tree (`--outline`, and the
                            // single-file outline shape's default).
                            EnrichmentWeight::Outline => {
                                render_full_outline(&mut out, path, syms, "", &mut sources);
                            }
                        }
                        let trimmed = out.trim_end_matches('\n');
                        (!trimmed.is_empty()).then(|| trimmed.to_string())
                    })
                    .clone();
                AnnotatedHit {
                    hit,
                    anchor: None,
                    enriched: true,
                    outline: body,
                    suppressed: false,
                }
            })
            .collect()
    }
}

impl BatchEnricher for HitstreamEnricher {
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
        weight: Option<EnrichmentWeight>,
    ) -> Result<Vec<AnnotatedHit>> {
        let paths = self.prepare_batch(&hits, observed).await;

        // The shared anchor core: symbol population + per-file coverage, then
        // the weight-selected projection. No lock guard is held across an
        // await (the ruled law) — `anchor_context` scopes its index guards
        // internally.
        let context = anchor_context(
            self.symbol_index.as_ref(),
            &self.client_manager,
            &self.fs_manager,
            &paths,
            None,
        )
        .await;

        // The glob leg (ws43-03): the batch requested an outline weight.
        if let Some(weight) = weight {
            return Ok(self.annotate_outlines(hits, &context, weight));
        }

        // The grep leg: a local ancestry walk per hit.
        Ok(hits
            .into_iter()
            .map(|hit| {
                let anchor = context.anchor_for(&hit.path, hit.line.saturating_sub(1));
                match anchor {
                    Anchor::Scope(trail) => AnnotatedHit::scoped(hit, trail),
                    Anchor::TopLevel => AnnotatedHit::top_level(hit),
                    Anchor::Unknown => AnnotatedHit::passthrough(hit),
                }
            })
            .collect())
    }
}
