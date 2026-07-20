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
//! Since brackets 04 the batch's [`WalkTier`] splits enrichment in two —
//! *declared by the command, decided by the anchor, never guessed from the
//! pattern*:
//!
//! - **Dig** (anchored inside a project root — the default, and the whole
//!   pre-tier behavior, byte-identical): the per-batch pipeline below, served
//!   project-grade from root instances.
//! - **Sweep** (anchored above every root, by its own declaration):
//!   file-grade enrichment only ([`HitstreamEnricher::enrich_sweep`]) —
//!   `documentSymbol`-class outlines/anchors from the rootless single-file
//!   singletons via the bracket seam
//!   ([`LspClientManager::with_single_file_bracket`]), at most one singleton
//!   per language. NO pool readiness, NO WS31 nudge (per-batch or walk-end —
//!   the nudge routes traffic to root instances), NO symbol-index population,
//!   NO per-hit-root spawns; the router-level query auto-mount is skipped for
//!   sweep batches too. A hit whose language has no capable singleton renders
//!   raw immediately — degrade is capability-shaped, never timeout-shaped
//!   (ruled); the per-batch budget survives only as the pathology backstop.
//!
//! The **dig** tier, per batch, in order:
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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use super::SourceLines;
use super::file_tools::{is_outline_suppressed, render_full_outline, render_symbol_line};
use super::filesystem_manager::{FilesystemManager, mtime_nanos, stat_with_retry};
use super::grep_server::{Anchor, AnchorContext, anchor_context, nudge_observed_files};
use crate::config::DispatchMethod;
use crate::hitstream::frame::{AnnotatedHit, WalkTier};
use crate::hitstream::{BatchEnricher, EnrichmentWeight, WireHit};
use crate::lsp::{Lane, LspClientManager};
use crate::symbol_index::{Symbol, SymbolIndex};

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
    /// Whether this walk declared the sweep tier (brackets 04). Set by the
    /// first sweep batch — the tier is per-walk (one enricher serves one
    /// `tool/hitstream` connection, and there are no mid-stream flips) — and
    /// read by [`BatchEnricher::observe_walk`], whose terminator carries no
    /// tier: a sweep's walk-end nudge/reap is skipped entirely, because the
    /// nudge routes `didChangeWatchedFiles` traffic to root instances and the
    /// sweep path never touches one.
    sweep_walk: std::sync::atomic::AtomicBool,
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
            sweep_walk: std::sync::atomic::AtomicBool::new(false),
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

    /// The grep leg's projection, shared by both tiers: a local ancestry walk
    /// per hit against the anchor context (project-grade or file-grade — the
    /// context construction decides, the projection cannot drift).
    fn annotate_anchors(hits: Vec<WireHit>, context: &AnchorContext) -> Vec<AnnotatedHit> {
        hits.into_iter()
            .map(|hit| {
                let anchor = context.anchor_for(&hit.path, hit.line.saturating_sub(1));
                match anchor {
                    Anchor::Scope(trail) => AnnotatedHit::scoped(hit, trail),
                    Anchor::TopLevel => AnnotatedHit::top_level(hit),
                    Anchor::Unknown => AnnotatedHit::passthrough(hit),
                }
            })
            .collect()
    }

    /// The sweep tier (brackets 04): file-grade enrichment through the
    /// rootless single-file singletons, and nothing else.
    ///
    /// The anchor declared this walk a sweep, so the batch preamble the dig
    /// tier runs is skipped whole: NO pool readiness (no per-hit-root
    /// spawns), NO WS31 nudge (the nudge routes traffic to root instances),
    /// NO symbol-index population, NO walk-end accumulation. Each distinct
    /// file is answered file-grade — its `documentSymbol` outline from a
    /// rootless singleton, one bracket per file on the enrichment lane
    /// ([`Self::single_file_symbols`]) — at a bounded cost of at most one
    /// singleton per language. Where no capable singleton exists the hit
    /// renders raw immediately: degrade is capability-shaped, never
    /// timeout-shaped (ruled); the annotator's per-batch budget survives only
    /// as the pathology backstop.
    async fn enrich_sweep(
        &self,
        hits: Vec<WireHit>,
        weight: Option<EnrichmentWeight>,
    ) -> Vec<AnnotatedHit> {
        // Remember the walk's declaration for the tier-less terminator
        // (`observe_walk` skips the nudge/reap on a sweep).
        self.sweep_walk
            .store(true, std::sync::atomic::Ordering::Release);

        let paths: Vec<PathBuf> = hits
            .iter()
            .map(|h| h.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let mut file_symbols: HashMap<PathBuf, Vec<Symbol>> = HashMap::new();
        let mut uncovered: HashSet<PathBuf> = HashSet::new();
        for path in &paths {
            match self.single_file_symbols(path).await {
                Some(symbols) => {
                    file_symbols.insert(path.clone(), symbols);
                }
                None => {
                    uncovered.insert(path.clone());
                }
            }
        }
        let context = AnchorContext::from_file_grade(file_symbols, uncovered);

        // The same weight-selected projections the dig tier serves, over the
        // file-grade context.
        if let Some(weight) = weight {
            self.annotate_outlines(hits, &context, weight)
        } else {
            Self::annotate_anchors(hits, &context)
        }
    }

    /// One file's file-grade symbols, through the bracket seam (brackets 02):
    /// `None` is the raw verdict (`#?` / `no outline`).
    ///
    /// Detects the file's language (the pool path's own detection: filesystem
    /// classification first, raw extension as the fallback), then walks the
    /// language's candidate single-file bindings
    /// ([`LspClientManager::single_file_binding_names`]) and runs one
    /// transaction bracket against the first binding with a capable singleton
    /// — open → `documentSymbol` → answer in the body, `didClose` as the
    /// teardown leg, on [`Lane::Enrichment`]. `with_single_file_bracket`
    /// answering `None` means no capable singleton for that binding
    /// (fail-closed: no manifest claim, no config opt-in, negative-cached, or
    /// dead) — the next binding is tried; a bracket that ran but degraded
    /// (budget backstop) or whose request failed answers raw. Root instances
    /// are never touched on this path.
    async fn single_file_symbols(&self, path: &Path) -> Option<Vec<Symbol>> {
        let lang = self.fs_manager.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        })?;
        // The document text is read once, up front — an unreadable (or
        // vanished mid-walk) file renders raw.
        let text = std::fs::read_to_string(path).ok()?;
        let uri = crate::lsp::lang::path_to_uri(path);

        for server_name in self.client_manager.single_file_binding_names(
            &lang,
            path,
            Some(DispatchMethod::DocumentSymbol),
        ) {
            let body_uri = uri.clone();
            let body_lang = lang.clone();
            let body_text = text.clone();
            let close_uri = uri.clone();
            let Some(outcome) = self
                .client_manager
                .with_single_file_bracket(
                    &lang,
                    &server_name,
                    Lane::Enrichment,
                    move |client| async move {
                        let client = client.lock().await;
                        client
                            .did_open(&body_uri, &body_lang, 1, &body_text)
                            .await
                            .ok()?;
                        client.document_symbols(&body_uri).await.ok()
                    },
                    move |client| async move {
                        let _ = client.lock().await.did_close(&close_uri).await;
                    },
                )
                .await
            else {
                // No capable singleton behind this binding — try the next.
                continue;
            };
            // A bracket that ran answers for the file: a completed
            // `documentSymbol` response parses through the index's own
            // flatten; a degraded bracket or a failed request is the raw
            // verdict (capability answered, service did not).
            return outcome
                .completed()
                .flatten()
                .map(|response| crate::symbol_index::flatten_symbols(&response));
        }
        None
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
    ///
    /// A **sweep** walk (brackets 04) skips this entirely: the nudge routes
    /// `didChangeWatchedFiles` traffic (and, for a pathless walk, the reap
    /// diff) to root instances, and the sweep path never touches one. The
    /// terminator carries no tier, so the walk's declaration is read from the
    /// batches ([`Self::sweep_walk`] — one enricher, one walk, no mid-stream
    /// flips).
    async fn observe_walk(&self, observed: Vec<(PathBuf, i64)>, reap_scopes: Option<Vec<PathBuf>>) {
        if self.sweep_walk.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
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
        tier: WalkTier,
    ) -> Result<Vec<AnnotatedHit>> {
        // The anchor-decided tier split (brackets 04): a sweep batch is served
        // file-grade through the rootless singletons — no batch preamble, no
        // pool readiness, no nudge, no root-instance traffic.
        if tier.is_sweep() {
            return Ok(self.enrich_sweep(hits, weight).await);
        }

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
        Ok(Self::annotate_anchors(hits, &context))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::{Config, LanguageConfig, ServerBinding, ServerDef};
    use crate::lsp::instance_key::Scope;
    use crate::lsp::test_support::mockls_bin;
    use std::collections::HashMap as StdHashMap;

    /// The mock language's file extension — unique so nothing else claims it.
    const SWEEP_LANG: &str = "x5swp";

    /// A config binding `SWEEP_LANG` to a mockls server with the user-scope
    /// `single_file = true` opt-in — the brackets-01 rootless spawn gate's
    /// config leg.
    fn sweep_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{SWEEP_LANG}");
        let mut server = StdHashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![SWEEP_LANG.to_string()],
                single_file: true,
                ..ServerDef::default()
            },
        );
        let mut language = StdHashMap::new();
        language.insert(
            SWEEP_LANG.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            log_retention_days: 7,
            notifications: None,
            icons: None,
            tools: None,
            resolved_commands: None,
            observability: None,
            roots: None,
            registry: None,
            permissions: None,
            servers: None,
            linter: StdHashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Two marker-bearing projects under one parent — the sweep shape: the
    /// anchor (the parent) lies above both. Each project holds one covered
    /// file with mockls-parseable symbols; project B also holds an
    /// uncovered-language file.
    fn sweep_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().canonicalize().expect("canonicalize");
        let body = "fn alpha() {\n    let hidden = 1;\n}\nfn beta() {\n}\ntop level line\n";
        let mut files = Vec::new();
        for (proj, name) in [("proj_a", "a"), ("proj_b", "b")] {
            let dir = root.join(proj);
            std::fs::create_dir(&dir).expect("create project");
            std::fs::create_dir(dir.join(".git")).expect("create marker");
            let file = dir.join(format!("{name}.{SWEEP_LANG}"));
            std::fs::write(&file, body).expect("write covered file");
            files.push(file);
        }
        let uncovered = root.join("proj_b").join("c.zzz9");
        std::fs::write(&uncovered, "no server speaks this\n").expect("write uncovered file");
        let b = files.pop().expect("file b");
        let a = files.pop().expect("file a");
        (tmp, a, b, uncovered)
    }

    fn hit(path: &Path, line: u32) -> WireHit {
        WireHit {
            path: path.to_path_buf(),
            line,
            column: 1,
            text: format!("line {line}"),
        }
    }

    /// The pool assertion behind the fan-out kill: after a sweep, the client
    /// registry holds not a single root-scoped instance — only rootless
    /// singletons (at most one per language).
    async fn assert_no_root_instances(manager: &LspClientManager, singletons: usize) {
        let clients = manager.clients().await;
        assert!(
            !clients.keys().any(|k| matches!(k.scope, Scope::Root(_))),
            "a sweep must never spawn a root instance: {:?}",
            clients.keys().collect::<Vec<_>>(),
        );
        assert_eq!(
            clients
                .keys()
                .filter(|k| k.scope == Scope::SingleFile)
                .count(),
            singletons,
            "bounded cost: at most one rootless singleton per language",
        );
    }

    /// The fan-out kill (brackets 04, proof b + c): a sweep spanning several
    /// markered projects — their roots even registered with the filesystem
    /// manager, as a mounted board would have them — spawns ZERO root
    /// instances. Covered-language hits come back file-grade through the one
    /// rootless singleton (the `#scope` trail from mockls's `documentSymbol`),
    /// an uncovered-language hit renders raw immediately, and the walk-end
    /// observation nudge is a no-op (no root-instance traffic, nothing
    /// spawned).
    #[tokio::test]
    async fn sweep_across_projects_spawns_no_root_instances() {
        let (_tmp, a, b, uncovered) = sweep_fixture();
        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(vec![
            a.parent().expect("proj a").to_path_buf(),
            b.parent().expect("proj b").to_path_buf(),
        ]);
        let manager = Arc::new(LspClientManager::new(
            sweep_config(),
            crate::logging::LoggingServer::new(),
            Arc::clone(&fs),
        ));
        let enricher = HitstreamEnricher::new(Arc::clone(&manager), fs, None, Vec::new());

        // Line 2 (1-based) sits inside `fn alpha`; line 6 is top-level.
        let hits = vec![hit(&a, 2), hit(&a, 6), hit(&b, 2), hit(&uncovered, 1)];
        let annotated = enricher
            .enrich(hits, Vec::new(), None, WalkTier::Sweep)
            .await
            .expect("sweep enrich");

        assert_eq!(annotated.len(), 4, "every hit survives");
        assert_eq!(
            annotated[0].anchor.as_deref(),
            Some("alpha"),
            "a covered hit gets the file-grade #scope trail via the singleton",
        );
        assert!(
            annotated[1].enriched && annotated[1].anchor.is_none(),
            "a top-level covered hit is enriched with no anchor",
        );
        assert_eq!(
            annotated[2].anchor.as_deref(),
            Some("alpha"),
            "the second project's hit rides the SAME singleton",
        );
        assert!(
            !annotated[3].enriched,
            "an uncovered language renders raw immediately (capability-shaped)",
        );

        assert_no_root_instances(&manager, 1).await;

        // The walk-end nudge of a sweep is skipped whole — no root-instance
        // traffic, and still nothing root-scoped in the pool.
        enricher
            .observe_walk(
                vec![(a.clone(), 1), (b.clone(), 1)],
                Some(vec![a.parent().expect("proj a").to_path_buf()]),
            )
            .await;
        assert_no_root_instances(&manager, 1).await;
    }

    /// Conscious conservatism (proof e): a sweep whose hits ALL land in one
    /// registered root still gets file-grade — no root instance, the
    /// singleton serves — because the anchor declared a sweep and there are
    /// no mid-stream flips.
    #[tokio::test]
    async fn one_root_sweep_stays_file_grade() {
        let (_tmp, a, _b, _uncovered) = sweep_fixture();
        let proj = a.parent().expect("proj a").to_path_buf();
        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(vec![proj]);
        let manager = Arc::new(LspClientManager::new(
            sweep_config(),
            crate::logging::LoggingServer::new(),
            Arc::clone(&fs),
        ));
        let enricher = HitstreamEnricher::new(Arc::clone(&manager), fs, None, Vec::new());

        let annotated = enricher
            .enrich(vec![hit(&a, 2)], Vec::new(), None, WalkTier::Sweep)
            .await
            .expect("sweep enrich");
        assert_eq!(
            annotated[0].anchor.as_deref(),
            Some("alpha"),
            "file-grade enrichment even though every hit shares one root",
        );
        assert_no_root_instances(&manager, 1).await;
    }

    /// The glob leg of the sweep tier (proof c, outline shape): a weighted
    /// sweep batch answers outline bodies from the singleton — listing weight
    /// renders the top-level symbols — and never spawns a root instance.
    #[tokio::test]
    async fn sweep_outline_weight_serves_file_grade_bodies() {
        let (_tmp, a, _b, uncovered) = sweep_fixture();
        let fs = Arc::new(FilesystemManager::new());
        let manager = Arc::new(LspClientManager::new(
            sweep_config(),
            crate::logging::LoggingServer::new(),
            Arc::clone(&fs),
        ));
        let enricher = HitstreamEnricher::new(Arc::clone(&manager), fs, None, Vec::new());

        let annotated = enricher
            .enrich(
                vec![hit(&a, 0), hit(&uncovered, 0)],
                Vec::new(),
                Some(EnrichmentWeight::Listing),
                WalkTier::Sweep,
            )
            .await
            .expect("sweep enrich");

        let outline = annotated[0].outline.as_deref().expect("outline body");
        assert!(
            outline.contains("alpha") && outline.contains("beta"),
            "listing weight renders the file's top-level symbols: {outline}",
        );
        assert!(
            !outline.contains("hidden"),
            "listing weight stays top-level (no nested tree): {outline}",
        );
        assert!(
            annotated[0].enriched && !annotated[0].suppressed,
            "the covered file is enriched, unsuppressed",
        );
        assert!(
            !annotated[1].enriched && annotated[1].outline.is_none(),
            "the uncovered file keeps the `no outline` degrade state",
        );
        assert_no_root_instances(&manager, 1).await;
    }
}
