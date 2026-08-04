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
//!   per language. NO pool readiness, NO WS31 nudge (the nudge routes traffic
//!   to root instances), NO symbol-index population,
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
//! 2. **The hit-file nudge** — the batch's own files, statted **here**, feed
//!    the root tracker's changed-set diff before this batch's anchors/outlines
//!    are derived (the nudge-then-anchor order). This is the only observation
//!    the search path contributes, and it is request-time consistency of
//!    exactly what is being asked about — bug 26's `ensure_symbols` mtime
//!    backstop is the precedent seam. The walk ships nothing (bug 146): its
//!    filtered yield was never coverage, and treating it as such condemned
//!    every watched file the filter hid. Add/update only — a hit set never
//!    proves absence, so it never reaps; deletion authority belongs to the
//!    supplemental probe, the open-document sweep, and the diagnose round's
//!    unfiltered walk.
//! 3. **Anchors or outlines** — the shared [`anchor_context`]
//!    populates/refreshes the symbol index and classifies per-file coverage;
//!    each hit maps onto the wire [`AnnotatedHit`]: the tri-state anchor
//!    (`#trail` / top-level / `#?`) for grep, or the outline body /
//!    suppression flag / could-not-enrich state for glob.
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
/// so one enricher serves one walk.
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
        }
    }

    /// The shared per-batch preamble: pool readiness (bounded) and the
    /// hit-file nudge (before anchors/outlines — the nudge-then-anchor order).
    /// Returns the batch's distinct canonical paths in a deterministic order.
    async fn prepare_batch(&self, hits: &[WireHit]) -> Vec<PathBuf> {
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

        // The hit-file nudge, BEFORE this batch's anchors/outlines (the
        // nudge-then-anchor order): the batch's own files, statted here, feed
        // the changed-set diff so the enrichment is derived from post-edit
        // content — request-time consistency of exactly what is being asked
        // about (bug 26's `ensure_symbols` mtime backstop is the precedent).
        // A stat that misses is simply omitted: search-path observations are
        // add/update only, so an omission can never fabricate a deletion. The
        // nudge also runs the daemon's own deletion authority for this root
        // (the supplemental probe and the open-document sweep) — see
        // `nudge_changed_set`.
        let batch_observed: Vec<(PathBuf, i64)> = paths
            .iter()
            .filter_map(|p| stat_with_retry(p).map(|md| (p.clone(), mtime_nanos(&md))))
            .collect();
        nudge_observed_files(&self.client_manager, &self.fs_manager, &batch_observed).await;

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
    /// NO symbol-index population. Each distinct
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
    async fn enrich(
        &self,
        hits: Vec<WireHit>,
        weight: Option<EnrichmentWeight>,
        tier: WalkTier,
    ) -> Result<Vec<AnnotatedHit>> {
        // The anchor-decided tier split (brackets 04): a sweep batch is served
        // file-grade through the rootless singletons — no batch preamble, no
        // pool readiness, no nudge, no root-instance traffic.
        if tier.is_sweep() {
            return Ok(self.enrich_sweep(hits, weight).await);
        }

        let paths = self.prepare_batch(&hits).await;

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
    /// and an uncovered-language hit renders raw immediately.
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
            .enrich(hits, None, WalkTier::Sweep)
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
            .enrich(vec![hit(&a, 2)], None, WalkTier::Sweep)
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
