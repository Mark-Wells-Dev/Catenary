// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! LSP client lifecycle: spawn, registry, dispatch, teardown.
//!
//! ## Registry isolation: no instance blocks what doesn't depend on it
//!
//! The standing doctrine (maintainer ruling, misc 208): **an instance's
//! slowness may only affect operations that depend on that instance
//! specifically.** The `clients` registry mutex is a lookup index, never a
//! wait point, enforced by three constructions:
//!
//! - **Snapshot, drop, then await** (bug 104): every lookup snapshots `Arc`
//!   handles under the registry guard and awaits client locks only after the
//!   guard is gone. A client mutex can be held for a full diagnose batch
//!   (settle included); awaiting one under the registry guard convoyed every
//!   manager lookup daemon-wide behind a single busy server.
//! - **Cold spawns hold a marker, not the registry** (misc 191, shared across
//!   the per-root and single-file paths by misc 208):
//!   [`LspClientManager::claim_spawn`] holds the registry lock only for the
//!   found-check and the marker lookup/insert; the process spawn and the
//!   `initialize` handshake run fully unlocked. Duplicate requesters of the
//!   same key await the marker's `Notify` and re-check — never a second
//!   spawn, never a daemon-wide stall.
//! - **Teardowns detach first** (bug 104 / misc 209): detach under the
//!   registry lock, run the shutdown round-trip after, and drop the board
//!   entry with the instance ([`LspClientManager::teardown_matching`]) so
//!   the snapshot keeps no ghost (bug 72).
//!
//! Lock order is strictly `clients` → `spawning`, and neither guard is ever
//! held across a client-lock await. The same principle governs the
//! transaction-bracket layer (ws48 ruling): a bracket serializes consumers of
//! ONE instance's document state — a bracket in root A must never block
//! file B or root C.

use anyhow::{Result, anyhow};
use ignore::WalkBuilder;
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use tokio_util::sync::CancellationToken;

use crate::bridge::filesystem_manager::{
    Change, ChangeKind, FilesystemManager, Root, mtime_nanos, observe_mtime, stat_with_retry,
};
use crate::config::{Config, DispatchMethod, LanguageConfig, ServerDef};
use crate::logging::LoggingServer;
use crate::lsp::LspClient;
use crate::lsp::bracket::{BracketOutcome, BracketQueues, Lane};
use crate::lsp::client::DocSync;
use crate::lsp::glob::{self, LspGlob};
use crate::lsp::instance_key::{InstanceKey, Scope};
use crate::lsp::rust_toolchain;
use crate::lsp::server::LspServer;
use crate::lsp::settle::{IdleDetector, SettleResult, await_idle};
use crate::lsp::state::{ServerLifecycle, ServerStatus};
use crate::recipes::InstallClass;
use crate::source::Source;

/// Looks up an existing client instance for a `(lang, server, root)` triple.
fn find_instance(
    clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>,
    lang: &str,
    server_name: &str,
    root: &Path,
) -> Option<Arc<Mutex<LspClient>>> {
    let key = InstanceKey::new(
        lang.to_string(),
        server_name.to_string(),
        Scope::Root(root.to_path_buf()),
    );
    clients.get(&key).cloned()
}

/// The `initializationOptions` for a root-scoped spawn: the user's
/// Catenary-config server options layered **over** the project's forwarded
/// config file (misc 202 follow-up).
///
/// Two data layers feed the initialize seam, and their order is a layering
/// decision, not an accident:
///
/// - **The project's config file** (rust-analyzer.toml at `root`, forwarded via
///   [`crate::lsp::project_config_forward::forwarded_options`]) is the base. It
///   is *project* data — settings that belong to the repository and travel with
///   it (its lint command, its build features).
/// - **The user's Catenary-config server options** (`[lsp.server.*]
///   .initialization_options`, `user`) overlay on top and **win on conflict**.
///   They are *machine-level* data the operator wrote into their own Catenary
///   config to shape how this machine drives the server — the more specific,
///   more deliberate layer, so it overrides a repository default it disagrees
///   with (a user who sets `check.command` in their Catenary config means it for
///   every project on the machine).
///
/// The merge is the existing object-level [`deep_merge`]: `user` overlaid onto
/// `file`, unrelated keys from both preserved. The result is the **user-options
/// input** to
/// [`ServerProfile::effective_initialization_options`](crate::lsp::server_behavior::ServerProfile::effective_initialization_options),
/// so the conformance **forced** overlay still wins over *both* layers (and
/// forbidden keys are still stripped) — that seam is unchanged and non-negotiable.
///
/// With no file forwarded this is the identity on `user` (today's behavior); with
/// a file but no user options it is the forwarded file alone.
fn initialization_options_with_project_config(
    root: &Path,
    server_name: &str,
    user: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    let profile = crate::lsp::server_behavior::ServerProfile::for_server(server_name);
    let file = crate::lsp::project_config_forward::forwarded_options(root, &profile);
    match (file, user) {
        // Project file is the base; the user's machine-level options win on top.
        (Some(file), Some(user)) => Some(crate::config::merge::deep_merge(&file, &user)),
        (Some(file), None) => Some(file),
        (None, user) => user,
    }
}

/// Tests whether a path matches a server's `file_patterns`.
///
/// If `patterns` is empty, returns `true` (no filter = match all).
/// Otherwise, matches the filename component of `path` against the
/// compiled globs.
fn file_matches_patterns(path: &Path, patterns: &[LspGlob]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let file_path = Path::new(file_name);
    patterns.iter().any(|g| g.is_match(file_path))
}

/// Builds the `file://` URI for a changed-set entry from its owning root and
/// root-relative path (WS31 Consumer A).
///
/// The baseline stores paths relative to the root (the root prefix is the outer
/// key); routing rebuilds the absolute path via `root.join(rel)` before
/// formatting the URI sent in `workspace/didChangeWatchedFiles`.
fn changed_file_uri(root: &Path, rel: &Path) -> String {
    crate::lsp::lang::path_to_uri(&root.join(rel))
}

/// Whether `server_name` is **blessed** — a diagnostics source — the
/// diagnostics-coverage classifier (diagnostics-debt 04b / DESIGN §"The blessed
/// set").
///
/// A blessed server is a diagnostics source; an unverified custom `[lsp.server.*]`
/// def is enrichment-only and never a diagnostics source, so a file whose only
/// covering server is unverified has no diagnostics coverage — the gate does not
/// arm for it. Delegates to [`crate::recipes::is_server_blessed`] (the active
/// manifest plus the operator opt-in), so a re-pin's classification takes effect
/// without a binary release and the whole daemon shares one predicate.
fn server_is_blessed(server_name: &str) -> bool {
    crate::recipes::is_server_blessed(server_name)
}

/// Maps a semantic [`ChangeKind`] to its LSP `FileChangeType` wire value:
/// Created ⇒ 1, Changed ⇒ 2, Deleted ⇒ 3.
///
/// The wire type carries the true semantic kind so it agrees with each server's
/// watch-kind mask: a `Created` change rides `FileChangeType` 1 (gated by the
/// `Create` bit), a `Changed` change rides 2 (gated by the `Change` bit), a
/// `Deleted` change rides 3 (gated by the `Delete` bit, full walks only). Per the
/// LSP spec, `workspace/didChangeWatchedFiles` is Catenary's channel for
/// filesystem-observed changes and its payload carries the real distinction;
/// `workspace/didCreateFiles` is a different, editor-initiated notification
/// Catenary does not use.
const fn change_kind_wire_type(kind: ChangeKind) -> u8 {
    match kind {
        ChangeKind::Created => 1,
        ChangeKind::Changed => 2,
        ChangeKind::Deleted => 3,
    }
}

/// RAII lifetime for an in-flight cold-spawn marker (misc 191).
///
/// The owner of a `(lang, server, root)` cold spawn holds one of these across
/// the whole spawn+`initialize` handshake. On `Drop` — success, failure, an
/// early `?` return, or a cancelled/dropped spawn future — it removes the
/// marker from the [`LspClientManager::spawning`] map and wakes every waiter
/// via [`tokio::sync::Notify::notify_waiters`]. Binding removal to the future's
/// lifetime is the failure semantics: a marker can never outlive its spawner,
/// so an abandoned or panicking spawn clears the key instead of wedging it
/// forever. Woken waiters re-check the registry and retry fresh.
///
/// The `Notify` handle stored here is the SAME `Arc` inserted into the map;
/// waiters clone it under the registry lock, so a waiter that took its clone
/// just before the guard drops still observes the wake (`notify_waiters` wakes
/// all *currently registered* waiters, and a duplicate requester registers
/// before releasing the registry lock).
struct SpawnMarkerGuard<'a> {
    spawning: &'a std::sync::Mutex<HashMap<InstanceKey, Arc<tokio::sync::Notify>>>,
    key: InstanceKey,
    notify: Arc<tokio::sync::Notify>,
}

impl Drop for SpawnMarkerGuard<'_> {
    fn drop(&mut self) {
        // Tiny sync section: remove the marker, then wake waiters. Never held
        // across an await (bug 104's lock-ordering doctrine is about the
        // async client/registry mutexes; this std mutex touches neither).
        self.spawning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
        self.notify.notify_waiters();
    }
}

/// The outcome of [`LspClientManager::claim_spawn`] — the shared
/// spawn-or-await gate for every cold-spawn path (misc 191, extracted as a
/// shared helper by misc 208).
///
/// `Found` hands back the registry's existing entry (live or tombstone) for
/// the key, snapshotted under the registry guard; the guard is gone by the
/// time the caller sees it, so a liveness check (which awaits the client's
/// own mutex) blocks only callers of THAT instance — never the registry
/// (bug 104). `Owner` means the caller won the cold spawn: the marker guard
/// clears the key and wakes waiters on every exit (RAII, misc 191), and the
/// spawn+`initialize` handshake runs with no registry involvement at all.
enum SpawnClaim<'a> {
    /// The registry already holds an entry (live or tombstone) for the key.
    Found(Arc<Mutex<LspClient>>),
    /// The caller owns this key's cold spawn; hold the guard across the
    /// handshake.
    Owner(SpawnMarkerGuard<'a>),
}

/// One alive rooted server covering a walked root, with its registered file
/// watchers (WS31 Consumer A). Produced by
/// [`LspClientManager::covering_watchers`] and consumed by the changed-set
/// routing and the walk-breadth gate's coverage check.
struct Covering {
    server: Arc<LspServer>,
    /// The owning client handle — needed by the changed-set routing's
    /// open-document leg (the didChange full-text relay, diagnostics-debt
    /// 01). Never locked while the clients registry lock is held (bug 104).
    client: Arc<Mutex<LspClient>>,
    name: String,
    watchers: Vec<crate::lsp::server::ParsedWatcher>,
}

/// Stats one supplemental watch-probe candidate and records it if it is a
/// present regular file the main walk did not already observe (bug 143).
///
/// Absence is the expected answer for most candidates (a marker name probed in
/// a directory that has none), and an absent path must contribute nothing — a
/// probe never invents a baseline member. The stat is the shared
/// [`stat_with_retry`] so the sub-millisecond atomic-rename window that would
/// otherwise read as "absent" (and, on a reaping sweep, as a deletion) is closed
/// the same way every walk surface closes it (WS31-review H1).
///
/// A candidate outside `root` is dropped: the per-root baseline keys
/// root-relative paths, so a path with no root-relative form has no
/// representation in the model (bug 143's out-of-root leg, flagged not invented).
fn probe_watched_path(
    root: &Path,
    abs: &Path,
    walked: &HashSet<&Path>,
    recorded: &mut HashSet<PathBuf>,
    extra: &mut Vec<(PathBuf, i64)>,
) {
    let Ok(rel) = abs.strip_prefix(root) else {
        return;
    };
    if walked.contains(rel) || recorded.contains(rel) {
        return;
    }
    let Some(metadata) = stat_with_retry(abs) else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    recorded.insert(rel.to_path_buf());
    extra.push((rel.to_path_buf(), mtime_nanos(&metadata)));
}

/// How wide the changed-set engine should walk for a given command — the
/// per-command pre-check gate (WS31 ticket 04, decision 018 —
/// filesystem-coherence changed-set).
///
/// Computed *before* the walk from two inputs: whether an active server covers
/// the scope ([`LspClientManager::has_covering_watchers`]) and what the command
/// needs fresh (its query type):
///
/// ```text
/// None    ⇔  no covering server, OR raw/--count grep, OR a (no LSP) path
/// Full    ⇔  covering server ∧ (enriched query ∨ diagnostics)
/// ```
///
/// `None` ⇒ skip the engine entirely (raw grep, `--count`, `(no LSP)` pay
/// nothing). `Full` ⇒ walk the registered-glob set in the root and reap
/// deletions. The old `Scoped` variant (glob's pattern-bounded walk) retired
/// with the ws43-03 cutover: glob's scoped add/update-only nudge now rides the
/// annotation batches (`reap_scopes: None` in `nudge_observed_files` — a
/// scoped observation set still never reaps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkBreadth {
    /// Skip the engine: no walk, no nudge.
    None,
    /// Full walk of the registered-glob set; reaps deletions.
    Full,
}

impl WalkBreadth {
    /// Whether this breadth runs the changed-set engine at all.
    #[must_use]
    pub(crate) const fn runs_engine(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this breadth reaps deletions (full walks only).
    #[must_use]
    pub(crate) const fn reaps(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Walks up from `file` toward `workspace_root`, returning the first
/// directory containing any marker.
///
/// Bounded by `workspace_root` — the walk never escapes above it.
/// Returns `workspace_root` if no marker is found.
///
/// `compiled_markers` contains only the glob-pattern entries from
/// `markers`, pre-compiled at config load time. Exact filenames in
/// `markers` use the fast `exists()` path; globs require reading
/// directory entries.
fn resolve_marker_root(
    file: &Path,
    markers: &[String],
    compiled_markers: &[LspGlob],
    workspace_root: &Path,
) -> PathBuf {
    let mut dir = if file.is_dir() {
        file.to_path_buf()
    } else {
        file.parent()
            .map_or_else(|| workspace_root.to_path_buf(), Path::to_path_buf)
    };

    loop {
        if dir_has_marker(&dir, markers, compiled_markers) {
            return dir;
        }

        // Stop at workspace root boundary.
        if dir == workspace_root {
            break;
        }

        // Move up one level, but never above workspace root.
        match dir.parent() {
            Some(parent) if parent.starts_with(workspace_root) || parent == workspace_root => {
                dir = parent.to_path_buf();
            }
            _ => break,
        }
    }

    workspace_root.to_path_buf()
}

/// Whether a directory directly contains any of the given markers.
///
/// Exact filenames (no glob metacharacters) use `exists()` — no
/// directory read needed. Glob patterns require reading directory
/// entries and matching against compiled matchers. The glob-readdir
/// branch is only entered when `compiled_markers` is non-empty.
fn dir_has_marker(dir: &Path, markers: &[String], compiled_markers: &[LspGlob]) -> bool {
    // Fast path: exact filename markers.
    for m in markers {
        if !glob::is_glob_pattern(m) && dir.join(m).exists() {
            return true;
        }
    }
    // Slow path: glob markers require readdir.
    if !compiled_markers.is_empty()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_path = Path::new(&name);
            for g in compiled_markers {
                if g.is_match(name_path) {
                    return true;
                }
            }
        }
    }
    false
}

/// Maximum strikes before a server instance is benched (misc 167).
///
/// The counter is clamped to `[0, MAX_SERVER_STRIKES]`; at the cap the
/// instance is out — no further demand-driven revives until the daemon
/// restarts or the root is retired and remounted (both reset the ledger).
const MAX_SERVER_STRIKES: u8 = 3;

/// Upper bound on how long a `grep`/`glob` query waits for server
/// spawn/settle before it serves its results UNENRICHED (misc 197 stage 1).
///
/// The query enrichment path ([`LspClientManager::ensure_and_wait_for_paths`])
/// can block unboundedly: a freshly-spawned or busy server sits in
/// `Initializing`/`Pending`/`Busy`, and [`LspClient::wait_ready`] loops on its
/// `state_notify` with no ceiling. Under a wedged/busy settle that turns a
/// query silent — the caller sees nothing until the server happens to drain.
///
/// Decision 025's additive doctrine: enrichment rides along where available,
/// but the ripgrep/readdir results are complete on their own and must never be
/// held hostage to it. So the *wait* is bounded, not the search. Past the
/// bound the query proceeds with whatever servers are ready; unenriched files
/// fall through the existing degraded-enrichment arm (grep's `#?`
/// could-not-enrich anchor, glob's missing outline) — no new output shape.
///
/// The value is generous by design (a few seconds, not milliseconds): a cold
/// rust-analyzer settle is normal and worth waiting for, so the bound must not
/// clip healthy enrichment. Five seconds sits above a warm server's ready
/// latency yet well under the host harness's slow-command auto-background
/// threshold — long enough that a normal settle enriches, short enough that a
/// wedged one never goes silent.
pub(crate) const QUERY_ENRICHMENT_BUDGET: Duration = Duration::from_secs(5);

/// Rung 1 of the daemon-teardown ladder (bug 130): upper bound on one child's
/// graceful leg — acquiring its client (a wedged in-flight request can hold
/// that mutex forever — the observed field wedge), sending the LSP
/// `shutdown`/`exit` sequence, and watching the process die.
///
/// Deliberately not generous: teardown blocks `catenary restart`, so the
/// whole ladder must stay seconds-scale.
const TEARDOWN_GRACEFUL_GRACE: Duration = Duration::from_secs(5);

/// Rung 2 of the ladder: grace after SIGTERM before escalating to SIGKILL.
const TEARDOWN_SIGTERM_GRACE: Duration = Duration::from_secs(2);

/// Whole-teardown ceiling regardless of fleet size (bug 130). Per-child
/// ladders run concurrently, so a healthy teardown never approaches this;
/// it is the hard stop that guarantees teardown ends even if a ladder
/// wedges. Stragglers still pending at the ceiling are killed (SIGKILL) by
/// PID.
const TEARDOWN_CEILING: Duration = Duration::from_secs(20);

/// Poll cadence for the ladder's is-the-child-dead checks.
const TEARDOWN_POLL: Duration = Duration::from_millis(25);

/// Injectable timings for the bounded teardown ladder (bug 130).
///
/// Production uses [`Self::PRODUCTION`] (5 s graceful / 2 s SIGTERM / 20 s
/// ceiling); tests shrink them via
/// `LspClientManager::teardown_timings_override` so ladder paths run in
/// milliseconds.
#[derive(Debug, Clone, Copy)]
struct TeardownTimings {
    /// Rung 1 bound: client acquisition + graceful `shutdown`/`exit` +
    /// death wait.
    graceful_grace: Duration,
    /// Rung 2 bound: SIGTERM-to-SIGKILL escalation grace.
    sigterm_grace: Duration,
    /// Whole-fleet hard stop.
    ceiling: Duration,
}

impl TeardownTimings {
    /// The production ladder: 5 s graceful, 2 s SIGTERM, 20 s ceiling.
    const PRODUCTION: Self = Self {
        graceful_grace: TEARDOWN_GRACEFUL_GRACE,
        sigterm_grace: TEARDOWN_SIGTERM_GRACE,
        ceiling: TEARDOWN_CEILING,
    };
}

/// Straggler ledger for the teardown ceiling (bug 130): every child starts
/// pending with an unknown PID; its ladder records the PID once harvested and
/// removes the entry when the child is down (or the ladder has done all it
/// can). Whatever remains when the ceiling expires is killed — and named —
/// from [`LspClientManager::shutdown_all`].
type PendingTeardowns = Arc<std::sync::Mutex<HashMap<InstanceKey, Option<u32>>>>;

/// Names one teardown-ladder straggler action in the firehose (bug 130):
/// the server identity plus the rung that acted. `warn!` — a misbehaving
/// server is a health finding, never a desktop interrupt.
fn warn_straggler(key: &InstanceKey, rung: &str, detail: &str) {
    warn!(
        source = Source::LspLifecycle.as_str(),
        language = key.language_id.as_str(),
        server = key.server.as_str(),
        scope_root = key.scope.root_path().map(|p| p.display().to_string()),
        rung = rung,
        "LSP teardown: {key} {detail}",
    );
}

/// Records a harvested PID on the straggler ledger (no-op once settled).
fn note_teardown_pid(pending: &PendingTeardowns, key: &InstanceKey, pid: u32) {
    let mut pending = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = pending.get_mut(key) {
        *entry = Some(pid);
    }
}

/// Settles one child on the straggler ledger: its ladder finished (child
/// down, or nothing left to signal), so the ceiling rung must skip it.
fn settle_teardown(pending: &PendingTeardowns, key: &InstanceKey) {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(key);
}

/// One instance's standing on the strike ledger (misc 167).
///
/// `+1` per failure observation (a crash while up, a revive spawn failure, a
/// revive `initialize` failure), `−1` per served request (a delivered
/// diagnostic / symbol / response — real work only; spawning and initializing
/// earn no credit, so a spawn-then-die-before-serving loop must climb).
/// Activity-driven, never a wall clock — the settle-monitor doctrine.
#[derive(Debug, Default, Clone, Copy)]
struct StrikeEntry {
    /// Current strikes, clamped to `[0, MAX_SERVER_STRIKES]`.
    strikes: u8,
    /// Whether the instance ever completed a served request — the terminal
    /// label's cause axis: struck out with zero successes is *broken*
    /// (config / environment), with prior successes *unstable* (crashes).
    ever_served: bool,
}

/// How a dead — or never-installed — server will behave on future demand
/// (misc 167 / misc 210).
///
/// The three strike-ledger arms ([`Revivable`](Self::Revivable) /
/// [`BenchedNeverStarted`](Self::BenchedNeverStarted) /
/// [`BenchedUnstable`](Self::BenchedUnstable)) are derived from the ledger by
/// [`verdict_of`]; they drive the revive gate and the receipt wording for files
/// a dead instance owed a result. The fourth arm,
/// [`NotInstalled`](Self::NotInstalled), is **not** ledger-derived — it is
/// produced only by [`LspClientManager::unavailable_diagnostic_servers`] for a
/// configured server whose binary never resolved pre-spawn (misc 210), so the
/// receipt can distinguish "not installed" from "keeps dying". It never comes
/// out of [`verdict_of`] and never rides the strike-count mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviveVerdict {
    /// Below the strike cap: the next demand that routes here attempts a
    /// revive through the normal spawn/initialize path.
    Revivable,
    /// Struck out with zero served requests ever — spawn/initialize never
    /// got it to serve. Fix the server (config / environment); no revives
    /// until the daemon restarts or the root remounts.
    BenchedNeverStarted,
    /// Struck out after previously serving — repeated crashes exhausted the
    /// strikes. No revives until the daemon restarts or the root remounts.
    BenchedUnstable,
    /// Configured but its binary never resolved pre-spawn (misc 210): a static
    /// condition, not a crash. No strike ledger entry and no bench — a later
    /// spawn demand re-resolves and heals coverage the moment the binary
    /// appears. Distinct from the benched arms so the receipt teaches "install
    /// it" rather than "it keeps dying".
    NotInstalled,
}

/// Derives a ledger entry's verdict: benched at the strike cap, with the
/// `ever_served` axis choosing the terminal cause (misc 167 — "hit 3 with
/// zero successes" is *broken*, "hit 3 with prior successes" is *unstable*).
const fn verdict_of(entry: StrikeEntry) -> ReviveVerdict {
    if entry.strikes < MAX_SERVER_STRIKES {
        ReviveVerdict::Revivable
    } else if entry.ever_served {
        ReviveVerdict::BenchedUnstable
    } else {
        ReviveVerdict::BenchedNeverStarted
    }
}

/// Emits the single strike-out health finding (`warn!` — a TUI health
/// finding, not a desktop interrupt: the bug-79 escape keeps the session
/// unblocked, so a strike-out is actionable but never urgent).
///
/// Only called on the cap crossing, so `verdict` is never `Revivable`.
fn warn_bench_once(key: &InstanceKey, verdict: ReviveVerdict) {
    let cause = if verdict == ReviveVerdict::BenchedNeverStarted {
        "never started (spawn/initialize failed repeatedly)"
    } else {
        "gave up after repeated crashes"
    };
    warn!(
        source = Source::LspLifecycle.as_str(),
        language = key.language_id.as_str(),
        server = key.server.as_str(),
        scope_root = key.scope.root_path().map(|p| p.display().to_string()),
        "Language server struck out: {} — {cause}. No further revive \
         attempts for this root until the daemon restarts or the root \
         remounts; run `catenary doctor {}` for the spawn transcript.",
        key.server,
        key.server,
    );
}

/// Whether a **blessed recipe** exists for `server` — the axis that picks the
/// not-installed teaching variant (misc 210 / the maintainer addendum).
///
/// A blessed recipe is one auto-install could actually act on: the active
/// manifest pins the server, a shipped recipe carries that exact pinned
/// version, and the blessing gate resolves ([`crate::install::BlessedRecipe::resolve`]).
/// This mirrors the shared eligibility gate both auto-install legs ask
/// ([`crate::auto_install::AutoInstaller::install_target`]) over the live
/// shipped data, so "the finding suggests auto-install" and "auto-install would
/// act" never disagree. When it returns `false` there is nothing for
/// auto-install to fetch, so the finding offers only the honest half — never a
/// suggestion auto-install cannot fulfill. The daemon reads the wired
/// installer's own data instead when the JIT seam is attached
/// ([`LspClientManager::blessed_recipe_exists`]).
fn has_blessed_recipe(server: &str) -> bool {
    let Ok(recipes) = crate::recipes::default_recipes() else {
        return false;
    };
    let Some(recipe) = recipes.get(server) else {
        return false;
    };
    let manifest = crate::recipes::active_manifest();
    let Some(version) = manifest.pinned_version(server) else {
        return false; // unpinned (or the rust-analyzer exemption)
    };
    if recipe.version != version {
        return false; // a recipe/manifest skew auto-install would refuse to kick
    }
    crate::install::BlessedRecipe::resolve(server, recipe, &manifest).is_some()
}

/// What `[servers] auto_install` can actually do for a server the spawn path
/// just found missing — the axis the not-installed teaching branches on
/// (bug 148).
///
/// The bug this types away: the finding recommended `auto_install = true`
/// unconditionally, including to users who already had it set and to servers
/// the flag could never act on. Each variant here is a distinct honest thing to
/// say, and only one of them recommends the flag.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutoInstallStance {
    /// The flag is unset and a blessed, version-matched recipe exists: the
    /// standing opt-in is genuinely worth teaching — the **only** branch that
    /// recommends it.
    OfferFlag,
    /// The flag is unset and nothing is installable anyway (no blessed
    /// recipe): the honest half only, as before.
    NoRecipe,
    /// The flag is on, the gates cleared, and this surfacing kicked the
    /// background install — the message announces it instead of advising.
    Kicked {
        /// The blessed pin the install lands.
        version: String,
        /// Fetch- or compile-class, for the honest "takes minutes" wording.
        class: InstallClass,
    },
    /// The flag is on and an install of this server is already in flight (the
    /// eager session-start leg, or another root's demand, got there first).
    InFlight {
        /// The blessed pin the running install lands.
        version: String,
    },
    /// The flag is on but nothing can act — `reason` says why, briefly. Never
    /// recommends the flag: it is already set and still cannot help.
    Blocked {
        /// The short, honest reason auto-install stands aside.
        reason: String,
    },
}

/// The not-installed finding text (misc 210 + bug 148), split from the `warn!`
/// so every teaching branch is unit-testable.
///
/// Honest first — "configured for `<language>` but not installed" — then one
/// branch per [`AutoInstallStance`]: a fired kick **announces the install**, a
/// running one says so, an enabled-but-powerless flag says why nothing can act,
/// and the `auto_install = true` recommendation survives only where it is
/// genuinely unset and a blessed recipe exists.
fn not_installed_message(server: &str, language: &str, stance: &AutoInstallStance) -> String {
    let honest = format!("{server} is configured for {language} but not installed.");
    let by_hand = "Install the binary and place it on PATH — coverage heals on the next demand, \
                   no daemon restart needed.";
    match stance {
        AutoInstallStance::OfferFlag => format!(
            "{honest} Install it with `catenary install {server}`, or set \
             `[servers] auto_install = true` to install missing blessed servers \
             automatically — at session start and on first demand."
        ),
        AutoInstallStance::NoRecipe => format!("{honest} {by_hand}"),
        AutoInstallStance::Kicked { version, class } => {
            let pace = match class {
                InstallClass::Fetch => String::new(),
                InstallClass::Compile => {
                    " (compiles from source — this can take minutes)".to_owned()
                }
            };
            format!(
                "{honest} `[servers] auto_install` is on: installing {server} {version} in the \
                 background{pace}; coverage arrives when it lands."
            )
        }
        AutoInstallStance::InFlight { version } => format!(
            "{honest} `[servers] auto_install` is on and an install of {server} {version} is \
             already running; coverage arrives when it lands."
        ),
        AutoInstallStance::Blocked { reason } => format!(
            "{honest} `[servers] auto_install` is on but cannot install {server}: {reason}. \
             {by_hand}"
        ),
    }
}

/// Emits the single not-installed health finding (`warn!` — a TUI health
/// finding, not a desktop interrupt: a missing binary is actionable but never
/// urgent, the same posture as a strike-out).
///
/// One calm finding per server (deduped by the caller); the wording comes from
/// [`not_installed_message`] and the `stance` its caller resolved (bug 148).
fn warn_not_installed(server: &str, language: &str, stance: &AutoInstallStance) {
    let message = not_installed_message(server, language, stance);
    warn!(
        source = Source::LspLifecycle.as_str(),
        language = language,
        server = server,
        "{message}",
    );
}

impl ReviveVerdict {
    /// Whether the verdict permits a demand-driven revive.
    #[must_use]
    pub const fn is_revivable(self) -> bool {
        matches!(self, Self::Revivable)
    }

    /// The short benched-cause label mirrored to the `state.json` board
    /// (`None` while revivable, and `None` for `NotInstalled` — a missing binary
    /// is never a bench, it carries the dedicated `not-installed` board state
    /// instead, misc 210).
    #[must_use]
    pub const fn bench_label(self) -> Option<&'static str> {
        match self {
            Self::Revivable | Self::NotInstalled => None,
            Self::BenchedNeverStarted => Some("never started"),
            Self::BenchedUnstable => Some("unstable"),
        }
    }
}

/// A configured server whose binary does not resolve pre-spawn (misc 210).
///
/// A missing binary is a **static** condition, knowable before the first spawn
/// attempt — not a crash. So [`LspClientManager::spawn_inner`] classifies it
/// pre-spawn as *not installed* and bails with this error rather than feeding
/// the strike ledger (no strikes, no bench) or leaving a dead tombstone.
/// Callers that would otherwise emit a generic `warn!` on any spawn failure
/// (`spawn_all`, `ensure_clients_for_paths`) downcast to this and stay quiet —
/// the one calm not-installed finding has already been surfaced (once per
/// server), so a second per-root/per-attempt line would just fight it.
#[derive(Debug, Error)]
#[error(
    "{server} ({language}) is configured but not installed — no binary resolved pre-spawn (misc 210)"
)]
pub struct NotInstalled {
    /// The configured server key whose binary is missing.
    pub server: String,
    /// The language the missing server was routed for.
    pub language: String,
}

/// Whether `err` (an `anyhow` chain) is the [`NotInstalled`] classification —
/// the signal a spawn-failure caller uses to suppress its generic `warn!`
/// (misc 210): the calm not-installed finding is already surfaced.
fn is_not_installed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<NotInstalled>().is_some()
}

/// The daemon's background auto-installer as the spawn path sees it — bug 148's
/// demand-driven (JIT) seam.
///
/// The eager leg runs at `SessionStart` off the dispatch context's installer;
/// this is the *same* [`crate::auto_install::AutoInstaller`] handed to the
/// manager (a cheap `Arc` clone), so both legs share one in-flight dedupe, one
/// concurrency cap, and one failure-warn ledger. Attached by
/// [`LspClientManager::attach_auto_installer`] in daemon wiring only —
/// doctor/CLI/test managers leave it unset and never kick.
struct JitAutoInstall {
    /// The daemon-wide installer: announce, dedupe, cap, snapshot records.
    installer: crate::auto_install::AutoInstaller,
    /// Weak handle to the manager that owns this seam. A landed install is a
    /// coverage change, so completion fires the same `spawn_all` pre-warm a
    /// `catenary pin` runs — through a `Weak`, so the seam never keeps its own
    /// manager alive (and a dead manager simply skips the pre-warm).
    manager: std::sync::Weak<LspClientManager>,
}

/// Manages the lifecycle of LSP clients, document state, and language detection.
///
/// Single authority for LSP server spawning, caching, shutdown, and document
/// lifecycle. Document versioning and open/close tracking live on each
/// [`LspClient`] — each server sees an independent monotonic version sequence.
pub struct LspClientManager {
    config: Arc<Config>,
    clients: Mutex<HashMap<InstanceKey, Arc<Mutex<LspClient>>>>,
    /// In-flight cold-spawn markers, one per `InstanceKey` (misc 191). A
    /// per-root spawn+`initialize` handshake is slow (rust-analyzer-class:
    /// hundreds of ms to seconds); holding the `clients` registry lock across
    /// it — the old anti-duplicate-spawn hold — stalled every manager lookup
    /// daemon-wide for the coldest server's latency (a self-inflicted mini-104).
    /// Now `spawn_inner` holds the registry lock only long enough to look up or
    /// insert a marker here, then drops it and runs the handshake unlocked.
    /// Duplicate requesters of the SAME key find the marker and await its
    /// [`Notify`](tokio::sync::Notify), never launching a second spawn; different
    /// keys and plain lookups take the registry lock unblocked. The marker's
    /// lifetime is bound to the spawner future by [`SpawnMarkerGuard`], whose
    /// `Drop` removes the entry and wakes waiters — so a spawn failure, an early
    /// return, or a cancelled/dropped spawn future all clear the key rather than
    /// wedging it forever; woken waiters re-check the registry and retry fresh.
    /// `std::sync::Mutex`: tiny critical sections, never held across `await`.
    spawning: std::sync::Mutex<HashMap<InstanceKey, Arc<tokio::sync::Notify>>>,
    /// The per-instance strike ledger (misc 167): the activity-driven revive
    /// gate. Keyed by the full `(language, server, root)` instance key and
    /// held OUTSIDE the client map so it survives tombstone removal and
    /// respawn — the whole point is remembering failures across instances.
    /// Cleared per-root on retirement (bug 93: a retired root's servers must
    /// not revive, and a remount starts fresh) and implicitly on daemon
    /// restart. `std::sync::Mutex`: tiny critical sections, never held
    /// across `await`.
    strikes: std::sync::Mutex<HashMap<InstanceKey, StrikeEntry>>,
    /// Servers whose not-installed finding has already fired this daemon
    /// lifetime (misc 210): one calm `warn!` **per server**, not per root and
    /// not per spawn attempt. Keyed by server name (a server missing from PATH
    /// is missing for every root, so the finding is a per-server truth), and
    /// cleared for a server the moment a later spawn demand finds its binary —
    /// so a re-removal can warn again. `std::sync::Mutex`: tiny critical
    /// sections, never held across `await`.
    not_installed_warned: std::sync::Mutex<HashSet<String>>,
    /// Negative cache for single-file server initialization failures.
    /// Contains `(language_id, server_name)` pairs where the server is
    /// configured with `single_file = true` but rejected null-workspace
    /// initialization at runtime. Uses `std::sync::Mutex` — reads are
    /// fast and non-contended.
    pub(crate) single_file_failures: std::sync::Mutex<HashSet<(String, String)>>,
    /// Last-demand clocks for rootless single-file singletons (brackets 01).
    /// Stamped by every [`Self::ensure_single_file_server`] hit and every
    /// successful [`Self::spawn_single_file`]; swept by
    /// [`Self::reap_idle_single_file_instances`] on the daemon's idle-expiry
    /// cadence — same lifetime rules as root instances, minus any
    /// root-tracker/ownership involvement. `std::sync::Mutex`: tiny critical
    /// sections, never held across `await`.
    single_file_last_use: std::sync::Mutex<HashMap<InstanceKey, Instant>>,
    /// Per-instance transaction-bracket queues (brackets 02). Serialized,
    /// run-to-completion access to an instance's document state: consumers
    /// of the rootless singletons go through
    /// [`Self::with_single_file_bracket`] rather than interleaving raw
    /// `didOpen`/request/`didClose` traffic. One queue per [`InstanceKey`],
    /// never global — the registry's own lock is lookup-scoped, and no
    /// manager lock is held across a bracket (bug 104).
    brackets: BracketQueues,
    /// Cache for root marker resolution results.
    /// Key: `(directory, server_name)` → resolved root path.
    /// Avoids re-walking the directory tree for files in the same
    /// directory. Cleared on root changes (`sync_roots`).
    marker_cache: std::sync::Mutex<HashMap<(PathBuf, String), PathBuf>>,
    logging: LoggingServer,
    fs: Arc<FilesystemManager>,
    /// `state.json` snapshot writer for live server-board mirroring.
    /// `None` in doctor/test contexts.
    snapshot: Option<Arc<crate::state_snapshot::SnapshotWriter>>,
    /// Bounded-teardown ladder timings (bug 130). Defaults to
    /// [`TeardownTimings::PRODUCTION`]; tests shrink them via
    /// [`Self::teardown_timings_override`] so ladder paths run in
    /// milliseconds.
    teardown_timings: TeardownTimings,
    /// The demand-driven auto-install seam (bug 148), wired once by
    /// [`Self::attach_auto_installer`] in daemon mode. `OnceLock` because the
    /// daemon builds the installer *after* the manager exists (the manager is
    /// already behind an `Arc` by then) and it never changes afterwards — the
    /// read on the spawn path is lock-free. Unset in doctor/CLI/test contexts,
    /// where nothing kicks.
    auto_install: std::sync::OnceLock<JitAutoInstall>,
}

impl LspClientManager {
    /// Creates a new `LspClientManager`.
    ///
    /// Workspace roots are sourced from the shared [`FilesystemManager`] —
    /// call [`FilesystemManager::set_roots`] before constructing this manager.
    #[must_use]
    pub fn new(
        config: impl Into<Arc<Config>>,
        logging: LoggingServer,
        fs: Arc<FilesystemManager>,
    ) -> Self {
        let config = config.into();
        Self {
            config,
            clients: Mutex::new(HashMap::new()),
            spawning: std::sync::Mutex::new(HashMap::new()),
            strikes: std::sync::Mutex::new(HashMap::new()),
            not_installed_warned: std::sync::Mutex::new(HashSet::new()),
            single_file_failures: std::sync::Mutex::new(HashSet::new()),
            single_file_last_use: std::sync::Mutex::new(HashMap::new()),
            brackets: BracketQueues::new(),
            marker_cache: std::sync::Mutex::new(HashMap::new()),
            logging,
            fs,
            snapshot: None,
            teardown_timings: TeardownTimings::PRODUCTION,
            auto_install: std::sync::OnceLock::new(),
        }
    }

    /// Wires the daemon's background auto-installer onto the spawn path —
    /// bug 148's demand-driven (JIT) leg.
    ///
    /// Called once from the daemon's session wiring with the same installer the
    /// `SessionStart` (eager) leg uses, so the two share the in-flight dedupe,
    /// the concurrency cap, and the once-per-lifetime failure warn. Takes
    /// `&Arc<Self>` to keep a `Weak` self-handle for the post-install pre-warm
    /// (`spawn_all`, the same leg a `catenary pin` runs) — a `Weak`, so this
    /// seam never forms an ownership cycle with the manager. A second call is a
    /// no-op: the seam is wired once per manager.
    pub fn attach_auto_installer(self: &Arc<Self>, installer: crate::auto_install::AutoInstaller) {
        if self
            .auto_install
            .set(JitAutoInstall {
                installer,
                manager: Arc::downgrade(self),
            })
            .is_err()
        {
            debug!(
                source = Source::LspLifecycle.as_str(),
                "auto-installer already attached to this manager — keeping the first",
            );
        }
    }

    /// Shrinks the bounded-teardown ladder timings (bug 130, test-only).
    ///
    /// Production always tears down with [`TeardownTimings::PRODUCTION`];
    /// tests inject millisecond-scale graces so the ladder's escalation and
    /// ceiling paths run fast. No production caller.
    #[cfg(test)]
    #[must_use]
    const fn teardown_timings_override(
        mut self,
        graceful_grace: Duration,
        sigterm_grace: Duration,
        ceiling: Duration,
    ) -> Self {
        self.teardown_timings = TeardownTimings {
            graceful_grace,
            sigterm_grace,
            ceiling,
        };
        self
    }

    /// Shrinks the per-bracket service budget (brackets 02, test-only).
    ///
    /// Production always serves with the constant
    /// [`crate::lsp::bracket::BRACKET_SERVICE_BUDGET`]; tests inject a
    /// millisecond budget so the completed-degraded backstop runs fast.
    /// No production caller.
    #[cfg(test)]
    #[must_use]
    fn bracket_budget_override(mut self, budget: Duration) -> Self {
        self.brackets = BracketQueues::with_budget(budget);
        self
    }

    /// Sets the `state.json` snapshot writer for live server-board mirroring.
    ///
    /// Called by [`crate::bridge::session::Session`] after construction in
    /// daemon mode. Doctor and test contexts skip this.
    pub fn set_snapshot(&mut self, writer: Arc<crate::state_snapshot::SnapshotWriter>) {
        self.snapshot = Some(writer);
    }

    /// Returns the `state.json` snapshot writer, when wired.
    ///
    /// Lets the diagnostics path record decision-027 coverage degradation on
    /// the server board (`degraded_since`/`degraded_reason`). `None` in doctor
    /// and test contexts where no snapshot is mirrored.
    #[must_use]
    pub const fn snapshot(&self) -> Option<&Arc<crate::state_snapshot::SnapshotWriter>> {
        self.snapshot.as_ref()
    }

    /// Returns a reference to the configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    // ── Strike ledger (misc 167) ─────────────────────────────────────

    /// Records one failure observation (`+1`) for an instance: a crash while
    /// up, a revive spawn failure, or a revive `initialize` failure.
    ///
    /// Clamped at [`MAX_SERVER_STRIKES`]. Crossing the cap benches the
    /// instance and emits a single `warn!()` — a TUI health finding, not a
    /// desktop interrupt: the bug-79 escape keeps the session unblocked
    /// (the gate still pays with an honest receipt), so a strike-out is
    /// actionable but never urgent.
    fn record_server_strike(&self, key: &InstanceKey) {
        let mut ledger = self
            .strikes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = ledger.entry(key.clone()).or_default();
        let before = entry.strikes;
        entry.strikes = entry.strikes.saturating_add(1).min(MAX_SERVER_STRIKES);
        let strikes = entry.strikes;
        let verdict = verdict_of(*entry);
        drop(ledger);
        // First crossing only — a repeat failure at the cap is clamped, so
        // `before < MAX` cannot re-fire the finding.
        let crossed = before < MAX_SERVER_STRIKES && strikes == MAX_SERVER_STRIKES;
        debug!(
            source = Source::LspLifecycle.as_str(),
            server = key.server.as_str(),
            scope_root = key.scope.root_path().map(|p| p.display().to_string()),
            "Strike {strikes}/{MAX_SERVER_STRIKES} recorded for {key}",
        );
        if crossed {
            warn_bench_once(key, verdict);
        }
        self.mirror_strikes(key, strikes, verdict);
    }

    /// Records one served request (`−1`) for an instance — a delivered
    /// diagnostic / symbol / response. Real work only: the spawn path and
    /// the eager health probe never call this, so only demand-side serving
    /// pays the counter down.
    pub fn record_server_service(&self, key: &InstanceKey) {
        let mut ledger = self
            .strikes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = ledger.get_mut(key) else {
            // No failure history: nothing to pay down, keep the ledger
            // sparse (the common healthy server never allocates).
            return;
        };
        let before = entry.strikes;
        entry.strikes = entry.strikes.saturating_sub(1);
        entry.ever_served = true;
        let strikes = entry.strikes;
        let verdict = verdict_of(*entry);
        drop(ledger);
        if before != strikes {
            debug!(
                source = Source::LspLifecycle.as_str(),
                server = key.server.as_str(),
                "Served work pays a strike down: {strikes}/{MAX_SERVER_STRIKES} for {key}",
            );
            self.mirror_strikes(key, strikes, verdict);
        }
    }

    /// Records one **verified-contract violation** (`+1`) for an instance
    /// (diagnostics-debt 05): a blessed server whose discipline owed an answer
    /// this diagnostics round and gave none — a declared-push server that never
    /// published, or a debounce server whose version echo never landed inside its
    /// declared bound.
    ///
    /// A server violating its adapter is sick the same way a crashing one is
    /// (DESIGN §"The floor is fault attribution"), so it feeds the **same** strike
    /// ledger a crash does: the same `+1`, the same bench-at-cap, the same
    /// pay-down on the next served round. Delegates to [`Self::record_server_strike`]
    /// — no rival ledger.
    pub fn record_contract_violation(&self, key: &InstanceKey) {
        debug!(
            source = Source::LspLifecycle.as_str(),
            server = key.server.as_str(),
            scope_root = key.scope.root_path().map(|p| p.display().to_string()),
            "Verified-contract violation for {key}: discipline owed an answer this \
             round and none came — striking the ledger",
        );
        self.record_server_strike(key);
    }

    /// The instance's current [`ReviveVerdict`] from the strike ledger.
    ///
    /// An instance the ledger has never seen is [`ReviveVerdict::Revivable`].
    pub fn revive_verdict(&self, key: &InstanceKey) -> ReviveVerdict {
        let ledger = self
            .strikes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger
            .get(key)
            .copied()
            .map_or(ReviveVerdict::Revivable, verdict_of)
    }

    /// Whether the ledger holds any failure history for the instance —
    /// the "a configured server has been failing here" signal that keeps the
    /// receipt honest even when no tombstone client survives (a spawn-fail
    /// class instance never enters the client map).
    fn strikes_recorded(&self, key: &InstanceKey) -> bool {
        let ledger = self
            .strikes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger.get(key).is_some_and(|e| e.strikes > 0)
    }

    /// Whether an in-flight cold-spawn marker (misc 191) is currently held for
    /// `key` — the state-based "this key's cold spawn is mid-handshake" signal
    /// tests poll to detect the window without a wall-clock sleep.
    #[cfg(test)]
    fn is_spawning(&self, key: &InstanceKey) -> bool {
        self.spawning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(key)
    }

    /// How many cold-spawn markers (misc 191) are in flight right now. Tests
    /// assert this is `0` after a spawn completes or fails — the guard cleared
    /// the key, never wedging it.
    #[cfg(test)]
    fn spawning_len(&self) -> usize {
        self.spawning
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Mirrors the instance's ledger standing onto the `state.json` board so
    /// the TUI/doctor health surfaces can read it. No-op without a wired
    /// snapshot (doctor / test contexts).
    fn mirror_strikes(&self, key: &InstanceKey, strikes: u8, verdict: ReviveVerdict) {
        if let Some(writer) = &self.snapshot {
            writer.update_strikes(key, strikes, verdict.bench_label());
        }
    }

    /// Drops ledger entries scoped to a retired root: retirement is the
    /// operator/idle-cycle reset (bug 93 — a retired root's servers must not
    /// revive, and a later remount starts with a clean slate).
    fn clear_strikes_for_root(&self, root: &Path) {
        self.strikes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|k, _| k.scope.root_path() != Some(root));
    }

    /// Drops one instance's ledger entry (an intentional shutdown/restart is
    /// not failure history).
    fn clear_strikes(&self, key: &InstanceKey) {
        self.strikes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
    }

    /// Drops the whole ledger (daemon shutdown; restart resets to zero).
    fn clear_all_strikes(&self) {
        self.strikes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    // ── Not-installed classification (misc 210) ──────────────────────────

    /// Classifies a configured-but-uninstalled server (misc 210): a static
    /// pre-spawn condition, **not** a strike-ledger failure.
    ///
    /// Surfaces exactly one calm `warn!` per server per daemon lifetime (the
    /// dedupe below) and mirrors the honest `not-installed` state onto the
    /// `state.json` board — never `initializing`, never a bench. This is also
    /// the ground-truth "wants but cannot spawn" signal, so the first surfacing
    /// resolves the [`AutoInstallStance`] — which, with `[servers] auto_install`
    /// on and the gates clear, **kicks the background install right here**
    /// (bug 148's JIT leg) — and the finding is worded from what actually
    /// happened. Repeat attempts take the already-surfaced path below and can
    /// never re-kick.
    fn classify_not_installed(&self, key: &InstanceKey, program: &str, def: &ServerDef) {
        if let Some(writer) = &self.snapshot {
            writer.mark_not_installed(key);
        }
        let first = self
            .not_installed_warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.server.clone());
        if !first {
            // Already warned once this daemon lifetime — a second root or a
            // repeat spawn demand does not re-fire the finding, and does not
            // kick a second install.
            debug!(
                source = Source::LspLifecycle.as_str(),
                server = key.server.as_str(),
                scope_root = key.scope.root_path().map(|p| p.display().to_string()),
                "not installed (already surfaced): {} resolves to `{program}` which is not on PATH",
                key.server,
            );
            return;
        }
        let stance = self.auto_install_stance(&key.server, def);
        warn_not_installed(&key.server, &key.language_id, &stance);
    }

    /// Resolves — and, when it can, **enacts** — what `[servers] auto_install`
    /// does for a server the spawn path just found missing (bug 148's JIT leg).
    ///
    /// With the flag on, the server faces exactly the gates
    /// [`crate::auto_install::detect_missing`] applies at session start: no
    /// `[lsp.server.*]` `path` override (the user's own resolution is never
    /// second-guessed), `prefer_managed` on (otherwise a landed install would
    /// never be consulted at spawn), and a blessed, version-matched recipe
    /// ([`crate::auto_install::AutoInstaller::install_target`]). Clearing them
    /// all kicks the background install through the daemon's shared installer —
    /// announce, in-flight dedupe, concurrency cap, one failure `warn!` per
    /// server per daemon lifetime, snapshot record, and the completion pre-warm.
    /// Anything short of that returns the honest reason instead, and the flag is
    /// only ever *recommended* when it is genuinely unset.
    ///
    /// Called once per server per daemon lifetime, from the first-surfacing
    /// branch of [`Self::classify_not_installed`].
    fn auto_install_stance(&self, server: &str, def: &ServerDef) -> AutoInstallStance {
        if !self.config.auto_install() {
            return if self.blessed_recipe_exists(server) {
                AutoInstallStance::OfferFlag
            } else {
                AutoInstallStance::NoRecipe
            };
        }
        if def.path.is_some() {
            return AutoInstallStance::Blocked {
                reason: format!(
                    "`[lsp.server.{server}] path` is your own resolution, which auto-install \
                     never replaces"
                ),
            };
        }
        if !self.config.prefer_managed() {
            return AutoInstallStance::Blocked {
                reason: "`[servers] prefer_managed = false` keeps managed installs out of spawn \
                         resolution"
                    .to_owned(),
            };
        }
        let Some(jit) = self.auto_install.get() else {
            return AutoInstallStance::Blocked {
                reason: "this process runs no background installer (the daemon installs blessed \
                         servers)"
                    .to_owned(),
            };
        };
        let Some(target) = jit.installer.install_target(server) else {
            return AutoInstallStance::Blocked {
                reason: format!("no blessed, version-matched install recipe pins {server}"),
            };
        };
        let version = target.version.clone();
        let class = target.class;
        let manager = jit.manager.clone();
        if jit.installer.kick(&target, move || {
            // A landed install is a coverage change: run the same
            // fire-and-forget `spawn_all` pre-warm a `catenary pin` (and the
            // session-start leg) runs, so the new server spawns for every live
            // matching root rather than lazily on the next query.
            if let Some(manager) = manager.upgrade() {
                tokio::spawn(async move { manager.spawn_all().await });
            }
        }) {
            AutoInstallStance::Kicked { version, class }
        } else {
            AutoInstallStance::InFlight { version }
        }
    }

    /// Whether an install of `server` is constructible at all — the
    /// recipe-teaching axis for the flag-unset branches.
    ///
    /// Reads the daemon installer's manifest/recipes when the JIT seam is wired
    /// (so the teaching and the kick can never disagree) and falls back to the
    /// live shipped data ([`has_blessed_recipe`]) elsewhere.
    fn blessed_recipe_exists(&self, server: &str) -> bool {
        self.auto_install.get().map_or_else(
            || has_blessed_recipe(server),
            |jit| jit.installer.install_target(server).is_some(),
        )
    }

    /// Clears a server's not-installed dedupe and its `not-installed` board
    /// entry when a later spawn demand finds the binary (misc 210).
    ///
    /// Installing the binary heals coverage without a daemon restart: the next
    /// spawn demand re-resolves, and — the binary now present — the spawn
    /// proceeds normally. This clears the stale classification so the fresh
    /// spawn registers clean, and re-arms the finding for a future removal.
    fn clear_not_installed(&self, key: &InstanceKey) {
        let cleared = self
            .not_installed_warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key.server);
        if let Some(writer) = &self.snapshot {
            writer.clear_not_installed(key);
        }
        if cleared {
            debug!(
                source = Source::LspLifecycle.as_str(),
                server = key.server.as_str(),
                scope_root = key.scope.root_path().map(|p| p.display().to_string()),
                "not-installed cleared: {} resolved and will spawn (misc 210)",
                key.server,
            );
        }
    }

    /// Whether server `name`'s not-installed finding has fired this daemon
    /// lifetime (misc 210). Test hook for the once-per-server dedupe.
    #[cfg(test)]
    fn not_installed_was_warned(&self, name: &str) -> bool {
        self.not_installed_warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name)
    }

    /// Whether `server_name`, resolved for `root`, is configured but **not
    /// installed** (misc 210) — the same pre-spawn resolution `spawn_inner`
    /// runs, without spawning.
    ///
    /// Resolves the spawn program exactly as the spawn path does (managed-home
    /// pin first, then PATH) and asks whether that binary is present
    /// ([`crate::health::servers::server_binary_installed`], shim-aware for the
    /// rust-analyzer rustup proxy). Used by
    /// [`Self::unavailable_diagnostic_servers`] so a missing-binary server
    /// degrades a file with a `not-installed` cause instead of a bare
    /// `[no LSP coverage]`. `None` server def (a binding with no resolvable
    /// def) reads as not not-installed here — there is nothing to spawn.
    fn is_server_not_installed(&self, server_name: &str, root: &Path) -> bool {
        let Some(server_def) = self.effective_server_def(server_name, root) else {
            return false;
        };
        let program = crate::managed_home::resolve_spawn_program(
            &crate::managed_home::ManagedHome::resolve(),
            &crate::recipes::active_manifest(),
            server_name,
            &server_def,
            self.config.prefer_managed(),
        );
        // Mirror the spawn-path exemption: a rust-toolchain wrap (rustup present
        // and a toolchain resolves for this root) delegates rust-analyzer
        // resolution to `rustup run`, so a bare PATH lookup is the wrong
        // question — the server is not not-installed here.
        if rust_toolchain::should_wrap(server_name, &program)
            && rust_toolchain::resolve_active_toolchain(root).is_some()
        {
            return false;
        }
        !crate::health::servers::server_binary_installed(server_name, &program)
    }

    /// Extracts `CommandsConfig` from each tracked root's project config.
    ///
    /// Returns a map from root path to the project's `[commands]` section.
    /// Roots without a `[commands]` section are omitted.
    pub fn project_commands(&self) -> HashMap<PathBuf, crate::config::CommandsConfig> {
        self.fs
            .root_views()
            .into_iter()
            .filter_map(|root| {
                root.config()
                    .commands
                    .clone()
                    .map(|cmds| (root.path().to_path_buf(), cmds))
            })
            .collect()
    }

    /// Whether the root's project config sets `disable_lsp` (workstream 34
    /// ticket 00).
    ///
    /// A disabled root stays tracked everywhere else (`roots ls`, build/command
    /// resolution, classification, linters, gate) but is dropped from what
    /// reaches language servers: every spawn path skips it, so navigation
    /// (grep/glob) yields no enrichment and the editing gate stays inert.
    /// Reads the toggle straight off the tracked [`Root`]'s config — the same
    /// map `resolve_root` consults, so a resolvable root is always
    /// config-complete (no spawn race; ticket 00a).
    #[must_use]
    pub fn is_lsp_disabled(&self, root: &Path) -> bool {
        self.fs.root(root).is_some_and(|r| r.config().disable_lsp)
    }

    /// Whether the root's project config sets `disable_diag` (workstream 34
    /// ticket 00).
    ///
    /// A surface suppressor: the editing→`catenary diagnostics` gate and its
    /// output are off for the root, but LSP servers still run for grep/glob.
    #[must_use]
    pub fn is_diag_disabled(&self, root: &Path) -> bool {
        self.fs.root(root).is_some_and(|r| r.config().disable_diag)
    }

    /// Whether the root's project config sets `disable_lint` (workstream 34
    /// ticket 00 / 01).
    ///
    /// A disabled root runs no standalone linters: it stays tracked everywhere
    /// else (LSP, build/command resolution, gate via LSP coverage), but the
    /// linter feeder is dropped for it.
    #[must_use]
    pub fn is_lint_disabled(&self, root: &Path) -> bool {
        self.fs.root(root).is_some_and(|r| r.config().disable_lint)
    }

    /// The effective linter set for a root (workstream 34 ticket 01).
    ///
    /// The user config's `[linter.rule.*]` unioned with the root's project
    /// `[linter.rule.*]`, the project winning on a name collision (so a project entry
    /// can override or `disable` a user-configured linter). Each entry carries
    /// its compiled routing globs, ready for [`LinterConfig::matches`]. The
    /// merge itself is the shared core's
    /// [`merge_effective_linters`](crate::linter::merge_effective_linters) —
    /// the same rule the CLI-side lint router applies (ws43-04), so query-time
    /// and diagnostics-time routing cannot drift.
    ///
    /// [`LinterConfig::matches`]: crate::config::LinterConfig::matches
    #[must_use]
    pub fn effective_linters(&self, root: &Path) -> HashMap<String, crate::config::LinterConfig> {
        let project = self.fs.root(root);
        let empty = HashMap::new();
        let project_linters = project.as_ref().map_or(&empty, |r| &r.config().linter);
        crate::linter::merge_effective_linters(&self.config.linter, project_linters)
    }

    /// The effective language configuration for a `(root, lang)` pair (bug 81 /
    /// misc 155).
    ///
    /// The global resolution ([`Config::resolve_language`] — shipped defaults
    /// overlaid with the user config) with the root's project-layer
    /// `[lsp.language.{lang}]` merged on top, the project winning per field. The
    /// merge is [`LanguageConfig::merge`] — the **same array-replace semantics
    /// the user layer already uses**: a project `servers` list *replaces* the
    /// binding, it never appends, and a project entry for a language the global
    /// layer never defined stands on its own. This is the dispatch-resolution
    /// counterpart to [`Self::effective_linters`] / [`Self::effective_weights`]
    /// (project-layer siblings that already reach dispatch); the language table
    /// was the one layer never consulted, so a project binding drove
    /// classification but never which server spawned or answered.
    ///
    /// Binding names still resolve against the merged server-def set spawn uses
    /// ([`Self::effective_server_def`]), so a project `[lsp.server.*]` def is a
    /// legal binding target. When the project layer contributes an override the
    /// merged `root_markers` are recompiled so [`LanguageConfig::marker_set`]
    /// stays truthful (the `#[serde(skip)]` compiled globs do not travel through
    /// [`LanguageConfig::merge`]).
    ///
    /// Returns `None` only when neither layer defines the language.
    #[must_use]
    pub fn effective_language(&self, root: &Path, lang: &str) -> Option<LanguageConfig> {
        let global = self.config.resolve_language(lang);
        let project = self
            .fs
            .root(root)
            .and_then(|r| r.config().language.get(lang).cloned());

        match (global, project) {
            (Some(global), Some(project)) => {
                let mut merged = global.clone();
                merged.merge(project);
                // `merge` copies `root_markers` but not the `#[serde(skip)]`
                // compiled globs; recompile so `marker_set` reflects a project
                // override. A malformed project glob would already have failed
                // the root's own `compile_markers` at load, so this cannot
                // regress a working config.
                let _ = merged.compile_markers();
                Some(merged)
            }
            (Some(global), None) => Some(global.clone()),
            (None, Some(project)) => Some(project),
            (None, None) => None,
        }
    }

    /// The effective cross-feeder diagnostic weights for a root (linters ticket
    /// 05).
    ///
    /// Built from the seeded code default
    /// ([`DiagnosticWeights::rust_analyzer_default`]) overlaid with the user-level
    /// `[lsp.server.*]` / `[linter.rule.*]` weight fields, then the root's project
    /// `.catenary.toml` overrides (project winning). Consumed per file by the
    /// `catenary diagnostics` cross-feeder reconciliation — the dedup keeper and
    /// the provisional challenge — over the merged set from every feeder.
    ///
    /// `root` is `None` for files outside every workspace root: only the seed +
    /// user layer applies, with no project overrides.
    ///
    /// [`DiagnosticWeights::rust_analyzer_default`]: crate::config::DiagnosticWeights::rust_analyzer_default
    #[must_use]
    pub fn effective_weights(&self, root: Option<&Path>) -> crate::config::DiagnosticWeights {
        let mut weights = crate::config::DiagnosticWeights::rust_analyzer_default();

        // User layer.
        for (name, def) in &self.config.server {
            weights.apply_server_def(name, def);
        }
        for (name, def) in &self.config.linter {
            weights.apply_linter(name, def);
        }

        // Project layer (overrides the user layer per source).
        if let Some(root) = root
            && let Some(r) = self.fs.root(root)
        {
            for (name, def) in &r.config().server {
                weights.apply_server_def(name, def);
            }
            for (name, def) in &r.config().linter {
                weights.apply_linter(name, def);
            }
        }

        weights
    }

    /// Whether a standalone linter covers `file` (workstream 34 ticket 01).
    ///
    /// Resolves `file` to its owning root and matches the **root-relative** path
    /// against that root's effective `[linter.rule.*]` patterns (user ∪ project),
    /// reusing [`LspGlob`]. A linter that declares `shebangs` (e.g. the default
    /// `shellcheck`) additionally covers an extensionless script whose `#!` line
    /// names one of them (ticket 03). Out-of-root files and `disable_lint` roots
    /// are never covered; an entry with `disable = true` or no routing (neither
    /// patterns nor a matching shebang) contributes nothing. This is the routing
    /// predicate behind both the editing-boundary coverage gate and the
    /// diagnostics-batch fan-out.
    #[must_use]
    pub fn lint_covers(&self, file: &Path) -> bool {
        let Some(root) = self.fs.resolve_root(file) else {
            return false;
        };
        if self.is_lint_disabled(&root) {
            return false;
        }
        let Ok(rel) = file.strip_prefix(&root) else {
            return false;
        };
        self.effective_linters(&root)
            .values()
            .any(|linter| !linter.disable && self.fs.linter_routes(linter, file, rel))
    }

    /// Names every diagnostic feeder — LSP server or standalone linter —
    /// configured to track `file`, sorted and deduplicated.
    ///
    /// A config-level projection of the editing-gate coverage predicates
    /// ([`Session::has_lsp_coverage`] + [`Self::lint_covers`]) that returns the
    /// feeders *by name* rather than a bool: an in-root file's LSP feeders are
    /// the configured servers bound to its language (the `has_configured_server`
    /// predicate — instance state is irrelevant, so a cold per-root instance of
    /// a warm language still counts) unless the root turns LSP off; an
    /// out-of-root file's are the single-file servers with a positive cache; a
    /// file's linter feeders are the root's effective `[linter.rule.*]` entries
    /// whose globs match. Every feeder named here would report on the file when
    /// `catenary diagnostics` runs, so a file the gate tracks
    /// (`Session::covered_for_diagnostics`) always yields at least one. The
    /// editing-gate message groups its outstanding files by these names.
    ///
    /// [`Session::has_lsp_coverage`]: crate::bridge::session::Session::has_lsp_coverage
    #[must_use]
    pub fn diagnostic_feeder_names(&self, file: &Path) -> Vec<String> {
        let mut names: BTreeSet<String> = BTreeSet::new();

        let lang = self.fs.language_id(file).or_else(|| {
            file.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });

        match self.fs.resolve_root(file) {
            Some(root) => {
                // In-root LSP feeders: every configured server bound to the
                // language, unless the root turns LSP off. Per-root resolution
                // so a project `[lsp.language.*]` binding reaches the feeder set;
                // a project `[lsp.server.*]` def is a legal binding target.
                if !self.is_lsp_disabled(&root)
                    && let Some(id) = lang.as_deref()
                    && let Some(lc) = self.effective_language(&root, id)
                {
                    for binding in lc.servers() {
                        if self.config.server.contains_key(&binding.name)
                            || self.effective_server_def(&binding.name, &root).is_some()
                        {
                            names.insert(binding.name.clone());
                        }
                    }
                }
                // Linter feeders: the root's effective rules whose globs match
                // the root-relative path.
                if !self.is_lint_disabled(&root)
                    && let Ok(rel) = file.strip_prefix(&root)
                {
                    for (name, linter) in self.effective_linters(&root) {
                        if !linter.disable && linter.matches(rel) {
                            names.insert(name);
                        }
                    }
                }
            }
            None => {
                // Out-of-root LSP feeders: single-file servers with a positive
                // (non-failed) cache entry — mirrors `has_single_file_coverage`.
                if let Some(id) = lang.as_deref()
                    && let Some(lc) = self.config.resolve_language(id)
                {
                    let failures = self
                        .single_file_failures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for binding in lc.servers() {
                        if self
                            .config
                            .server
                            .get(&binding.name)
                            .is_some_and(|def| def.single_file)
                            && !failures.contains(&(id.to_string(), binding.name.clone()))
                        {
                            names.insert(binding.name.clone());
                        }
                    }
                }
            }
        }

        names.into_iter().collect()
    }

    /// Resolves the effective root for a server instance given a file path.
    ///
    /// If the language has active `root_markers`, walks up from `file`
    /// toward `workspace_root` and returns the first directory containing
    /// any marker. Results are cached by `(directory, language_id)`.
    ///
    /// Returns `workspace_root` when:
    /// - The language has no root markers.
    /// - No marker is found within the workspace root.
    fn resolve_server_root(&self, file: &Path, lang: &str, workspace_root: &Path) -> PathBuf {
        let Some(lang_config) = self.effective_language(workspace_root, lang) else {
            return workspace_root.to_path_buf();
        };
        let Some((markers, compiled)) = lang_config.marker_set() else {
            return workspace_root.to_path_buf();
        };

        let dir = if file.is_dir() {
            file.to_path_buf()
        } else {
            file.parent()
                .map_or_else(|| workspace_root.to_path_buf(), Path::to_path_buf)
        };

        let cache_key = (dir, lang.to_string());
        {
            let cache = self
                .marker_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cached) = cache.get(&cache_key) {
                return cached.clone();
            }
        }

        let resolved = resolve_marker_root(file, markers, compiled, workspace_root);

        let mut cache = self
            .marker_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(cache_key, resolved.clone());

        resolved
    }

    /// Spawns LSP servers for languages detected in the workspace.
    ///
    /// Walks workspace roots (respecting `.gitignore`), classifies files via
    /// [`FilesystemManager`], and spawns servers for configured languages
    /// that have matching files. Servers that fail to spawn are logged and
    /// skipped — a misconfigured server should not prevent others from starting.
    ///
    /// Spawns a separate `Scope::Root` instance per root. Unrelated
    /// projects never share an LSP server.
    pub async fn spawn_all(&self) {
        let roots = self.fs.roots();

        // Each tracked root already carries its `.catenary.toml` config +
        // classification (loaded at birth, ticket 00a) — no config loading
        // here. Surface any orphan `[lsp.server.*]` entries while we hold every
        // root's config.
        for root in self.fs.root_views() {
            crate::config::validate::warn_orphan_project_servers(
                root.config(),
                &self.config,
                root.path(),
            );
        }

        let configured_keys: HashSet<&str> =
            self.config.language.keys().map(String::as_str).collect();

        // Detect languages per root and spawn only the languages each
        // root actually contains. A flat union across all roots would
        // leak markerless languages (no `root_markers`, e.g. julia,
        // bash, yaml) into roots that have no files of that language —
        // a language detected in one served root would spawn a server
        // in every served root.
        for root in &roots {
            // `disable_lsp` roots stay tracked but never reach a language
            // server (ticket 00).
            if self.is_lsp_disabled(root) {
                continue;
            }

            let detected = self
                .fs
                .detect_workspace_languages(std::slice::from_ref(root), &configured_keys);

            if detected.is_empty() {
                continue;
            }

            let mut sorted: Vec<&str> = detected.iter().map(String::as_str).collect();
            sorted.sort_unstable();
            info!(
                "Detected languages in {}: {}",
                root.display(),
                sorted.join(", ")
            );

            for lang in &detected {
                let Some(lang_config) = self.effective_language(root, lang) else {
                    continue;
                };

                // If the language has root markers but this root doesn't
                // contain any, defer to lazy spawn on first need.
                if let Some((markers, compiled)) = lang_config.marker_set()
                    && !dir_has_marker(root, markers, compiled)
                {
                    debug!(
                        language = lang.as_str(),
                        "No root marker at {} — deferring to lazy spawn",
                        root.display(),
                    );
                    continue;
                }

                for binding in lang_config.servers() {
                    if let Err(e) = self.ensure_server(lang, &binding.name, root).await {
                        // A not-installed server already surfaced its one calm
                        // finding in `spawn_inner` (misc 210); a second generic
                        // spawn-fail warn here would just fight it.
                        if is_not_installed(&e) {
                            continue;
                        }
                        warn!(
                            source = Source::LspLifecycle.as_str(),
                            language = lang.as_str(),
                            server = binding.name.as_str(),
                            scope_root = %root.display(),
                            "Failed to spawn LSP server for {lang} at {}: {e}",
                            root.display(),
                        );
                    }
                }
            }
        }
    }

    /// Returns whether any server for this language is single-file
    /// **diagnostics** coverage for out-of-root files.
    ///
    /// Used by the hook layer to decide whether out-of-root edits
    /// should be gated by `start_editing`. Servers that failed at
    /// runtime (negative cache) are excluded.
    ///
    /// Two legs count (brackets 01):
    /// - the manifest's verified `single_file = "serves-diagnostics"` claim
    ///   ([`crate::recipes::SingleFileSupport::serves_diagnostics`]) — the
    ///   maintainer ruling: the servers that get stray-file diagnostics are
    ///   the ones that can serve them. An `enrichment-only` server may spawn
    ///   rootless but must never arm this gate;
    /// - the pre-existing user-scope `single_file = true` config opt-in
    ///   (`[lsp.server.*]`), unchanged.
    #[must_use]
    pub fn has_single_file_coverage(&self, lang: &str) -> bool {
        let Some(lang_config) = self.config.resolve_language(lang) else {
            return false;
        };
        let failures = self
            .single_file_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lang_config.servers().iter().any(|binding| {
            let Some(def) = self.config.server.get(&binding.name) else {
                return false;
            };
            // Only a BLESSED single-file server is diagnostics coverage
            // (diagnostics-debt 04b): an unverified server is enrichment-only and
            // never a diagnostics source, so it must not arm the gate.
            (def.single_file
                || crate::lsp::server_behavior::ServerProfile::for_server(&binding.name)
                    .single_file()
                    .serves_diagnostics())
                && server_is_blessed(&binding.name)
                && !failures.contains(&(lang.to_string(), binding.name.clone()))
        })
    }

    /// Names the candidate single-file server bindings for `lang` and `path`
    /// — the sweep tier's config-level projection (brackets 04).
    ///
    /// Mirrors the tier-3 filters of `get_servers` (single-file servers
    /// resolve their binding globally — there is no project layer to
    /// consult): the language's configured bindings, minus method-disabled
    /// ones for `method`, minus bindings with no user-scope server def, minus
    /// file-pattern mismatches. Purely config-level by design: the rootless
    /// spawn gate (manifest capability / config opt-in / negative cache) is
    /// applied fail-closed inside
    /// [`Self::with_single_file_bracket`], which answers `None` for an
    /// unqualified name — so a caller iterates these names and the bracket
    /// seam decides capability.
    #[must_use]
    pub fn single_file_binding_names(
        &self,
        lang: &str,
        path: &Path,
        method: Option<DispatchMethod>,
    ) -> Vec<String> {
        let Some(lang_config) = self.config.resolve_language(lang) else {
            return Vec::new();
        };
        lang_config
            .servers()
            .iter()
            .filter(|binding| !method.is_some_and(|m| binding.is_method_disabled(m)))
            .filter(|binding| {
                self.config
                    .server
                    .get(&binding.name)
                    .is_some_and(|def| file_matches_patterns(path, &def.compiled_patterns))
            })
            .map(|binding| binding.name.clone())
            .collect()
    }

    /// Resolves the server binding the rootless single-file DIAGNOSTICS serve
    /// uses for a markerless `path` of language `lang` (brackets 03).
    ///
    /// The per-binding form of [`Self::has_single_file_coverage`] — the same
    /// qualification legs (verified `serves-diagnostics` capability or the
    /// user-scope `single_file = true` opt-in, blessed, not negative-cached),
    /// plus the binding's file-pattern gate the tier-3 dispatch applies
    /// ([`get_servers`]'s `compiled_patterns` check) — returning the first
    /// qualifying binding's `(language, server)` pair for
    /// [`Self::with_single_file_bracket`]. `None` means no server serves
    /// single-file diagnostics for this language: the serve then answers with
    /// the honest disclosure instead of a mount or a refusal.
    #[must_use]
    pub fn single_file_diagnostics_server(&self, lang: &str, path: &Path) -> Option<String> {
        let lang_config = self.config.resolve_language(lang)?;
        let failures = self
            .single_file_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lang_config
            .servers()
            .iter()
            .find(|binding| {
                let Some(def) = self.config.server.get(&binding.name) else {
                    return false;
                };
                if !file_matches_patterns(path, &def.compiled_patterns) {
                    return false;
                }
                (def.single_file
                    || crate::lsp::server_behavior::ServerProfile::for_server(&binding.name)
                        .single_file()
                        .serves_diagnostics())
                    && server_is_blessed(&binding.name)
                    && !failures.contains(&(lang.to_string(), binding.name.clone()))
            })
            .map(|binding| binding.name.clone())
    }

    /// The first server binding configured for `lang` regardless of
    /// single-file capability — the name the rootless serve's disclosure line
    /// carries when [`Self::single_file_diagnostics_server`] finds no
    /// qualifying binding (brackets 03: an `enrichment-only` / `unsupported`
    /// server still gets named in the honest answer).
    #[must_use]
    pub fn first_bound_server(&self, lang: &str) -> Option<String> {
        self.config
            .resolve_language(lang)?
            .servers()
            .first()
            .map(|binding| binding.name.clone())
    }

    /// Returns whether any **blessed** server is configured for this language in
    /// `root` — the diagnostics coverage gate (diagnostics-debt 04b).
    ///
    /// Used by the editing-boundary gate to decide whether an in-root edit
    /// has diagnostics coverage. Unlike [`Self::has_single_file_coverage`], this
    /// does not require `single_file` mode or a running instance — it reports
    /// purely config-level coverage. A configured but cold per-root instance
    /// still counts as covered (granularity Decision 3): a warm language's
    /// in-root file must not be silently dropped just because no instance has
    /// spawned yet. Files whose language has no `servers` binding —
    /// classification-only entries, or types absent from every `[lsp.language.*]`
    /// table (`.txt`, logs, data/scratch files) — return `false`, so
    /// non-served in-root edits flow free.
    ///
    /// **Only a blessed server binding counts** (DESIGN §"The blessed set"): an
    /// unverified custom `[lsp.server.*]` def is enrichment-only, so a file whose
    /// *only* covering server is unverified has no diagnostics coverage — the gate
    /// never arms for it and its receipt bucket renders `[not diagnostics-covered]`.
    /// A file also covered by a blessed server or a linter is still covered.
    ///
    /// Resolves the binding per-root ([`Self::effective_language`]), so a project
    /// `[lsp.language.*]` rebinding decides coverage; a project `[lsp.server.*]`
    /// def is a legal binding target ([`Self::effective_server_def`]).
    #[must_use]
    pub fn has_configured_server(&self, root: &Path, lang: &str) -> bool {
        let Some(lang_config) = self.effective_language(root, lang) else {
            return false;
        };
        lang_config.servers().iter().any(|binding| {
            server_is_blessed(&binding.name)
                && (self.config.server.contains_key(&binding.name)
                    || self.effective_server_def(&binding.name, root).is_some())
        })
    }

    /// Returns whether this language's *only* configured, defined servers in
    /// `root` are **unverified** — enrichment-only, so a diagnostics server
    /// exists but Catenary withholds its diagnostics (diagnostics-debt 04b).
    ///
    /// The complement of [`Self::has_configured_server`] within the
    /// server-is-defined set: at least one binding resolves to a real server def,
    /// and **none** of the defined bindings is blessed. This is the signal for the
    /// "not diagnostics-covered" skip bucket (DESIGN §"The blessed set" footgun
    /// ruling) — distinct from the truly-uncovered case (no server defined at
    /// all), because here a server *does* exist; the wording must declare that,
    /// never blame Catenary. Returns `false` when a blessed server also covers the
    /// language (that file is genuinely covered, not unverified-only).
    #[must_use]
    pub fn has_unverified_only_server(&self, root: &Path, lang: &str) -> bool {
        let Some(lang_config) = self.effective_language(root, lang) else {
            return false;
        };
        let mut any_defined = false;
        for binding in lang_config.servers() {
            let defined = self.config.server.contains_key(&binding.name)
                || self.effective_server_def(&binding.name, root).is_some();
            if !defined {
                continue;
            }
            if server_is_blessed(&binding.name) {
                // A blessed server covers the language ⇒ genuinely covered,
                // never unverified-only.
                return false;
            }
            any_defined = true;
        }
        any_defined
    }

    /// Returns the current workspace roots.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.fs.roots()
    }

    /// Removes a workspace root and shuts down all server instances
    /// bound to it.
    ///
    /// # Errors
    ///
    /// Returns an error if the root path cannot be converted to a valid URI.
    pub async fn remove_root(&self, root: &Path) -> Result<()> {
        // Re-install the surviving roots (config-complete `Root`s), dropping the
        // removed one. Its per-root config + classification leave with it (they
        // live on the `Root`); a bare `set_roots` would have erased the kept
        // roots' configs too.
        let kept: Vec<Arc<Root>> = self
            .fs
            .root_views()
            .into_iter()
            .filter(|r| r.path() != root)
            .collect();
        self.fs.set_roots_rich(kept);

        // Drop the changed-set baseline and generation counter for the removed
        // root (path-keyed caches NOT folded onto `Root`; same leak/staleness
        // reasons as the sync_roots cleanup).
        self.fs.remove_root_baseline(root);

        // Shut down per-root instances bound to the removed root.
        self.shutdown_root_instances(root).await;

        Ok(())
    }

    /// Synchronizes workspace roots with a new set.
    ///
    /// Diffs against current roots: adds new ones, removes stale ones.
    /// Removed roots have their per-root instances shut down. Added
    /// roots get new `Scope::Root` instances spawned for languages
    /// that already have active instances.
    ///
    /// Returns every root whose per-root state was dropped: the set diff
    /// (old set − new set) plus any orphaned instance scope no new root
    /// covers (misc 183) — so the caller can react to removal without
    /// recomputing the diff. `Session::sync_roots` uses it as the single
    /// source of truth for evicting per-root `SymbolIndex` entries (bug #36).
    ///
    /// `new_roots` are config-complete [`Root`]s (loaded at birth by the
    /// tracker, ticket 00a): installing them via `fs.set_roots_rich` makes each
    /// path resolvable and its config readable in one atomic swap, so there is
    /// no "load config before `set_roots`" reorder — a resolvable root is never
    /// observed without its config (the `disable_lsp` spawn race is gone by
    /// construction).
    ///
    /// # Errors
    ///
    /// Returns an error if any root path cannot be converted to a valid URI.
    pub async fn sync_roots(&self, new_roots: Vec<Arc<Root>>) -> Result<Vec<PathBuf>> {
        // Snapshot the old paths, then compute the diff against it. The diff
        // uses the snapshot (not `fs.roots()`), so `fs.set_roots_rich` can run
        // later.
        let current_roots = self.fs.roots();
        let new_paths: Vec<PathBuf> = new_roots.iter().map(|r| r.path().to_path_buf()).collect();

        let to_add: Vec<PathBuf> = new_paths
            .iter()
            .filter(|r| !current_roots.contains(r))
            .cloned()
            .collect();
        let to_remove: Vec<PathBuf> = current_roots
            .iter()
            .filter(|r| !new_paths.contains(r))
            .cloned()
            .collect();

        // Install the config-complete roots atomically. Each `Root` carries its
        // config + classification, so this single swap makes a root resolvable
        // and config-readable together — no separate prime step, no reorder
        // (ticket 00a).
        self.fs.set_roots_rich(new_roots);

        // Reconcile actual instances against the new set, not just the
        // bookkeeping diff (misc 183): an instance whose scope no new root
        // covers was spawned while its root was uninstalled (a request racing
        // the expiry sweep) or sits under a removed ancestor — either way the
        // set diff can never name it again, and it would hold its server
        // processes until daemon restart. Sweeping here restores the invariant
        // at every sync: no installed root ⇒ no per-root instances.
        let orphaned = self.orphaned_root_scopes(&new_paths, &to_remove).await;

        if to_add.is_empty() && to_remove.is_empty() && orphaned.is_empty() {
            return Ok(to_remove);
        }

        info!(
            "Syncing roots: {} added, {} removed, {} orphaned",
            to_add.len(),
            to_remove.len(),
            orphaned.len(),
        );

        let set_changed = !to_add.is_empty() || !to_remove.is_empty();
        if set_changed {
            // Clear marker cache — root boundaries changed. (An orphan-only
            // sweep leaves the boundaries alone, so the cache stays.)
            self.marker_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }

        // Removed roots dropped their config + classification with the
        // `set_roots_rich` swap above; here drop the path-keyed caches that are
        // NOT folded onto `Root` so removed-root entries don't accumulate (a
        // leak) and a later re-mount diffs against a fresh baseline (cold-start
        // full set). Orphaned scopes get the same cleanup — their entries are
        // exactly as unreachable.
        for removed in to_remove.iter().chain(orphaned.iter()) {
            self.fs.remove_root_baseline(removed);
        }

        // Shut down per-root instances for removed roots and orphaned scopes.
        for removed in to_remove.iter().chain(orphaned.iter()) {
            self.shutdown_root_instances(removed).await;
        }

        if set_changed {
            // Shut down single-file servers and clear the cache — root
            // changes may have brought previously-unrooted files into scope
            // of per-root instances. Single-file servers are lazily
            // re-spawned on the next request if still needed.
            self.shutdown_single_file_instances().await;
        }

        // Spawn instances for added roots.
        if !to_add.is_empty() {
            self.spawn_for_added_roots(&to_add).await;
        }

        // Removed and orphaned both left tracked state; hand the caller the
        // union so per-root cache eviction (bug #36) covers orphans too.
        let mut dropped = to_remove;
        dropped.extend(orphaned);
        Ok(dropped)
    }

    /// Distinct `Scope::Root` scope paths among live instances that no root
    /// in `new_paths` covers (component-prefix), excluding those `to_remove`
    /// already names (the set diff orders their shutdown).
    ///
    /// These are misc-183 orphans: instances spawned for a root that was
    /// concurrently uninstalled, or scoped under a removed ancestor root —
    /// shapes the old−new set diff structurally cannot see.
    async fn orphaned_root_scopes(
        &self,
        new_paths: &[PathBuf],
        to_remove: &[PathBuf],
    ) -> Vec<PathBuf> {
        let clients = self.clients.lock().await;
        let mut orphaned: Vec<PathBuf> = clients
            .keys()
            .filter_map(|k| match &k.scope {
                Scope::Root(r)
                    if !new_paths.iter().any(|p| r.starts_with(p)) && !to_remove.contains(r) =>
                {
                    Some(r.clone())
                }
                _ => None,
            })
            .collect();
        drop(clients);
        orphaned.sort_unstable();
        orphaned.dedup();
        orphaned
    }

    /// Returns clients for a file path, filtered by capability,
    /// `file_patterns`, and `disabled_methods`, in priority order
    /// (from the `servers` list in `[lsp.language.*]`).
    ///
    /// Resolves language from path via `FilesystemManager`, iterates
    /// the binding's servers, filters by:
    /// 1. `disabled_methods` on the binding (per-binding suppression)
    /// 2. `file_patterns` on `[lsp.server.*]` (filename-level glob)
    /// 3. The given capability check
    ///
    /// `method` is the [`DispatchMethod`] being dispatched. Pass
    /// `None` when the caller has its own suppression mechanism
    /// (e.g., diagnostic dispatch uses the `diagnostics` flag).
    ///
    /// Returns an empty Vec when no server matches. On empty result,
    /// emits a `debug!()` with the file path.
    ///
    /// A dead per-root instance on a still-live root is not skipped forever
    /// (misc 167): the lookup collects it as a revive candidate, attempts one
    /// strike-gated [`Self::revive_server`] pass, and re-runs the lookup so a
    /// revived server answers this very demand. A benched instance
    /// ([`ReviveVerdict`]) stays down — its tombstone remains as evidence for
    /// the diagnostics degradation surface. A retired root never reaches the
    /// revive pass: retirement removes its instances from the map and clears
    /// its ledger entries (bug 93).
    ///
    /// Does not block on server readiness — callers must call
    /// `wait_ready_for_path` or `wait_ready_all` before invoking.
    pub async fn get_servers(
        &self,
        path: &Path,
        capability: fn(&LspServer) -> bool,
        method: Option<DispatchMethod>,
    ) -> Vec<Arc<Mutex<LspClient>>> {
        let mut revive_attempted = false;
        loop {
            let (result, dead) = self.collect_servers(path, capability, method).await;
            if dead.is_empty() || revive_attempted {
                return result;
            }
            revive_attempted = true;
            for key in dead {
                // Best-effort: a failed or gated revive leaves the binding
                // skipped on the second pass, exactly as before misc 167.
                let _ = self.revive_server(&key).await;
            }
        }
    }

    /// One lookup pass for [`Self::get_servers`]: the matching live clients in
    /// priority order, plus the instance keys of dead-but-present per-root
    /// instances the demand routed to (the misc 167 revive candidates).
    ///
    /// Two phases: the registry lock is held only to snapshot candidate
    /// instances — never across a client-lock await. A client mutex can be
    /// held for a full diagnose batch (settle included), and awaiting one
    /// under the registry guard convoyed every manager lookup daemon-wide
    /// behind a single busy server (bug 104).
    #[allow(
        clippy::too_many_lines,
        reason = "workspace folder fallback adds branches but logic is linear"
    )]
    async fn collect_servers(
        &self,
        path: &Path,
        capability: fn(&LspServer) -> bool,
        method: Option<DispatchMethod>,
    ) -> (Vec<Arc<Mutex<LspClient>>>, Vec<InstanceKey>) {
        // Detect language: primary (FilesystemManager) then fallback (raw extension).
        let Some(lang_id) = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }) else {
            return (Vec::new(), Vec::new());
        };

        // Resolve owning workspace root. If unrooted, fall through to
        // tier 3 (single-file servers).
        if let Some(root) = self.fs.resolve_root(path) {
            // Tiers 1–2: rooted file lookup. Resolve the binding per-root so a
            // project `[lsp.language.*]` rebinding decides dispatch.
            let Some(lang_config) = self.effective_language(&root, &lang_id) else {
                return (Vec::new(), Vec::new());
            };
            // Resolve marker root once for all servers in this language.
            let resolved = self.resolve_server_root(path, &lang_id, &root);

            // Phase 1 — registry snapshot: resolve each binding to its
            // candidate instance under the registry lock, awaiting no client
            // lock. The `bool` marks a workspace-root fallback whose folder
            // capability is checked in phase 2, after the guard drops.
            let mut candidates: Vec<(String, Arc<Mutex<LspClient>>, bool)> = Vec::new();
            {
                let clients = self.clients.lock().await;
                for binding in lang_config.servers() {
                    let skip = |reason: &str| {
                        debug!(
                            source = Source::LspDispatch.as_str(),
                            server = binding.name.as_str(),
                            "get_servers: skipped {}: {reason}",
                            binding.name,
                        );
                    };
                    if method.is_some_and(|m| binding.is_method_disabled(m)) {
                        skip("method disabled");
                        continue;
                    }
                    // A project `[lsp.server.*]` def is a legal binding target, so
                    // resolve the def per-root rather than user-config only.
                    let Some(server_def) = self.effective_server_def(&binding.name, &root) else {
                        skip("server def not found");
                        continue;
                    };
                    if !file_matches_patterns(path, &server_def.compiled_patterns) {
                        skip("file_patterns mismatch");
                        continue;
                    }
                    if let Some(c) = find_instance(&clients, &lang_id, &binding.name, &resolved) {
                        candidates.push((binding.name.clone(), c, false));
                    } else if resolved != root {
                        // No instance at marker root — a workspace-root
                        // instance is the fallback if it supports workspace
                        // folders (checked in phase 2).
                        if let Some(ws) = find_instance(&clients, &lang_id, &binding.name, &root) {
                            candidates.push((binding.name.clone(), ws, true));
                        } else {
                            skip(&format!("no instance for root {}", resolved.display()));
                        }
                    } else {
                        debug!(
                            source = Source::LspDispatch.as_str(),
                            server = binding.name.as_str(),
                            "get_servers: skipped {}: no instance for root {}",
                            binding.name,
                            resolved.display(),
                        );
                    }
                }
            }

            // Phase 2 — per-client checks with the registry guard dropped:
            // waiting on a busy candidate stalls only this lookup, never the
            // registry.
            let mut result = Vec::new();
            let mut dead: Vec<InstanceKey> = Vec::new();
            for (name, client, ws_fallback) in candidates {
                let skip = |reason: &str| {
                    debug!(
                        source = Source::LspDispatch.as_str(),
                        server = name.as_str(),
                        "get_servers: skipped {name}: {reason}",
                    );
                };
                let locked = client.lock().await;
                if ws_fallback && !locked.supports_workspace_folders() {
                    skip("no instance for marker root, workspace instance not folder-capable");
                    continue;
                }
                if !locked.is_alive() || locked.lifecycle().is_terminal() {
                    // Dead-but-present on a live root: a misc 167 revive
                    // candidate (the terminal-but-lingering `Failed` class
                    // included — a fresh spawn renegotiates capabilities).
                    if let Some(key) = locked.server().key() {
                        dead.push(key);
                    }
                    skip("server not alive");
                    continue;
                }
                if !capability(locked.server()) {
                    skip("capability not supported");
                    continue;
                }
                drop(locked);
                result.push(client);
            }

            if result.is_empty() && !lang_config.servers().is_empty() {
                debug!(
                    source = Source::LspDispatch.as_str(),
                    language = lang_id.as_str(),
                    "No server supports the requested capability for {lang_id} file: {}",
                    path.display(),
                );
            }

            return (result, dead);
        }

        // Tier 3: single-file servers for unrooted files. Unrooted ⇒ no project
        // layer to consult, so the binding resolves globally. Out of misc 167
        // scope: single-file instances keep their own negative cache and are
        // never revive candidates.
        let Some(lang_config) = self.config.resolve_language(&lang_id) else {
            return (Vec::new(), Vec::new());
        };
        let mut result = Vec::new();
        for binding in lang_config.servers() {
            if method.is_some_and(|m| binding.is_method_disabled(m)) {
                continue;
            }
            let Some(server_def) = self.config.server.get(&binding.name) else {
                continue;
            };
            if !file_matches_patterns(path, &server_def.compiled_patterns) {
                continue;
            }
            let Some(client) = self
                .ensure_single_file_server(&lang_id, &binding.name)
                .await
            else {
                continue;
            };
            let locked = client.lock().await;
            if !locked.is_alive() || !capability(locked.server()) {
                continue;
            }
            drop(locked);
            result.push(client);
        }

        (result, Vec::new())
    }

    /// Waits for every server bound to this path's language binding.
    ///
    /// Resolves language from path, iterates all servers in the
    /// binding, waits for each to reach Ready or terminal state.
    /// Dead servers don't block — they return immediately. Servers
    /// that haven't been spawned yet are skipped (not spawned).
    /// Unrooted files wait on single-file servers (tier 3).
    pub async fn wait_ready_for_path(&self, path: &Path) {
        // Detect language: primary (FilesystemManager) then fallback (raw extension).
        let Some(lang_id) = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }) else {
            return;
        };

        // Collect matching instances under the lock, then release before
        // waiting. No client lock is awaited under the registry guard (bug
        // 104): a workspace-root fallback's folder capability is checked in
        // the wait loop below, after the guard drops. The `bool` marks such a
        // fallback.
        #[allow(
            clippy::option_if_let_else,
            reason = "if/else clearer than map_or_else here"
        )]
        let to_wait: Vec<(Arc<Mutex<LspClient>>, bool)> = {
            let clients = self.clients.lock().await;
            if let Some(root) = self.fs.resolve_root(path) {
                // Tiers 1–2: rooted file. Resolve the binding per-root so a
                // project `[lsp.language.*]` rebinding is waited on.
                let Some(lang_config) = self.effective_language(&root, &lang_id) else {
                    return;
                };
                // Use resolve_server_root to match the instance key used
                // by ensure_clients_for_paths and get_servers.
                let resolved = self.resolve_server_root(path, &lang_id, &root);
                let mut instances = Vec::new();
                for binding in lang_config.servers() {
                    if let Some(c) = find_instance(&clients, &lang_id, &binding.name, &resolved) {
                        instances.push((c, false));
                    } else if resolved != root
                        && let Some(ws) = find_instance(&clients, &lang_id, &binding.name, &root)
                    {
                        // No instance at marker root — fall back to the
                        // workspace-root instance if it supports workspace
                        // folders (checked after the guard drops).
                        instances.push((ws, true));
                    }
                }
                instances
            } else {
                // Tier 3: single-file servers. Unrooted ⇒ global binding.
                let Some(lang_config) = self.config.resolve_language(&lang_id) else {
                    return;
                };
                lang_config
                    .servers()
                    .iter()
                    .filter_map(|binding| {
                        let sf_key = InstanceKey::new(
                            lang_id.clone(),
                            binding.name.clone(),
                            Scope::SingleFile,
                        );
                        clients.get(&sf_key).cloned().map(|c| (c, false))
                    })
                    .collect()
            }
        };

        for (client_mutex, ws_fallback) in to_wait {
            let locked = client_mutex.lock().await;
            if ws_fallback && !locked.supports_workspace_folders() {
                continue;
            }
            locked.wait_ready().await;
            drop(locked);
        }
    }

    /// Waits for every active instance across all bindings.
    ///
    /// Clones the client map, waits for each to reach Ready or
    /// terminal state. Dead servers return immediately.
    pub async fn wait_ready_all(&self) {
        let clients = self.clients.lock().await.clone();
        for (_key, client_mutex) in clients {
            client_mutex.lock().await.wait_ready().await;
        }
    }

    /// Spawns missing servers for the given paths and waits for
    /// the relevant servers to be ready.
    ///
    /// Combines [`ensure_clients_for_paths`](Self::ensure_clients_for_paths)
    /// (spawn) with per-path [`wait_ready_for_path`](Self::wait_ready_for_path).
    /// Closes the lazy-spawn gap: after this call, all servers for the
    /// discovered languages are Ready (or terminal). Only waits for
    /// servers bound to the given paths — unrelated servers are not blocked on.
    pub async fn ensure_and_wait_for_paths(&self, paths: &[PathBuf]) {
        self.ensure_clients_for_paths(paths).await;
        for path in paths {
            self.wait_ready_for_path(path).await;
        }
    }

    /// [`ensure_and_wait_for_paths`](Self::ensure_and_wait_for_paths) bounded by
    /// `budget` — the query-path variant (misc 197 stage 1).
    ///
    /// `grep`/`glob` enrichment must never go silent under a wedged/busy
    /// settle or a slow spawn. The WHOLE ensure — the cold-spawn/`initialize`
    /// handshake AND the per-path readiness wait — is raced against `budget`.
    /// On timeout the call returns and the query proceeds with whatever servers
    /// are ready: the results are already complete (ripgrep/readdir produced
    /// them), only enrichment degrades — an un-ready file falls through the
    /// existing could-not-enrich arm (grep's `#?` anchor, glob's missing
    /// outline).
    ///
    /// Bounding the spawn leg too is safe: a dropped cold-spawn future clears
    /// its marker via the [`SpawnMarkerGuard`] (misc 191), so a later query
    /// retries the handshake fresh rather than finding a wedged key. The trip is
    /// a `debug` breadcrumb, not a `warn!`/`error!`: a slow settle/spawn is
    /// expected, not an actionable break, and the degradation already shows in
    /// the receipt.
    pub async fn ensure_and_wait_for_paths_bounded(&self, paths: &[PathBuf], budget: Duration) {
        let ensure = self.ensure_and_wait_for_paths(paths);
        if tokio::time::timeout(budget, ensure).await.is_err() {
            debug!(
                source = Source::LspLifecycle.as_str(),
                budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                paths = paths.len(),
                "query enrichment ensure hit its bound — serving results unenriched (misc 197)",
            );
        }
    }

    /// Spawns with capability-driven scope (no project-scope check).
    ///
    /// Production code should use [`Self::ensure_server`] which handles
    /// project-scope routing. This wrapper exists for tests that need
    /// explicit scope control.
    #[cfg(test)]
    async fn spawn(
        &self,
        server_name: &str,
        lang: &str,
        root: &Path,
    ) -> Result<(InstanceKey, Arc<Mutex<LspClient>>)> {
        self.spawn_inner(server_name, lang, root, false).await
    }

    /// Spawns a project-scoped server instance using the effective
    /// (merged) `ServerDef` for the root.
    ///
    /// Production code uses [`Self::ensure_server`] which handles
    /// project-scope detection internally. This wrapper exists for
    /// tests that need explicit project-scoped spawning.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No effective server def can be computed for this server+root.
    /// - The server fails to spawn or initialize.
    #[cfg(test)]
    async fn spawn_project_scoped(
        &self,
        server_name: &str,
        lang: &str,
        root: &Path,
    ) -> Result<(InstanceKey, Arc<Mutex<LspClient>>)> {
        self.spawn_inner(server_name, lang, root, true).await
    }

    /// Spawn-or-await for `key` — the shared cold-spawn gate (misc 191,
    /// extracted per misc 208): loops until the registry holds an entry the
    /// caller can use ([`SpawnClaim::Found`]) or this task claims the
    /// in-flight marker and owns the cold spawn ([`SpawnClaim::Owner`]).
    ///
    /// The registry lock is held only for the found-check (`find`, a pure
    /// lookup over the guarded map — it must not await) and the marker
    /// lookup/insert — never across a process spawn or `initialize`
    /// handshake, so a cold spawn stalls nothing that doesn't depend on this
    /// key specifically. Three outcomes per iteration:
    ///
    ///   found      → return the entry (the caller checks liveness AFTER
    ///                this returns — no registry guard is held by then),
    ///   marker set → another task owns this key's spawn; wait its Notify,
    ///                then loop to re-check (never a duplicate spawn),
    ///   no marker  → claim the marker and return as the owner.
    ///
    /// The wait's wake-safety: `enable()` arms the `Notified` future
    /// (registers as a waiter) without awaiting, and the owner's guard-drop
    /// removes the marker and calls `notify_waiters` under the SAME std lock.
    /// Doing the presence-check AND the `enable()` under one lock hold means:
    /// if the marker is still present, the owner has not notified yet (notify
    /// follows remove, both under the lock we hold), so our registration is
    /// guaranteed to catch the coming wake; if the marker is gone, the owner
    /// already finished and we drop straight through to re-check the
    /// registry. Either way there is no missed-wake hang. Lock order stays
    /// strictly `clients` → `spawning`.
    async fn claim_spawn<F>(&self, key: &InstanceKey, find: F) -> SpawnClaim<'_>
    where
        F: Fn(&HashMap<InstanceKey, Arc<Mutex<LspClient>>>) -> Option<Arc<Mutex<LspClient>>>,
    {
        loop {
            let clients = self.clients.lock().await;

            if let Some(found) = find(&clients) {
                drop(clients);
                return SpawnClaim::Found(found);
            }

            // Marker decision, atomic with the found-check above (both under
            // the registry guard). Either claim the key and spawn as its
            // owner, or wait on the marker another task already holds — never
            // a second spawn of the same key.
            let notify = self
                .spawning
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
                .cloned();

            if let Some(notify) = notify {
                let notified = notify.notified();
                tokio::pin!(notified);
                let still_pending = {
                    let spawning = self
                        .spawning
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if spawning.contains_key(key) {
                        // Register under the lock — the owner cannot notify
                        // until we release it.
                        notified.as_mut().enable();
                        true
                    } else {
                        // Owner finished between our clone and this recheck.
                        false
                    }
                };
                drop(clients);
                if still_pending {
                    notified.await;
                }
                // Loop: re-check the registry (fresh instance, tombstone, or
                // a cleared key to claim).
                continue;
            }

            // No marker: claim the key. Concurrent claimants serialize on the
            // std lock, so exactly one wins the insert; a loser gets back the
            // winner's marker and waits on it next iteration.
            let ours = Arc::new(tokio::sync::Notify::new());
            let claimed = self
                .spawning
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key.clone())
                .or_insert_with(|| ours.clone())
                .clone();
            drop(clients);
            if Arc::ptr_eq(&claimed, &ours) {
                return SpawnClaim::Owner(SpawnMarkerGuard {
                    spawning: &self.spawning,
                    key: key.clone(),
                    notify: ours,
                });
            }
            // Lost the claim race — loop and wait on the winner's marker.
        }
    }

    /// Shared spawn implementation.
    ///
    /// Every instance gets `Scope::Root(root)`. `project_scoped`
    /// controls server def resolution (merged vs user-level) but
    /// does not affect scope selection.
    #[allow(clippy::too_many_lines, reason = "spawn + tombstone path")]
    async fn spawn_inner(
        &self,
        server_name: &str,
        lang: &str,
        root: &Path,
        project_scoped: bool,
    ) -> Result<(InstanceKey, Arc<Mutex<LspClient>>)> {
        // Defensive leg (bug 93): never spawn a per-root server against a root
        // whose directory is gone. A landed/removed worktree that slipped
        // retirement would otherwise have the language server spawned with a
        // deleted `cwd` — the process dies instantly and the flagship server's
        // status wears a phantom `initialize failed` routed by a path that can
        // route nothing again. Refuse before spawn with a `root gone — retired`
        // reason (debug, not error: this is expected convergence, not an
        // actionable break — `error!` would fire a desktop notification).
        if !root.exists() {
            debug!(
                source = Source::LspLifecycle.as_str(),
                server = server_name,
                scope_root = %root.display(),
                "root gone — retired: skipping per-root spawn for a removed directory",
            );
            anyhow::bail!("root gone — retired: {}", root.display());
        }

        // The strike gate (misc 167): a benched instance is out — every spawn
        // path (first spawn, demand revive, `ensure_clients_for_paths` retry)
        // funnels through here, so the bench holds no matter which surface
        // asks. Cleared by root retirement or daemon restart. A retired root
        // never reaches this point (the gone-root bail above and the ledger
        // clear in `shutdown_root_instances` both precede it).
        let ledger_key = InstanceKey::new(
            lang.to_string(),
            server_name.to_string(),
            Scope::Root(root.to_path_buf()),
        );
        if !self.revive_verdict(&ledger_key).is_revivable() {
            debug!(
                source = Source::LspLifecycle.as_str(),
                server = server_name,
                scope_root = %root.display(),
                "Not spawning {server_name}: struck out (misc 167)",
            );
            anyhow::bail!(
                "{server_name} ({lang}) struck out after repeated failures — \
                 benched until daemon restart or root remount"
            );
        }

        let server_def = if project_scoped {
            self.effective_server_def(server_name, root)
                .ok_or_else(|| {
                    anyhow!(
                        "No effective server def for '{server_name}' at {}",
                        root.display()
                    )
                })?
        } else {
            self.config
                .server
                .get(server_name)
                .ok_or_else(|| {
                    anyhow!("Server '{server_name}' not found in [lsp.server.*] config")
                })?
                .clone()
        };

        // Spawn-or-await through the shared gate (misc 191 / misc 208): the
        // registry lock is held only for the found-check and the marker
        // lookup/insert — never across the spawn+`initialize` handshake
        // below, so a cold spawn stalls no unrelated manager lookup. The
        // `_marker` guard clears the key on every exit of the owner path.
        let _marker = match self
            .claim_spawn(&ledger_key, |clients| {
                find_instance(clients, lang, server_name, root)
            })
            .await
        {
            SpawnClaim::Found(found) => {
                // Liveness is checked with no registry guard held (bug 104):
                // a busy existing instance blocks only this caller.
                let key = {
                    let locked = found.lock().await;
                    if !locked.is_alive() {
                        anyhow::bail!("LSP server '{server_name}' ({lang}) is dead");
                    }
                    locked
                        .server()
                        .key()
                        .ok_or_else(|| anyhow!("Existing server missing instance key"))?
                };
                return Ok((key, found));
            }
            SpawnClaim::Owner(marker) => marker,
        };

        // Spawn resolution (lsm 02): a pinned blessed server prefers its
        // managed-home install; the user's `path` override or
        // `[servers] prefer_managed = false` opt back out to PATH.
        let program = crate::managed_home::resolve_spawn_program(
            &crate::managed_home::ManagedHome::resolve(),
            &crate::recipes::active_manifest(),
            server_name,
            &server_def,
            self.config.prefer_managed(),
        );
        let program = program.as_str();

        let args: Vec<&str> = server_def
            .args
            .iter()
            .map(|s: &String| s.as_str())
            .collect();
        let root_str = root.display().to_string();

        // Rust-toolchain pin resolution (misc 176 / bug 92). When the rust
        // engine is spawned through the bare `rust-analyzer` proxy key, ask
        // rustup to resolve this root's active toolchain and rewrite the spawn
        // to run through `rustup run <toolchain> …` — so both rust-analyzer AND
        // the flycheck `cargo`/`rustc` it spawns honor the project pin on every
        // layout (proxied or not). Resolution failure, no rustup on PATH, or a
        // `path` override all fall through to the unchanged spawn below.
        // Owned buffers here outlive the borrowed `spawn` arguments. Resolved
        // *before* the not-installed check (misc 210): a resolved wrap means
        // rustup itself execs rust-analyzer, so the installed-binary question is
        // rustup's to answer at exec time, not a bare-`rust-analyzer` PATH lookup.
        let wrap = if rust_toolchain::should_wrap(server_name, program) {
            rust_toolchain::resolve_active_toolchain(root)
                .map(|toolchain| rust_toolchain::wrap_spawn(program, &args, &toolchain))
        } else {
            None
        };

        // Pre-spawn resolution (misc 210): a missing binary is a static
        // condition, knowable before the first spawn attempt — not a crash. The
        // resolver above already prefers the managed-home install and falls back
        // to PATH, so `program` is the exact command we would exec; if it does
        // not resolve to an installed binary, classify **not installed** — no
        // strike-ledger entry, no bench, no dead tombstone. One calm finding per
        // server and an honest `not-installed` board state, then bail with the
        // typed error `spawn_all`/`ensure_clients_for_paths` recognize so they
        // do not pile a second per-root warning on top. This is also the
        // ground-truth "wants but cannot spawn" signal, so with
        // `[servers] auto_install` on the classification kicks the background
        // install here (bug 148's JIT leg) instead of merely advising a flag.
        // A later spawn demand
        // re-resolves; if the binary appeared (a `catenary install`, an
        // auto-install pre-warm), the classification clears and the spawn
        // proceeds normally — coverage heals without a daemon restart. The
        // strike ledger keeps its real job: servers that exist and fail (a
        // mid-flight ENOENT — binary deleted between here and exec — still falls
        // through to the strike path in `LspClient::spawn`'s `Err` arm below).
        //
        // A resolved rust-toolchain wrap is exempt: rustup resolved a toolchain
        // and will exec rust-analyzer through `rustup run`, so a bare
        // `rust-analyzer` PATH lookup is the wrong installation question — the
        // component (or its absence) surfaces through the spawn/init path as
        // before (misc 162's rust-analyzer exemption).
        if wrap.is_none() && !crate::health::servers::server_binary_installed(server_name, program)
        {
            self.classify_not_installed(&ledger_key, program, &server_def);
            return Err(anyhow::Error::new(NotInstalled {
                server: server_name.to_string(),
                language: lang.to_string(),
            }));
        }
        // The binary resolved (or the wrap delegates resolution to rustup):
        // clear any stale not-installed classification so a heal-on-install
        // registers clean and re-arms the finding for a future removal (misc 210).
        self.clear_not_installed(&ledger_key);

        info!(
            source = Source::LspLifecycle.as_str(),
            server = server_name,
            scope_root = %root.display(),
            "Spawning LSP server for {lang}: {} {}",
            program,
            server_def.args.join(" ")
        );
        let (spawn_program, spawn_args, spawn_env): (&str, Vec<&str>, Option<HashMap<_, _>>) =
            if let Some(wrap) = &wrap {
                info!(
                    source = Source::LspLifecycle.as_str(),
                    server = server_name,
                    scope_root = %root.display(),
                    toolchain = %wrap.toolchain,
                    "rust-toolchain: spawning rust-analyzer through `rustup run {}`",
                    wrap.toolchain,
                );
                // Overlay `RUSTUP_TOOLCHAIN` under the configured env: a user's
                // explicit env value wins on key conflict (ServerDef::env
                // semantics), so an operator can still override the toolchain.
                let mut env = wrap.env.clone();
                if let Some(cfg_env) = server_def.env.as_ref() {
                    for (k, v) in cfg_env {
                        env.insert(k.clone(), v.clone());
                    }
                }
                (
                    wrap.program.as_str(),
                    wrap.args.iter().map(String::as_str).collect(),
                    Some(env),
                )
            } else {
                (program, args, server_def.env.clone())
            };

        let mut client = match LspClient::spawn(
            spawn_program,
            &spawn_args,
            lang,
            server_name,
            self.logging.clone(),
            server_def.settings.clone(),
            spawn_env.as_ref(),
            &root_str,
        ) {
            Ok(client) => client,
            Err(e) => {
                // A spawn that won't start is a failure observation (misc
                // 167): `+1`, so a binary that can never come up racks its
                // three strikes across a few cheap failed spawns and benches.
                self.record_server_strike(&ledger_key);
                return Err(e);
            }
        };

        // Set scope before initialize so the reader loop has it for
        // all protocol messages, including the init exchange itself.
        client.server().set_scope(Scope::Root(root.to_path_buf()));

        // The instance key is stable once the scope is set. Wire the snapshot
        // and register the board entry *before* initialize so the server is
        // visible as `initializing` during the (sometimes slow) handshake — and
        // so a failed init surfaces as `failed` instead of never appearing.
        let key = client
            .server()
            .key()
            .ok_or_else(|| anyhow!("Failed to construct instance key"))?;
        if let Some(writer) = &self.snapshot {
            client.server().set_snapshot(writer.clone());
            writer.register_server(&key, &crate::state_snapshot::now_iso());
        }

        // Forward the project's server config file (rust-analyzer.toml) as
        // client config, layered UNDER the user's Catenary-config options (misc
        // 202 follow-up). The conformance forced overlay in
        // `effective_initialization_options` still wins over both. Spawn-time
        // only — a mid-session file edit takes effect on the next spawn.
        let init_options = initialization_options_with_project_config(
            root,
            server_name,
            server_def.initialization_options.clone(),
        );
        if let Err(e) = client.initialize(&[root.to_path_buf()], init_options).await {
            // Surface the init failure on the board (snapshot-only — the caller
            // already handles the Err; no extra user notification).
            if let Some(writer) = &self.snapshot {
                writer.update_state(&key, &ServerLifecycle::Failed);
            }
            // Mark the instance's own lifecycle terminal too, not just the
            // snapshot: a server that failed to initialize (the julia/r
            // "dies during `initialize`" class) is Failed, not stuck
            // Initializing. This lets the diagnostics degradation path
            // (`unavailable_diagnostic_servers`, decision 027) recognize the
            // tombstone as unavailable even when the process lingers after
            // rejecting init, so the file degrades with an `unavailable:`
            // banner instead of reading as `[no LSP coverage]`.
            client.server().set_lifecycle(ServerLifecycle::Failed);
            // A failed `initialize` is a failure observation (misc 167):
            // `+1`, charged here — so mark the tombstone as already charged,
            // keeping the demand-side revive from double-counting the same
            // death when it later finds this instance.
            self.record_server_strike(&ledger_key);
            client.mark_death_strike_counted();
            // Tombstone: insert the dead client so `find_instance` returns
            // `Some` on subsequent calls.  `ensure_clients_for_paths` skips
            // bindings that already have an entry (dead or alive), and
            // `ensure_server` bails with "is dead" — the retry path is the
            // strike-gated demand revive (`get_servers`, misc 167). Re-acquire
            // the registry only to insert (the marker held our claim across the
            // handshake, misc 191); dropping `_marker` afterward wakes any
            // waiters, who find the tombstone and bail "is dead".
            self.clients
                .lock()
                .await
                .insert(key, Arc::new(Mutex::new(client)));
            return Err(e);
        }

        let client_mutex = Arc::new(Mutex::new(client));
        // Re-acquire the registry only to publish the live instance (misc 191:
        // the marker, not the registry lock, held our claim across the
        // handshake). No client mutex is awaited under this guard (bug 104).
        self.clients
            .lock()
            .await
            .insert(key.clone(), client_mutex.clone());

        // Eager health probe: transition Probing → Healthy before the
        // snapshot seed so the TUI shows "ready" immediately.
        self.run_eager_health_probe(&client_mutex, lang, root).await;

        // Seed the snapshot's post-probe state. The eager health probe
        // transitions Probing -> Healthy via `try_transition_probing_to_healthy`,
        // which bypasses `persist_state`, so mirror the current state here.
        if let Some(writer) = &self.snapshot {
            let lifecycle = client_mutex.lock().await.server().lifecycle();
            writer.update_state(&key, &lifecycle);
        }

        Ok((key, client_mutex))
    }

    /// Runs an eager health probe on a freshly spawned server.
    ///
    /// Finds the first file matching `lang` under `root`, opens it on
    /// the server, sends `documentSymbol`, and **leaves it held open**
    /// (bug 133 lean 2). The probe used to close its document — but in a
    /// minimal fixture the first matching file is the very file a diagnose
    /// serve is about to open, and the probe's `didClose` owed a clear
    /// that a starved unversioned-push server (taplo, under peak-parallel
    /// load) could flush late and OUT OF ORDER: the close-clear landing
    /// after the serve's real diagnostics and settling dirty→clean. With
    /// no close leg no clear is ever owed: the serve reuses the still-open
    /// document through the change gate ([`Self::open_document_on`] —
    /// `didChange` when disk content moved since the probe, nothing when
    /// unchanged), so the misordered-clear class cannot arise on the
    /// probed file. If no matching file exists or the probe fails, the
    /// server stays in its current state and will transition on the first
    /// real request.
    async fn run_eager_health_probe(
        &self,
        client_mutex: &Arc<Mutex<LspClient>>,
        lang: &str,
        root: &Path,
    ) {
        // Walk the root for the first file matching the language. Sorted, so
        // the pick is deterministic (bug 133 lean 2): the probed file stays
        // held open for the serve to reuse, and a reproducible pick keeps
        // that lifecycle observable — test fixtures that need a file to stay
        // closed (watched-files routing) bait the probe with a
        // `_probe_bait.<lang>` file that sorts first.
        let probe_path = {
            let walker = ignore::WalkBuilder::new(root)
                .git_ignore(true)
                .hidden(true)
                .sort_by_file_name(std::ffi::OsStr::cmp)
                .build();

            let mut found = None;
            for entry in walker.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let path = entry.path();
                let matches = self.fs.language_id(path).as_deref() == Some(lang)
                    || path.extension().and_then(|e| e.to_str()) == Some(lang);
                if matches {
                    found = Some(path.to_path_buf());
                    break;
                }
            }
            found
        };

        let Some(probe_path) = probe_path else {
            debug!(
                "No {lang} file found under {} for eager health probe",
                root.display(),
            );
            return;
        };

        // Query-cycle open through the held-open gate: at spawn time nothing
        // is held, so this is a plain didOpen. Deliberately NO close leg
        // (bug 133 lean 2): the document stays held open — query-opened,
        // unowned — so the diagnose serve reuses it via the change gate
        // instead of close→reopen, and no didClose-triggered clear can ever
        // race a later round's fresher verdict.
        let Ok((uri, _)) = self
            .open_document_on(&probe_path, client_mutex, None, None)
            .await
        else {
            debug!("Eager probe didOpen failed for {}", probe_path.display());
            return;
        };

        let mut client = client_mutex.lock().await;
        // The probe's didOpen races server init (the elm cold-download class:
        // a still-loading server drops the publish this open should have
        // provoked, and never re-pushes). Mark the document so a post-probe
        // demand that finds NO publish heard re-syncs it — a same-text
        // `didChange` through the change gate
        // ([`LspClient::plan_document_sync`]) — re-earning the evidence with
        // a stimulus the serve's settle discipline brackets. A heard publish
        // stands the re-sync down: the cached entry is the evidence.
        client.mark_probe_opened(&uri);
        client.run_health_probe(&uri).await;
        drop(client);
    }

    /// Spawns a single-file server with null workspace.
    ///
    /// Sends `initialize` with `rootUri: null` and
    /// `workspaceFolders: null`. If the server initializes successfully,
    /// inserts a `Scope::SingleFile` client. If initialization fails,
    /// negative-caches the `(lang, server)` pair.
    ///
    /// Only call for servers with `single_file = true` in config.
    ///
    /// # Errors
    ///
    /// Returns an error if the server definition is missing from config
    /// or the server rejects null-workspace initialization.
    async fn spawn_single_file(
        &self,
        server_name: &str,
        lang: &str,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let server_def = self
            .config
            .server
            .get(server_name)
            .ok_or_else(|| anyhow!("Server '{server_name}' not found in [lsp.server.*] config"))?
            .clone();

        let sf_key = InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::SingleFile);

        // Spawn-or-await through the shared gate (misc 208): the registry
        // lock is held only for the found-check and the marker lookup/insert
        // — program resolution, the process spawn, and the `initialize`
        // handshake below all run unlocked, so a cold singleton spawn stalls
        // nothing that doesn't depend on this key. (Pre-208 this path held
        // the registry lock across the whole handshake: a daemon-wide lookup
        // stall on every singleton cold spawn.)
        let _marker = match self
            .claim_spawn(&sf_key, |clients| clients.get(&sf_key).cloned())
            .await
        {
            SpawnClaim::Found(existing) => {
                // Liveness is checked with no registry guard held (bug 104 —
                // pre-208 this await ran UNDER the registry lock).
                if existing.lock().await.is_alive() {
                    return Ok(existing);
                }
                anyhow::bail!("Single-file LSP server '{server_name}' ({lang}) is dead");
            }
            SpawnClaim::Owner(marker) => marker,
        };

        // The owner path can be reached by a waiter whose owner just FAILED:
        // a failed singleton init leaves no tombstone (the negative cache,
        // not the registry, is the single-file failure memory), so a woken
        // waiter finds an empty registry and re-claims. Honor the cache here
        // so a failed init fans out as one handshake, not one per waiter.
        if self
            .single_file_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&(lang.to_string(), server_name.to_string()))
        {
            anyhow::bail!(
                "Single-file LSP server '{server_name}' ({lang}) rejected \
                 null-workspace initialization (negative-cached)"
            );
        }

        // Spawn resolution (lsm 02): same order as the per-root spawn — the
        // managed home for a pinned blessed server, PATH as the fallback.
        let program = crate::managed_home::resolve_spawn_program(
            &crate::managed_home::ManagedHome::resolve(),
            &crate::recipes::active_manifest(),
            server_name,
            &server_def,
            self.config.prefer_managed(),
        );
        info!(
            "Spawning single-file LSP server for {lang}: {} {}",
            program,
            server_def.args.join(" ")
        );

        let args: Vec<&str> = server_def.args.iter().map(String::as_str).collect();
        let mut client = LspClient::spawn(
            &program,
            &args,
            lang,
            server_name,
            self.logging.clone(),
            server_def.settings.clone(),
            server_def.env.as_ref(),
            "",
        )?;

        // Set scope before initialize so the reader loop has it for
        // all protocol messages, including the init exchange itself.
        client.server().set_scope(Scope::SingleFile);

        // Wire the snapshot and register the board entry *before* initialize —
        // same discipline as the per-root spawn — so the singleton is visible
        // as `initializing` during the handshake and its lifecycle transitions
        // mirror to the board thereafter (misc 209: singletons previously
        // never registered, so the board could not show them and the teardown
        // paths' entry removal had nothing to remove).
        if let Some(writer) = &self.snapshot {
            client.server().set_snapshot(writer.clone());
            writer.register_server(&sf_key, &crate::state_snapshot::now_iso());
        }

        // Initialize with null workspace (single-file mode per LSP spec).
        if let Err(e) = client
            .initialize(&[], server_def.initialization_options.clone())
            .await
        {
            info!(
                source = Source::LspLifecycle.as_str(),
                language = lang,
                server = server_name,
                "Server '{server_name}' rejected single-file mode: {e}",
            );
            self.single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((lang.to_string(), server_name.to_string()));
            // No instance remains (the negative cache, not a tombstone, is
            // the single-file failure memory) — a lingering board entry would
            // be exactly the ghost class, so drop it (bug 72 / misc 209).
            if let Some(writer) = &self.snapshot {
                writer.remove_server(&sf_key);
            }
            // Dropping `_marker` on return wakes waiters; each re-claims in
            // turn and bails on the negative cache above — one failed
            // handshake total.
            return Err(e);
        }

        let client_mutex = Arc::new(Mutex::new(client));
        // Re-acquire the registry only to publish the live instance (the
        // marker, not the registry lock, held the claim across the
        // handshake — misc 191/208). No client mutex is awaited under this
        // guard (bug 104).
        self.clients
            .lock()
            .await
            .insert(sf_key.clone(), client_mutex.clone());

        // A fresh singleton starts its idle clock now (brackets 01).
        self.touch_single_file(&sf_key);

        Ok(client_mutex)
    }

    /// Stamps a rootless singleton's idle clock at now (brackets 01).
    ///
    /// Called on every [`Self::ensure_single_file_server`] hit and every
    /// successful [`Self::spawn_single_file`], so an actively-demanded
    /// singleton never idle-expires.
    fn touch_single_file(&self, key: &InstanceKey) {
        self.single_file_last_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone(), Instant::now());
    }

    /// Sweeps rootless single-file singletons idle past `idle`, returning the
    /// reaped keys (brackets 01).
    ///
    /// The rootless tier's lifetime leg: singletons spawn on demand
    /// ([`Self::ensure_single_file_server`]) and idle-expire under the same
    /// lifetime rules as root instances — the daemon's reaper drives this on
    /// the ephemeral-root sweep cadence with the ephemeral-root idle window —
    /// minus any root-tracker/ownership involvement (no pinning, no
    /// pre-warm). Every qualifying demand refreshes the clock, so a singleton
    /// under active use never expires; a genuinely idle one shuts down and
    /// the next demand respawns it fresh. `now` is injected so tests drive
    /// expiry deterministically (a stale `Instant::now() - Duration`), the
    /// same seam [`crate::router`]'s ephemeral mounts use.
    ///
    /// Detaches every instance matching `filter` under the registry lock,
    /// then — registry free — shuts each down and drops its board entry,
    /// returning the torn-down keys.
    ///
    /// The one teardown chokepoint (misc 209, generalized across scopes by
    /// misc 208): every teardown path — the single-instance restart
    /// ([`Self::shutdown_instance`]), the root retirement
    /// ([`Self::shutdown_root_instances`]), the roots-change singleton sweep
    /// ([`Self::shutdown_single_file_instances`]), and the idle reap
    /// ([`Self::reap_idle_single_file_instances`]) — routes through here, so
    /// a detach is structurally paired with its board-entry removal (the
    /// snapshot keeps no ghost, bug 72) and the shutdown round-trip never
    /// runs under the registry guard (bug 104). Removal-first preserves the
    /// invariant: a detached instance is unreachable by lookup before its
    /// process goes.
    async fn teardown_matching(
        &self,
        mut filter: impl FnMut(&InstanceKey) -> bool,
        reason: &'static str,
    ) -> Vec<InstanceKey> {
        // Detach under the registry lock, shut down after (bug 104).
        let detached: Vec<(InstanceKey, Arc<Mutex<LspClient>>)> = {
            let mut clients = self.clients.lock().await;
            clients.extract_if(|k, _| filter(k)).collect()
        };
        let mut torn_down = Vec::with_capacity(detached.len());
        for (key, client_mutex) in detached {
            let sr = key.scope.root_path().map(|p| p.display().to_string());
            info!(
                source = Source::LspLifecycle.as_str(),
                server = key.server.as_str(),
                scope_root = sr.as_deref(),
                reason,
                "Shutting down LSP server instance {key}",
            );
            let mut client = client_mutex.lock().await;
            if client.is_alive()
                && let Err(e) = client.shutdown().await
            {
                info!(
                    source = Source::LspLifecycle.as_str(),
                    server = key.server.as_str(),
                    scope_root = sr.as_deref(),
                    "Failed to shutdown LSP server instance {key}: {e}",
                );
            }
            drop(client);
            drop(client_mutex);
            // The instance is gone — drop its board entry so the snapshot
            // does not keep a stale ghost (bug 72). Ordered after the client
            // drops so the reader loop's `on_shutdown` (which cannot upgrade
            // its `Weak` once the last `LspServer` ref is gone) never
            // re-creates one behind us.
            if let Some(writer) = &self.snapshot {
                writer.remove_server(&key);
            }
            torn_down.push(key);
        }
        torn_down
    }

    /// Same teardown discipline as [`Self::shutdown_single_file_instances`]:
    /// detach under the registry lock, shut down after (bug 104). Board
    /// entries are dropped so the snapshot keeps no ghost (bug 72).
    pub async fn reap_idle_single_file_instances(
        &self,
        now: Instant,
        idle: Duration,
    ) -> Vec<InstanceKey> {
        // Phase 1 — pick the expired singletons under the registry lock. A
        // clock-less instance (defensive; spawn always stamps one) adopts
        // `now`, earning a full idle window rather than expiring on sight.
        let expired: Vec<InstanceKey> = {
            let clients = self.clients.lock().await;
            let mut clocks = self
                .single_file_last_use
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            clients
                .keys()
                .filter(|k| k.scope == Scope::SingleFile)
                .filter(|k| {
                    let last = *clocks.entry((*k).clone()).or_insert(now);
                    now.saturating_duration_since(last) >= idle
                })
                .cloned()
                .collect()
        };
        if expired.is_empty() {
            return expired;
        }

        // Phase 2 — the shared teardown chokepoint (misc 209): detach under
        // the registry lock, shut down after (bug 104), board entry dropped
        // with the instance (bug 72).
        let reaped = self
            .teardown_matching(
                |k| k.scope == Scope::SingleFile && expired.contains(k),
                "idle-expired",
            )
            .await;

        // Drop the reaped clocks so a later respawn starts a fresh window.
        {
            let mut clocks = self
                .single_file_last_use
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for key in &reaped {
                clocks.remove(key);
            }
        }
        reaped
    }

    /// Returns a single-file server for the given language and server,
    /// spawning one if needed.
    ///
    /// The rootless spawn gate (brackets 01): a server qualifies through the
    /// manifest's verified `single_file` capability
    /// ([`crate::recipes::SingleFileSupport::may_spawn_rootless`] —
    /// `enrichment-only` or `serves-diagnostics`; the fail-closed default for a
    /// server without a claim is `unsupported`, never spawned rootless), or
    /// through the pre-existing user-scope `single_file = true` config opt-in.
    /// Checks the negative cache first — if the server previously
    /// rejected null-workspace initialization, returns `None` without
    /// a spawn attempt. Returns `None` for dead servers (tombstones).
    /// Every hit refreshes the singleton's idle clock
    /// ([`Self::reap_idle_single_file_instances`]).
    async fn ensure_single_file_server(
        &self,
        lang: &str,
        server_name: &str,
    ) -> Option<Arc<Mutex<LspClient>>> {
        // Rootless spawn gate: the manifest capability (fail closed), or the
        // user-scope config opt-in.
        let def = self.config.server.get(server_name)?;
        if !(def.single_file
            || crate::lsp::server_behavior::ServerProfile::for_server(server_name)
                .single_file()
                .may_spawn_rootless())
        {
            return None;
        }

        // Check negative cache.
        {
            let failures = self
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if failures.contains(&(lang.to_string(), server_name.to_string())) {
                return None;
            }
        }

        // Check for existing instance. Snapshot the handle under the
        // registry lock, check liveness after the guard drops (bug 104 /
        // misc 208 — this await previously ran UNDER the registry lock).
        let sf_key = InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::SingleFile);
        let existing = {
            let clients = self.clients.lock().await;
            clients.get(&sf_key).cloned()
        };
        if let Some(existing) = existing {
            if existing.lock().await.is_alive() {
                // Demand refreshes the singleton's idle clock (brackets 01).
                self.touch_single_file(&sf_key);
                return Some(existing);
            }
            // Dead — don't retry.
            return None;
        }

        // No failure and no existing instance — try to spawn.
        self.spawn_single_file(server_name, lang).await.ok()
    }

    /// Runs one transaction bracket against the rootless singleton for
    /// `(lang, server_name)` — the single-file access path (brackets 02).
    ///
    /// Ensures the singleton through the brackets-01 gate
    /// ([`Self::ensure_single_file_server`]), then serves `body` (open →
    /// request(s) → answer) and `close` (the teardown leg) as one bracket
    /// on the instance's own queue: concurrent consumers serialize at
    /// transaction boundaries, debt-payment ahead of enrichment, and the
    /// bracket runs to completion — a budget-expired body still gets its
    /// close, degrading the answer to raw
    /// ([`BracketOutcome::Degraded`]).
    ///
    /// Returns `None` when no capable singleton exists — the gate refused,
    /// the server negative-cached, or the instance is dead. That is the
    /// capability-shaped degrade: whether a query enriches is decided by
    /// "does a capable server exist", never by racing a clock; the budget
    /// inside the bracket is only the pathology backstop.
    ///
    /// Registry locks stay lookup-scoped throughout: the ensure path drops
    /// the client-map guard before returning, and the bracket queue holds
    /// nothing above the instance's own queue (bug 104).
    #[allow(
        clippy::similar_names,
        reason = "`lang` and `lane` are both established domain vocabulary"
    )]
    pub async fn with_single_file_bracket<T, B, BFut, C, CFut>(
        &self,
        lang: &str,
        server_name: &str,
        lane: Lane,
        body: B,
        close: C,
    ) -> Option<BracketOutcome<T>>
    where
        T: Send + 'static,
        B: FnOnce(Arc<Mutex<LspClient>>) -> BFut + Send + 'static,
        BFut: std::future::Future<Output = T> + Send + 'static,
        C: FnOnce(Arc<Mutex<LspClient>>) -> CFut + Send + 'static,
        CFut: std::future::Future<Output = ()> + Send + 'static,
    {
        let client = self.ensure_single_file_server(lang, server_name).await?;
        let key = InstanceKey::new(lang.to_string(), server_name.to_string(), Scope::SingleFile);
        let opened = Arc::clone(&client);
        Some(
            self.brackets
                .run(&key, lane, move || body(opened), move || close(client))
                .await,
        )
    }

    /// Get-then-spawn composition.
    ///
    /// Looks up an existing `Scope::Root(root)` instance. On miss,
    /// spawns a new per-root instance. Dead servers are left as
    /// tombstones — this path does not restart them; a crashed server
    /// is revived on demand through the strike-gated
    /// [`Self::revive_server`] (misc 167), which `get_servers` drives.
    /// Intentional restarts (e.g. after `sync_roots`) go through
    /// [`Self::shutdown_instance`] which removes the entry so a fresh
    /// spawn can occur.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The server previously died (tombstone).
    /// - The server definition is missing from config.
    /// - The server fails to spawn or initialize.
    async fn ensure_server(
        &self,
        lang: &str,
        server_name: &str,
        root: &Path,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let project_scoped = self.is_project_scoped(lang, root);

        // Fast path: check for an existing instance. The registry guard drops
        // before the client lock is awaited: a client mutex can be held for a
        // full diagnose batch (settle included), and awaiting it under the
        // registry lock convoyed every manager lookup daemon-wide behind one
        // busy server (bug 104).
        let found = {
            let clients = self.clients.lock().await;
            find_instance(&clients, lang, server_name, root)
        };
        if let Some(found) = found {
            if found.lock().await.is_alive() {
                return Ok(found);
            }
            anyhow::bail!("LSP server '{server_name}' ({lang}) is dead");
        }

        // Miss — spawn only for a path some installed root covers
        // (misc 183). A request can race a root's teardown: resolution
        // saw the root installed, the expiry sweep then removed it and
        // shut its instances down, and a spawn here would recreate the
        // server set for a root no sync diff will ever name again — a
        // permanent orphan holding its processes until daemon restart.
        // Bailing degrades just this request (decision 027: coverage
        // loss, not a wedge); the next query re-mounts and respawns.
        if self.fs.resolve_root(root).is_none() {
            anyhow::bail!(
                "no installed root covers '{}'; refusing per-root spawn of '{server_name}' ({lang})",
                root.display()
            );
        }

        // Spawn with correct scope (spawn_inner handles its own
        // double-check).
        let (_key, client) = self
            .spawn_inner(server_name, lang, root, project_scoped)
            .await?;
        Ok(client)
    }

    /// Opens (or change-syncs) a document on a specific client through the
    /// held-open change gate (diagnostics-debt 01).
    ///
    /// Reads the file and consults the per-connection registry
    /// ([`LspClient::plan_document_sync`]): first open sends `didOpen`; an
    /// open document whose disk content moved since the last send (mtime
    /// fast-path, content hash breaking the same-mtime tie) sends
    /// `didChange` (full sync, whole text, version++); an unchanged open
    /// document sends **nothing**. Versions are real, monotonic per URI
    /// per client — each server gets an independent sequence.
    ///
    /// A query cycle against a held-open document therefore never reopens
    /// it — and never closes it (no close leg exists here); the didChange
    /// full-text relay for a moved open document is the query-side half of
    /// the watch-before-query invariant (see [`Self::nudge_changed_set`]).
    ///
    /// `owner` tags the document as held by a ROOT (root-ownership stage 3): the
    /// diagnose serve passes the root the file belongs to, so root retirement
    /// (worktree removal) closes exactly that root's held-open documents
    /// ([`Self::close_agent_docs`]); query callers pass `None`.
    ///
    /// Returns the document URI and the [`DocSync`] action taken.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the LSP notification
    /// fails.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "Client lock held across notification send"
    )]
    pub async fn open_document_on(
        &self,
        path: &Path,
        client: &Arc<Mutex<LspClient>>,
        parent_id: Option<String>,
        owner: Option<&str>,
    ) -> Result<(String, DocSync)> {
        let canonical = path.canonicalize()?;
        let uri = crate::lsp::lang::path_to_uri(&canonical);
        let text = tokio::fs::read_to_string(&canonical).await?;
        // Disk state for the change gate: the mtime stamp plus the content
        // hash of the text about to be sent (misc 190's two-leg shape).
        let mtime = std::fs::metadata(&canonical)
            .ok()
            .map(|m| crate::bridge::filesystem_manager::mtime_nanos(&m));
        let hash = crate::symbol_index::hash_bytes(text.as_bytes());

        let mut client = client.lock().await;
        client.set_parent_id(parent_id);

        if !client.is_alive() {
            client.set_parent_id(None);
            return Err(anyhow!(
                "[{}] server is no longer running",
                client.language()
            ));
        }

        // Held-open lifecycle (diagnostics-debt 01): didOpen once per
        // connection; after that, sync traffic only when disk content moved
        // since the last send — an unchanged document sends nothing. The
        // registry is committed only after a successful send, so a dropped
        // notification is retried by the next demand.
        let action = client.plan_document_sync(&uri, mtime, hash);
        match action {
            DocSync::Open(version) => {
                let language_id = self
                    .fs
                    .language_id(path)
                    .unwrap_or_else(|| "plaintext".to_string());
                // Drop any cached publish for content the server never saw
                // (e.g. a watched-files-induced stale publish — the
                // unlinked-file class): only evidence for the content being
                // sent may survive to retrieval.
                client.clear_diagnostics_for(&[&uri]);
                client.did_open(&uri, &language_id, version, &text).await?;
                client.commit_document_sync(&uri, version, mtime, hash);
            }
            DocSync::Change(version) => {
                // The cached publish refers to the pre-change text — stale by
                // construction. The post-change publish (or the next round's
                // didSave-triggered one) re-earns the entry.
                client.clear_diagnostics_for(&[&uri]);
                client.did_change(&uri, version, &text).await?;
                client.commit_document_sync(&uri, version, mtime, hash);
            }
            DocSync::Unchanged => {}
        }
        if let Some(owner) = owner {
            client.tag_document_owner(&uri, owner);
        }

        drop(client);
        Ok((uri, action))
    }

    /// Closes every held-open document `owner` (a ROOT path, root-ownership stage
    /// 3) holds, across every server connection — the batch-end leg of the
    /// held-open lifecycle (diagnostics-debt 01), dispatched from root retirement
    /// (worktree removal). Daemon death closes implicitly (bug 79, unchanged).
    ///
    /// Bug 104 discipline: the clients registry lock is held only to snapshot
    /// instance handles — each client lock is awaited after the guard drops.
    pub async fn close_agent_docs(&self, owner: &str) {
        let snapshot: Vec<Arc<Mutex<LspClient>>> =
            self.clients.lock().await.values().cloned().collect();
        for client_mutex in snapshot {
            let closed = client_mutex.lock().await.close_owned_documents(owner).await;
            if closed > 0 {
                debug!(
                    source = Source::LspDispatch.as_str(),
                    "closed {closed} held-open document(s) for stopping agent",
                );
            }
        }
    }

    /// Returns diagnostic-enabled servers for a file path without opening
    /// the document.
    ///
    /// Applies both the capability gate ([`LspServer::supports_diagnostics`])
    /// and the config-level filter ([`LanguageConfig::diagnostics_enabled`]).
    /// Returns an empty Vec when no server qualifies.
    pub async fn diagnostic_servers(&self, path: &Path) -> Vec<Arc<Mutex<LspClient>>> {
        let servers = self
            .get_servers(path, LspServer::supports_diagnostics, None)
            .await;

        if servers.is_empty() {
            return Vec::new();
        }

        let lang_id = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        });
        // Resolve the binding per-root so a project `[lsp.language.*]` rebinding's
        // `diagnostics` flags govern delivery; unrooted files use the global
        // binding.
        let lang_config = lang_id.as_deref().and_then(|id| {
            self.fs.resolve_root(path).map_or_else(
                || self.config.resolve_language(id).cloned(),
                |root| self.effective_language(&root, id),
            )
        });

        let mut clients = Vec::new();
        for client in &servers {
            let server_name = client.lock().await.server_name().to_string();
            let enabled = lang_config
                .as_ref()
                .is_some_and(|lc| lc.diagnostics_enabled(&server_name));
            if enabled {
                clients.push(client.clone());
            }
        }

        clients
    }

    /// Returns the names of diagnostics-enabled server bindings for `path`'s
    /// language whose instance exists but is **dead** — the
    /// spawn-failure / dies-at-`initialize` class (e.g. a julia/r LS that
    /// exits during the handshake leaves a dead tombstone).
    ///
    /// [`Self::diagnostic_servers`] returns only *live* clients, so a
    /// spawn-failed file would otherwise read as `[no LSP coverage]` —
    /// indistinguishable from a genuinely uncovered file. Decision 027 rules
    /// that a configured server which cannot start is a **coverage
    /// degradation**, not an absence of coverage: the caller routes such a
    /// file to the degraded / unverified path (the receipt's `unavailable:`
    /// banner) instead, with the same treatment a mid-run death gets. An empty
    /// result means no configured diagnostic server is dead — the file is
    /// either live-covered or genuinely uncovered.
    ///
    /// A `diagnostics = false` binding is intentionally silent, not
    /// unavailable, so it is excluded (it stays `[no LSP coverage]`). Unrooted
    /// files (tier-3 single-file servers) are out of scope here and yield an
    /// empty result.
    ///
    /// Each name is paired with its [`ReviveVerdict`] from the strike ledger
    /// (misc 167), so the receipt can distinguish "dead, will retry on
    /// demand" from "struck out". A binding with **no** surviving instance
    /// but recorded strikes (the spawn-fail class, which never enters the
    /// client map) is included too — a server that keeps failing to spawn is
    /// a degradation, never a silent `[no LSP coverage]`.
    pub async fn unavailable_diagnostic_servers(
        &self,
        path: &Path,
    ) -> Vec<(String, ReviveVerdict)> {
        let Some(lang_id) = self.fs.language_id(path).or_else(|| {
            path.extension()
                .and_then(|e| e.to_str())
                .map(str::to_string)
        }) else {
            return Vec::new();
        };
        let Some(root) = self.fs.resolve_root(path) else {
            return Vec::new();
        };
        // Resolve the binding + server defs per-root (misc 155).
        let Some(lang_config) = self.effective_language(&root, &lang_id) else {
            return Vec::new();
        };
        let resolved = self.resolve_server_root(path, &lang_id, &root);

        // Phase 1 — registry snapshot: resolve each qualifying binding to its
        // candidate instance (or `None` for the spawn-fail class) under the
        // registry lock, awaiting no client lock (bug 104 / misc 208 — the
        // liveness probe below previously ran under the guard).
        let candidates: Vec<(String, Option<Arc<Mutex<LspClient>>>)> = {
            let clients = self.clients.lock().await;
            lang_config
                .servers()
                .iter()
                .filter(|binding| lang_config.diagnostics_enabled(&binding.name))
                .filter(|binding| {
                    self.effective_server_def(&binding.name, &root)
                        .is_some_and(|def| file_matches_patterns(path, &def.compiled_patterns))
                })
                .map(|binding| {
                    let mut instance = find_instance(&clients, &lang_id, &binding.name, &resolved);
                    if instance.is_none() && resolved != root {
                        // No instance at the marker root — fall back to a
                        // workspace-root instance (mirrors `get_servers`).
                        instance = find_instance(&clients, &lang_id, &binding.name, &root);
                    }
                    (binding.name.clone(), instance)
                })
                .collect()
        };

        // Phase 2 — per-client checks with the registry guard dropped:
        // waiting on a busy candidate stalls only this lookup, never the
        // registry.
        let mut names = Vec::new();
        for (name, instance) in candidates {
            let Some(client) = instance else {
                // No tombstone survives a spawn failure; the ledger still
                // remembers (misc 167), so the receipt stays honest. Mirror
                // the instance lookup: marker root first, workspace root as
                // the fallback.
                let ledger_key = [resolved.as_path(), root.as_path()]
                    .into_iter()
                    .map(|r| {
                        InstanceKey::new(
                            lang_id.clone(),
                            name.clone(),
                            Scope::Root(r.to_path_buf()),
                        )
                    })
                    .find(|k| self.strikes_recorded(k));
                if let Some(key) = ledger_key {
                    names.push((name, self.revive_verdict(&key)));
                } else if self.is_server_not_installed(&name, &root) {
                    // No instance and no strikes, but the binary never resolved
                    // pre-spawn (misc 210): a configured-but-uninstalled server
                    // is a coverage degradation, not a bare `[no LSP coverage]`.
                    // Name it with the dedicated `NotInstalled` cause so the
                    // receipt teaches "install it", never "keeps dying".
                    names.push((name, ReviveVerdict::NotInstalled));
                }
                continue;
            };
            let locked = client.lock().await;
            let dead = !locked.is_alive() || locked.lifecycle().is_terminal();
            let key = locked.server().key();
            drop(locked);
            if dead {
                let verdict = key.map_or(ReviveVerdict::Revivable, |k| self.revive_verdict(&k));
                names.push((name, verdict));
            }
        }
        names
    }

    /// Revives a dead per-root instance on demand — the strike-gated respawn
    /// path (misc 167; grew out of decision 027's in-run diagnostics
    /// recovery, which routes through here too).
    ///
    /// Sequence:
    ///
    /// 1. **Charge the observed death** (`+1`) exactly once per instance: a
    ///    tombstone found dead here is one crash observation; the per-client
    ///    `death_strike_counted` flag dedupes repeat demands against the same
    ///    tombstone, and an `initialize`-failure tombstone arrives already
    ///    charged from the spawn path.
    /// 2. **Consult the gate**: a benched instance ([`ReviveVerdict`]) is not
    ///    revived — the tombstone stays on the map as evidence, so the
    ///    diagnostics degradation surface keeps naming it honestly.
    /// 3. **Respawn**: remove the tombstone (shutting down a still-alive
    ///    lingering process so it is never leaked) and spawn a fresh instance
    ///    through the normal spawn/initialize path, bounded by the same spawn
    ///    and initialize budgets as a first spawn. A failed respawn records
    ///    its own strike inside [`Self::spawn_inner`].
    ///
    /// Returns the fresh, live client on success, or `None` when the revive
    /// is gated or fails (the caller degrades: no further attempts this run —
    /// the next demand retries, strikes permitting).
    ///
    /// Only `Scope::Root` instances are revivable — the per-file fan-out's
    /// server groups are all root-scoped; a `SingleFile` key returns `None`.
    pub async fn revive_server(&self, key: &InstanceKey) -> Option<Arc<Mutex<LspClient>>> {
        let root = key.scope.root_path()?.to_path_buf();

        // Leg 1: charge the observed death (once per instance).
        let existing = self.clients.lock().await.get(key).cloned();
        if let Some(existing) = existing {
            let mut client = existing.lock().await;
            let is_dead = !client.is_alive() || client.lifecycle().is_terminal();
            let charge = is_dead && !client.death_strike_counted();
            if charge {
                client.mark_death_strike_counted();
            }
            drop(client);
            if charge {
                self.record_server_strike(key);
            }
        }

        // Leg 2: the gate. Benched ⇒ no revive, tombstone retained.
        if !self.revive_verdict(key).is_revivable() {
            debug!(
                source = Source::LspLifecycle.as_str(),
                server = key.server.as_str(),
                scope_root = %root.display(),
                "Not reviving {}: struck out (misc 167)",
                key.server,
            );
            return None;
        }

        // Leg 3: remove the tombstone under a tight lock, then shut down any
        // lingering process outside it (a dead tombstone needs no shutdown; a
        // still-alive one is closed so it is never leaked).
        let removed = self.clients.lock().await.remove(key);
        if let Some(old) = removed {
            let mut old = old.lock().await;
            if old.is_alive() {
                let _ = old.shutdown().await;
            }
            drop(old);
        }

        let project_scoped = self.is_project_scoped(&key.language_id, &root);
        match self
            .spawn_inner(&key.server, &key.language_id, &root, project_scoped)
            .await
        {
            Ok((_key, client)) => {
                info!(
                    source = Source::LspLifecycle.as_str(),
                    server = key.server.as_str(),
                    scope_root = %root.display(),
                    "Revived {} on demand",
                    key.server,
                );
                Some(client)
            }
            Err(e) => {
                debug!(
                    source = Source::LspLifecycle.as_str(),
                    server = key.server.as_str(),
                    scope_root = %root.display(),
                    "Demand revive failed: {e}",
                );
                None
            }
        }
    }

    /// Returns the live, diagnostics-enabled clients scoped to `root` that
    /// advertise `workspace/diagnostic` support.
    ///
    /// The whole-root `catenary diagnostics .` scope (workstream 37 ticket 04)
    /// routes to one `workspace/diagnostic` request per returned client instead
    /// of the per-file fan-out. Matched exactly on the instance's scope root (a
    /// sub-root directory or an untracked path never matches), then filtered by
    /// the per-language `diagnostics_enabled` binding and the runtime capability.
    /// An empty result routes the scope back to the fan-out fallback — so a
    /// not-yet-spawned or incapable server degrades gracefully.
    pub async fn workspace_diagnostic_clients(&self, root: &Path) -> Vec<Arc<Mutex<LspClient>>> {
        // Phase 1 — registry snapshot: root-scoped, diagnostics-enabled
        // candidates under the registry lock, awaiting no client lock (bug
        // 104 / misc 208 — the capability probe below previously ran under
        // the guard).
        let candidates: Vec<Arc<Mutex<LspClient>>> = {
            let clients = self.clients.lock().await;
            clients
                .iter()
                .filter(|(key, _)| key.scope.root_path() == Some(root))
                .filter(|(key, _)| {
                    self.effective_language(root, &key.language_id)
                        .is_some_and(|lc| lc.diagnostics_enabled(&key.server))
                })
                .map(|(_, client)| client.clone())
                .collect()
        };

        // Phase 2 — the liveness/capability probe with the registry guard
        // dropped: a busy candidate stalls only this lookup, never the
        // registry.
        let mut result = Vec::new();
        for client in candidates {
            let locked = client.lock().await;
            let capable = locked.is_alive() && locked.server().supports_workspace_diagnostics();
            drop(locked);
            if capable {
                result.push(client);
            }
        }
        result
    }

    /// Spawns LSP servers for new languages detected in the given file paths.
    ///
    /// Used by workspace-wide tools (grep, glob) to discover languages added
    /// mid-session. For each path, detects the language via
    /// [`FilesystemManager`] and resolves the owning root. Only spawns
    /// servers for configured languages that don't already have an instance
    /// covering the file's root. Unrooted files are skipped. Servers that
    /// fail to spawn are logged and skipped.
    ///
    /// For workspace-folder-capable servers, marker roots within a workspace
    /// root are sent as `workspace/didChangeWorkspaceFolders` additions to
    /// the existing workspace-root instance instead of spawning a redundant
    /// server.
    pub async fn ensure_clients_for_paths(&self, paths: &[PathBuf]) {
        let configured_keys: HashSet<&str> =
            self.config.language.keys().map(String::as_str).collect();

        // Collect (language, server_name, root) triples that need spawning,
        // and (client, marker_root, language, server_name) candidates for
        // workspace folder additions. The candidate's folder capability is
        // probed after the registry guard drops — awaiting a client lock
        // under the registry guard convoyed every manager lookup daemon-wide
        // behind a single busy server (bug 104).
        let mut to_spawn: HashSet<(String, String, PathBuf)> = HashSet::new();
        let mut folder_candidates: Vec<(Arc<Mutex<LspClient>>, PathBuf, String, String)> =
            Vec::new();

        {
            let active = self.clients.lock().await;
            for path in paths {
                let lang = self.fs.language_id(path).or_else(|| {
                    path.extension()
                        .and_then(|e| e.to_str())
                        .map(str::to_string)
                });

                let Some(lang) = lang else { continue };
                if !configured_keys.contains(lang.as_str()) {
                    continue;
                }

                // Skip unrooted files.
                let Some(root) = self.fs.resolve_root(path) else {
                    continue;
                };

                // Skip `disable_lsp` roots — no on-demand spawn (ticket 00).
                if self.is_lsp_disabled(&root) {
                    continue;
                }

                let Some(lang_config) = self.effective_language(&root, &lang) else {
                    continue;
                };

                // Check all servers in the binding, not just the first.
                // Resolve marker root once per language — all servers
                // share the same markers.
                let resolved = self.resolve_server_root(path, &lang, &root);
                for binding in lang_config.servers() {
                    if find_instance(&active, &lang, &binding.name, &resolved).is_some() {
                        continue;
                    }
                    // No instance at marker root. For workspace-folder-capable
                    // servers, send the marker root as a workspace folder
                    // addition to the workspace-root instance (capability
                    // checked below, outside the guard).
                    if resolved != root
                        && let Some(ws) = find_instance(&active, &lang, &binding.name, &root)
                    {
                        folder_candidates.push((
                            ws,
                            resolved.clone(),
                            lang.clone(),
                            binding.name.clone(),
                        ));
                        continue;
                    }
                    to_spawn.insert((lang.clone(), binding.name.clone(), resolved.clone()));
                }
            }
        }

        // Send workspace folder additions to existing instances.
        // Deduplication is handled by LspClient::add_workspace_folder
        // (tracks added folders across calls). A candidate whose instance
        // turns out not to support workspace folders degrades to a per-root
        // spawn — the same decision the pre-bug-104 in-guard check made.
        for (client, marker_root, lang, server_name) in folder_candidates {
            let mut locked = client.lock().await;
            if !locked.supports_workspace_folders() {
                drop(locked);
                to_spawn.insert((lang, server_name, marker_root));
                continue;
            }
            if locked.is_alive()
                && let Err(e) = locked.add_workspace_folder(&marker_root).await
            {
                debug!(
                    "Failed to add workspace folder {}: {e}",
                    marker_root.display(),
                );
            }
        }

        if to_spawn.is_empty() {
            return;
        }

        let mut sorted: Vec<&str> = to_spawn.iter().map(|(l, _, _)| l.as_str()).collect();
        sorted.sort_unstable();
        sorted.dedup();
        info!("Mid-session server spawn for: {}", sorted.join(", "));

        for (lang, server_name, root) in &to_spawn {
            if let Err(e) = self.ensure_server(lang, server_name, root).await {
                // A not-installed server already surfaced its one calm finding
                // in `spawn_inner` (misc 210); suppress the generic warn here.
                if is_not_installed(&e) {
                    continue;
                }
                warn!(
                    source = Source::LspLifecycle.as_str(),
                    language = lang.as_str(),
                    server = server_name.as_str(),
                    scope_root = %root.display(),
                    "Failed to spawn LSP server for {lang} ({server_name}): {e}",
                );
            }
        }
    }

    /// Returns a snapshot of all clients (including dead ones).
    pub async fn clients(&self) -> HashMap<InstanceKey, Arc<Mutex<LspClient>>> {
        self.clients.lock().await.clone()
    }

    /// Returns a snapshot of rooted clients only (excluding single-file
    /// servers).
    ///
    /// Single-file servers have no project context and are excluded from
    /// workspace-wide fan-out operations (grep, workspace/symbol).
    pub async fn rooted_clients(&self) -> HashMap<InstanceKey, Arc<Mutex<LspClient>>> {
        self.clients
            .lock()
            .await
            .iter()
            .filter(|(k, _)| k.scope != Scope::SingleFile)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Snapshots the alive rooted servers whose scope root is within `root` and
    /// that registered at least one file watcher (WS31 Consumer A).
    ///
    /// Each client lock is held only briefly — long enough to clone the
    /// `(server Arc, name, watcher list)` — so no lock is held across the diff,
    /// the union filter, the notify, or the settle. Shared by
    /// [`nudge_changed_set`](Self::nudge_changed_set) (which routes to them) and
    /// [`has_covering_watchers`](Self::has_covering_watchers) (the walk-breadth
    /// gate's coverage input).
    async fn covering_watchers(&self, root: &Path) -> Vec<Covering> {
        let mut covering: Vec<Covering> = Vec::new();
        for (key, client_mutex) in self.rooted_clients().await {
            if !key.scope.root_path().is_some_and(|r| r.starts_with(root)) {
                continue;
            }
            let client = client_mutex.lock().await;
            if !client.is_alive() {
                drop(client);
                continue;
            }
            let watchers = client.server().watched_files_snapshot();
            if watchers.is_empty() {
                drop(client);
                continue;
            }
            covering.push(Covering {
                server: client.server().clone(),
                client: Arc::clone(&client_mutex),
                name: client.server_name().to_string(),
                watchers,
            });
            drop(client);
        }
        covering
    }

    /// Returns whether any alive rooted server under `root` registered a file
    /// watcher — the coverage input to the walk-breadth pre-check gate (WS31
    /// ticket 04).
    ///
    /// `false` ⇒ no server cares about filesystem changes under this root, so a
    /// coherence walk would route nothing: the gate classifies the query as
    /// [`WalkBreadth::None`] and the caller skips the engine entirely (no walk,
    /// no nudge). `true` ⇒ a covering server exists, so the gate is `Full`
    /// (enriched `grep` / `diagnostics`) or `Scoped` (`glob`) per the query.
    pub async fn has_covering_watchers(&self, root: &Path) -> bool {
        !self.covering_watchers(root).await.is_empty()
    }

    /// Observes the union of registered watcher globs with the search filters
    /// **off** — the supplemental observation leg (bug 143).
    ///
    /// Catenary runs no OS watcher: every observation set fed to
    /// [`nudge_changed_set`](Self::nudge_changed_set) comes from one of
    /// Catenary's own walks, and each is built in search posture
    /// (`git_ignore(true).hidden(true)` — right for `grep`/`glob`). Registrations
    /// were consumed only as a *filter* over what those walks happened to see,
    /// never as a *subscription* driving what we look at, so three path classes
    /// were structurally unobservable however correctly delivery fanned out:
    /// dotfiles (`**/.lattice.toml`), gitignored paths (rust-analyzer's `baseUri`
    /// watchers on `target/…/out`), and paths outside every root. A server's
    /// autonomous interests — config reload, manifest watching — have no
    /// Catenary request to front-run, so nothing ever looked on their behalf.
    ///
    /// This leg closes that scope, registration-driven and unconditional
    /// (maintainer ruling, bug 143): the union of registered globs says what to
    /// observe, and it is observed with hidden/gitignore filtering off. The main
    /// walk keeps its search posture untouched — it is never de-filtered.
    ///
    /// **Cost bounding.** The plan per watcher is derived once at registration
    /// ([`WatchProbe`](crate::lsp::watch_probe::WatchProbe)) and is targeted
    /// stats, never a second full walk:
    ///
    /// - a literal pattern (`build/compile_commands.json`, a `baseUri`-anchored
    ///   literal) is one stat;
    /// - a `**/`-prefixed literal marker (`**/.lattice.toml`) is stat'd at the
    ///   root plus each directory the main walk **already visited** (parents of
    ///   `observed`), so its cost scales with — and stays below — the walk that
    ///   produced it;
    /// - a `baseUri`-anchored pattern that genuinely needs recursion
    ///   (`{ baseUri: …/out, pattern: "**/*" }`) gets a de-filtered walk of that
    ///   one server-named directory — the only leg that recurses, and the
    ///   directory must be a **proper descendant** of `root`: a base at or above
    ///   the root is the main walk's territory, and de-filtering that is exactly
    ///   what the ruling forbids. It runs on every nudge like the stat legs; a
    ///   walk gated on `reap` would leave the annotator's per-batch nudges blind
    ///   to it, and the first *reaping* nudge would then read a
    ///   present-since-before-we-looked file as `Created` against an
    ///   already-populated baseline — invisible to a Change-only watcher.
    /// - a wildcarded name that is not `baseUri`-anchored (`**/*.rs`, `**/*.md`)
    ///   plans nothing — the main walk already serves it in search posture.
    ///
    /// **Reap safety.** The result is merged into the walk's observation set
    /// before the baseline diff, so a supplementally-observed path is a normal
    /// baseline member and a reaping sweep may delete it. Every leg is therefore
    /// deterministic and un-truncated: a probe that finds nothing contributes
    /// nothing (the file is genuinely absent), and enumerated dir-walk entries
    /// follow the walks' "stat-with-retry, sentinel on miss, **never omit**"
    /// contract (WS31-review H1) so a racing stat cannot false-reap them.
    ///
    /// **Out-of-root paths are dropped.** The per-root baseline keys
    /// root-relative paths and `changed_file_uri` rebuilds the URI by joining
    /// them onto the root, so a path outside `root` (rust-analyzer's
    /// `/home/…/.config/rust-analyzer` watcher) has no representation in the
    /// model. It is skipped here rather than modelled by invention — flagged for
    /// a maintainer ruling (bug 143).
    fn supplemental_watch_observations(
        root: &Path,
        covering: &[Covering],
        observed: &[(PathBuf, i64)],
    ) -> Vec<(PathBuf, i64)> {
        let mut literals: BTreeSet<PathBuf> = BTreeSet::new();
        let mut markers: BTreeSet<PathBuf> = BTreeSet::new();
        let mut walk_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for c in covering {
            for watcher in &c.watchers {
                let plan = watcher.probe();
                if plan.is_empty() {
                    // A wildcarded name with no `baseUri` anchor: the main walk
                    // serves it in search posture and nothing supplemental is
                    // affordable (that would be a second full walk).
                    continue;
                }
                literals.extend(plan.paths().iter().cloned());
                markers.extend(plan.suffixes().iter().cloned());
                walk_dirs.extend(plan.dirs().iter().cloned());
            }
        }
        if literals.is_empty() && markers.is_empty() && walk_dirs.is_empty() {
            return Vec::new();
        }

        // Paths the main walk already observed need no probe — it saw them in
        // search posture and its entry is authoritative.
        let walked: HashSet<&Path> = observed.iter().map(|(rel, _)| rel.as_path()).collect();
        let mut recorded: HashSet<PathBuf> = HashSet::new();
        let mut extra: Vec<(PathBuf, i64)> = Vec::new();

        for literal in &literals {
            let abs = if literal.is_absolute() {
                literal.clone()
            } else {
                root.join(literal)
            };
            probe_watched_path(root, &abs, &walked, &mut recorded, &mut extra);
        }

        if !markers.is_empty() {
            // The root, plus every directory the main walk already visited.
            let mut dirs: BTreeSet<&Path> = BTreeSet::new();
            dirs.insert(Path::new(""));
            for (rel, _) in observed {
                if let Some(parent) = rel.parent() {
                    dirs.insert(parent);
                }
            }
            for dir in dirs {
                for marker in &markers {
                    let abs = root.join(dir).join(marker);
                    probe_watched_path(root, &abs, &walked, &mut recorded, &mut extra);
                }
            }
        }

        for dir in &walk_dirs {
            if dir == root || !dir.starts_with(root) {
                debug!(
                    source = Source::LspDispatch.as_str(),
                    "supplemental watch observation skips base {} (not a proper \
                     descendant of the root)",
                    dir.display(),
                );
                continue;
            }
            let defiltered = WalkBuilder::new(dir)
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .parents(false)
                .build();
            for entry in defiltered.flatten() {
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    continue;
                }
                let Ok(rel) = entry.path().strip_prefix(root) else {
                    continue;
                };
                if walked.contains(rel) || !recorded.insert(rel.to_path_buf()) {
                    continue;
                }
                // Enumerated present ⇒ never omitted (WS31-review H1): a racing
                // stat records the sentinel rather than dropping the file, which
                // a reaping sweep would read as a deletion.
                extra.push((rel.to_path_buf(), observe_mtime(entry.path())));
            }
        }

        extra
    }

    /// Diffs one coherence walk's observations against the per-root baseline and
    /// routes the resulting changed set to each covering server, then settles
    /// every server that received changes (WS31 Consumer A — the precise,
    /// per-server changed-set nudge).
    ///
    /// `observed` is the set of `(root-relative path, mtime)` pairs the walk
    /// visited, widened by the supplemental observation leg
    /// ([`supplemental_watch_observations`](Self::supplemental_watch_observations),
    /// bug 143) so the registered patterns the search-posture walk cannot see —
    /// dotfiles, gitignored paths — are observed too.
    ///
    /// `exclude` maps a root-relative path to the **server names that receive it
    /// via document-sync** this round (the diagnostics edited-set, which rides
    /// didOpen/didSave). Those servers drop it from the emission, but it stays in
    /// the baseline for everyone. The map is per-server because document-sync is
    /// per-server: a file edited and diagnosed by *taplo* is watched by *lattice*,
    /// which is never sent the document and would otherwise be starved of the
    /// change permanently — the baseline advances for the whole root, so a later
    /// walk sees no delta to re-emit (bug 143). A server the file is not
    /// document-synced to receives the ordinary watched-files route.
    ///
    /// The pipeline:
    ///
    /// 1. Snapshot the rooted servers whose scope root is within `root`, with
    ///    each server's registered watchers ([`watched_files_snapshot`]).
    /// 2. Filter `observed` to the **union** of those servers' watch globs — the
    ///    baseline tracks only files some server asked to watch.
    /// 3. [`diff_and_update`](FilesystemManager::diff_and_update) the filtered
    ///    set into the baseline → the
    ///    [`ChangeSet`](crate::bridge::filesystem_manager::ChangeSet). The first
    ///    walk runs against an empty baseline ⇒ the cold-start full candidate set.
    /// 4. Fan out: each server receives only the changes matching **its** globs
    ///    and watch-**kind** mask (via
    ///    [`covers`](crate::lsp::server::ParsedWatcher::covers)), minus
    ///    `exclude`, as `workspace/didChangeWatchedFiles` — for **closed**
    ///    documents; a change to an **open** document dispatches as the
    ///    didChange full-text relay instead (the two forms of the
    ///    watch-before-query invariant — see the step-4 comment in the body;
    ///    diagnostics-debt 01). The wire
    ///    `FileChangeType` carries the true semantic [`ChangeKind`] (Created ⇒ 1,
    ///    Changed ⇒ 2, Deleted ⇒ 3), agreeing with the kind-mask filter. The
    ///    first walk's cold snapshot is `Changed`; only a path absent from an
    ///    already-populated baseline is `Created`; a baseline entry a full walk
    ///    did not visit (only when `reap`) is `Deleted`
    ///    (decision 018 — filesystem-coherence changed-set).
    /// 5. Settle each notified server (idle + drain) so the caller's enrichment /
    ///    diagnostics read reflects the post-nudge state.
    ///
    /// A server that registered nothing, or whose globs/kinds match nothing,
    /// gets nothing. With no changes since the last walk, step 3 yields an empty
    /// set and nothing is sent (the bug-38 no-repeat property).
    ///
    /// `reap` selects the diff variant (WS31 ticket 04): a **full** walk
    /// ([`WalkBreadth::Full`] — enriched `grep`, `diagnostics`) passes `true`,
    /// so any baseline entry the walk did not visit is reaped as
    /// [`ChangeKind::Deleted`] (wire `FileChangeType` 3, gated by the `Delete`
    /// watch-kind bit). A **scoped** observation set (`glob`'s annotation
    /// batches, and the annotator's per-batch nudges) passes `false`
    /// (add/update only): it cannot assert a baseline entry outside its scope
    /// is gone, so it must never reap.
    ///
    /// **Delivery is best-effort and the per-root baseline is shared.** Step 3
    /// advances the baseline **once**, *before* the per-server notify loop, and
    /// that baseline is keyed by **root only** — shared across every server
    /// covering the root, not isolated per server. So a `didChangeWatchedFiles`
    /// notify that fails for one covering server (a dying/broken-pipe server)
    /// would otherwise lose those changes for it permanently: the next walk diffs
    /// against the advanced baseline and emits nothing. To recover, a failed
    /// notify reverts the entries it routed via
    /// [`revert_baseline_changes`](FilesystemManager::revert_baseline_changes) so
    /// the **next** walk re-emits them to **all** covering servers — a duplicate
    /// `didChangeWatchedFiles` to a server that already received it is
    /// harmless/idempotent (this may re-notify a *healthy* covering server too,
    /// since the baseline is shared). The revert is **kind-faithful**: a re-emit
    /// preserves the original `FileChangeType` (a reverted Created re-emits
    /// Created, a reverted Changed re-emits Changed, a reverted Deleted re-routes
    /// Deleted), so a single-kind watcher is not mis-served (WS31-review-D D2).
    /// A `Deleted` only re-routes on the next *full* walk, and a Deleted whose file
    /// reappears before that walk re-emits as `Changed` — see
    /// `revert_baseline_changes` for both inherent residuals (WS31-review F4).
    ///
    /// [`watched_files_snapshot`]: crate::lsp::server::LspServer::watched_files_snapshot
    #[allow(
        clippy::too_many_lines,
        reason = "one linear pipeline; the open-document relay leg (diagnostics-debt 01) \
                  adds the second dispatch form of the watch-before-query invariant"
    )]
    pub async fn nudge_changed_set(
        &self,
        root: &Path,
        observed: &[(PathBuf, i64)],
        exclude: &HashMap<PathBuf, BTreeSet<String>>,
        reap: bool,
    ) {
        // Step 1: snapshot covering servers + their watchers. Lock each client
        // only briefly to clone the (server Arc, name, watcher list) — no lock
        // is held across the diff, the union filter, the notify, or the settle.
        let covering = self.covering_watchers(root).await;

        if covering.is_empty() {
            return;
        }

        // Step 1b: the supplemental observation leg (bug 143). The walk that
        // produced `observed` ran in search posture, so whatever the union of
        // registered globs asks for in a hidden or gitignored path was never
        // looked at. Serve those patterns here, filters off, and merge the
        // result into the walk's set before the union filter — from step 2 on
        // they are ordinary observations.
        let supplemental = Self::supplemental_watch_observations(root, &covering, observed);
        let observed: Cow<'_, [(PathBuf, i64)]> = if supplemental.is_empty() {
            Cow::Borrowed(observed)
        } else {
            debug!(
                source = Source::LspDispatch.as_str(),
                "supplemental watch observation added {} path(s) the search-posture \
                 walk could not see",
                supplemental.len(),
            );
            let mut merged = observed.to_vec();
            merged.extend(supplemental);
            Cow::Owned(merged)
        };

        // Step 2: filter observations to the union of registered watch globs —
        // the baseline tracks a file if SOME covering server's glob matches it,
        // regardless of kind. To ever reap a `Deleted` for a path it must have
        // been baselined while present, so a Delete-only watcher (mask 4, no
        // Create/Change bit) must still get its files into the baseline — else
        // the reaping sweep can never emit their deletion. Probe all three kinds
        // (Created OR Changed OR Deleted): a present, observed file passing
        // `covers(.., Deleted)` means some watcher's glob matches it AND wants
        // deletes — exactly the membership question. Per-event-kind filtering is
        // still done at routing (Step 4), so a Create-only watcher never sees a
        // delete and vice versa; widening here only affects baseline membership.
        let watched: Vec<(PathBuf, i64)> = observed
            .iter()
            .filter(|(rel, _)| {
                let abs = root.join(rel);
                covering.iter().any(|c| {
                    c.watchers.iter().any(|w| {
                        w.covers(rel, &abs, ChangeKind::Created)
                            || w.covers(rel, &abs, ChangeKind::Changed)
                            || w.covers(rel, &abs, ChangeKind::Deleted)
                    })
                })
            })
            .cloned()
            .collect();

        // Step 3: diff + merge into the per-root baseline. A full walk reaps
        // deletions (baseline entries the complete walk did not visit); a scoped
        // walk records and updates only.
        let change_set = if reap {
            self.fs.diff_update_and_reap(root, &watched)
        } else {
            self.fs.diff_and_update(root, &watched)
        };
        if change_set.is_empty() {
            return;
        }

        // Step 4 + 5: per-server routing then settle.
        //
        // **Watch-before-query invariant (stated, load-bearing):** Catenary is
        // the single disk walker — one walker dispatching to n servers, never
        // n walkers — and before ANY server query, pending disk knowledge is
        // delivered. Two dispatch forms, keyed on open state
        // (diagnostics-debt 01):
        //
        // - a **closed** document routes as `workspace/didChangeWatchedFiles`
        //   (the classic relay);
        // - an **open** (held-open or query-opened) document routes as the
        //   didChange full-text relay through the change gate
        //   ([`Self::open_document_on`]) — servers treat the client's text as
        //   the truth for open documents and do not deliver watched-files
        //   events for them, so a watched-files route would be dropped on the
        //   server floor. For the same reason, servers cannot detect
        //   out-of-band writes to open documents at all: the mtime+hash check
        //   at each dispatch (here, and at diagnostics round start) IS the
        //   detection.
        for c in &covering {
            // Each routed entry carries its true wire `FileChangeType`, matching
            // the semantic kind that passed this server's watch-kind mask. The
            // `&Change` is retained so a failed delivery can revert exactly the
            // entries this server should have received (F4 recovery, below).
            let mut candidates: Vec<(String, u8, &Change)> = Vec::new();
            for change in &change_set.changes {
                // Suppressed only for the servers this round document-syncs the
                // file to — a watching server that is never sent the document
                // still needs the watched-files route (bug 143).
                if exclude
                    .get(&change.rel)
                    .is_some_and(|synced| synced.contains(c.name.as_str()))
                {
                    continue;
                }
                let abs = root.join(&change.rel);
                if c.watchers
                    .iter()
                    .any(|w| w.covers(&change.rel, &abs, change.kind))
                {
                    candidates.push((
                        changed_file_uri(root, &change.rel),
                        change_kind_wire_type(change.kind),
                        change,
                    ));
                }
            }

            if candidates.is_empty() {
                continue;
            }

            // Partition on open state (one client lock — never held across
            // the notify/settle below, bug 104 discipline). A `Deleted` for
            // an **open** document has no text to relay: the document cannot
            // outlive its file, so it is force-closed here (owners dropped)
            // and the deletion then routes as watched-files like any closed
            // file — reap consumers (delete-masked watchers, server file
            // indices) still hear it.
            let mut open_docs: Vec<(String, u8, &Change)> = Vec::new();
            let mut routed: Vec<(String, u8, &Change)> = Vec::new();
            {
                let mut client = c.client.lock().await;
                for (uri, wire, change) in candidates {
                    if !client.is_document_open(&uri) {
                        routed.push((uri, wire, change));
                    } else if change.kind == ChangeKind::Deleted {
                        client.close_document_on_disk_delete(&uri).await;
                        routed.push((uri, wire, change));
                    } else {
                        open_docs.push((uri, wire, change));
                    }
                }
                drop(client);
            }

            // Open-document leg: the didChange full-text relay.
            let mut relayed = false;
            for (_, _, change) in &open_docs {
                let abs = root.join(&change.rel);
                match self.open_document_on(&abs, &c.client, None, None).await {
                    Ok(_) => relayed = true,
                    Err(e) => {
                        debug!(
                            source = Source::LspDispatch.as_str(),
                            server = c.name.as_str(),
                            "changed-set nudge didChange relay dropped: {e}",
                        );
                        // Same F4 recovery as the watched-files leg: revert so
                        // the next walk re-emits.
                        self.fs.revert_baseline_changes(root, &[(*change).clone()]);
                    }
                }
            }

            if routed.is_empty() {
                if relayed {
                    self.settle_after_nudge(c).await;
                }
                continue;
            }

            let changes: Vec<(&str, u8)> =
                routed.iter().map(|(u, t, _)| (u.as_str(), *t)).collect();
            if let Err(e) = c
                .server
                .notify(
                    "workspace/didChangeWatchedFiles",
                    crate::lsp::params::did_change_watched_files(&changes),
                    None,
                )
                .await
            {
                debug!(
                    source = Source::LspDispatch.as_str(),
                    server = c.name.as_str(),
                    "changed-set nudge notify dropped: {e}",
                );
                // F4: the per-root baseline already advanced (step 3) and is
                // shared across every covering server, so a dropped notify would
                // otherwise lose these changes for this server permanently — the
                // next walk diffs against the advanced baseline and emits nothing
                // (even across respawn; the baseline is torn down only on
                // `catenary unpin`). Revert exactly the entries routed here so the NEXT
                // walk re-emits them to all covering servers (an idempotent
                // duplicate to servers that did receive it). Best-effort: see
                // `revert_baseline_changes` for the Deleted (full-walk-only)
                // limitation.
                let reverted: Vec<Change> = routed.iter().map(|(_, _, ch)| (*ch).clone()).collect();
                self.fs.revert_baseline_changes(root, &reverted);
            }

            self.settle_after_nudge(c).await;
        }
    }

    /// Settles one covering server after a changed-set dispatch: waits for it
    /// to go idle, then drains the stdio pipe so its post-nudge state is
    /// visible before the caller's read. Shared by both dispatch forms
    /// (watched-files and the open-document didChange relay).
    async fn settle_after_nudge(&self, c: &Covering) {
        let result = await_idle(
            &c.server,
            IdleDetector::unconditional(),
            CancellationToken::new(),
            &c.name,
        )
        .await;
        debug!(
            source = Source::LspDispatch.as_str(),
            server = c.name.as_str(),
            "changed-set nudge settle: {result:?}",
        );
        if result != SettleResult::RootDied
            && let Err(e) = c.server.drain().await
        {
            debug!(
                source = Source::LspDispatch.as_str(),
                server = c.name.as_str(),
                "changed-set nudge drain: {e}",
            );
        }
    }

    /// Returns status of all active servers.
    pub async fn all_server_status(&self) -> Vec<ServerStatus> {
        let clients = self.clients.lock().await.clone();
        let mut statuses = Vec::new();

        for (key, client_mutex) in &clients {
            let status = client_mutex.lock().await.status(key);
            statuses.push(status);
        }

        statuses
    }

    /// Shuts down a specific server instance if it exists.
    ///
    /// Routes through [`Self::teardown_matching`] (misc 208 — this path
    /// previously held the registry guard across the client-lock await AND
    /// the shutdown round-trip, convoying every manager lookup behind one
    /// instance's teardown).
    pub async fn shutdown_instance(&self, key: &InstanceKey) {
        let removed = self
            .teardown_matching(|k| k == key, "instance shutdown")
            .await;
        // An intentional shutdown is not failure history (misc 167): a
        // deliberate restart starts with a clean strike slate.
        if !removed.is_empty() {
            self.clear_strikes(key);
        }
    }

    /// Shuts down all instances bound to a specific root.
    ///
    /// Only affects `Scope::Root(path)` instances where the path matches.
    /// Workspace-scoped and other instances are untouched. Teardown
    /// discipline (detach → shutdown → board-entry drop) lives in
    /// [`Self::teardown_matching`].
    async fn shutdown_root_instances(&self, root: &Path) {
        self.teardown_matching(
            |k| matches!(&k.scope, Scope::Root(r) if r.as_path() == root),
            "root retired",
        )
        .await;
        // Retirement resets the strike ledger for the root (misc 167 / bug
        // 93): the retired root's servers must not revive — their instances
        // just left the map, so no demand can find them — and a later remount
        // starts with a clean slate.
        self.clear_strikes_for_root(root);
    }

    /// Shuts down all single-file server instances and clears the
    /// single-file cache.
    ///
    /// Called when workspace roots change — previously-unrooted files may
    /// now be covered by workspace or per-root instances. Single-file
    /// servers are lazily re-spawned on the next request if still needed.
    async fn shutdown_single_file_instances(&self) {
        // The shared teardown chokepoint (misc 209): detach under the
        // registry lock, shut down after (bug 104), board entry dropped with
        // the instance (bug 72) — this path previously skipped the board
        // removal, leaving singleton ghosts on the state.json board after
        // every roots change.
        self.teardown_matching(|k| k.scope == Scope::SingleFile, "roots changed")
            .await;

        self.single_file_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        // The singletons are gone — drop their idle clocks too (brackets 01).
        self.single_file_last_use
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Spawns per-root instances for newly added roots.
    ///
    /// For each active language+server, spawns a `Scope::Root` instance
    /// for each added root that has matching files.
    async fn spawn_for_added_roots(&self, added_roots: &[PathBuf]) {
        // Collect active languages with their server names.
        let clients = self.clients.lock().await.clone();
        let mut active_langs: HashMap<String, Vec<String>> = HashMap::new();
        for key in clients.keys() {
            let entries = active_langs.entry(key.language_id.clone()).or_default();
            if !entries.contains(&key.server) {
                entries.push(key.server.clone());
            }
        }
        drop(clients);

        if active_langs.is_empty() {
            return;
        }

        // Detect per root and spawn only the languages each root actually
        // contains. Detecting a union across all added roots would leak
        // markerless languages (no `root_markers`) into added roots that
        // have no files of that language.
        let configured_keys: HashSet<&str> = active_langs.keys().map(String::as_str).collect();

        for root in added_roots {
            // Skip `disable_lsp` roots — tracked, but no language server (ticket 00).
            if self.is_lsp_disabled(root) {
                continue;
            }

            let detected = self
                .fs
                .detect_workspace_languages(std::slice::from_ref(root), &configured_keys);

            for lang in &detected {
                let Some(servers) = active_langs.get(lang) else {
                    continue;
                };
                // Per-root markers so a project `[lsp.language.*]` `root_markers`
                // override governs the sub-root gate (misc 155).
                let lang_config = self.effective_language(root, lang);
                let marker_set = lang_config.as_ref().and_then(LanguageConfig::marker_set);
                // Skip roots without markers when markers are configured.
                if marker_set.is_some_and(|(m, c)| !dir_has_marker(root, m, c)) {
                    continue;
                }
                for server_name in servers {
                    if let Err(e) = self.ensure_server(lang, server_name, root).await {
                        warn!(
                            source = Source::LspLifecycle.as_str(),
                            language = lang.as_str(),
                            server = server_name.as_str(),
                            "Failed to spawn instance for {lang} ({server_name}) at {}: {e}",
                            root.display(),
                        );
                    }
                }
            }
        }
    }

    /// Whether a language is project-scoped in the given root.
    ///
    /// Rule A: returns `true` if the root's project config has a
    /// `[lsp.language.{lang}]` entry. This triggers tier 1 — an
    /// isolated per-root instance.
    #[must_use]
    pub fn is_project_scoped(&self, lang: &str, root: &Path) -> bool {
        self.fs
            .root(root)
            .is_some_and(|r| r.config().language.contains_key(lang))
    }

    /// Returns the effective `ServerDef` for a server in a root.
    ///
    /// Deep-merges the root's project `[lsp.server.{name}]` (if any)
    /// over the user-level `[lsp.server.{name}]`. Returns the user-level
    /// def unchanged if no project override exists, or the project def alone
    /// when the server is defined only at project scope — so a project
    /// `[lsp.server.*]` def is a legal spawn/binding target (misc 155).
    #[must_use]
    pub fn effective_server_def(&self, server_name: &str, root: &Path) -> Option<ServerDef> {
        let user_def = self.config.server.get(server_name);

        let project_def = self
            .fs
            .root(root)
            .and_then(|r| r.config().server.get(server_name).cloned());

        // At most one layer defines the server: return whichever exists (a
        // project-only def is a legal spawn/binding target). Only when both are
        // present do we field-merge below.
        let (user_def, project_def) = match (user_def, project_def) {
            (Some(user_def), Some(project_def)) => (user_def, project_def),
            (None, project_def) => return project_def,
            (Some(user_def), None) => return Some(user_def.clone()),
        };

        // Field-level merge: project fields override user fields when
        // present. Settings use deep_merge for nested object merging.
        let mut merged = user_def.clone();
        // A project `path` override relocates the executable (misc 162); it
        // carries its own args, mirroring the pre-162 command+args coupling.
        if project_def.path.is_some() {
            merged.path.clone_from(&project_def.path);
            merged.args.clone_from(&project_def.args);
        }
        if project_def.initialization_options.is_some() {
            merged
                .initialization_options
                .clone_from(&project_def.initialization_options);
        }
        if project_def.min_severity.is_some() {
            merged.min_severity.clone_from(&project_def.min_severity);
        }
        if !project_def.file_patterns.is_empty() {
            merged.file_patterns.clone_from(&project_def.file_patterns);
            merged
                .compiled_patterns
                .clone_from(&project_def.compiled_patterns);
        }
        if let Some(ref project_env) = project_def.env {
            if let Some(ref user_env) = user_def.env {
                let mut env = user_env.clone();
                env.extend(project_env.iter().map(|(k, v)| (k.clone(), v.clone())));
                merged.env = Some(env);
            } else {
                merged.env = Some(project_env.clone());
            }
        }
        if let Some(ref project_settings) = project_def.settings {
            if let Some(ref user_settings) = user_def.settings {
                merged.settings = Some(crate::config::merge::deep_merge(
                    user_settings,
                    project_settings,
                ));
            } else {
                merged.settings = Some(project_settings.clone());
            }
        }

        Some(merged)
    }

    /// Returns the effective settings `Value` for a server in a root.
    ///
    /// Deep-merges the root's project `[lsp.server.{name}].settings`
    /// over the user-level `[lsp.server.{name}].settings`.
    #[must_use]
    pub fn effective_settings(&self, server_name: &str, root: &Path) -> Option<serde_json::Value> {
        let user_settings = self
            .config
            .server
            .get(server_name)
            .and_then(|d| d.settings.clone());

        let project_settings = self.fs.root(root).and_then(|r| {
            r.config()
                .server
                .get(server_name)
                .and_then(|d| d.settings.clone())
        });

        match (user_settings, project_settings) {
            (Some(user), Some(project)) => Some(crate::config::merge::deep_merge(&user, &project)),
            (None, Some(project)) => Some(project),
            (Some(user), None) => Some(user),
            (None, None) => None,
        }
    }

    /// Shuts down all active clients through the bounded teardown ladder
    /// (bug 130) — teardown always ends.
    ///
    /// Per child, concurrently: the graceful LSP `shutdown`/`exit` sequence
    /// gets [`TEARDOWN_GRACEFUL_GRACE`]; a child still alive past it gets
    /// SIGTERM and [`TEARDOWN_SIGTERM_GRACE`]; a child still alive past that
    /// gets SIGKILL. The whole fleet is additionally bounded by
    /// [`TEARDOWN_CEILING`] regardless of size — stragglers still pending at
    /// the ceiling are killed (SIGKILL) by PID. Every straggler action is named in
    /// the firehose (`warn!` with the server identity and the rung that
    /// acted); a clean graceful exit is `debug!` chatter.
    pub async fn shutdown_all(&self) {
        let timings = self.teardown_timings;
        let started = std::time::Instant::now();

        // Detach the fleet from the registry. The registry lock is
        // short-held everywhere, but a wedged holder must not pin teardown —
        // bound the acquisition by the ceiling and abandon gracefulness past
        // it (the children die with the process: `Connection::drop` SIGKILLs
        // by PID at runtime teardown).
        let Ok(mut clients) = tokio::time::timeout(timings.ceiling, self.clients.lock()).await
        else {
            warn!(
                source = Source::LspLifecycle.as_str(),
                "LSP teardown: client registry lock not acquired within {:?}; \
                 abandoning graceful shutdown (children are SIGKILLed by \
                 connection drop at process exit)",
                timings.ceiling,
            );
            self.clear_all_strikes();
            return;
        };
        let fleet: Vec<(InstanceKey, Arc<Mutex<LspClient>>)> = clients.drain().collect();
        drop(clients);

        let pending: PendingTeardowns = Arc::new(std::sync::Mutex::new(
            fleet.iter().map(|(k, _)| (k.clone(), None)).collect(),
        ));

        let mut ladders = tokio::task::JoinSet::new();
        for (key, client_mutex) in fleet {
            ladders.spawn(Self::child_teardown_ladder(
                key,
                client_mutex,
                timings,
                Arc::clone(&pending),
            ));
        }

        // The whole-teardown ceiling: normally the concurrent ladders finish
        // well inside it (max per-child ≈ graceful + SIGTERM graces). Past it,
        // stop waiting, kill what the ladders left behind, and name each one.
        // One budget covers the registry wait above and this drain together.
        let remaining = timings.ceiling.saturating_sub(started.elapsed());
        let drained = tokio::time::timeout(remaining, async {
            while ladders.join_next().await.is_some() {}
        })
        .await;

        if drained.is_err() {
            ladders.abort_all();
            let stragglers: Vec<(InstanceKey, Option<u32>)> = {
                let mut pending = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.drain().collect()
            };
            for (key, pid) in stragglers {
                if let Some(pid) = pid {
                    catenary_proc::kill_process(pid);
                    warn_straggler(
                        &key,
                        "ceiling",
                        &format!(
                            "outlived the whole-teardown ceiling ({:?}); sent SIGKILL",
                            timings.ceiling
                        ),
                    );
                } else {
                    warn_straggler(
                        &key,
                        "ceiling",
                        &format!(
                            "outlived the whole-teardown ceiling ({:?}) with no \
                             harvestable PID (client wedged); it dies with the process",
                            timings.ceiling
                        ),
                    );
                }
            }
        }

        // Daemon shutdown resets the ledger (misc 167): a restart is the
        // ticket's "restart resets S to 0".
        self.clear_all_strikes();
    }

    /// One child's teardown ladder (bug 130).
    ///
    /// Rung 1 bounds the whole graceful leg — client acquisition, the LSP
    /// `shutdown`/`exit` sequence, and the wait for process death — under
    /// `graceful_grace`. A child still alive past it gets SIGTERM and
    /// `sigterm_grace` (rung 2), then SIGKILL (rung 3). The child's PID is
    /// recorded on `pending` once harvested and its entry removed when the
    /// ladder finishes, so the ceiling rung in [`Self::shutdown_all`] only
    /// sees genuine stragglers.
    #[allow(
        clippy::too_many_lines,
        reason = "sequential ladder rungs; extraction would harm readability"
    )]
    async fn child_teardown_ladder(
        key: InstanceKey,
        client_mutex: Arc<Mutex<LspClient>>,
        timings: TeardownTimings,
        pending: PendingTeardowns,
    ) {
        // The server handle is exported through a slot so the later rungs can
        // reach the PID even when the graceful future is dropped at its
        // deadline mid-`shutdown`.
        let server_slot: Arc<std::sync::Mutex<Option<Arc<LspServer>>>> =
            Arc::new(std::sync::Mutex::new(None));

        // Rung 1 — graceful. One grace bounds the lock acquisition too: a
        // wedged in-flight request can hold the client mutex forever (the
        // observed field wedge), and teardown must not inherit that wait.
        let graceful = tokio::time::timeout(timings.graceful_grace, {
            let slot = Arc::clone(&server_slot);
            let pending = Arc::clone(&pending);
            let key = key.clone();
            let client_mutex = Arc::clone(&client_mutex);
            async move {
                let mut client = client_mutex.lock().await;
                let server = Arc::clone(client.server());
                *slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&server));
                if let Some(pid) = server.pid() {
                    note_teardown_pid(&pending, &key, pid);
                }
                if !server.is_alive() {
                    return;
                }
                if let Err(e) = client.shutdown().await {
                    // The failure may be the server dying mid-handshake —
                    // which is success here; the death wait below decides.
                    debug!(
                        source = Source::LspLifecycle.as_str(),
                        server = key.server.as_str(),
                        "LSP teardown: graceful shutdown of {key} failed: {e}",
                    );
                }
                drop(client);
                while server.is_alive() {
                    tokio::time::sleep(TEARDOWN_POLL).await;
                }
            }
        })
        .await;

        if graceful.is_ok() {
            debug!(
                source = Source::LspLifecycle.as_str(),
                server = key.server.as_str(),
                "LSP teardown: {key} shut down gracefully",
            );
            settle_teardown(&pending, &key);
            return;
        }

        // Rung 1 expired. Recover the server handle: the slot when the lock
        // arrived in time, else one immediate `try_lock` (the holder may have
        // released since).
        let server = server_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .or_else(|| {
                client_mutex
                    .try_lock()
                    .ok()
                    .map(|client| Arc::clone(client.server()))
            });
        let Some(server) = server else {
            // The client mutex never unlocked and there is no path to the
            // PID. Name it; it dies with the process (`Connection::drop`
            // SIGKILLs by PID at runtime teardown).
            warn_straggler(
                &key,
                "abandoned",
                "unreachable (client mutex wedged, no PID); it dies with the process",
            );
            settle_teardown(&pending, &key);
            return;
        };
        if !server.is_alive() {
            // Died at the bell.
            settle_teardown(&pending, &key);
            return;
        }
        let Some(pid) = server.pid() else {
            warn_straggler(
                &key,
                "abandoned",
                "still alive past the graceful grace but has no PID to signal; \
                 it dies with the process",
            );
            settle_teardown(&pending, &key);
            return;
        };
        note_teardown_pid(&pending, &key, pid);

        // Rung 2 — SIGTERM.
        catenary_proc::terminate_process(pid);
        warn_straggler(
            &key,
            "sigterm",
            &format!(
                "did not answer graceful shutdown within {:?}; sent SIGTERM",
                timings.graceful_grace
            ),
        );
        let died = tokio::time::timeout(timings.sigterm_grace, async {
            while server.is_alive() {
                tokio::time::sleep(TEARDOWN_POLL).await;
            }
        })
        .await;
        if died.is_ok() {
            settle_teardown(&pending, &key);
            return;
        }

        // Rung 3 — SIGKILL. Not refusable — nothing left to wait for, and
        // the ceiling rung has nothing more to offer this child.
        catenary_proc::kill_process(pid);
        warn_straggler(
            &key,
            "sigkill",
            &format!(
                "survived SIGTERM for {:?}; sent SIGKILL",
                timings.sigterm_grace
            ),
        );
        settle_teardown(&pending, &key);
    }

    /// Installs (or replaces) a single root's project config, preserving the
    /// other tracked roots (test-only).
    ///
    /// Folds what tests previously did by inserting into the removed
    /// `project_configs` side-table: builds a config-complete [`Root`] and swaps
    /// it into the filesystem manager's root map.
    #[cfg(test)]
    fn install_root_config(&self, root: PathBuf, config: crate::config::ProjectConfig) {
        let mut roots = self.fs.root_views();
        roots.retain(|r| r.path() != root);
        roots.push(Arc::new(Root::new(root, config)));
        self.fs.set_roots_rich(roots);
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::{DispatchMethod, LanguageConfig, ServerBinding, ServerDef};
    use anyhow::Result;

    const MOCK_LANG_A: &str = "yX4Za";

    fn test_logging() -> LoggingServer {
        LoggingServer::new()
    }

    fn test_fs() -> Arc<FilesystemManager> {
        Arc::new(FilesystemManager::new())
    }

    fn test_fs_with_roots(roots: &[&str]) -> Arc<FilesystemManager> {
        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(roots.iter().map(PathBuf::from).collect());
        fs
    }

    /// Builds bare (default-config) `Root`s for `sync_roots` in tests.
    fn rich(paths: &[&str]) -> Vec<Arc<Root>> {
        paths
            .iter()
            .map(|p| Arc::new(Root::bare(PathBuf::from(p))))
            .collect()
    }

    /// Like [`rich`] but for owned `PathBuf`s (e.g. tempdir paths).
    fn rich_bufs(paths: Vec<PathBuf>) -> Vec<Arc<Root>> {
        paths.into_iter().map(|p| Arc::new(Root::bare(p))).collect()
    }

    fn test_config_raw() -> Config {
        Config {
            language: HashMap::new(),
            server: HashMap::new(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        }
    }

    fn test_config() -> Arc<Config> {
        Arc::new(test_config_raw())
    }

    /// Test helper: spawns the first server for a language using the first root.
    ///
    /// Replaces the removed `ensure_server_for_language` for test convenience.
    async fn ensure_first_server(
        manager: &LspClientManager,
        lang: &str,
    ) -> Result<Arc<Mutex<LspClient>>> {
        let lang_config = manager
            .config
            .resolve_language(lang)
            .ok_or_else(|| anyhow!("No LSP server configured for language '{lang}'"))?;
        let server_name = &lang_config
            .servers()
            .first()
            .ok_or_else(|| anyhow!("No servers configured for language '{lang}'"))?
            .name;
        let root = manager
            .fs
            .roots()
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("No workspace roots available for spawning '{lang}'"))?;
        manager.ensure_server(lang, server_name, &root).await
    }

    /// Locate the mockls binary in the same directory as the test executable.
    /// During `cargo test`, all binaries are built into the same `target/debug/deps`
    /// parent directory.
    fn mockls_bin() -> PathBuf {
        let test_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .map(|p| p.join("mockls"));
        test_exe.unwrap_or_else(|| PathBuf::from("mockls"))
    }

    fn mockls_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config whose mockls never answers `shutdown` (`--hang-on shutdown`):
    /// the bug-130 straggler stand-in for the teardown ladder's SIGTERM rung.
    fn mockls_hang_shutdown_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-hang");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--hang-on".to_string(),
                    "shutdown".to_string(),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config whose mockls both hangs on `shutdown` AND ignores SIGTERM:
    /// the bug-130 stand-in for the teardown ladder's SIGKILL rung. The
    /// `trap '' TERM` runs before `exec`, and an ignored signal disposition
    /// survives exec, so mockls runs with SIGTERM ignored.
    #[cfg(unix)]
    fn mockls_term_immune_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-immune");
        let script = format!(
            "trap '' TERM; exec '{}' {MOCK_LANG_A} --hang-on shutdown",
            bin.to_string_lossy()
        );
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some("sh".to_string()),
                args: vec!["-c".to_string(), script],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config whose language `MOCK_LANG_A` is globally bound to a single
    /// "shipped default" mockls server, with a second mockls server defined but
    /// NOT bound. A project `.catenary.toml` can rebind the language to the
    /// alternate server — the reroute-over-the-shipped-default shape (bug 81 /
    /// misc 155). Returns the `(default, alternate)` server names.
    fn mockls_default_plus_alt_config() -> (Arc<Config>, String, String) {
        let bin = mockls_bin();
        let default_name = format!("mockls-{MOCK_LANG_A}-default");
        let alt_name = format!("mockls-{MOCK_LANG_A}-alt");
        let mut server = HashMap::new();
        for name in [&default_name, &alt_name] {
            server.insert(
                name.clone(),
                ServerDef {
                    path: Some(bin.to_string_lossy().to_string()),
                    args: vec![MOCK_LANG_A.to_string()],
                    ..ServerDef::default()
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(default_name.clone())]),
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        });
        (config, default_name, alt_name)
    }

    fn mockls_workspace_folders_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-wf");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config with two legacy mockls servers for the same language.
    fn mockls_multi_server_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_a = format!("mockls-{MOCK_LANG_A}-a");
        let server_b = format!("mockls-{MOCK_LANG_A}-b");
        let mut server = HashMap::new();
        for name in [&server_a, &server_b] {
            server.insert(
                name.clone(),
                ServerDef {
                    path: Some(bin.to_string_lossy().to_string()),
                    args: vec![MOCK_LANG_A.to_string()],
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    ..ServerDef::default()
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![
                    ServerBinding::new(server_a),
                    ServerBinding::new(server_b),
                ]),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config with two workspace-folders-capable mockls servers for the same language.
    fn mockls_multi_server_workspace_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_a = format!("mockls-{MOCK_LANG_A}-wf-a");
        let server_b = format!("mockls-{MOCK_LANG_A}-wf-b");
        let mut server = HashMap::new();
        for name in [&server_a, &server_b] {
            server.insert(
                name.clone(),
                ServerDef {
                    path: Some(bin.to_string_lossy().to_string()),
                    args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                    initialization_options: None,
                    settings: None,
                    min_severity: None,
                    ..ServerDef::default()
                },
            );
        }
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![
                    ServerBinding::new(server_a),
                    ServerBinding::new(server_b),
                ]),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config with one workspace-capable and one legacy mockls for the same language.
    fn mockls_mixed_capability_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_ws = format!("mockls-{MOCK_LANG_A}-ws");
        let server_legacy = format!("mockls-{MOCK_LANG_A}-leg");
        let mut server = HashMap::new();
        server.insert(
            server_ws.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                ..ServerDef::default()
            },
        );
        server.insert(
            server_legacy.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![
                    ServerBinding::new(server_ws),
                    ServerBinding::new(server_legacy),
                ]),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    #[tokio::test]
    async fn test_roots_returns_initial_roots() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        let roots = manager.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_roots_empty_initial() -> Result<()> {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());

        assert!(manager.roots().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        assert_eq!(manager.roots().len(), 2);

        manager.remove_root(Path::new("/tmp/root_a")).await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_adds_and_removes() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a", "/tmp/root_b"]),
        );

        // Sync: remove /tmp/root_a, keep /tmp/root_b, add /tmp/root_c
        manager
            .sync_roots(rich(&["/tmp/root_b", "/tmp/root_c"]))
            .await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_b"));
        assert_eq!(roots[1], PathBuf::from("/tmp/root_c"));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_no_change() -> Result<()> {
        let manager = LspClientManager::new(
            test_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp/root_a"]),
        );

        manager.sync_roots(rich(&["/tmp/root_a"])).await?;

        let roots = manager.roots();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/tmp/root_a"));
        Ok(())
    }

    /// Checks whether any client in the map has the given language ID.
    fn has_language(clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>, lang: &str) -> bool {
        clients.keys().any(|k| k.language_id == lang)
    }

    #[tokio::test]
    async fn test_sync_roots_legacy_removes_per_root() -> Result<()> {
        // mockls without --workspace-folders does NOT advertise workspace folder support.
        // Removing a root should shut down the Scope::Root instance for that root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            !client.lock().await.supports_workspace_folders(),
            "mockls (no flags) should NOT support workspace folders"
        );

        assert!(has_language(&manager.clients().await, MOCK_LANG_A));

        // sync_roots removes /tmp — the per-root instance should be shut down.
        manager.sync_roots(rich(&["/var"])).await?;

        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "Scope::Root(/tmp) instance should be removed when /tmp is dropped"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_legacy_keeps_unchanged_root() -> Result<()> {
        // Adding a root should NOT shut down the existing legacy instance
        // for a root that is still present.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        // sync_roots adds /var but keeps /tmp — the /tmp instance stays.
        manager.sync_roots(rich(&["/tmp", "/var"])).await?;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "Scope::Root(/tmp) instance should remain when /tmp is still a root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn ensure_server_refuses_uncovered_root() -> Result<()> {
        // misc 183: a spawn racing a root's teardown must not recreate
        // instances for an uninstalled root — the sync diff could never name
        // them again, leaking the server set until daemon restart.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let binding = manager
            .config()
            .resolve_language(MOCK_LANG_A)
            .expect("config")
            .servers()[0]
            .name
            .clone();

        let Err(err) = manager
            .ensure_server(MOCK_LANG_A, &binding, Path::new("/var"))
            .await
        else {
            anyhow::bail!("spawn for an uncovered root must be refused")
        };
        assert!(
            err.to_string().contains("no installed root covers"),
            "unexpected refusal message: {err}"
        );
        assert!(
            manager.clients().await.is_empty(),
            "a refused spawn must leave no instance behind"
        );

        // A nested scope under an installed root is covered — marker roots
        // inside a tracked project are legitimate per-root scopes.
        let nested = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("tempdir under /tmp");
        manager
            .ensure_server(MOCK_LANG_A, &binding, nested.path())
            .await?;
        assert_eq!(manager.clients().await.len(), 1);

        Ok(())
    }

    #[tokio::test]
    async fn sync_roots_sweeps_orphaned_instances() -> Result<()> {
        // misc 183 shape: an instance spawned while its root was installed
        // survives the root's uninstall when the removal bypassed the sync
        // diff (a request racing the expiry sweep). The next sync must sweep
        // it even though the set diff is empty, and report it so per-root
        // caches evict (bug #36).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );
        let binding = manager
            .config()
            .resolve_language(MOCK_LANG_A)
            .expect("config")
            .servers()[0]
            .name
            .clone();
        manager
            .ensure_server(MOCK_LANG_A, &binding, Path::new("/var"))
            .await?;
        assert!(has_language(&manager.clients().await, MOCK_LANG_A));

        // Simulate the race: /var leaves the installed set without a sync.
        manager.fs.set_roots(vec![PathBuf::from("/tmp")]);

        // A same-set sync: to_add and to_remove are both empty, yet the
        // orphaned /var instance must be swept and reported.
        let dropped = manager.sync_roots(rich(&["/tmp"])).await?;
        assert_eq!(dropped, vec![PathBuf::from("/var")]);
        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "orphaned per-root instance must be shut down by the reconcile"
        );

        Ok(())
    }

    /// mockls with `--send-configuration-request` sends a `workspace/configuration`
    /// request with `section: "mockls"` during initialization. This test verifies
    /// that configured settings are threaded through to the response handler.
    #[tokio::test]
    async fn test_configuration_returns_settings() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-cfg");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--send-configuration-request".to_string(),
                ],
                settings: Some(serde_json::json!({"mockls": {"key": "value"}})),
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // ensure_first_server spawns + initializes; mockls sends workspace/configuration
        // during init. If Catenary responds correctly, initialization succeeds.
        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_notifies_supported_client() -> Result<()> {
        // mockls with --workspace-folders DOES advertise workspace folder support.
        // When roots change, it should receive a notification instead of being shut down.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert!(
            client.lock().await.supports_workspace_folders(),
            "mockls --workspace-folders should support workspace folders"
        );

        assert!(has_language(&manager.clients().await, MOCK_LANG_A));

        // sync_roots should send notification, NOT shut down the client
        manager.sync_roots(rich(&["/tmp", "/var"])).await?;

        // Client should still be active (not removed)
        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "mockls client should still be active after sync_roots (workspace folders supported)"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_spawns_new_language() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        // A file with the mock language extension triggers a spawn
        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "ensure_clients_for_paths should spawn the mock language server"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_skips_existing() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Pre-spawn the server
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);

        // ensure_clients_for_paths should not fail or double-spawn
        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        assert_eq!(
            manager.clients().await.len(),
            1,
            "should not create a duplicate client"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_ignores_unconfigured() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // .xyz has no configured server — should be silently skipped
        let paths = vec![PathBuf::from("/tmp/test.xyz")];
        manager.ensure_clients_for_paths(&paths).await;

        assert!(
            manager.clients().await.is_empty(),
            "unconfigured languages should not trigger a spawn"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_scope_aware() -> Result<()> {
        // Spawns instances per root, not per language.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        assert!(manager.clients().await.is_empty());

        // Paths in two different roots should spawn two instances.
        let paths = vec![
            PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}")),
            PathBuf::from(format!("/var/test.{MOCK_LANG_A}")),
        ];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            2,
            "Should have two root-scoped instances"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_project_scoped() -> Result<()> {
        // ensure_clients_for_paths should use spawn_project_scoped for
        // roots with project config, producing Scope::Root even when
        // the server supports workspace folders.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Add project config with [lsp.language.{MOCK_LANG_A}] (Rule A).
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(PathBuf::from("/tmp"), pc);

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            1,
            "project-scoped root should produce Scope::Root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_non_project_uses_root_scope() -> Result<()> {
        // All servers get Scope::Root regardless of workspace folder support.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            1,
            "all servers should be Scope::Root"
        );
        Ok(())
    }

    // --- Two-step spawn and InstanceKey tests ---

    #[tokio::test]
    async fn test_spawn_always_root_scope() -> Result<()> {
        // Even with workspace folder support, scope is always Root.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set after init");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_legacy_scope() -> Result<()> {
        // mockls without workspace folders gets Scope::Root(root) key after init.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let key = client
            .lock()
            .await
            .server()
            .key()
            .expect("key should be set after init");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        Ok(())
    }

    #[tokio::test]
    async fn spawn_against_gone_root_is_retired_not_init_failed() {
        // Bug 93, defensive leg: a per-root spawn whose root directory no longer
        // exists (a landed/removed worktree that slipped retirement) must refuse
        // BEFORE spawning — the error names `root gone — retired`, never a
        // phantom `initialize failed`, and no tombstone client is inserted (so
        // the board never wears a Fatal for a path that can route nothing again).
        let gone = PathBuf::from("/catenary-nonexistent-root-93");
        assert!(!gone.exists(), "the test root must not exist");
        let config = mockls_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();
        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        // The Ok variant (`LspClient`) is not `Debug`, so take the error via
        // `.err()` (which discards Ok without needing a `Debug` bound) rather
        // than `expect_err`.
        let err = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, &gone)
            .await
            .err();
        let msg = err
            .expect("a spawn against a gone root must fail")
            .to_string();
        assert!(
            msg.contains("root gone — retired"),
            "the refusal names the retired root, not an init failure: {msg}",
        );
        assert!(
            !msg.contains("initialize failed") && !msg.contains("died during"),
            "never a phantom init-failure for a removed directory: {msg}",
        );
        assert!(
            manager.clients().await.is_empty(),
            "no tombstone client is inserted for a gone root",
        );
    }

    #[tokio::test]
    async fn test_spawn_runs_eager_health_probe() -> Result<()> {
        // A freshly spawned server transitions to Healthy via the eager
        // health probe when a matching file exists under the root.
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let probe_file = root.join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&probe_file, "fn hello\nhello\n")?;

        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(vec![root.to_path_buf()]);

        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);
        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;

        assert_eq!(
            client.lock().await.lifecycle(),
            crate::lsp::state::ServerLifecycle::Healthy,
            "Server should be Healthy after eager health probe"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_stays_probing_without_matching_file() -> Result<()> {
        // Without a matching file the eager probe is skipped and the
        // server remains in Probing.
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        // No file matching MOCK_LANG_A in the root.

        let fs = Arc::new(FilesystemManager::new());
        fs.set_roots(vec![root.to_path_buf()]);

        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);
        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;

        assert_eq!(
            client.lock().await.lifecycle(),
            crate::lsp::state::ServerLifecycle::Probing,
            "Server should stay Probing when no matching file exists"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_server_idempotent() -> Result<()> {
        // Second call returns same client, no double-spawn.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client1 = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let client2 = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Same Arc — no double spawn
        assert!(Arc::ptr_eq(&client1, &client2));
        assert_eq!(manager.clients().await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_server_dead_tombstone() -> Result<()> {
        // Dead server returns error on re-ensure.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server by shutting it down without removing from map
        client.lock().await.shutdown().await?;
        // Wait briefly for the process to die
        tokio::time::sleep(Duration::from_millis(100)).await;

        let result = ensure_first_server(&manager, MOCK_LANG_A).await;
        assert!(result.is_err(), "dead server should return error");
        Ok(())
    }

    #[tokio::test]
    async fn test_clients_returns_instance_keys() -> Result<()> {
        // clients() map has InstanceKey keys.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let clients = manager.clients().await;

        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("should have one key");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert!(
            matches!(key.scope, Scope::Root(_)),
            "mockls without workspace folders should be Root-scoped"
        );
        Ok(())
    }

    // --- Per-root instance lifecycle ---

    /// Helper: count instances with a specific scope kind for a language.
    fn count_scope(
        clients: &HashMap<InstanceKey, Arc<Mutex<LspClient>>>,
        lang: &str,
        scope_kind: &str,
    ) -> usize {
        clients
            .keys()
            .filter(|k| k.language_id == lang && k.scope.kind_str() == scope_kind)
            .count()
    }

    #[tokio::test]
    async fn test_spawn_all_multi_root_legacy() -> Result<()> {
        // Legacy server (no workspace folders) should get one instance per root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        manager.spawn_all().await;

        // The mock language uses extension-based detection via the fallback path.
        // Neither /tmp nor /var will have files matching the mock extension,
        // so spawn_all detects nothing. Instead, manually spawn to test
        // the multi-root expansion logic.
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // First root spawned. Now check that spawn() can create a second
        // instance for the other root.
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let (_key, _client) = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        let clients = manager.clients().await;
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            2,
            "Legacy server should have two root-scoped instances"
        );

        // Verify distinct root paths.
        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter(|k| k.language_id == MOCK_LANG_A)
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(&PathBuf::from("/tmp")));
        assert!(root_paths.contains(&PathBuf::from("/var")));

        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_markerless_scoped_per_root() -> Result<()> {
        // A markerless language (no `root_markers`) detected in one root
        // must NOT spawn a server at a different root that has no files of
        // that language. Regression for the union-detection leak where a
        // language found in one served root (e.g. julia in a homelab repo)
        // spawned servers in every served root.
        const LANG_B: &str = "zZ9Qb";

        let root_a = tempfile::tempdir().expect("tempdir a");
        let root_b = tempfile::tempdir().expect("tempdir b");

        // root_a has only a LANG_A file; root_b has only a LANG_B file.
        std::fs::write(root_a.path().join(format!("a.{MOCK_LANG_A}")), "x").expect("write a");
        std::fs::write(root_b.path().join(format!("b.{LANG_B}")), "x").expect("write b");

        // Two markerless languages, each with its own mockls server.
        let bin = mockls_bin();
        let mut server = HashMap::new();
        let mut language = HashMap::new();
        for lang in [MOCK_LANG_A, LANG_B] {
            let server_name = format!("mockls-{lang}");
            server.insert(
                server_name.clone(),
                ServerDef {
                    path: Some(bin.to_string_lossy().to_string()),
                    args: vec![lang.to_string()],
                    ..ServerDef::default()
                },
            );
            language.insert(
                lang.to_string(),
                LanguageConfig {
                    servers: Some(vec![ServerBinding::new(server_name)]),
                    ..LanguageConfig::default()
                },
            );
        }
        let config = Arc::new(Config {
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        });

        let fs = test_fs_with_roots(&[
            root_a.path().to_str().expect("path a"),
            root_b.path().to_str().expect("path b"),
        ]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        manager.spawn_all().await;

        let clients = manager.clients().await;
        let roots_for = |lang: &str| -> HashSet<PathBuf> {
            clients
                .keys()
                .filter(|k| k.language_id == lang)
                .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
                .collect()
        };

        assert_eq!(
            roots_for(MOCK_LANG_A),
            HashSet::from([root_a.path().to_path_buf()]),
            "LANG_A should spawn only at root_a (the root that contains its files)",
        );
        assert_eq!(
            roots_for(LANG_B),
            HashSet::from([root_b.path().to_path_buf()]),
            "LANG_B should spawn only at root_b (the root that contains its files)",
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_multi_root_per_root() -> Result<()> {
        // Even with workspace folder support, each root gets its own instance.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let binding = manager
            .config()
            .resolve_language(MOCK_LANG_A)
            .expect("config")
            .servers()[0]
            .name
            .clone();
        manager
            .ensure_server(MOCK_LANG_A, &binding, Path::new("/tmp"))
            .await?;
        manager
            .ensure_server(MOCK_LANG_A, &binding, Path::new("/var"))
            .await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Each root should have its own server instance"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_adds_new_instance() -> Result<()> {
        // Adding a root should spawn a new per-root instance.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        manager.sync_roots(rich(&["/tmp", "/var"])).await?;

        let clients = manager.clients().await;
        assert!(
            has_language(&clients, MOCK_LANG_A),
            "Original server should stay alive after sync_roots"
        );
        assert_eq!(
            count_scope(&clients, MOCK_LANG_A, "root"),
            1,
            "Original per-root instance should remain"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root_legacy_shutdown() -> Result<()> {
        // remove_root should shut down the Scope::Root instance for the removed root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        manager.remove_root(Path::new("/tmp")).await?;

        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "Per-root instance should be removed after remove_root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remove_root_shuts_down_instance() -> Result<()> {
        // Per-root instance is shut down when its root is removed.
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        manager.remove_root(Path::new("/tmp")).await?;

        assert!(
            !has_language(&manager.clients().await, MOCK_LANG_A),
            "Per-root instance should be removed after remove_root"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_no_change_noop() -> Result<()> {
        // Identical root set produces no spawns or shutdowns.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let before = manager.clients().await.len();

        manager.sync_roots(rich(&["/tmp"])).await?;

        assert_eq!(
            manager.clients().await.len(),
            before,
            "No-change sync should not alter client count"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_shutdown_root_instances_selective() -> Result<()> {
        // Only Scope::Root instances matching the root are shut down.
        // Other roots and workspace instances are untouched.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Spawn two root-scoped instances.
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let _ = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        assert_eq!(manager.clients().await.len(), 2);

        // Shut down only /var instances.
        manager.shutdown_root_instances(Path::new("/var")).await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1, "Only /var instance should be removed");
        let remaining_key = clients.keys().next().expect("one remaining");
        assert_eq!(
            remaining_key.scope,
            Scope::Root(PathBuf::from("/tmp")),
            "/tmp instance should remain"
        );

        Ok(())
    }

    #[tokio::test]
    async fn shutdown_root_instances_drops_snapshot_entry() -> Result<()> {
        // Bug 72: a per-root teardown must drop the server's board entry, not
        // leave it as a healthy ghost with `state_since` frozen at spawn.
        let mut manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let writer = crate::state_snapshot::SnapshotWriter::with_coalesce(
            &tokio::runtime::Handle::current(),
            dir.path(),
            crate::state_snapshot::DaemonInfo {
                instance_id: "daemon:test".to_string(),
                pid: 1,
                version: "test".to_string(),
                started_at: "t0".to_string(),
            },
            Duration::from_millis(10),
        );
        manager.set_snapshot(writer.clone());

        // Two root-scoped instances; each registers a board entry on spawn.
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let _ = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        // Tear down only /var — the misc-150/151 per-root reap chokepoint.
        manager.shutdown_root_instances(Path::new("/var")).await;
        writer.flush_now();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(writer.path()).expect("read snapshot"))
                .expect("parse snapshot");
        let servers = json["servers"].as_array().expect("servers array");
        assert!(
            servers.iter().all(|s| s["scope_root"] != "/var"),
            "the reaped /var instance must leave no board entry: {servers:?}"
        );
        assert!(
            servers.iter().any(|s| s["scope_root"] == "/tmp"),
            "the surviving /tmp instance stays on the board: {servers:?}"
        );

        Ok(())
    }

    // --- ServerStatus enrichment ---

    #[tokio::test]
    async fn test_server_status_enriched() -> Result<()> {
        // status(&key) populates server_name, scope_kind, scope_root.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let locked = client.lock().await;
        let key = locked.server().key().expect("key should be set");
        let status = locked.status(&key);
        drop(locked);

        assert_eq!(status.language, MOCK_LANG_A);
        assert_eq!(status.server_name, format!("mockls-{MOCK_LANG_A}"));
        assert_eq!(status.scope_kind, "root");
        assert_eq!(status.scope_root, "/tmp");
        assert_eq!(status.state.display_state(), "initializing");
        Ok(())
    }

    #[tokio::test]
    async fn test_server_status_root_scope() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_workspace_folders_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let locked = client.lock().await;
        let key = locked.server().key().expect("key should be set");
        let status = locked.status(&key);
        drop(locked);

        assert_eq!(status.scope_kind, "root");
        assert_eq!(status.scope_root, "/tmp");
        Ok(())
    }

    #[tokio::test]
    async fn test_all_server_status_multi_instance() -> Result<()> {
        // Two instances of the same language produce two status entries
        // with different scope info.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let _ = manager
            .spawn(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        let statuses = manager.all_server_status().await;
        assert_eq!(statuses.len(), 2, "should have two status entries");

        let roots: HashSet<String> = statuses.iter().map(|s| s.scope_root.clone()).collect();
        assert!(roots.contains("/tmp"), "should include /tmp root");
        assert!(roots.contains("/var"), "should include /var root");

        for s in &statuses {
            assert_eq!(s.language, MOCK_LANG_A);
            assert_eq!(s.server_name, server_name);
            assert_eq!(s.scope_kind, "root");
        }

        Ok(())
    }

    // --- match_file_changes ---

    // --- file_matches_patterns ---

    mod file_patterns_matching {
        use super::*;
        use crate::lsp::glob::LspGlob;

        fn compile(patterns: &[&str]) -> Vec<LspGlob> {
            patterns
                .iter()
                .map(|p| LspGlob::new(p).expect("valid glob"))
                .collect()
        }

        #[test]
        fn empty_patterns_matches_all() {
            assert!(file_matches_patterns(Path::new("/tmp/test.rs"), &[]));
            assert!(file_matches_patterns(Path::new("/tmp/PKGBUILD"), &[]));
        }

        #[test]
        fn exact_filename_match() {
            let patterns = compile(&["PKGBUILD"]);
            assert!(file_matches_patterns(
                Path::new("/home/user/PKGBUILD"),
                &patterns
            ));
        }

        #[test]
        fn exact_filename_no_match() {
            let patterns = compile(&["PKGBUILD"]);
            assert!(!file_matches_patterns(
                Path::new("/home/user/script.sh"),
                &patterns
            ));
        }

        #[test]
        fn glob_extension_match() {
            let patterns = compile(&["*.ebuild"]);
            assert!(file_matches_patterns(
                Path::new("/repo/foo.ebuild"),
                &patterns
            ));
        }

        #[test]
        fn glob_extension_no_match() {
            let patterns = compile(&["*.ebuild"]);
            assert!(!file_matches_patterns(Path::new("/repo/foo.rs"), &patterns));
        }

        #[test]
        fn multiple_patterns_any_match() {
            let patterns = compile(&["PKGBUILD", "*.ebuild"]);
            assert!(file_matches_patterns(
                Path::new("/repo/PKGBUILD"),
                &patterns
            ));
            assert!(file_matches_patterns(
                Path::new("/repo/foo.ebuild"),
                &patterns
            ));
            assert!(!file_matches_patterns(
                Path::new("/repo/script.sh"),
                &patterns
            ));
        }

        #[test]
        fn no_filename_returns_false() {
            // A path that is just "/" has no file_name component.
            let patterns = compile(&["*"]);
            assert!(!file_matches_patterns(Path::new("/"), &patterns));
        }

        #[test]
        fn star_does_not_cross_separator() {
            // LspGlob uses literal_separator(true): * should not match paths.
            let patterns = compile(&["*.rs"]);
            // "foo.rs" matches
            assert!(file_matches_patterns(Path::new("/tmp/foo.rs"), &patterns));
            // "src/foo.rs" as a single filename component would not occur,
            // but matching against just the filename means this works normally.
            assert!(file_matches_patterns(
                Path::new("/project/src/foo.rs"),
                &patterns
            ));
        }
    }

    // --- get_servers ---

    #[tokio::test]
    async fn test_get_servers_single_server() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        // Pre-spawn the server
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        // Use a capability that mockls supports (document symbols — all mockls
        // instances advertise it).
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_capability_filter() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        // Use a capability that mockls does NOT support (pull diagnostics
        // requires --pull-diagnostics flag which mockls_config doesn't set).
        let servers = manager
            .get_servers(&path, LspServer::supports_pull_diagnostics, None)
            .await;
        assert!(
            servers.is_empty(),
            "mockls (default) does not support pull diagnostics, should return empty"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_match() -> Result<()> {
        // file_patterns filters within the language. Use a pattern that
        // matches the filename of a file with the mock extension.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fp");
        let pattern = "special.*".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Filename "special.yX4Za" matches pattern "special.*"
        let path = PathBuf::from(format!("/tmp/special.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "special.{MOCK_LANG_A} should match file_patterns=[\"special.*\"]"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_no_match() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fp2");
        let pattern = "special.*".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Filename "other.yX4Za" does NOT match pattern "special.*"
        let path = PathBuf::from(format!("/tmp/other.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert!(
            servers.is_empty(),
            "other.{MOCK_LANG_A} should not match file_patterns=[\"special.*\"]"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_file_patterns_glob() -> Result<()> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-fpg");
        let pattern = format!("*.{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                initialization_options: None,
                settings: None,
                min_severity: None,
                file_patterns: vec![pattern.clone()],
                compiled_patterns: vec![
                    crate::lsp::glob::LspGlob::new(&pattern).expect("valid glob"),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/foo.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1, "*.ext glob should match");
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_empty_file_patterns() -> Result<()> {
        // Server with no file_patterns matches all files for the language.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/anything.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "empty file_patterns should match all files"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_revives_dead_server_on_demand() -> Result<()> {
        // misc 167: a server killed while up leaves its tombstone on the map;
        // the next demand that routes to it revives it (the pkill-then-
        // diagnose live-fire shape). Before misc 167 this returned empty
        // forever ("skipped: server not alive").
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server (the tombstone stays in the map).
        client.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1, "the dead server is revived on demand");
        let revived = servers.first().expect("one revived server");
        assert!(
            revived.lock().await.is_alive(),
            "the revived instance is live"
        );
        assert!(
            !Arc::ptr_eq(&client, revived),
            "a fresh instance answers, not the tombstone"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_benches_after_three_crash_strikes() -> Result<()> {
        // misc 167: each observed death charges +1 with no served work to
        // offset it; the third death benches the instance — demand stops
        // reviving, the tombstone stays as evidence, and no further spawn
        // occurs (strikes-exhaust-honestly).
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );
        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));

        let mut current = ensure_first_server(&manager, MOCK_LANG_A).await?;
        for round in 1..=2u8 {
            current.lock().await.shutdown().await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let servers = manager
                .get_servers(&path, LspServer::supports_document_symbols, None)
                .await;
            assert_eq!(servers.len(), 1, "strike {round}: still revivable");
            current = servers.into_iter().next().expect("revived instance");
        }

        // Third death: the demand charges the third strike and benches.
        current.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert!(servers.is_empty(), "three strikes: benched, no revive");

        // The tombstone is retained as evidence, and a repeat demand does not
        // respawn behind the bench (same Arc across calls).
        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1, "the benched tombstone is retained");
        let tombstone = clients.values().next().expect("tombstone").clone();
        let again = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert!(again.is_empty(), "benched stays benched");
        let clients_again = manager.clients().await;
        let tombstone_again = clients_again.values().next().expect("tombstone").clone();
        assert!(
            Arc::ptr_eq(&tombstone, &tombstone_again),
            "no respawn behind the bench"
        );

        // The unavailable surface names the benched server with the ticket's
        // terminal cause: it crashed with zero served requests, so the label
        // axis reads "never started".
        let unavailable = manager.unavailable_diagnostic_servers(&path).await;
        assert_eq!(unavailable.len(), 1, "the benched server is named");
        assert_eq!(unavailable[0].1, ReviveVerdict::BenchedNeverStarted);
        Ok(())
    }

    #[tokio::test]
    async fn test_retired_root_stays_dead_no_revive() -> Result<()> {
        // bug 93 guard: retirement removes the root's instances and clears
        // its ledger — later demand never revives (or spawns) for it.
        let dir = tempfile::tempdir()?;
        let root_path = dir.path().to_path_buf();
        let root = root_path.to_string_lossy().to_string();
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);

        // Retire the root.
        let removed = manager.sync_roots(rich(&[])).await?;
        assert_eq!(removed, vec![root_path.clone()]);
        assert!(
            manager.clients().await.is_empty(),
            "retirement shuts the root's instances down"
        );

        let path = root_path.join(format!("test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert!(servers.is_empty(), "no server answers for a retired root");
        assert!(
            manager.clients().await.is_empty(),
            "no revive or spawn happens for a retired root"
        );
        Ok(())
    }

    /// Config whose mockls rejects `initialize` (the julia/r
    /// dies-at-handshake class) — every spawn attempt fails init.
    fn mockls_failing_init_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--fail-on".to_string(),
                    "initialize".to_string(),
                ],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            ..test_config_raw()
        })
    }

    #[tokio::test]
    async fn test_init_failing_server_benches_after_three_attempts() -> Result<()> {
        // misc 167 "broken = benched" subsumption: a server that cannot
        // initialize racks +1 per failed attempt with no served work, reaches
        // the cap in three cheap failures, and is benched with the
        // `never started` terminal cause.
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let manager = LspClientManager::new(
            mockls_failing_init_config(),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );
        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));

        // Attempt 1 (first spawn): init fails → strike 1, charged tombstone.
        assert!(
            ensure_first_server(&manager, MOCK_LANG_A).await.is_err(),
            "the handshake-rejecting server fails its first spawn"
        );
        // Attempts 2 and 3 (demand revives): init fails again → strikes 2, 3.
        for round in 2..=3u8 {
            let servers = manager
                .get_servers(&path, LspServer::supports_document_symbols, None)
                .await;
            assert!(servers.is_empty(), "attempt {round} fails init");
        }

        // Benched: the tombstone Arc stays identical across further demand —
        // no fourth spawn attempt.
        let tombstone = manager
            .clients()
            .await
            .values()
            .next()
            .cloned()
            .expect("a failed-init tombstone is retained");
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert!(servers.is_empty(), "benched: no revive");
        let tombstone_again = manager
            .clients()
            .await
            .values()
            .next()
            .cloned()
            .expect("tombstone retained");
        assert!(
            Arc::ptr_eq(&tombstone, &tombstone_again),
            "no spawn once benched"
        );

        // Honest surface: named with the never-started terminal cause.
        let unavailable = manager.unavailable_diagnostic_servers(&path).await;
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].1, ReviveVerdict::BenchedNeverStarted);
        Ok(())
    }

    #[test]
    fn strike_ledger_decays_on_service_and_labels_unstable() {
        // misc 167 unit: served work pays a strike down (activity-driven
        // decay), and a cap reached AFTER serving carries the `unstable`
        // cause, not `never started`.
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let key = InstanceKey::new(
            "rust".to_string(),
            "ra".to_string(),
            Scope::Root(PathBuf::from("/p")),
        );
        assert!(manager.revive_verdict(&key).is_revivable());

        manager.record_server_strike(&key);
        manager.record_server_strike(&key);
        assert!(
            manager.revive_verdict(&key).is_revivable(),
            "two strikes: still revivable"
        );

        manager.record_server_service(&key);
        manager.record_server_strike(&key);
        assert!(
            manager.revive_verdict(&key).is_revivable(),
            "the served credit kept it below the cap"
        );

        manager.record_server_strike(&key);
        assert_eq!(
            manager.revive_verdict(&key),
            ReviveVerdict::BenchedUnstable,
            "served before striking out: unstable, not never-started"
        );

        // The counter clamps at the cap; service on a fresh key is a no-op
        // (the ledger stays sparse for healthy servers).
        manager.record_server_strike(&key);
        assert_eq!(manager.revive_verdict(&key), ReviveVerdict::BenchedUnstable);
        let fresh = InstanceKey::new(
            "rust".to_string(),
            "ra".to_string(),
            Scope::Root(PathBuf::from("/q")),
        );
        manager.record_server_service(&fresh);
        assert!(manager.revive_verdict(&fresh).is_revivable());
        assert!(!manager.strikes_recorded(&fresh), "no entry allocated");
    }

    #[test]
    fn contract_violation_feeds_the_same_strike_ledger() {
        // diagnostics-debt 05: a verified-contract violation (a blessed server
        // whose discipline owed an answer this round and gave none) feeds the
        // SAME strike ledger a crash does — the same `+1`, the same bench at the
        // cap, the same pay-down on the next served round. No rival ledger.
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let key = InstanceKey::new(
            "typescript".to_string(),
            "typescript-language-server".to_string(),
            Scope::Root(PathBuf::from("/p")),
        );
        assert!(manager.revive_verdict(&key).is_revivable());

        // A violation records a strike, exactly like a crash.
        manager.record_contract_violation(&key);
        assert!(
            manager.strikes_recorded(&key),
            "the violation struck the ledger"
        );
        assert!(
            manager.revive_verdict(&key).is_revivable(),
            "one strike: still revivable"
        );

        // A served round pays it back down — the same ledger a crash feeds.
        manager.record_server_service(&key);
        assert!(
            !manager.strikes_recorded(&key),
            "served work paid the violation strike down — one ledger, not two"
        );

        // Repeated violations bench at the cap. This key has served once (the
        // pay-down above set `ever_served`), so the terminal cause is `unstable`
        // — the same axis a crashing-after-serving server lands on.
        for _ in 0..MAX_SERVER_STRIKES {
            manager.record_contract_violation(&key);
        }
        assert_eq!(
            manager.revive_verdict(&key),
            ReviveVerdict::BenchedUnstable,
            "chronic contract violations bench the server the same way crashes do"
        );
    }

    #[test]
    fn strike_ledger_clears_for_retired_root_only() {
        // misc 167 / bug 93: retirement clears the retired root's entries —
        // a remount starts fresh — while other roots keep their history.
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let key_a = InstanceKey::new(
            "rust".to_string(),
            "ra".to_string(),
            Scope::Root(PathBuf::from("/a")),
        );
        let key_b = InstanceKey::new(
            "rust".to_string(),
            "ra".to_string(),
            Scope::Root(PathBuf::from("/b")),
        );
        for _ in 0..3 {
            manager.record_server_strike(&key_a);
            manager.record_server_strike(&key_b);
        }
        assert!(!manager.revive_verdict(&key_a).is_revivable());
        assert!(!manager.revive_verdict(&key_b).is_revivable());

        manager.clear_strikes_for_root(Path::new("/a"));
        assert!(
            manager.revive_verdict(&key_a).is_revivable(),
            "the retired root's slate is clean"
        );
        assert!(
            !manager.revive_verdict(&key_b).is_revivable(),
            "the other root keeps its bench"
        );
    }

    // ── Not-installed classification (misc 210) ──────────────────────────

    /// A config whose `MOCK_LANG_A` binds a diagnostics server pointing at
    /// `program` — used with a bogus key or a nonexistent `path` to exercise the
    /// not-installed pre-spawn classification (misc 210). When `path_override` is
    /// set, the binary resolves through that concrete path (so a test can create
    /// it mid-flight to heal); otherwise `program` is the server key resolved on
    /// PATH.
    fn not_installed_config(program: &str, path_override: Option<&str>) -> Arc<Config> {
        let mut server = HashMap::new();
        server.insert(
            program.to_string(),
            ServerDef {
                path: path_override.map(str::to_string),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(program)]),
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            ..test_config_raw()
        })
    }

    #[test]
    fn not_installed_message_teaches_both_exits_only_with_a_recipe() {
        // The maintainer addendum: with the flag UNSET, a blessed-recipe server
        // names both auto-heal exits (the one-shot install and the standing
        // auto_install opt-in); a recipe-less server gets only the honest half —
        // never a suggestion auto_install cannot fulfill.
        let with_recipe = not_installed_message("tombi", "toml", &AutoInstallStance::OfferFlag);
        assert!(
            with_recipe.contains("catenary install tombi"),
            "recipe variant names the one-shot install: {with_recipe}"
        );
        assert!(
            with_recipe.contains("auto_install = true"),
            "recipe variant names the standing opt-in: {with_recipe}"
        );
        assert!(
            with_recipe.contains("configured for toml but not installed"),
            "recipe variant keeps the honest half: {with_recipe}"
        );

        let no_recipe = not_installed_message("mycustomls", "toml", &AutoInstallStance::NoRecipe);
        assert!(
            no_recipe.contains("configured for toml but not installed"),
            "recipe-less variant is honest: {no_recipe}"
        );
        assert!(
            !no_recipe.contains("auto_install"),
            "recipe-less variant must not suggest auto_install: {no_recipe}"
        );
        assert!(
            !no_recipe.contains("catenary install"),
            "recipe-less variant must not suggest an install it cannot fulfill: {no_recipe}"
        );
    }

    #[test]
    fn not_installed_message_announces_a_fired_kick_instead_of_advising() {
        // bug 148: with the flag already on and the install kicked, the finding
        // reports what is happening — it never recommends the setting that is
        // already set.
        let kicked = not_installed_message(
            "tombi",
            "toml",
            &AutoInstallStance::Kicked {
                version: "1.2.4".to_string(),
                class: InstallClass::Fetch,
            },
        );
        assert!(
            kicked.contains("installing tombi 1.2.4 in the background"),
            "the kick is announced: {kicked}"
        );
        assert!(
            !kicked.contains("auto_install = true"),
            "a fired kick never recommends the flag: {kicked}"
        );
        assert!(
            !kicked.contains("take minutes"),
            "fetch-class is quick — no compile warning: {kicked}"
        );

        // Compile-class states its minutes, the same honesty as the
        // session-start announcement.
        let compiling = not_installed_message(
            "tombi",
            "toml",
            &AutoInstallStance::Kicked {
                version: "1.2.4".to_string(),
                class: InstallClass::Compile,
            },
        );
        assert!(
            compiling.contains("can take minutes"),
            "compile-class warns: {compiling}"
        );

        // An install already running (the eager leg got there first) says so.
        let in_flight = not_installed_message(
            "tombi",
            "toml",
            &AutoInstallStance::InFlight {
                version: "1.2.4".to_string(),
            },
        );
        assert!(
            in_flight.contains("already running"),
            "a running install is reported: {in_flight}"
        );
        assert!(
            !in_flight.contains("auto_install = true"),
            "never recommends the flag that is already on: {in_flight}"
        );
    }

    #[test]
    fn not_installed_message_never_recommends_an_enabled_flag_that_cannot_act() {
        // bug 148's honesty leg: flag on, server ineligible — say why nothing
        // can act, briefly, and point at the only exit that works.
        let blocked = not_installed_message(
            "mycustomls",
            "toml",
            &AutoInstallStance::Blocked {
                reason: "no blessed, version-matched install recipe pins mycustomls".to_string(),
            },
        );
        assert!(
            blocked.contains("configured for toml but not installed"),
            "honest first: {blocked}"
        );
        assert!(
            blocked.contains("no blessed, version-matched install recipe"),
            "the reason nothing can act is named: {blocked}"
        );
        assert!(
            !blocked.contains("auto_install = true"),
            "never recommends a flag that is already set: {blocked}"
        );
        assert!(
            blocked.contains("place it on PATH"),
            "the exit that does work is named: {blocked}"
        );
    }

    // ── The demand-driven (JIT) auto-install leg (bug 148) ───────────────

    /// A config binding `MOCK_LANG_A` to `server` with `[servers]
    /// auto_install` as asked — the JIT-leg fixture. `path_override` and
    /// `prefer_managed` drive the ineligibility branches.
    fn jit_config(
        server: &str,
        auto_install: bool,
        prefer_managed: bool,
        path_override: Option<&str>,
    ) -> Arc<Config> {
        let mut defs = HashMap::new();
        defs.insert(
            server.to_string(),
            ServerDef {
                path: path_override.map(str::to_string),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server)]),
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server: defs,
            servers: Some(crate::config::ServersConfig {
                prefer_managed,
                auto_install,
            }),
            ..test_config_raw()
        })
    }

    /// The stub-seamed installer the JIT tests attach: the auto-install
    /// module's own scaffolding (synthetic blessed manifest + version-matched
    /// recipe for `auto_install::test_support::SERVER`) over a tempdir managed
    /// home, with a staging runner so a landed install is observable.
    fn jit_installer(
        home_root: &Path,
        runs: Arc<std::sync::atomic::AtomicUsize>,
        gate: Option<Arc<tokio::sync::Semaphore>>,
    ) -> crate::auto_install::AutoInstaller {
        crate::auto_install::test_support::installer_with_runner(
            home_root,
            Box::new(crate::auto_install::test_support::StagingRunner {
                home_root: home_root.to_path_buf(),
                gate,
                runs,
            }),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_install_stance_kicks_at_the_spawn_failure_then_reports_in_flight() {
        // bug 148's JIT leg: the flag is on and every gate clears, so the
        // ground-truth "wants but cannot spawn" signal kicks the background
        // install itself. A second resolution while that install runs reports
        // it honestly rather than kicking a duplicate.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = crate::auto_install::test_support::SERVER;
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let runs = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let manager = Arc::new(LspClientManager::new(
            jit_config(server, true, true, None),
            test_logging(),
            test_fs(),
        ));
        manager.attach_auto_installer(jit_installer(&home_root, runs.clone(), Some(gate.clone())));

        let def = ServerDef::default();
        let stance = manager.auto_install_stance(server, &def);
        assert!(
            matches!(&stance, AutoInstallStance::Kicked { version, .. } if version == crate::auto_install::test_support::VERSION),
            "the spawn failure kicks the install: {stance:?}"
        );

        // The install is still gated, so its in-flight seat is held: a second
        // resolution reports the running install instead of kicking again.
        let again = manager.auto_install_stance(server, &def);
        assert!(
            matches!(&again, AutoInstallStance::InFlight { .. }),
            "a second demand never double-kicks: {again:?}"
        );

        // Release the gate and let the one install land (also drains the
        // blocking task before the runtime is dropped).
        gate.add_permits(1);
        for _ in 0..200 {
            if runs.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "exactly one install ran");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_install_stance_blocks_honestly_when_the_flag_cannot_act() {
        // bug 148: with the flag ALREADY on, an ineligible server must never be
        // told to set it — each branch names why nothing can act instead. The
        // gates mirror `detect_missing`'s, one for one.
        use std::sync::atomic::AtomicUsize;
        let server = crate::auto_install::test_support::SERVER;
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let def_with_path = ServerDef {
            path: Some("/nowhere/mine".to_string()),
            ..ServerDef::default()
        };

        // An explicit `path` override is the user's own resolution.
        let manager = Arc::new(LspClientManager::new(
            jit_config(server, true, true, Some("/nowhere/mine")),
            test_logging(),
            test_fs(),
        ));
        manager.attach_auto_installer(jit_installer(
            &home_root,
            Arc::new(AtomicUsize::new(0)),
            None,
        ));
        let stance = manager.auto_install_stance(server, &def_with_path);
        assert!(
            matches!(&stance, AutoInstallStance::Blocked { reason } if reason.contains("path")),
            "a path override blocks the kick: {stance:?}"
        );

        // `prefer_managed = false`: a landed install would never be consulted.
        let manager = Arc::new(LspClientManager::new(
            jit_config(server, true, false, None),
            test_logging(),
            test_fs(),
        ));
        manager.attach_auto_installer(jit_installer(
            &home_root,
            Arc::new(AtomicUsize::new(0)),
            None,
        ));
        let stance = manager.auto_install_stance(server, &ServerDef::default());
        assert!(
            matches!(&stance, AutoInstallStance::Blocked { reason } if reason.contains("prefer_managed")),
            "prefer_managed = false blocks the kick: {stance:?}"
        );

        // No blessed, version-matched recipe: nothing is constructible.
        let manager = Arc::new(LspClientManager::new(
            jit_config("not-a-blessed-server", true, true, None),
            test_logging(),
            test_fs(),
        ));
        manager.attach_auto_installer(jit_installer(
            &home_root,
            Arc::new(AtomicUsize::new(0)),
            None,
        ));
        let stance = manager.auto_install_stance("not-a-blessed-server", &ServerDef::default());
        assert!(
            matches!(&stance, AutoInstallStance::Blocked { reason } if reason.contains("no blessed")),
            "an unblessed server blocks the kick: {stance:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auto_install_stance_offers_the_flag_only_while_it_is_unset() {
        // The surviving advice branch: the flag is genuinely off, so the
        // standing opt-in is worth teaching — and only for a server it could
        // actually fetch.
        use std::sync::atomic::AtomicUsize;
        let server = crate::auto_install::test_support::SERVER;
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let manager = Arc::new(LspClientManager::new(
            jit_config(server, false, true, None),
            test_logging(),
            test_fs(),
        ));
        manager.attach_auto_installer(jit_installer(
            &home_root,
            Arc::new(AtomicUsize::new(0)),
            None,
        ));

        assert_eq!(
            manager.auto_install_stance(server, &ServerDef::default()),
            AutoInstallStance::OfferFlag,
            "a blessed, recipe-backed server gets the standing opt-in"
        );
        assert_eq!(
            manager.auto_install_stance("not-a-blessed-server", &ServerDef::default()),
            AutoInstallStance::NoRecipe,
            "nothing installable — never a suggestion auto-install cannot fulfill"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_failure_kicks_the_background_install_exactly_once() -> Result<()> {
        // The whole JIT leg end to end: repeated spawn demands for a missing
        // blessed server kick exactly ONE background install — the
        // already-surfaced dedupe holds the second and third attempts — and the
        // install lands in the managed home.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = crate::auto_install::test_support::SERVER;
        let version = crate::auto_install::test_support::VERSION;
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let home_dir = tempfile::tempdir()?;
        let home_root = home_dir.path().join("servers");
        let runs = Arc::new(AtomicUsize::new(0));
        let manager = Arc::new(LspClientManager::new(
            jit_config(server, true, true, None),
            test_logging(),
            test_fs_with_roots(&[&root]),
        ));
        manager.attach_auto_installer(jit_installer(&home_root, runs.clone(), None));

        // Three spawn demands against the same missing server. The Ok variant
        // (`LspClient`) is not `Debug`, so take the error via `.err()`.
        for _ in 0..3 {
            let err = manager
                .spawn(server, MOCK_LANG_A, dir.path())
                .await
                .err()
                .expect("a missing binary never spawns");
            assert!(
                is_not_installed(&err),
                "each attempt classifies not-installed"
            );
        }
        assert!(
            manager.not_installed_was_warned(server),
            "the calm finding fired once"
        );

        for _ in 0..200 {
            if runs.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "one kick across repeated spawn demands"
        );
        assert!(
            crate::managed_home::ManagedHome::at(home_root)
                .pinned_executable(server, version, server)
                .is_some(),
            "the JIT install landed in the managed home at the pin"
        );
        Ok(())
    }

    #[tokio::test]
    async fn no_attached_installer_never_kicks_and_says_so() {
        // Doctor/CLI/test managers wire no installer: the flag being on cannot
        // conjure one, and the finding says that instead of advising the flag.
        let server = crate::auto_install::test_support::SERVER;
        let manager = Arc::new(LspClientManager::new(
            jit_config(server, true, true, None),
            test_logging(),
            test_fs(),
        ));
        let stance = manager.auto_install_stance(server, &ServerDef::default());
        assert!(
            matches!(&stance, AutoInstallStance::Blocked { reason } if reason.contains("background installer")),
            "no installer wired, nothing kicked: {stance:?}"
        );
    }

    #[test]
    fn has_blessed_recipe_matches_the_auto_install_gate() {
        // taplo is blessed and shipped with a version-matched recipe (the
        // recipe-teaching axis is true); a bare mock name has neither.
        assert!(
            has_blessed_recipe("taplo"),
            "taplo is a blessed, recipe-backed server"
        );
        assert!(
            !has_blessed_recipe("mockls-not-a-real-server"),
            "an unknown server has no blessed recipe"
        );
    }

    #[tokio::test]
    async fn not_installed_burns_no_strikes_and_no_bench() -> Result<()> {
        // misc 210: a configured-but-uninstalled server (bogus PATH key, no
        // `path` override) is classified pre-spawn — the spawn bails with the
        // typed NotInstalled error, records ZERO strikes, and leaves the ledger
        // untouched (never a bench).
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let program = "definitely-not-on-path-mockls-xyz";
        let manager = LspClientManager::new(
            not_installed_config(program, None),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );

        // The Ok variant (`LspClient`) is not `Debug`, so take the error via
        // `.err()` rather than `expect_err`.
        let err = ensure_first_server(&manager, MOCK_LANG_A)
            .await
            .err()
            .expect("a missing binary must not spawn");
        assert!(
            is_not_installed(&err),
            "the bail is the typed NotInstalled classification, got: {err}"
        );

        // No strike ledger entry — a missing binary is static, not a failure.
        let key = InstanceKey::new(
            MOCK_LANG_A.to_string(),
            program.to_string(),
            Scope::Root(dir.path().to_path_buf()),
        );
        assert!(
            !manager.strikes_recorded(&key),
            "not-installed records no strike"
        );
        assert!(
            manager.revive_verdict(&key).is_revivable(),
            "not-installed never benches the ledger"
        );
        // No tombstone: the client map stays empty (a not-installed server never
        // spawned a process to leave behind).
        assert!(
            manager.clients().await.is_empty(),
            "not-installed leaves no dead tombstone"
        );
        // The finding fired exactly once (the per-server dedupe is armed).
        assert!(
            manager.not_installed_was_warned(program),
            "the calm finding fired for the server"
        );
        Ok(())
    }

    #[tokio::test]
    async fn not_installed_finding_is_one_per_server_not_per_root() -> Result<()> {
        // misc 210: one calm finding per server, not per root or per attempt. A
        // second root and a repeat demand both find the dedupe already armed.
        let dir_a = tempfile::tempdir()?;
        let dir_b = tempfile::tempdir()?;
        let root_a = dir_a.path().to_string_lossy().to_string();
        let root_b = dir_b.path().to_string_lossy().to_string();
        let program = "definitely-not-on-path-mockls-multi";
        let manager = LspClientManager::new(
            not_installed_config(program, None),
            test_logging(),
            test_fs_with_roots(&[&root_a, &root_b]),
        );

        // Two roots, two spawn demands, plus a repeat against the first root.
        // The Ok variant (`LspClient`) is not `Debug`, so take the error via
        // `.err()`.
        for root in [dir_a.path(), dir_b.path(), dir_a.path()] {
            let err = manager
                .spawn(program, MOCK_LANG_A, root)
                .await
                .err()
                .expect("missing binary never spawns");
            assert!(
                is_not_installed(&err),
                "each attempt classifies not-installed"
            );
        }
        // The dedupe holds a single server-name entry regardless of root/attempt.
        assert!(manager.not_installed_was_warned(program));
        Ok(())
    }

    #[tokio::test]
    async fn not_installed_heals_on_install_without_a_restart() -> Result<()> {
        // misc 210: installing the binary heals coverage without a daemon
        // restart — a later spawn demand re-resolves, and with the binary now
        // present the spawn proceeds normally (the opposite of the strike
        // bench's "until restart or remount"). Simulated by a `path` override
        // that starts missing and is created (a real mockls) mid-flight.
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let program = format!("mockls-{MOCK_LANG_A}-heal");
        let binary = dir.path().join("not-yet-installed");
        let manager = LspClientManager::new(
            not_installed_config(&program, Some(&binary.to_string_lossy())),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );

        // Before install: the override points at a nonexistent file → not
        // installed, no strikes, no tombstone. The Ok variant (`LspClient`) is
        // not `Debug`, so take the error via `.err()`.
        let err = manager
            .spawn(&program, MOCK_LANG_A, dir.path())
            .await
            .err()
            .expect("missing binary never spawns");
        assert!(is_not_installed(&err));
        let key = InstanceKey::new(
            MOCK_LANG_A.to_string(),
            program.clone(),
            Scope::Root(dir.path().to_path_buf()),
        );
        assert!(!manager.strikes_recorded(&key), "no strike before install");
        assert!(manager.not_installed_was_warned(&program));

        // The install lands: copy the real mockls to the override path.
        std::fs::copy(mockls_bin(), &binary).expect("stage the installed binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&binary).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&binary, perms).expect("chmod +x");
        }

        // The next spawn demand re-resolves and now spawns normally — no daemon
        // restart, and the not-installed dedupe clears (re-arming a future
        // removal). `?` propagates any spawn failure (the Ok tuple holds a
        // non-`Debug` client, so no `expect` on it).
        let (_key, client) = manager.spawn(&program, MOCK_LANG_A, dir.path()).await?;
        assert!(client.lock().await.is_alive(), "the healed server is live");
        assert!(
            !manager.not_installed_was_warned(&program),
            "the classification clears on heal — a future removal can warn again"
        );
        Ok(())
    }

    #[tokio::test]
    async fn not_installed_snapshot_state_is_honest_not_initializing() -> Result<()> {
        // misc 210: the board entry for a not-installed server reads
        // "not-installed" — never the old "initializing" + benched
        // contradiction, and never a bench label.
        let dir = tempfile::tempdir()?;
        let snap_dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let program = "definitely-not-on-path-mockls-snap";
        let mut manager = LspClientManager::new(
            not_installed_config(program, None),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );
        let writer = crate::state_snapshot::SnapshotWriter::with_coalesce(
            &tokio::runtime::Handle::current(),
            snap_dir.path(),
            crate::state_snapshot::DaemonInfo {
                instance_id: "daemon:test".to_string(),
                pid: 1,
                version: "test".to_string(),
                started_at: "t0".to_string(),
            },
            Duration::from_millis(10),
        );
        manager.set_snapshot(writer.clone());

        // The Ok variant (`LspClient`) is not `Debug`, so take the error via
        // `.err()`.
        let err = manager
            .spawn(program, MOCK_LANG_A, dir.path())
            .await
            .err()
            .expect("missing binary never spawns");
        assert!(is_not_installed(&err));
        writer.flush_now();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(writer.path()).expect("read snapshot"))
                .expect("parse snapshot");
        let servers = json["servers"].as_array().expect("servers array");
        let entry = servers
            .iter()
            .find(|s| s["server"] == program)
            .expect("the not-installed server has a board entry");
        assert_eq!(
            entry["state"], "not-installed",
            "the board reads not-installed, never initializing: {entry}"
        );
        assert!(
            entry.get("benched").is_none() || entry["benched"].is_null(),
            "not-installed is never a bench: {entry}"
        );
        assert!(
            entry.get("strikes").is_none() || entry["strikes"] == 0,
            "not-installed records no strikes: {entry}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn not_installed_surfaces_on_the_unavailable_receipt() -> Result<()> {
        // misc 210 / decision 027: a configured-but-uninstalled diagnostics
        // server is a coverage degradation, not a bare `[no LSP coverage]`. The
        // unavailable surface names it with the dedicated NotInstalled cause so
        // the receipt teaches "install it", not "keeps dying".
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let program = "definitely-not-on-path-mockls-receipt";
        let manager = LspClientManager::new(
            not_installed_config(program, None),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );
        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "x").expect("write a covered file");

        let unavailable = manager.unavailable_diagnostic_servers(&path).await;
        assert_eq!(unavailable.len(), 1, "the not-installed server is named");
        assert_eq!(unavailable[0].0, program);
        assert_eq!(
            unavailable[0].1,
            ReviveVerdict::NotInstalled,
            "typed with the not-installed cause, distinct from the benched arms"
        );
        Ok(())
    }

    #[tokio::test]
    async fn not_installed_clears_when_auto_install_prewarms() -> Result<()> {
        // misc 210 interaction: when auto_install lands the binary, the router
        // runs `spawn_all` as the pre-warm. That re-probe re-resolves the now-
        // present binary and heals coverage — the not-installed classification
        // clears naturally, no fight with the install announcement, no restart.
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_string_lossy().to_string();
        let program = format!("mockls-{MOCK_LANG_A}-prewarm");
        let binary = dir.path().join("landed-by-auto-install");
        // A real file of the language so `spawn_all`'s detection fires.
        std::fs::write(dir.path().join(format!("a.{MOCK_LANG_A}")), "x").expect("write lang file");
        let manager = LspClientManager::new(
            not_installed_config(&program, Some(&binary.to_string_lossy())),
            test_logging(),
            test_fs_with_roots(&[&root]),
        );

        // Boot pre-warm: the binary is absent → not-installed, no spawn.
        manager.spawn_all().await;
        assert!(
            manager.clients().await.is_empty(),
            "no server spawns while the binary is missing"
        );
        assert!(
            manager.not_installed_was_warned(&program),
            "the not-installed finding fired once at boot"
        );

        // Auto-install lands the binary (its managed home would resolve the
        // same way; here the concrete override path is created).
        std::fs::copy(mockls_bin(), &binary).expect("stage the landed binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&binary).expect("metadata").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&binary, perms).expect("chmod +x");
        }

        // The router's post-install pre-warm: the same `spawn_all` the pin path
        // runs. The re-probe finds the binary and spawns it.
        manager.spawn_all().await;
        assert_eq!(
            manager.clients().await.len(),
            1,
            "the landed install's pre-warm spawns the healed server"
        );
        assert!(
            !manager.not_installed_was_warned(&program),
            "the pre-warm cleared the not-installed classification"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_disabled_methods() -> Result<()> {
        // disabled_methods on the binding suppresses the server for that method.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding {
                    name: server_name,
                    diagnostics: true,
                    disabled_methods: vec![DispatchMethod::References],
                }]),
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        });

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));

        // Method that is disabled — should return empty.
        let servers = manager
            .get_servers(
                &path,
                LspServer::supports_references,
                Some(DispatchMethod::References),
            )
            .await;
        assert!(
            servers.is_empty(),
            "disabled method should suppress the server"
        );

        // Different method — should return the server.
        let servers = manager
            .get_servers(
                &path,
                LspServer::supports_document_symbols,
                Some(DispatchMethod::DocumentSymbol),
            )
            .await;
        assert_eq!(
            servers.len(),
            1,
            "non-disabled method should still return the server"
        );

        // No method (diagnostics path) — should return the server.
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "None method should bypass disabled_methods check"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_outside_roots_spawns_single_file() {
        // Files outside all roots get a single-file server (tier 3)
        // when the server is configured with single_file = true.
        let manager = LspClientManager::new(
            mockls_single_file_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/other/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "file outside roots should get single-file server"
        );

        // Verify it's a SingleFile instance.
        let clients = manager.clients().await;
        assert!(
            clients
                .keys()
                .any(|k| k.scope == Scope::SingleFile && k.language_id == MOCK_LANG_A),
            "should have a single-file instance"
        );
    }

    #[tokio::test]
    async fn test_get_servers_unknown_language() {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let servers = manager
            .get_servers(
                Path::new("/tmp/test.xyz"),
                LspServer::supports_references,
                None,
            )
            .await;
        assert!(servers.is_empty(), "unknown language should return empty");
    }

    #[tokio::test]
    async fn test_get_servers_priority_order() -> Result<()> {
        // With multiple servers in the binding, result preserves order.
        // (Currently only one server per language is spawned, so this test
        // exercises the path ordering with a single entry — 1c-01 will
        // extend it to multiple.)
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        Ok(())
    }

    // --- Multi-server spawning (1c-01) ---

    #[tokio::test]
    async fn test_spawn_all_multi_server() -> Result<()> {
        // Two workspace-capable servers for one language: spawn_all creates
        // two entries in the client map with different InstanceKeys.
        let config = mockls_multi_server_workspace_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .iter()
            .map(|b| b.name.clone())
            .collect();
        assert_eq!(bindings.len(), 2);

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // spawn_all won't detect files (synthetic extension), so spawn directly
        // using the same pattern spawn_all uses.
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, Path::new("/tmp"))
                .await?;
        }

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Two servers should produce two client map entries"
        );

        let server_names: HashSet<String> = clients.keys().map(|k| k.server.clone()).collect();
        assert!(server_names.contains(&bindings[0]));
        assert!(server_names.contains(&bindings[1]));
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_multi_server_legacy() -> Result<()> {
        // Two legacy servers, two roots: 2 servers × 2 roots = 4 instances.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // Simulate spawn_all's multi-server + per-root logic.
        let roots = manager.roots();
        for name in &bindings {
            let client = manager.ensure_server(MOCK_LANG_A, name, &roots[0]).await?;
            let key = client.lock().await.server().key();
            let Some(key) = key else {
                continue;
            };
            if matches!(key.scope, Scope::Root(_)) {
                for root in &roots[1..] {
                    manager.spawn(name, MOCK_LANG_A, root).await?;
                }
            }
        }

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 4, "2 legacy servers × 2 roots = 4 instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 4);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_mixed_capability() -> Result<()> {
        // Two servers, two roots: each server gets a per-root instance
        // = 4 total instances.
        let config = mockls_mixed_capability_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        let roots = manager.roots();
        for name in &bindings {
            for root in &roots {
                manager.ensure_server(MOCK_LANG_A, name, root).await?;
            }
        }

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            4,
            "2 servers × 2 roots = 4 per-root instances"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 4);
        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_clients_for_paths_multi_server() -> Result<()> {
        // New files trigger spawning of all servers in the binding.
        let manager = LspClientManager::new(
            mockls_multi_server_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_clients_for_paths(&paths).await;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "ensure_clients_for_paths should spawn all servers in the binding"
        );

        let server_names: HashSet<String> = clients.keys().map(|k| k.server.clone()).collect();
        assert_eq!(server_names.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_legacy_for_added_roots_multi_server() -> Result<()> {
        // Adding a root spawns per-root instances for all legacy servers.
        // Uses a tempdir with real files so detect_workspace_languages succeeds.
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");

        // Create files with the synthetic extension so language detection works.
        std::fs::write(root_a.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");
        std::fs::write(root_b.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");

        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let fs = test_fs();
        fs.set_roots(vec![root_a.path().to_path_buf()]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        // Spawn both servers for root_a.
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, root_a.path())
                .await?;
        }
        assert_eq!(manager.clients().await.len(), 2);

        // sync_roots adds root_b — both legacy servers should get root_b instances.
        manager
            .sync_roots(rich_bufs(vec![
                root_a.path().to_path_buf(),
                root_b.path().to_path_buf(),
            ]))
            .await?;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 4, "2 legacy servers × 2 roots = 4 instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 4);

        // Verify both roots are represented.
        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(root_a.path()));
        assert!(root_paths.contains(root_b.path()));
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_remove_multi_server() -> Result<()> {
        // Removing a root shuts down per-root instances for all servers.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // Spawn both servers for both roots (4 instances total).
        for name in &bindings {
            manager
                .ensure_server(MOCK_LANG_A, name, Path::new("/tmp"))
                .await?;
            manager.spawn(name, MOCK_LANG_A, Path::new("/var")).await?;
        }
        assert_eq!(manager.clients().await.len(), 4);

        // Remove /var — should shut down both servers' /var instances.
        manager.sync_roots(rich(&["/tmp"])).await?;

        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Only /tmp instances should remain after removing /var"
        );
        for key in clients.keys() {
            assert_eq!(
                key.scope,
                Scope::Root(PathBuf::from("/tmp")),
                "All remaining instances should be for /tmp"
            );
        }
        Ok(())
    }

    // --- Wait primitives (1c-02) ---

    #[tokio::test]
    async fn test_wait_ready_for_path_healthy() -> Result<()> {
        // Server reaches ready state: wait_ready_for_path returns.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;

        // If we got here, it didn't hang.
        Ok(())
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_dead() -> Result<()> {
        // Server dies: wait_ready_for_path returns (doesn't hang).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Kill the server.
        client.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;

        // If we got here, dead server didn't block.
        Ok(())
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_unrooted() {
        // File outside roots: returns immediately (no servers to wait for).
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let path = PathBuf::from(format!("/other/test.{MOCK_LANG_A}"));
        manager.wait_ready_for_path(&path).await;
    }

    #[tokio::test]
    async fn test_wait_ready_for_path_no_config() {
        // Unconfigured language: returns immediately.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        manager
            .wait_ready_for_path(Path::new("/tmp/test.xyz"))
            .await;
    }

    #[tokio::test]
    async fn test_wait_ready_all_mixed() -> Result<()> {
        // Some healthy, some dead: returns after all settle.
        let config = mockls_multi_server_config();
        let bindings: Vec<String> = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .iter()
            .map(|b| b.name.clone())
            .collect();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Spawn both servers.
        let client_a = manager
            .ensure_server(MOCK_LANG_A, &bindings[0], Path::new("/tmp"))
            .await?;
        let _client_b = manager
            .ensure_server(MOCK_LANG_A, &bindings[1], Path::new("/tmp"))
            .await?;

        // Kill one server.
        client_a.lock().await.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // wait_ready_all should still return (dead server doesn't block).
        manager.wait_ready_all().await;

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_and_wait_for_paths() -> Result<()> {
        // Spawns new servers and returns after they're ready.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        assert!(manager.clients().await.is_empty());

        let paths = vec![PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"))];
        manager.ensure_and_wait_for_paths(&paths).await;

        assert!(
            has_language(&manager.clients().await, MOCK_LANG_A),
            "ensure_and_wait_for_paths should spawn the server"
        );
        Ok(())
    }

    // --- Document lifecycle (1c-03) ---

    #[tokio::test]
    async fn test_open_document_on_single_client() -> Result<()> {
        // open_document_on returns URI + sync action, and sends didOpen.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Created after spawn, so the eager health probe never saw it — this
        // test exercises the plain first-open leg (the probe-reuse leg is
        // `eager_probe_document_stays_open_for_the_serve`).
        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let (uri, action) = manager.open_document_on(&path, &client, None, None).await?;
        assert!(uri.starts_with("file://"));
        assert!(
            matches!(action, DocSync::Open(_)),
            "first open sends didOpen, got {action:?}"
        );
        assert!(
            client.lock().await.is_document_open(&uri),
            "Client should track the document as open"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_open_document_on_second_call() -> Result<()> {
        // Held-open change gate (diagnostics-debt 01): a second call with the
        // file unchanged on disk sends NOTHING (no duplicate didOpen, no
        // didChange); a call after the disk content moved sends didChange
        // with the next real version.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Created after spawn so the eager health probe never opened it —
        // the change-gate sequence below starts from a genuinely fresh URI.
        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        let (uri1, action1) = manager.open_document_on(&path, &client, None, None).await?;
        let DocSync::Open(v1) = action1 else {
            anyhow::bail!("first open must send didOpen, got {action1:?}");
        };

        let (uri2, action2) = manager.open_document_on(&path, &client, None, None).await?;
        assert_eq!(uri1, uri2);
        assert_eq!(
            action2,
            DocSync::Unchanged,
            "an unchanged held-open document gets no sync traffic"
        );

        // Content moved on disk (same or new mtime — the hash breaks the
        // tie either way): the next call relays didChange at the next
        // real version.
        std::fs::write(&path, "content changed").expect("write");
        let (_, action3) = manager.open_document_on(&path, &client, None, None).await?;
        assert_eq!(
            action3,
            DocSync::Change(v1 + 1),
            "moved disk content relays didChange with a bumped real version"
        );
        assert!(client.lock().await.is_document_open(&uri1));
        Ok(())
    }

    #[tokio::test]
    async fn eager_probe_document_stays_open_for_the_serve() -> Result<()> {
        // Bug 133 lean 2: the eager health probe leaves its document held
        // open so the diagnose serve REUSES it instead of close→reopen. The
        // probe picks the first matching file under the root — in a minimal
        // fixture, the very file under diagnosis — and a didClose there owed
        // a clear a starved unversioned-push server could flush late and out
        // of order onto the serve's fresher verdict. Every close path drops
        // the held-open registry entry together with its didClose, so the
        // entry's survival after spawn is the proof no didClose was sent.
        // Default mockls publishes on the probe's didOpen, so the evidence
        // is HEARD and the serve reuses the document with no sync traffic
        // at all (the dropped-publish variant is
        // `eager_probe_unheard_document_resyncs_on_first_demand`).
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_config(), test_logging(), fs);

        // The fixture exists BEFORE spawn, so the eager probe opens it.
        let path = dir.path().join(format!("probe.{MOCK_LANG_A}"));
        std::fs::write(&path, "fn probe\n").expect("write");

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let uri = crate::lsp::lang::path_to_uri(&path.canonicalize()?);
        assert!(
            client.lock().await.is_document_open(&uri),
            "the probe leaves its document held open — no didClose was sent"
        );

        // The serve's open on unchanged content: reuse, no sync traffic —
        // the probe-time publish (heard before the probe's documentSymbol
        // response resolved) is the evidence.
        let (serve_uri, action) = manager.open_document_on(&path, &client, None, None).await?;
        assert_eq!(serve_uri, uri);
        assert_eq!(
            action,
            DocSync::Unchanged,
            "the serve reuses the probe's still-open document"
        );

        // Moved disk content: the honest reuse — didChange on the open
        // document (version 2 continues the probe's open at 1), never a
        // close→reopen.
        std::fs::write(&path, "fn probe changed\n").expect("write");
        let (_, action) = manager.open_document_on(&path, &client, None, None).await?;
        assert_eq!(
            action,
            DocSync::Change(2),
            "changed content relays didChange on the open document, not a reopen"
        );
        assert!(client.lock().await.is_document_open(&uri));
        Ok(())
    }

    #[tokio::test]
    async fn eager_probe_unheard_document_resyncs_on_first_demand() -> Result<()> {
        // Bug 133 lean 2, the dropped-publish leg: a server that never
        // publishes for the probe's didOpen (here `--no-push-diagnostics`;
        // in the wild, a still-loading server dropping the publish mid-init)
        // leaves the probe-opened document with NO heard evidence — so the
        // first demand re-syncs it with a same-text didChange (version 2
        // continues the probe's open at 1), re-earning the stimulus. The
        // committed re-sync clears the mark: the next unchanged demand sends
        // nothing. Still no didClose anywhere.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(
            mockls_config_with_args(&["--no-push-diagnostics"]),
            test_logging(),
            fs,
        );

        let path = dir.path().join(format!("probe.{MOCK_LANG_A}"));
        std::fs::write(&path, "fn probe\n").expect("write");

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let uri = crate::lsp::lang::path_to_uri(&path.canonicalize()?);
        assert!(client.lock().await.is_document_open(&uri));

        let (_, action) = manager.open_document_on(&path, &client, None, None).await?;
        assert_eq!(
            action,
            DocSync::Change(2),
            "no publish heard: the first demand re-syncs the probe-opened document"
        );
        let (_, action) = manager.open_document_on(&path, &client, None, None).await?;
        assert_eq!(
            action,
            DocSync::Unchanged,
            "the committed re-sync clears the mark"
        );
        assert!(client.lock().await.is_document_open(&uri));
        Ok(())
    }

    #[tokio::test]
    async fn test_close_tracked_document() -> Result<()> {
        // close_tracked_document removes per-client tracking and sends didClose.
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = test_fs_with_roots(&[]);
        fs.set_roots(vec![dir.path().to_path_buf()]);
        let manager = LspClientManager::new(mockls_multi_server_config(), test_logging(), fs);

        let path = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&path, "content").expect("write");

        manager
            .ensure_clients_for_paths(std::slice::from_ref(&path))
            .await;

        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 2);

        // Open document on both servers.
        let mut uri = String::new();
        for c in &servers {
            (uri, _) = manager.open_document_on(&path, c, None, None).await?;
        }

        // Verify all clients have the document open.
        for c in &servers {
            assert!(c.lock().await.is_document_open(&uri));
        }

        // Close on each while holding the lock.
        for c in &servers {
            c.lock().await.close_tracked_document(&uri).await;
        }

        // Verify all clients no longer track the document.
        for c in &servers {
            assert!(
                !c.lock().await.is_document_open(&uri),
                "Document should be closed on all clients"
            );
        }
        Ok(())
    }

    // --- Project config infrastructure tests ---

    #[test]
    fn test_is_project_scoped_with_language() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            "rust".to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("rust-analyzer")]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(root.clone(), pc);

        assert!(manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_is_project_scoped_without_language() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                args: Vec::new(),
                settings: Some(serde_json::json!({"key": "value"})),
                ..ServerDef::default()
            },
        );
        manager.install_root_config(root.clone(), pc);

        assert!(!manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_is_project_scoped_no_config() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/project");

        assert!(!manager.is_project_scoped("rust", &root));
    }

    #[test]
    fn test_effective_server_def_merge() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                args: vec!["--log-level".to_string(), "info".to_string()],
                settings: Some(serde_json::json!({"check": {"command": "clippy"}, "cargo": {"features": ["a"]}})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        // Project config only overrides settings (no `path` = inherit the user
        // executable + args; the key `rust-analyzer` still spawns).
        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                path: None, // no override = inherit from user
                args: Vec::new(),
                settings: Some(serde_json::json!({"check": {"command": "check"}, "new_key": true})),
                ..ServerDef::default()
            },
        );
        manager.install_root_config(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        // path/args inherited from user (key spawns rust-analyzer on PATH)
        assert_eq!(merged.path, None);
        assert_eq!(merged.program("rust-analyzer"), "rust-analyzer");
        assert_eq!(merged.args, vec!["--log-level", "info"]);

        // settings deep-merged
        let settings = merged.settings.expect("settings");
        assert_eq!(settings["check"]["command"], "check"); // project overrides
        assert_eq!(settings["cargo"]["features"], serde_json::json!(["a"])); // user preserved
        assert_eq!(settings["new_key"], true); // project adds
    }

    #[test]
    fn test_effective_server_def_full_override() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                args: vec!["--log-level".to_string(), "info".to_string()],
                settings: Some(serde_json::json!({"key": "user"})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                path: Some("/opt/custom/rust-analyzer".to_string()),
                args: vec!["--custom".to_string()],
                settings: Some(serde_json::json!({"key": "project"})),
                min_severity: Some("warning".to_string()),
                ..ServerDef::default()
            },
        );
        manager.install_root_config(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        // A project `path` relocates the executable; the key stays the identity.
        assert_eq!(merged.path.as_deref(), Some("/opt/custom/rust-analyzer"));
        assert_eq!(merged.program("rust-analyzer"), "/opt/custom/rust-analyzer");
        assert_eq!(merged.args, vec!["--custom"]);
        assert_eq!(merged.min_severity.as_deref(), Some("warning"));
        assert_eq!(merged.settings.expect("settings")["key"], "project");
    }

    #[test]
    fn test_effective_server_def_no_project() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                args: Vec::new(),
                settings: Some(serde_json::json!({"key": "user"})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let def = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");

        assert_eq!(def.path, None);
        assert_eq!(def.program("rust-analyzer"), "rust-analyzer");
        assert_eq!(def.settings.expect("settings")["key"], "user");
    }

    #[test]
    fn test_effective_settings_merge() {
        let mut config = test_config_raw();
        config.server.insert(
            "ra".to_string(),
            ServerDef {
                args: Vec::new(),
                settings: Some(serde_json::json!({"a": 1, "b": {"c": 2}})),
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "ra".to_string(),
            ServerDef {
                args: Vec::new(),
                settings: Some(serde_json::json!({"b": {"d": 3}})),
                ..ServerDef::default()
            },
        );
        manager.install_root_config(root.clone(), pc);

        let settings = manager
            .effective_settings("ra", &root)
            .expect("should exist");
        assert_eq!(settings["a"], 1);
        assert_eq!(settings["b"]["c"], 2);
        assert_eq!(settings["b"]["d"], 3);
    }

    /// Project-layer sibling of the config-level pinned
    /// `user_config_language_binding_reroutes_over_the_shipped_default`
    /// (bug 81 / misc 155): the global layer binds a language to a "shipped
    /// default" server, and a root's `.catenary.toml` `[lsp.language.*]`
    /// `servers` list REPLACES it (array-replace, never append) — the reroute
    /// is by binding, and it holds both when the default server is absent from
    /// the effective binding and while its definition is still present (the
    /// masked shape). A server defined only in the root's `[lsp.server.*]` is a
    /// legal binding target.
    #[test]
    fn project_config_language_binding_reroutes_over_the_shipped_default() {
        let mut config = test_config_raw();
        // The shipped default: language MOCK_LANG_A → "shipped-default".
        config
            .server
            .insert("shipped-default".to_string(), ServerDef::default());
        // A second globally-defined server the project can rebind to.
        config
            .server
            .insert("alt".to_string(), ServerDef::default());
        config.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("shipped-default")]),
                ..LanguageConfig::default()
            },
        );

        let manager = LspClientManager::new(
            Arc::new(config),
            test_logging(),
            test_fs_with_roots(&["/p"]),
        );
        let root = PathBuf::from("/p");

        // Before any project config: the global binding governs.
        assert_eq!(
            manager
                .effective_language(&root, MOCK_LANG_A)
                .expect("global binding")
                .servers(),
            &[ServerBinding::new("shipped-default")],
        );

        // The project rebinds to a globally-defined `alt` server — the default
        // definition is still present (the masked shape).
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("alt")]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(root.clone(), pc);

        let rebound = manager
            .effective_language(&root, MOCK_LANG_A)
            .expect("project binding");
        assert_eq!(
            rebound.servers(),
            &[ServerBinding::new("alt")],
            "the project binding must REPLACE the shipped default, not merge with it",
        );
        // The shipped-default definition survives — the reroute is by binding,
        // not by removing the default.
        assert!(manager.config().server.contains_key("shipped-default"));
        // `alt` resolves through the merged server-def set.
        assert!(manager.effective_server_def("alt", &root).is_some());

        // A server defined only at project scope is a legal binding target.
        let mut pc2 = crate::config::ProjectConfig::default();
        pc2.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("proj-only")]),
                ..LanguageConfig::default()
            },
        );
        pc2.server
            .insert("proj-only".to_string(), ServerDef::default());
        manager.install_root_config(root.clone(), pc2);

        assert_eq!(
            manager
                .effective_language(&root, MOCK_LANG_A)
                .expect("project binding")
                .servers(),
            &[ServerBinding::new("proj-only")],
        );
        // The project-only server def resolves even though no user-level def
        // exists — so it can spawn and answer. The key IS the executable (misc
        // 162): with no `path` override it spawns `proj-only` on PATH.
        let proj_def = manager
            .effective_server_def("proj-only", &root)
            .expect("project-only def resolves");
        assert_eq!(proj_def.program("proj-only"), "proj-only");
        assert!(
            !manager.config().server.contains_key("proj-only"),
            "the target is defined only at project scope",
        );
    }

    // --- find_instance tests ---

    #[tokio::test]
    async fn test_find_instance_root_match() -> Result<()> {
        let bin = mockls_bin();
        let root_client = Arc::new(Mutex::new(LspClient::spawn_quiet(
            bin.to_str().expect("bin"),
            &[],
            "rust",
            "ra",
            test_logging(),
            None,
        )?));

        let mut clients: HashMap<InstanceKey, Arc<Mutex<LspClient>>> = HashMap::new();
        clients.insert(
            InstanceKey::new(
                "rust".to_string(),
                "ra".to_string(),
                Scope::Root(PathBuf::from("/project")),
            ),
            root_client.clone(),
        );

        let result = find_instance(&clients, "rust", "ra", Path::new("/project"));
        assert!(result.is_some());
        assert!(Arc::ptr_eq(&result.expect("found"), &root_client));

        drop(clients);
        drop(root_client);
        Ok(())
    }

    // --- Project-scoped spawning (1d-02) ---

    #[tokio::test]
    async fn test_spawn_project_scoped_forces_root() -> Result<()> {
        // Project-scoped root gets Scope::Root even if the server
        // supports workspaceFolders.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Add project config with [lsp.language.{MOCK_LANG_A}] (Rule A).
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name.clone())]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(PathBuf::from("/tmp"), pc);

        let (key, client) = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, Path::new("/tmp"))
            .await?;

        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        // Even though server advertises workspace folders, scope is Root.
        assert!(client.lock().await.supports_workspace_folders());
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_project_scoped_effective_def() -> Result<()> {
        // Project-scoped instance uses merged settings from
        // effective_server_def.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-ps");
        let mut config = test_config_raw();
        config.server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                settings: Some(serde_json::json!({"user_key": true})),
                ..ServerDef::default()
            },
        );
        config.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name.clone())]),
                ..LanguageConfig::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Project config overrides settings.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name.clone())]),
                ..LanguageConfig::default()
            },
        );
        pc.server.insert(
            server_name.clone(),
            ServerDef {
                args: Vec::new(),
                settings: Some(serde_json::json!({"project_key": true})),
                ..ServerDef::default()
            },
        );
        manager.install_root_config(PathBuf::from("/tmp"), pc);

        let (key, client) = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, Path::new("/tmp"))
            .await?;

        assert_eq!(key.scope, Scope::Root(PathBuf::from("/tmp")));
        // Server should be alive (spawned with user path + project settings).
        assert!(client.lock().await.is_alive());
        // Settings should be the merged result.
        let settings = client.lock().await.server().settings().cloned();
        let settings = settings.expect("should have settings");
        assert_eq!(settings["user_key"], true);
        assert_eq!(settings["project_key"], true);
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_mixed_roots() -> Result<()> {
        // Two roots with real files: one with project config (Rule A),
        // one without. spawn_all should produce workspace + project-scoped
        // root instances.
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");
        std::fs::write(root_a.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");
        std::fs::write(root_b.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        // Write .catenary.toml for root_b so the root is config-complete when
        // it is born (ticket 00a) — root_a has none and loads bare.
        let project_toml = format!("[lsp.language.{MOCK_LANG_A}]\nservers = [\"{server_name}\"]\n");
        std::fs::write(root_b.path().join(".catenary.toml"), project_toml).expect("write");

        let fs = test_fs();
        fs.set_roots_rich(vec![
            Arc::new(Root::load(root_a.path().to_path_buf())),
            Arc::new(Root::load(root_b.path().to_path_buf())),
        ]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        manager.spawn_all().await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 2, "Should have two per-root instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 2);

        // Both roots should have instances.
        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(&root_a.path().to_path_buf()));
        assert!(root_paths.contains(&root_b.path().to_path_buf()));

        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_project_scoped_single_root() -> Result<()> {
        // Single root with project config: spawn_all produces
        // Scope::Root even for workspace-capable server.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(format!("file.{MOCK_LANG_A}")), "content").expect("write");

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let project_toml = format!("[lsp.language.{MOCK_LANG_A}]\nservers = [\"{server_name}\"]\n");
        std::fs::write(root.path().join(".catenary.toml"), project_toml).expect("write");

        let fs = test_fs();
        fs.set_roots_rich(vec![Arc::new(Root::load(root.path().to_path_buf()))]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        manager.spawn_all().await;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("one key");
        assert_eq!(
            key.scope,
            Scope::Root(root.path().to_path_buf()),
            "Project-scoped root should force Scope::Root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_spawn_all_workspace_excludes_project_root() -> Result<()> {
        // Three roots: two normal, one project-scoped. The workspace
        // instance should NOT include the project-scoped root in its
        // workspaceFolders (verified by instance count — if the project
        // root were in the workspace, spawn_project_scoped would have
        // been blocked by find_instance returning the workspace instance).
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");
        let root_c = tempfile::tempdir().expect("tempdir");
        for root in [&root_a, &root_b, &root_c] {
            std::fs::write(root.path().join(format!("file.{MOCK_LANG_A}")), "content")
                .expect("write");
        }

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        // Only root_b is project-scoped.
        let project_toml = format!("[lsp.language.{MOCK_LANG_A}]\nservers = [\"{server_name}\"]\n");
        std::fs::write(root_b.path().join(".catenary.toml"), project_toml).expect("write");

        let fs = test_fs();
        fs.set_roots_rich(vec![
            Arc::new(Root::load(root_a.path().to_path_buf())),
            Arc::new(Root::load(root_b.path().to_path_buf())),
            Arc::new(Root::load(root_c.path().to_path_buf())),
        ]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        manager.spawn_all().await;

        let clients = manager.clients().await;
        // 3 per-root instances — one per root.
        assert_eq!(
            clients.len(),
            3,
            "Should have one per-root instance per root"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 3);

        let root_paths: HashSet<PathBuf> = clients
            .keys()
            .filter_map(|k| k.scope.root_path().map(Path::to_path_buf))
            .collect();
        assert!(root_paths.contains(&root_a.path().to_path_buf()));
        assert!(root_paths.contains(&root_b.path().to_path_buf()));
        assert!(root_paths.contains(&root_c.path().to_path_buf()));
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_project_root() -> Result<()> {
        // get_servers for a file in a project-scoped root returns the
        // project instance, not the workspace instance.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // /var has project config.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name.clone())]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(PathBuf::from("/var"), pc);

        // Spawn workspace instance for /tmp.
        let ws_client = manager
            .ensure_server(MOCK_LANG_A, &server_name, Path::new("/tmp"))
            .await?;
        // Spawn project-scoped for /var.
        let (_, project_client) = manager
            .spawn_project_scoped(&server_name, MOCK_LANG_A, Path::new("/var"))
            .await?;

        // get_servers for a file in /var should return the project instance.
        let path = PathBuf::from(format!("/var/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        assert!(
            Arc::ptr_eq(&servers[0], &project_client),
            "Should return the project-scoped instance, not the workspace one"
        );

        // get_servers for a file in /tmp should return the workspace instance.
        let path = PathBuf::from(format!("/tmp/test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1);
        assert!(
            Arc::ptr_eq(&servers[0], &ws_client),
            "Should return the workspace instance for /tmp"
        );

        Ok(())
    }

    /// misc 155 (bug 81): a root `.catenary.toml` `[lsp.language.*]` binding
    /// reroutes dispatch to the rebound server even when the shipped default
    /// server is NOT spawned — the exact CI shape that exposed bug 81 (only the
    /// rebound server present). `get_servers` returns the rebound instance.
    #[tokio::test]
    async fn project_language_binding_reroutes_dispatch_default_absent() -> Result<()> {
        let (config, default_name, alt_name) = mockls_default_plus_alt_config();
        // A real root dir: the per-root spawn guard (bug 93) refuses a spawn
        // against a directory that does not exist on disk.
        let proj_dir = tempfile::tempdir()?;
        let root = proj_dir.path();
        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[root.to_str().expect("proj path")]),
        );

        // The root rebinds the language to `alt` — a project-scoped root.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(alt_name.clone())]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(root.to_path_buf(), pc);

        // Only the rebound `alt` server is spawned; the shipped default is absent.
        let (_, alt_client) = manager
            .spawn_project_scoped(&alt_name, MOCK_LANG_A, root)
            .await?;

        let path = root.join(format!("test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "the project binding must route to the rebound server",
        );
        assert!(
            Arc::ptr_eq(&servers[0], &alt_client),
            "get_servers must return the rebound `alt` instance",
        );
        // The shipped default is not the answering server.
        assert_ne!(alt_name, default_name);
        Ok(())
    }

    /// misc 155 (bug 81): the *masked* shape — the shipped default server IS
    /// spawned and alive, yet the root's project binding still reroutes dispatch
    /// to the rebound server. The reroute is by binding, not by the default's
    /// absence (locally lattice-was-installed masked the non-reroute).
    #[tokio::test]
    async fn project_language_binding_reroutes_dispatch_default_present() -> Result<()> {
        let (config, default_name, alt_name) = mockls_default_plus_alt_config();
        // Real root dirs: the per-root spawn guard (bug 93) refuses a spawn
        // against a directory that does not exist on disk.
        let shared_dir = tempfile::tempdir()?;
        let proj_dir = tempfile::tempdir()?;
        let shared = shared_dir.path();
        let root = proj_dir.path();
        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[
                shared.to_str().expect("shared path"),
                root.to_str().expect("proj path"),
            ]),
        );

        // The root rebinds the language to `alt`.
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(alt_name.clone())]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(root.to_path_buf(), pc);

        // The shipped default is spawned and alive at a shared, non-rebound root,
        // AND at the project root (the masked shape: the default is present
        // everywhere the global binding would reach).
        let default_shared = manager
            .ensure_server(MOCK_LANG_A, &default_name, shared)
            .await?;
        assert!(default_shared.lock().await.is_alive());
        let (_, default_at_proj) = manager
            .spawn_project_scoped(&default_name, MOCK_LANG_A, root)
            .await?;
        assert!(default_at_proj.lock().await.is_alive());

        // The rebound server is also spawned at the project root.
        let (_, alt_client) = manager
            .spawn_project_scoped(&alt_name, MOCK_LANG_A, root)
            .await?;

        // Dispatch for a project file still routes ONLY to the rebound `alt`
        // instance — the still-alive default is not in the binding.
        let path = root.join(format!("test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "the project binding replaces the default, so only `alt` answers",
        );
        assert!(
            Arc::ptr_eq(&servers[0], &alt_client),
            "get_servers must return `alt`, never the still-alive shipped default",
        );
        Ok(())
    }

    /// misc 155: a project binding whose target server is defined ONLY in the
    /// root's own `[lsp.server.*]` (no user-level def) spawns and answers.
    #[tokio::test]
    async fn project_defined_server_binding_spawns_and_answers() -> Result<()> {
        // Global config: no server bound to the language at all.
        let mut config = test_config_raw();
        config.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(Vec::new()),
                ..LanguageConfig::default()
            },
        );
        // A real root dir: the per-root spawn guard (bug 93) refuses a spawn
        // against a directory that does not exist on disk.
        let proj_dir = tempfile::tempdir()?;
        let root = proj_dir.path();
        let manager = LspClientManager::new(
            Arc::new(config),
            test_logging(),
            test_fs_with_roots(&[root.to_str().expect("proj path")]),
        );

        // The root both binds the language AND defines its target server.
        let proj_server = format!("proj-{MOCK_LANG_A}");
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(proj_server.clone())]),
                ..LanguageConfig::default()
            },
        );
        pc.server.insert(
            proj_server.clone(),
            ServerDef {
                path: Some(mockls_bin().to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        manager.install_root_config(root.to_path_buf(), pc);

        // The project-only server is a legal spawn target and starts.
        let (_, client) = manager
            .spawn_project_scoped(&proj_server, MOCK_LANG_A, root)
            .await?;
        assert!(client.lock().await.is_alive());

        // Dispatch routes to it even though it exists only at project scope.
        let path = root.join(format!("test.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_document_symbols, None)
            .await;
        assert_eq!(servers.len(), 1, "the project-only server must answer");
        assert!(Arc::ptr_eq(&servers[0], &client));
        assert!(
            !manager.config().server.contains_key(&proj_server),
            "the answering server is defined only at project scope",
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_add_project_scoped() -> Result<()> {
        // Adding a root with project [lsp.language.*] spawns Scope::Root,
        // no didChangeWorkspaceFolders to workspace instance.
        let root_a = tempfile::tempdir().expect("tempdir");
        let root_b = tempfile::tempdir().expect("tempdir");
        std::fs::write(root_a.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");
        std::fs::write(root_b.path().join(format!("file.{MOCK_LANG_A}")), "content")
            .expect("write");

        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let fs = test_fs();
        fs.set_roots(vec![root_a.path().to_path_buf()]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        // Spawn workspace instance for root_a.
        let ws = manager
            .ensure_server(MOCK_LANG_A, &server_name, root_a.path())
            .await?;
        assert!(ws.lock().await.supports_workspace_folders());

        // root_b is project-scoped (carries a `[lsp.language.*]` config).
        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name.clone())]),
                ..LanguageConfig::default()
            },
        );

        // sync_roots adds root_b (config-complete) alongside the bare root_a.
        manager
            .sync_roots(vec![
                Arc::new(Root::bare(root_a.path().to_path_buf())),
                Arc::new(Root::new(root_b.path().to_path_buf(), pc)),
            ])
            .await?;

        let clients = manager.clients().await;
        // Both roots get per-root instances.
        assert_eq!(clients.len(), 2, "Should have two per-root instances");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_add_spawns_instance() -> Result<()> {
        // Adding a root spawns a new per-root instance.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));

        // Spawn per-root instance for /tmp.
        let client = manager
            .ensure_server(MOCK_LANG_A, &server_name, Path::new("/tmp"))
            .await?;
        assert!(client.lock().await.is_alive());

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_remove_root() -> Result<()> {
        // Removing a root: its per-root instance is shut down and
        // project config cleaned up.
        let config = mockls_workspace_folders_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&["/tmp", "/var"]),
        );

        // Spawn instances for both roots.
        let _ = manager
            .ensure_server(MOCK_LANG_A, &server_name, Path::new("/tmp"))
            .await?;
        let _ = manager
            .ensure_server(MOCK_LANG_A, &server_name, Path::new("/var"))
            .await?;
        assert_eq!(manager.clients().await.len(), 2);

        // Remove /var.
        manager.sync_roots(rich(&["/tmp"])).await?;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1, "/var instance should be removed");
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 1);

        Ok(())
    }

    /// Config with mockls that accepts null-workspace (single-file mode).
    fn mockls_single_file_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-sf");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                single_file: true,
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config with mockls that rejects null-workspace initialization.
    fn mockls_reject_null_workspace_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-rnw");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--reject-null-workspace".to_string(),
                ],
                single_file: true,
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    #[tokio::test]
    async fn test_single_file_spawn_accepts_null_workspace() -> Result<()> {
        // mockls without --reject-null-workspace accepts single-file mode.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let client = manager.spawn_single_file(&server_name, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());

        // Verify scope is SingleFile.
        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("should have one client");
        assert_eq!(key.scope, Scope::SingleFile);
        assert_eq!(key.language_id, MOCK_LANG_A);

        // No failure should be cached (server accepted).
        assert!(
            !manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&(MOCK_LANG_A.to_string(), server_name)),
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_spawn_rejects_null_workspace() -> Result<()> {
        // mockls with --reject-null-workspace rejects single-file mode.
        let config = mockls_reject_null_workspace_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let result = manager.spawn_single_file(&server_name, MOCK_LANG_A).await;
        assert!(result.is_err(), "Should fail with null workspace rejection");

        // Negative cache should be set.
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&(MOCK_LANG_A.to_string(), server_name)),
        );

        // No client should be stored.
        assert!(manager.clients().await.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_negative_cache_prevents_retry() -> Result<()> {
        // After negative cache, ensure_single_file_server returns None
        // without attempting to spawn.
        let config = mockls_reject_null_workspace_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        // First attempt — spawns and fails.
        let first = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await;
        assert!(first.is_none());

        // Second attempt — should return None from cache without spawning.
        let second = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await;
        assert!(second.is_none());

        // Still no clients.
        assert!(manager.clients().await.is_empty());

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_positive_cache_returns_same_handle() -> Result<()> {
        // After positive spawn, ensure_single_file_server returns the
        // same handle on subsequent calls.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let first = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await
            .expect("should spawn");
        let second = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await
            .expect("should return cached");

        assert!(Arc::ptr_eq(&first, &second), "Should be the same handle");
        assert_eq!(manager.clients().await.len(), 1);

        Ok(())
    }

    /// Config binding `MOCK_LANG_A` to a server NAMED after the blessed
    /// `mockls-event` persona, with NO `single_file` config opt-in — the
    /// manifest's `single_file` capability is the only thing that can open
    /// the rootless gate (brackets 01).
    fn mockls_persona_named_config() -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = "mockls-event".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    #[tokio::test]
    async fn test_registry_capability_opens_the_rootless_gate() -> Result<()> {
        // brackets 01: the manifest's `single_file` capability alone — no
        // `single_file = true` config opt-in — admits a rootless spawn. The
        // `mockls-event` persona row carries `serves-diagnostics`
        // (may-spawn-rootless), so the null-root spawn + handshake completes
        // against the real mockls binary.
        let manager =
            LspClientManager::new(mockls_persona_named_config(), test_logging(), test_fs());

        let client = manager
            .ensure_single_file_server(MOCK_LANG_A, "mockls-event")
            .await
            .expect("the registry capability admits the rootless spawn");
        assert!(client.lock().await.is_alive());

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 1);
        let key = clients.keys().next().expect("one singleton");
        assert_eq!(key.scope, Scope::SingleFile);
        assert_eq!(key.server, "mockls-event");
        Ok(())
    }

    #[tokio::test]
    async fn test_unsupported_capability_fails_closed_on_the_rootless_gate() {
        // A server with NO manifest claim and NO config opt-in must never
        // spawn rootless (fail closed, brackets 01): no client appears, and
        // no spawn was even attempted — the negative cache stays empty
        // because the gate refused before any process launch.
        let manager = LspClientManager::new(mockls_config(), test_logging(), test_fs());
        let server_name = format!("mockls-{MOCK_LANG_A}");

        let result = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await;
        assert!(result.is_none(), "unsupported must never spawn rootless");
        assert!(manager.clients().await.is_empty());
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the gate refuses before any spawn attempt",
        );
    }

    #[tokio::test]
    async fn test_reap_idle_single_file_expires_and_respawns() -> Result<()> {
        // brackets 01 lifecycle: a rootless singleton idle past the window is
        // reaped (shut down, unregistered); a fresh or actively-demanded one
        // is kept; the next demand after expiry respawns on demand. Driven
        // with explicit windows — no wall-clock waits.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();
        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let _ = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await
            .expect("spawn");
        assert_eq!(manager.clients().await.len(), 1);

        // Inside the idle window nothing is reaped.
        let kept = manager
            .reap_idle_single_file_instances(Instant::now(), Duration::from_hours(1))
            .await;
        assert!(kept.is_empty(), "a freshly-demanded singleton is not idle");
        assert_eq!(manager.clients().await.len(), 1);

        // Past the window (a zero idle bound expires any stamped clock) it is
        // reaped and shut down.
        let reaped = manager
            .reap_idle_single_file_instances(Instant::now(), Duration::ZERO)
            .await;
        assert_eq!(reaped.len(), 1, "the idle singleton is reaped");
        assert_eq!(reaped[0].scope, Scope::SingleFile);
        assert!(
            manager.clients().await.is_empty(),
            "the reaped singleton left the registry"
        );

        // On demand after expiry: the next demand respawns fresh.
        let respawned = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_name)
            .await;
        assert!(
            respawned.is_some(),
            "the next demand respawns the expired singleton"
        );
        assert_eq!(manager.clients().await.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_reap_idle_single_file_skips_rooted_instances() -> Result<()> {
        // The sweep is scope-narrow: per-root instances are governed by root
        // lifetime (sync_roots / root expiry), never by the rootless idle
        // clock — even a zero idle bound touches nothing rooted.
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);

        let reaped = manager
            .reap_idle_single_file_instances(Instant::now(), Duration::ZERO)
            .await;
        assert!(reaped.is_empty(), "a rooted instance is never reaped here");
        assert_eq!(manager.clients().await.len(), 1);
        Ok(())
    }

    /// A flush-fast `state.json` writer rooted in `dir` for board assertions.
    fn test_snapshot_writer(dir: &Path) -> Arc<crate::state_snapshot::SnapshotWriter> {
        crate::state_snapshot::SnapshotWriter::with_coalesce(
            &tokio::runtime::Handle::current(),
            dir,
            crate::state_snapshot::DaemonInfo {
                instance_id: "daemon:test".to_string(),
                pid: 1,
                version: "test".to_string(),
                started_at: "t0".to_string(),
            },
            Duration::from_millis(10),
        )
    }

    /// Flushes and parses the board's `servers` array from the snapshot file.
    fn board_servers(writer: &crate::state_snapshot::SnapshotWriter) -> Vec<serde_json::Value> {
        writer.flush_now();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(writer.path()).expect("read snapshot"))
                .expect("parse snapshot");
        json["servers"].as_array().expect("servers array").clone()
    }

    /// misc 209: a `sync_roots`-triggered singleton teardown must drop the
    /// board entry — the same discipline the idle reap and the per-root
    /// teardown already follow (bug 72). Registration at spawn is asserted
    /// first so the removal check cannot pass vacuously.
    #[tokio::test]
    async fn sync_roots_teardown_drops_singleton_board_entry() -> Result<()> {
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();
        let mut manager = LspClientManager::new(config, test_logging(), test_fs());
        let dir = tempfile::tempdir()?;
        let writer = test_snapshot_writer(dir.path());
        manager.set_snapshot(writer.clone());

        let _ = manager.spawn_single_file(&server_name, MOCK_LANG_A).await?;
        let servers = board_servers(&writer);
        assert!(
            servers.iter().any(|s| s["scope_kind"] == "single_file"),
            "a live singleton registers a board entry: {servers:?}"
        );

        // A roots change tears down every rootless singleton (`sync_roots` →
        // `shutdown_single_file_instances`); the board entry goes with it.
        let root = tempfile::tempdir()?;
        manager
            .sync_roots(rich_bufs(vec![root.path().to_path_buf()]))
            .await?;
        assert!(
            manager.clients().await.is_empty(),
            "the roots change tears the singleton down"
        );
        let servers = board_servers(&writer);
        assert!(
            servers.iter().all(|s| s["scope_kind"] != "single_file"),
            "the singleton teardown must leave no board ghost: {servers:?}"
        );
        Ok(())
    }

    /// The idle reap's board discipline, proven non-vacuously: the registered
    /// singleton entry leaves the board when the reap tears the instance down
    /// (previously `remove_server` here removed nothing — singletons never
    /// registered).
    #[tokio::test]
    async fn reap_idle_drops_singleton_board_entry() -> Result<()> {
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();
        let mut manager = LspClientManager::new(config, test_logging(), test_fs());
        let dir = tempfile::tempdir()?;
        let writer = test_snapshot_writer(dir.path());
        manager.set_snapshot(writer.clone());

        let _ = manager.spawn_single_file(&server_name, MOCK_LANG_A).await?;
        let servers = board_servers(&writer);
        assert!(
            servers.iter().any(|s| s["scope_kind"] == "single_file"),
            "a live singleton registers a board entry: {servers:?}"
        );

        let reaped = manager
            .reap_idle_single_file_instances(Instant::now(), Duration::ZERO)
            .await;
        assert_eq!(reaped.len(), 1, "the idle singleton is reaped");
        let servers = board_servers(&writer);
        assert!(
            servers.iter().all(|s| s["scope_kind"] != "single_file"),
            "the reap must leave no board ghost: {servers:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_falls_through_to_single_file() -> Result<()> {
        // File outside all roots → tier 3 single-file server spawned.
        let config = mockls_single_file_config();

        // No roots — every file is unrooted.
        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let path = PathBuf::from(format!("/some/random/file.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_references, None)
            .await;
        assert_eq!(servers.len(), 1, "Should have spawned a single-file server");

        // Verify it's a SingleFile instance.
        let clients = manager.clients().await;
        let key = clients.keys().next().expect("should have one client");
        assert_eq!(key.scope, Scope::SingleFile);

        Ok(())
    }

    #[tokio::test]
    async fn test_get_servers_unrooted_rejects_returns_empty() -> Result<()> {
        // File outside all roots, server rejects null workspace → empty.
        let config = mockls_reject_null_workspace_config();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let path = PathBuf::from(format!("/some/random/file.{MOCK_LANG_A}"));
        let servers = manager
            .get_servers(&path, LspServer::supports_references, None)
            .await;
        assert!(
            servers.is_empty(),
            "Should return empty when server rejects"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_rooted_clients_excludes_single_file() -> Result<()> {
        // rooted_clients() should not include single-file servers.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let _ = manager.spawn_single_file(&server_name, MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);
        assert!(
            manager.rooted_clients().await.is_empty(),
            "Single-file servers should be excluded from rooted_clients"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_root_added_routes_to_workspace() -> Result<()> {
        // After a root is added for a path previously served by a
        // single-file server, get_servers routes to the workspace instance.
        // sync_roots cleans up single-file servers.
        let config = mockls_single_file_config();

        let root = tempfile::tempdir().expect("tempdir");
        let file_path = root.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&file_path, "content").expect("write");

        // Start with no roots — file gets single-file server.
        let fs = test_fs();
        let manager = LspClientManager::new(config, test_logging(), fs.clone());

        let servers = manager
            .get_servers(&file_path, LspServer::supports_references, None)
            .await;
        assert_eq!(servers.len(), 1);

        // Verify single-file instance exists.
        assert_eq!(
            count_scope(&manager.clients().await, MOCK_LANG_A, "single_file"),
            1
        );

        // Add the root via sync_roots — this shuts down single-file
        // instances and clears failure cache.
        manager
            .sync_roots(rich_bufs(vec![root.path().to_path_buf()]))
            .await?;

        // Single-file instance should be cleaned up.
        assert_eq!(
            count_scope(&manager.clients().await, MOCK_LANG_A, "single_file"),
            0,
            "Single-file server should be shut down after root added"
        );

        // Failure cache should be cleared.
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "Failure cache should be cleared after sync_roots"
        );

        // Spawn the rooted server and verify get_servers routes there.
        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let servers = manager
            .get_servers(&file_path, LspServer::supports_references, None)
            .await;
        assert_eq!(servers.len(), 1);

        // The returned client should be rooted, not single-file.
        let clients = manager.clients().await;
        assert!(
            clients.keys().all(|k| k.scope != Scope::SingleFile),
            "No single-file instances should remain"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_single_file_different_languages_independent() -> Result<()> {
        // Two different languages outside roots → independent single-file
        // servers with independent cache entries.
        let bin = mockls_bin();
        let lang_b = "qR7bZ";
        let server_a = format!("mockls-{MOCK_LANG_A}-sf2");
        let server_b = format!("mockls-{lang_b}-sf2");
        let mut config = test_config_raw();
        for (name, lang) in [(&server_a, MOCK_LANG_A), (&server_b, lang_b)] {
            config.server.insert(
                name.clone(),
                ServerDef {
                    path: Some(bin.to_string_lossy().to_string()),
                    args: vec![lang.to_string()],
                    single_file: true,
                    ..ServerDef::default()
                },
            );
            config.language.insert(
                lang.to_string(),
                LanguageConfig {
                    servers: Some(vec![ServerBinding::new(name.clone())]),
                    ..LanguageConfig::default()
                },
            );
        }

        let manager = LspClientManager::new(config, test_logging(), test_fs());

        // Spawn single-file for language A.
        let client_a = manager
            .ensure_single_file_server(MOCK_LANG_A, &server_a)
            .await
            .expect("should spawn for lang A");
        assert!(client_a.lock().await.is_alive());

        // Spawn single-file for language B.
        let client_b = manager
            .ensure_single_file_server(lang_b, &server_b)
            .await
            .expect("should spawn for lang B");
        assert!(client_b.lock().await.is_alive());

        // Should be different instances.
        assert!(!Arc::ptr_eq(&client_a, &client_b));
        assert_eq!(manager.clients().await.len(), 2);

        // Neither should be in the failure cache.
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
        );

        Ok(())
    }

    #[tokio::test]
    async fn single_file_bracket_serializes_concurrent_consumers() -> Result<()> {
        // brackets 02: the rootless access path serves consumers as whole
        // transactions. Two concurrent brackets against the one live
        // singleton never interleave their open→answer→close legs — the
        // bodies deliberately release the client lock across their yield
        // point, so only the bracket queue can be doing the serializing.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();
        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let log: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let bracket = |tag: &'static str| {
            let body_log = log.clone();
            let close_log = log.clone();
            (
                move |client: Arc<Mutex<LspClient>>| async move {
                    // Touch the live session, then release the client lock
                    // before yielding — an unserialized peer WOULD
                    // interleave across this sleep.
                    assert!(client.lock().await.is_alive());
                    body_log.lock().expect("log").push(format!("{tag}:open"));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    body_log.lock().expect("log").push(format!("{tag}:answer"));
                },
                move |_client: Arc<Mutex<LspClient>>| async move {
                    close_log.lock().expect("log").push(format!("{tag}:close"));
                },
            )
        };

        let (body_a, close_a) = bracket("a");
        let (body_b, close_b) = bracket("b");
        let (out_a, out_b) = tokio::join!(
            manager.with_single_file_bracket(
                MOCK_LANG_A,
                &server_name,
                Lane::Enrichment,
                body_a,
                close_a,
            ),
            manager.with_single_file_bracket(
                MOCK_LANG_A,
                &server_name,
                Lane::Enrichment,
                body_b,
                close_b,
            ),
        );
        assert_eq!(out_a, Some(BracketOutcome::Completed(())));
        assert_eq!(out_b, Some(BracketOutcome::Completed(())));
        assert_eq!(
            manager.clients().await.len(),
            1,
            "one singleton serves both"
        );

        let events = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(events.len(), 6, "two full brackets: {events:?}");
        for chunk in events.chunks(3) {
            let tag = chunk[0].split(':').next().expect("tag");
            let expected: Vec<String> = ["open", "answer", "close"]
                .iter()
                .map(|leg| format!("{tag}:{leg}"))
                .collect();
            assert_eq!(chunk, expected, "bracket interleaved: {events:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn single_file_bracket_is_capability_shaped() {
        // brackets 02: whether the access path enriches at all is decided
        // by "does a capable server exist" — no manifest claim and no
        // config opt-in means the gate refuses and the bracket path answers
        // `None` (serve raw), with no spawn and no queueing.
        let manager = LspClientManager::new(mockls_config(), test_logging(), test_fs());
        let server_name = format!("mockls-{MOCK_LANG_A}");

        let out = manager
            .with_single_file_bracket(
                MOCK_LANG_A,
                &server_name,
                Lane::Enrichment,
                |_client| async { 1 },
                |_client| async {},
            )
            .await;
        assert!(
            out.is_none(),
            "capability-shaped degrade: no capable server"
        );
        assert!(manager.clients().await.is_empty());
    }

    #[tokio::test]
    async fn single_file_bracket_budget_expiry_completes_degraded() -> Result<()> {
        // brackets 02 backstop at the seam: an injected tiny budget cuts a
        // wedged body, but the transaction completes degraded — the close
        // leg runs, the singleton survives, and its queue serves the next
        // bracket normally.
        let config = mockls_single_file_config();
        let server_name = config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()[0]
            .name
            .clone();
        let manager = LspClientManager::new(config, test_logging(), test_fs())
            .bracket_budget_override(Duration::from_millis(50));

        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed_tx = closed.clone();
        let out = manager
            .with_single_file_bracket(
                MOCK_LANG_A,
                &server_name,
                Lane::Enrichment,
                |_client| std::future::pending::<()>(),
                move |_client| async move {
                    closed_tx.store(true, std::sync::atomic::Ordering::SeqCst);
                },
            )
            .await
            .expect("a capable singleton exists");
        assert!(out.is_degraded(), "budget expiry degrades the answer");
        assert!(
            closed.load(std::sync::atomic::Ordering::SeqCst),
            "the close leg still ran",
        );

        // The singleton survived the degraded bracket and serves fresh.
        let next = manager
            .with_single_file_bracket(
                MOCK_LANG_A,
                &server_name,
                Lane::DebtPayment,
                |client| async move { client.lock().await.is_alive() },
                |_client| async {},
            )
            .await
            .expect("the singleton is still capable");
        assert_eq!(next.completed(), Some(true));
        Ok(())
    }

    #[tokio::test]
    async fn test_did_change_configuration_notification() -> Result<()> {
        // did_change_configuration sends notification with empty settings.
        // mockls with --send-configuration-request will respond to it.
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-dcc");
        let mut config = test_config_raw();
        config.server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![
                    MOCK_LANG_A.to_string(),
                    "--send-configuration-request".to_string(),
                ],
                settings: Some(serde_json::json!({"key": "value"})),
                ..ServerDef::default()
            },
        );
        config.language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&["/tmp"]));
        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // Send didChangeConfiguration — should not error.
        let result = client.lock().await.did_change_configuration().await;
        assert!(
            result.is_ok(),
            "did_change_configuration should succeed: {result:?}"
        );

        Ok(())
    }

    // ── Root marker resolution tests ─────────────────────────────────

    #[test]
    fn test_resolve_marker_root_finds_nearest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("packages").join("crate_a");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(sub.join("Cargo.toml"), "").expect("write marker");

        let file = sub.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "").expect("write file");

        let resolved = resolve_marker_root(&file, &["Cargo.toml".into()], &[], &ws);
        assert_eq!(resolved, sub);
    }

    #[test]
    fn test_resolve_marker_root_workspace_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("packages").join("no_marker");
        std::fs::create_dir_all(&sub).expect("mkdir");

        let file = sub.join("lib.rs");
        std::fs::write(&file, "").expect("write file");

        let resolved = resolve_marker_root(&file, &["Cargo.toml".into()], &[], &ws);
        assert_eq!(resolved, ws);
    }

    #[test]
    fn test_resolve_marker_root_at_workspace_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).expect("mkdir");
        std::fs::write(ws.join("Cargo.toml"), "").expect("write marker");

        let file = ws.join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "").expect("write file");

        let resolved = resolve_marker_root(&file, &["Cargo.toml".into()], &[], &ws);
        assert_eq!(resolved, ws);
    }

    #[test]
    fn test_resolve_marker_root_never_escapes_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let parent = dir.path().join("parent");
        let ws = parent.join("workspace");
        std::fs::create_dir_all(&ws).expect("mkdir");
        // Marker is above workspace root — should NOT be found.
        std::fs::write(parent.join("Cargo.toml"), "").expect("write marker");

        let file = ws.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "").expect("write file");

        let resolved = resolve_marker_root(&file, &["Cargo.toml".into()], &[], &ws);
        assert_eq!(resolved, ws);
    }

    #[test]
    fn test_resolve_marker_root_nested_nearest_wins() {
        // workspace/Cargo.toml (workspace manifest)
        // workspace/crate_a/Cargo.toml (crate manifest)
        // File is in crate_a → crate_a wins.
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let crate_a = ws.join("crate_a");
        std::fs::create_dir_all(&crate_a).expect("mkdir");
        std::fs::write(ws.join("Cargo.toml"), "").expect("write ws marker");
        std::fs::write(crate_a.join("Cargo.toml"), "").expect("write crate marker");

        let file = crate_a.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "").expect("write file");

        let resolved = resolve_marker_root(&file, &["Cargo.toml".into()], &[], &ws);
        assert_eq!(resolved, crate_a);
    }

    #[test]
    fn test_dir_has_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");

        assert!(dir_has_marker(dir.path(), &["Cargo.toml".into()], &[]));
        assert!(!dir_has_marker(dir.path(), &["go.mod".into()], &[]));
    }

    #[test]
    fn test_resolve_marker_root_empty_markers() {
        // Empty markers list should return workspace root immediately.
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).expect("mkdir");
        std::fs::write(ws.join("Cargo.toml"), "").expect("write marker");

        let file = ws.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "").expect("write file");

        let resolved = resolve_marker_root(&file, &[], &[], &ws);
        assert_eq!(resolved, ws);
    }

    #[test]
    fn test_marker_cache_hit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("crate_a");
        std::fs::create_dir_all(sub.join("src")).expect("mkdir");
        std::fs::write(sub.join("Cargo.toml"), "").expect("write marker");

        let file1 = sub.join("src").join("lib.rs");
        let file2 = sub.join("src").join("main.rs");
        std::fs::write(&file1, "").expect("write");
        std::fs::write(&file2, "").expect("write");

        let mut config = test_config_raw();
        config.language.insert(
            "rust".to_string(),
            LanguageConfig {
                root_markers: Some(vec!["Cargo.toml".into()]),
                ..LanguageConfig::default()
            },
        );

        let fs = test_fs_with_roots(&[ws.to_str().expect("ws")]);
        let manager = LspClientManager::new(config, test_logging(), fs);

        let r1 = manager.resolve_server_root(&file1, "rust", &ws);
        let r2 = manager.resolve_server_root(&file2, "rust", &ws);
        assert_eq!(r1, sub);
        assert_eq!(r2, sub);

        // Verify cache was populated.
        let cache_len = manager
            .marker_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        // Both files are in the same directory, so one cache entry.
        assert_eq!(cache_len, 1);
    }

    #[test]
    fn test_resolve_server_root_no_markers() {
        let ws = PathBuf::from("/workspace");
        let config = test_config_raw();
        let manager = LspClientManager::new(config, test_logging(), test_fs());

        let file = PathBuf::from("/workspace/src/lib.rs");
        let resolved = manager.resolve_server_root(&file, "nonexistent", &ws);
        assert_eq!(resolved, ws);
    }

    #[test]
    fn test_resolve_server_root_disabled_markers() {
        // root_markers = [] → no marker resolution.
        let ws = PathBuf::from("/workspace");
        let mut config = test_config_raw();
        config.language.insert(
            "rust".to_string(),
            LanguageConfig {
                root_markers: Some(Vec::new()),
                ..LanguageConfig::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let file = PathBuf::from("/workspace/src/lib.rs");
        let resolved = manager.resolve_server_root(&file, "rust", &ws);
        assert_eq!(resolved, ws);
    }

    #[test]
    fn test_active_markers_states() {
        // None → not set → None
        let lc = LanguageConfig::default();
        assert!(lc.active_markers().is_none());

        // Some(empty) → disabled
        let lc = LanguageConfig {
            root_markers: Some(Vec::new()),
            ..LanguageConfig::default()
        };
        assert!(lc.active_markers().is_none());

        // Some(non-empty) → active
        let lc = LanguageConfig {
            root_markers: Some(vec!["Cargo.toml".into()]),
            ..LanguageConfig::default()
        };
        assert_eq!(lc.active_markers(), Some(&["Cargo.toml".into()][..]));
    }

    // ── Glob marker tests ────────────────────────────────────────────

    #[test]
    fn test_dir_has_marker_glob() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("project.sln"), "").expect("write");

        let glob = LspGlob::new("*.sln").expect("compile glob");
        // Exact marker doesn't match, but glob does.
        assert!(!dir_has_marker(dir.path(), &[], &[]));
        assert!(dir_has_marker(dir.path(), &[], &[glob]));
    }

    #[test]
    fn test_dir_has_marker_mixed_exact_and_glob() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");

        let glob = LspGlob::new("*.sln").expect("compile glob");
        // Exact marker matches — glob branch should not even be needed.
        assert!(dir_has_marker(dir.path(), &["Cargo.toml".into()], &[glob],));
    }

    #[test]
    fn test_dir_has_marker_glob_no_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("readme.txt"), "").expect("write");

        let glob = LspGlob::new("*.sln").expect("compile glob");
        assert!(!dir_has_marker(dir.path(), &[], &[glob]));
    }

    #[test]
    fn test_resolve_marker_root_with_glob() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("packages").join("my_project");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(sub.join("my_project.csproj"), "").expect("write marker");

        let file = sub.join("src").join("Program.cs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "").expect("write file");

        let glob = LspGlob::new("*.csproj").expect("compile glob");
        let resolved = resolve_marker_root(&file, &[], &[glob], &ws);
        assert_eq!(resolved, sub);
    }

    #[test]
    fn test_resolve_marker_root_glob_fallback_to_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("packages").join("no_marker");
        std::fs::create_dir_all(&sub).expect("mkdir");

        let file = sub.join("lib.rs");
        std::fs::write(&file, "").expect("write file");

        let glob = LspGlob::new("*.sln").expect("compile glob");
        let resolved = resolve_marker_root(&file, &[], &[glob], &ws);
        assert_eq!(resolved, ws);
    }

    #[test]
    fn test_resolve_marker_root_mixed_exact_and_glob() {
        // Exact marker at workspace root, glob marker at sub.
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("sub_project");
        std::fs::create_dir_all(&sub).expect("mkdir");
        std::fs::write(ws.join("Cargo.toml"), "").expect("write exact marker");
        std::fs::write(sub.join("project.csproj"), "").expect("write glob marker");

        let file = sub.join("src").join("Main.cs");
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, "").expect("write file");

        let glob = LspGlob::new("*.csproj").expect("compile glob");
        // Nearest directory with any marker wins — sub has *.csproj.
        let resolved = resolve_marker_root(&file, &["Cargo.toml".into()], &[glob], &ws);
        assert_eq!(resolved, sub);
    }

    #[test]
    fn test_compile_markers_separates_exact_and_glob() {
        let mut lc = LanguageConfig {
            root_markers: Some(vec![
                "Cargo.toml".into(),
                "*.sln".into(),
                "go.mod".into(),
                "*.csproj".into(),
            ]),
            ..LanguageConfig::default()
        };
        lc.compile_markers().expect("compile");
        // Only glob patterns are compiled.
        assert_eq!(lc.compiled_markers.len(), 2);
    }

    #[test]
    fn test_compile_markers_no_globs() {
        let mut lc = LanguageConfig {
            root_markers: Some(vec!["Cargo.toml".into(), "go.mod".into()]),
            ..LanguageConfig::default()
        };
        lc.compile_markers().expect("compile");
        assert!(lc.compiled_markers.is_empty());
    }

    #[test]
    fn test_compile_markers_none() {
        let mut lc = LanguageConfig::default();
        lc.compile_markers().expect("compile");
        assert!(lc.compiled_markers.is_empty());
    }

    // ── Manager operations tests (mutant audit 03-06) ──────────────

    /// `project_commands` returns commands from loaded project configs.
    #[test]
    fn test_project_commands_returns_loaded() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());

        // No project configs loaded → empty
        assert!(
            manager.project_commands().is_empty(),
            "should be empty with no project configs"
        );

        // Load a project config with commands
        let pc = crate::config::ProjectConfig {
            commands: Some(crate::config::CommandsConfig::default()),
            ..crate::config::ProjectConfig::default()
        };
        let root = PathBuf::from("/project");
        manager.install_root_config(root.clone(), pc);

        let cmds = manager.project_commands();
        assert_eq!(cmds.len(), 1, "should have one entry");
        assert!(cmds.contains_key(&root), "should contain the project root");
    }

    /// `project_commands` omits roots without a `[commands]` section.
    #[test]
    fn test_project_commands_omits_no_commands() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());

        let pc = crate::config::ProjectConfig::default(); // commands = None
        let root = PathBuf::from("/project");
        manager.install_root_config(root, pc);

        assert!(
            manager.project_commands().is_empty(),
            "roots without commands should be omitted"
        );
    }

    /// `is_lsp_disabled` reads `disable_lsp` off the loaded project config and
    /// is orthogonal to `disable_diag` (ticket 00).
    #[test]
    fn test_is_lsp_disabled_reads_project_config() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let disabled = PathBuf::from("/disabled");
        let enabled = PathBuf::from("/enabled");

        // Unknown root → not disabled (default false).
        assert!(!manager.is_lsp_disabled(&disabled));

        manager.install_root_config(
            disabled.clone(),
            crate::config::ProjectConfig {
                disable_lsp: true,
                ..crate::config::ProjectConfig::default()
            },
        );
        manager.install_root_config(enabled.clone(), crate::config::ProjectConfig::default());

        assert!(manager.is_lsp_disabled(&disabled), "disable_lsp root");
        assert!(
            !manager.is_lsp_disabled(&enabled),
            "default root is enabled"
        );
        assert!(
            !manager.is_diag_disabled(&disabled),
            "disable_lsp must not imply disable_diag"
        );
    }

    /// `is_diag_disabled` reads `disable_diag` and is orthogonal to
    /// `disable_lsp` (ticket 00).
    #[test]
    fn test_is_diag_disabled_reads_project_config() {
        let manager = LspClientManager::new(test_config(), test_logging(), test_fs());
        let root = PathBuf::from("/diag-off");

        assert!(!manager.is_diag_disabled(&root));

        manager.install_root_config(
            root.clone(),
            crate::config::ProjectConfig {
                disable_diag: true,
                ..crate::config::ProjectConfig::default()
            },
        );

        assert!(manager.is_diag_disabled(&root));
        assert!(
            !manager.is_lsp_disabled(&root),
            "disable_diag must not imply disable_lsp"
        );
    }

    /// A config binding a blessed server (`clangd`) to one language and an
    /// unverified custom server to another, for the classification-gate tests
    /// (diagnostics-debt 04b).
    fn blessed_and_unverified_config() -> Arc<Config> {
        let mut server = HashMap::new();
        server.insert(
            "clangd".to_string(),
            ServerDef {
                path: Some("/usr/bin/clangd".to_string()),
                ..ServerDef::default()
            },
        );
        server.insert(
            "my-custom".to_string(),
            ServerDef {
                path: Some("/usr/bin/my-custom".to_string()),
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            "c".to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("clangd".to_string())]),
                ..LanguageConfig::default()
            },
        );
        language.insert(
            "custlang".to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("my-custom".to_string())]),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    #[test]
    fn has_configured_server_requires_a_blessed_binding() {
        // The diagnostics-coverage gate counts only BLESSED servers
        // (diagnostics-debt 04b): a blessed binding (clangd) covers its language;
        // a language whose only binding is an unverified custom def does NOT.
        let manager = LspClientManager::new(
            blessed_and_unverified_config(),
            test_logging(),
            test_fs_with_roots(&["/ws"]),
        );
        let root = PathBuf::from("/ws");
        assert!(
            manager.has_configured_server(&root, "c"),
            "clangd is blessed — its language is covered"
        );
        assert!(
            !manager.has_configured_server(&root, "custlang"),
            "an unverified-only language has no diagnostics coverage"
        );
    }

    #[test]
    fn has_unverified_only_server_flags_the_unblessed_only_case() {
        // The complement (diagnostics-debt 04b): the unverified-only language is
        // the "not diagnostics-covered" case (a server exists, unblessed); a
        // blessed language is NOT unverified-only; an unbound language is neither.
        let manager = LspClientManager::new(
            blessed_and_unverified_config(),
            test_logging(),
            test_fs_with_roots(&["/ws"]),
        );
        let root = PathBuf::from("/ws");
        assert!(
            manager.has_unverified_only_server(&root, "custlang"),
            "a custom-only language is unverified-only — the receipt declares it"
        );
        assert!(
            !manager.has_unverified_only_server(&root, "c"),
            "a blessed language is covered, never unverified-only"
        );
        assert!(
            !manager.has_unverified_only_server(&root, "python"),
            "an unbound language has no server at all — not unverified-only"
        );
    }

    /// `effective_weights` seeds the rust-analyzer default, overlays the
    /// user-level `[lsp.server.*]` / `[linter.rule.*]` weights, and lets a per-root
    /// project override win (linters ticket 05).
    #[test]
    fn test_effective_weights_layers_seed_user_and_project() {
        use crate::config::{BASELINE_WEIGHT, LinterConfig, ServerDef};
        use std::collections::HashMap;

        let mut config = test_config_raw();
        // User overrides rust-analyzer's native weight and adds a linter weight.
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                weight: Some(20),
                ..ServerDef::default()
            },
        );
        let mut sc = LinterConfig::new("shellcheck", vec![], vec![]).expect("compile");
        sc.weight = Some(70);
        config.linter.insert("shellcheck".to_string(), sc);
        let manager = LspClientManager::new(Arc::new(config), test_logging(), test_fs());

        // Unknown root → seed + user layer.
        let w = manager.effective_weights(Some(&PathBuf::from("/unknown")));
        assert_eq!(w.weight("rust-analyzer"), 20, "user override of native");
        assert_eq!(w.weight("rustc"), 100, "seeded flycheck weight survives");
        assert_eq!(w.weight("shellcheck"), 70, "user linter weight");
        assert_eq!(w.weight("unlisted"), BASELINE_WEIGHT);
        assert!(w.is_provisional("rust-analyzer", "E0107"), "seeded band");

        // None root → same seed + user layer, no project override.
        let none = manager.effective_weights(None);
        assert_eq!(none.weight("rust-analyzer"), 20);

        // Project override wins for that root.
        let overridden = PathBuf::from("/override");
        let mut project = crate::config::ProjectConfig::default();
        project.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                weight: Some(3),
                sources: HashMap::from([("rustc".to_string(), 90)]),
                ..ServerDef::default()
            },
        );
        manager.install_root_config(overridden.clone(), project);
        let proj = manager.effective_weights(Some(&overridden));
        assert_eq!(proj.weight("rust-analyzer"), 3, "project native wins");
        assert_eq!(proj.weight("rustc"), 90, "project sub-source override");
    }

    /// `lint_covers` matches the root-relative path against the effective linter
    /// set (user ∪ project), and never covers out-of-root files (ticket 01).
    #[test]
    fn test_lint_covers_user_and_project_union() {
        use crate::config::LinterConfig;

        let mut config = test_config_raw();
        config.linter.insert(
            "shellcheck".to_string(),
            LinterConfig::new("shellcheck", vec![], vec!["**/*.sh".to_string()])
                .expect("compile user linter"),
        );
        let manager = LspClientManager::new(Arc::new(config), test_logging(), test_fs());

        let root = PathBuf::from("/proj");
        let mut project = crate::config::ProjectConfig::default();
        project.linter.insert(
            "actionlint".to_string(),
            LinterConfig::new(
                "actionlint",
                vec![],
                vec![".github/workflows/*.yml".to_string()],
            )
            .expect("compile project linter"),
        );
        manager.install_root_config(root.clone(), project);

        // User linter (shellcheck) matches a .sh anywhere in the root.
        assert!(manager.lint_covers(&root.join("scripts/build.sh")));
        // Project linter (actionlint) matches a workflow, root-relative.
        assert!(manager.lint_covers(&root.join(".github/workflows/ci.yml")));
        // A YAML outside the workflows dir is NOT matched (path glob, not name).
        assert!(!manager.lint_covers(&root.join("docs/config.yml")));
        // A non-matching file in the root.
        assert!(!manager.lint_covers(&root.join("README.md")));
        // An out-of-root file is never covered (routing is root-relative).
        assert!(!manager.lint_covers(Path::new("/elsewhere/x.sh")));
    }

    /// `disable_lint` zeroes linter coverage for the root (ticket 00 / 01).
    #[test]
    fn test_lint_covers_respects_disable_lint() {
        use crate::config::LinterConfig;

        let mut config = test_config_raw();
        config.linter.insert(
            "shellcheck".to_string(),
            LinterConfig::new("shellcheck", vec![], vec!["**/*.sh".to_string()]).expect("compile"),
        );
        let manager = LspClientManager::new(Arc::new(config), test_logging(), test_fs());

        let root = PathBuf::from("/proj");
        manager.install_root_config(
            root.clone(),
            crate::config::ProjectConfig {
                disable_lint: true,
                ..crate::config::ProjectConfig::default()
            },
        );

        assert!(
            !manager.lint_covers(&root.join("x.sh")),
            "disable_lint must zero linter coverage despite a matching user linter"
        );
    }

    /// A project `[linter.rule.<name>] disable = true` overrides a user linter of the
    /// same name (ticket 01 — project wins on a name collision).
    #[test]
    fn test_lint_covers_project_disable_overrides_user() {
        use crate::config::LinterConfig;

        let mut config = test_config_raw();
        config.linter.insert(
            "shellcheck".to_string(),
            LinterConfig::new("shellcheck", vec![], vec!["**/*.sh".to_string()]).expect("compile"),
        );
        let manager = LspClientManager::new(Arc::new(config), test_logging(), test_fs());

        let root = PathBuf::from("/proj");
        let mut overridden =
            LinterConfig::new("shellcheck", vec![], vec!["**/*.sh".to_string()]).expect("compile");
        overridden.disable = true;
        let mut project = crate::config::ProjectConfig::default();
        project.linter.insert("shellcheck".to_string(), overridden);
        manager.install_root_config(root.clone(), project);

        assert!(
            !manager.lint_covers(&root.join("x.sh")),
            "a project disable override must beat the user linter"
        );
    }

    /// `rooted_clients` includes rooted servers (not just single-file).
    #[tokio::test]
    async fn test_rooted_clients_includes_rooted() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let rooted = manager.rooted_clients().await;
        assert_eq!(
            rooted.len(),
            1,
            "rooted_clients should include the spawned server"
        );

        let key = rooted.keys().next().expect("one key");
        assert_eq!(key.language_id, MOCK_LANG_A);
        assert!(
            matches!(key.scope, Scope::Root(_)),
            "scope should be Root, got {:?}",
            key.scope
        );

        Ok(())
    }

    /// `shutdown_instance` removes the server from the client map.
    #[tokio::test]
    async fn test_shutdown_instance_removes_from_map() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert_eq!(manager.clients().await.len(), 1);

        let key = client.lock().await.server().key().expect("key");
        manager.shutdown_instance(&key).await;

        assert!(
            manager.clients().await.is_empty(),
            "client should be removed after shutdown_instance"
        );
        Ok(())
    }

    /// `shutdown_all` empties the client map.
    #[tokio::test]
    async fn test_shutdown_all_empties_map() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(!manager.clients().await.is_empty());

        manager.shutdown_all().await;

        assert!(
            manager.clients().await.is_empty(),
            "all clients should be removed after shutdown_all"
        );
        Ok(())
    }

    /// Polls the reader-loop liveness flag until the child is dead, then
    /// asserts. The teardown ladder's kills reach the flag via pipe EOF.
    async fn wait_dead(server: &Arc<LspServer>) {
        for _ in 0..80 {
            if !server.is_alive() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            !server.is_alive(),
            "child should be dead after the teardown ladder"
        );
    }

    /// Bug 130 rung 2: a child that never answers `shutdown` gets SIGTERM
    /// past the graceful grace and teardown stays bounded.
    #[tokio::test]
    async fn teardown_ladder_sigterms_hung_shutdown() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_hang_shutdown_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        )
        .teardown_timings_override(
            Duration::from_millis(200),
            Duration::from_millis(500),
            Duration::from_secs(10),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server = Arc::clone(client.lock().await.server());
        assert!(server.is_alive(), "mockls should be up before teardown");

        let started = std::time::Instant::now();
        manager.shutdown_all().await;
        let elapsed = started.elapsed();

        assert!(
            manager.clients().await.is_empty(),
            "registry should drain on teardown"
        );
        assert!(
            elapsed >= Duration::from_millis(200),
            "the graceful grace must elapse before escalation, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "teardown must end inside the ceiling, took {elapsed:?}"
        );
        wait_dead(&server).await;
        Ok(())
    }

    /// Bug 130 rung 3: a child that hangs on `shutdown` AND ignores SIGTERM
    /// gets SIGKILL after both graces; teardown still ends.
    #[cfg(unix)]
    #[tokio::test]
    async fn teardown_ladder_sigkills_term_immune_child() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_term_immune_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        )
        .teardown_timings_override(
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_secs(10),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server = Arc::clone(client.lock().await.server());
        assert!(server.is_alive(), "mockls should be up before teardown");

        let started = std::time::Instant::now();
        manager.shutdown_all().await;
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(400),
            "both graces must elapse before SIGKILL, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "teardown must end inside the ceiling, took {elapsed:?}"
        );
        wait_dead(&server).await;
        Ok(())
    }

    /// Bug 130 field wedge: a client mutex held by a stuck in-flight request
    /// cannot pin teardown — the ladder abandons the child within its grace
    /// and `shutdown_all` returns.
    #[tokio::test]
    async fn teardown_bounded_when_client_mutex_wedged() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        )
        .teardown_timings_override(
            Duration::from_millis(150),
            Duration::from_millis(100),
            Duration::from_secs(5),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        // Wedge: hold the client mutex across teardown, as a stuck
        // in-flight request would.
        let guard = client.lock().await;

        let started = std::time::Instant::now();
        manager.shutdown_all().await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "a wedged client mutex must not pin teardown, took {elapsed:?}"
        );
        assert!(
            manager.clients().await.is_empty(),
            "registry should drain even when a client is wedged"
        );
        drop(guard);
        Ok(())
    }

    /// Bug 130 ceiling: a ladder that cannot finish (graces longer than the
    /// ceiling) is cut off — teardown ends at the ceiling and the straggler
    /// gets SIGKILL via its harvested PID.
    #[tokio::test]
    async fn teardown_ceiling_ends_wedged_ladder() -> Result<()> {
        let manager = LspClientManager::new(
            mockls_hang_shutdown_config(),
            test_logging(),
            test_fs_with_roots(&["/tmp"]),
        )
        .teardown_timings_override(
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_millis(400),
        );

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        let server = Arc::clone(client.lock().await.server());
        assert!(server.is_alive(), "mockls should be up before teardown");

        let started = std::time::Instant::now();
        manager.shutdown_all().await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "the ceiling must end teardown regardless of ladder graces, \
             took {elapsed:?}"
        );
        wait_dead(&server).await;
        Ok(())
    }

    /// `effective_server_def` applies `file_patterns` override from project config.
    #[test]
    fn test_effective_server_def_file_patterns_override() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                file_patterns: vec!["*.rs".to_string()],
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        // Project config with non-empty file_patterns → override
        let mut pc = crate::config::ProjectConfig::default();
        pc.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                file_patterns: vec!["*.py".to_string()],
                ..ServerDef::default()
            },
        );
        manager.install_root_config(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");
        assert_eq!(
            merged.file_patterns,
            vec!["*.py"],
            "project file_patterns should override user"
        );
    }

    /// `effective_server_def` preserves user `file_patterns` when project has empty.
    #[test]
    fn test_effective_server_def_empty_file_patterns_no_override() {
        let mut config = test_config_raw();
        config.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                file_patterns: vec!["*.rs".to_string()],
                ..ServerDef::default()
            },
        );

        let manager = LspClientManager::new(config, test_logging(), test_fs());
        let root = PathBuf::from("/project");

        // Project config with empty file_patterns → should NOT override
        let mut pc = crate::config::ProjectConfig::default();
        pc.server
            .insert("rust-analyzer".to_string(), ServerDef::default());
        manager.install_root_config(root.clone(), pc);

        let merged = manager
            .effective_server_def("rust-analyzer", &root)
            .expect("should exist");
        assert_eq!(
            merged.file_patterns,
            vec!["*.rs"],
            "user file_patterns should be preserved when project has empty"
        );
    }

    /// A config-bearing root's `[lsp.language.*]` classification reaches
    /// `FilesystemManager` (the tables are derived on the `Root`, ticket 00a).
    #[test]
    fn test_root_config_feeds_classification() {
        let root = PathBuf::from("/project");
        let fs = test_fs_with_roots(&["/project"]);
        let manager = LspClientManager::new(test_config(), test_logging(), Arc::clone(&fs));

        let mut pc = crate::config::ProjectConfig::default();
        pc.language.insert(
            "custom".to_string(),
            LanguageConfig {
                extensions: Some(vec!["xyz".to_string()]),
                ..LanguageConfig::default()
            },
        );
        manager.install_root_config(root, pc);

        // Verify: a file with .xyz extension under this root should resolve
        // to the "custom" language via per-root classification.
        let lang = fs.language_id(Path::new("/project/test.xyz"));
        assert_eq!(
            lang.as_deref(),
            Some("custom"),
            "per-root classification should map .xyz to custom"
        );
    }

    /// A root with no `[lsp.language.*]` classification falls through to the global
    /// tables (no per-root entry).
    #[test]
    fn test_root_config_empty_classification_falls_through() {
        let fs = test_fs_with_roots(&["/project"]);
        let manager = LspClientManager::new(test_config(), test_logging(), Arc::clone(&fs));

        let root = PathBuf::from("/project");
        // No classification fields → empty tables.
        manager.install_root_config(root, crate::config::ProjectConfig::default());

        // No per-root classification, so language_id returns None for an
        // unknown extension.
        let lang = fs.language_id(Path::new("/project/test.xyz"));
        assert!(
            lang.is_none(),
            "empty classification tables should not match"
        );
    }

    /// `wait_ready_for_path` actually waits for server readiness.
    #[tokio::test]
    async fn test_wait_ready_for_path_waits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
        std::fs::write(&file, "")?;

        let manager = LspClientManager::new(
            mockls_config(),
            test_logging(),
            test_fs_with_roots(&[dir.path().to_str().expect("utf8")]),
        );

        let _ = ensure_first_server(&manager, MOCK_LANG_A).await?;

        // wait_ready_for_path should not return until server is ready
        manager.wait_ready_for_path(&file).await;

        // After waiting, the server should be in Probing or Healthy state
        let clients = manager.clients().await;
        let (_, client) = clients.iter().next().expect("should have client");
        let lifecycle = client.lock().await.lifecycle();
        assert!(
            lifecycle == crate::lsp::state::ServerLifecycle::Probing
                || lifecycle == crate::lsp::state::ServerLifecycle::Healthy,
            "server should be Probing or Healthy after wait_ready, got {lifecycle:?}"
        );
        Ok(())
    }

    /// `wait_ready_for_path` finds the instance when markers resolve
    /// to a sub-crate root different from the workspace root.
    #[tokio::test]
    async fn test_wait_ready_for_path_marker_root() -> Result<()> {
        // Layout: workspace/sub_crate/Cargo.toml + workspace/sub_crate/src/lib.yX4Za
        // Marker root = workspace/sub_crate, workspace root = workspace.
        let dir = tempfile::tempdir()?;
        let ws = dir.path().join("workspace");
        let sub = ws.join("sub_crate");
        let src = sub.join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(sub.join("Cargo.toml"), "").expect("marker");
        let file = src.join(format!("lib.{MOCK_LANG_A}"));
        std::fs::write(&file, "").expect("file");

        let config = mockls_legacy_markers_config(vec!["Cargo.toml".into()]);
        let ws_str = ws.to_str().expect("utf8");
        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&[ws_str]));

        // Spawn at the marker root (sub_crate), matching what
        // ensure_clients_for_paths would do.
        let server_name = &manager
            .config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .first()
            .expect("binding")
            .name;
        manager
            .ensure_server(MOCK_LANG_A, server_name, &sub)
            .await?;

        // Before the fix, this would fail to find the instance
        // (looking up workspace root instead of marker root) and
        // return immediately without waiting.
        manager.wait_ready_for_path(&file).await;

        // Verify we actually found and waited on the server.
        let clients = manager.clients().await;
        let (_, client) = clients.iter().next().expect("should have client");
        let lifecycle = client.lock().await.lifecycle();
        assert!(
            lifecycle == crate::lsp::state::ServerLifecycle::Probing
                || lifecycle == crate::lsp::state::ServerLifecycle::Healthy,
            "server should be Probing or Healthy after wait_ready, got {lifecycle:?}"
        );
        Ok(())
    }

    // ── Marker / scope decoupling tests ─────────────────────────────

    /// Config with workspace-folder-capable server AND root markers.
    fn mockls_workspace_folders_markers_config(markers: Vec<String>) -> Arc<Config> {
        let bin = mockls_bin();
        // The `mockls-event` persona (blessed, event discipline; diagnostics-debt
        // 04c) so `get_servers(supports_diagnostics)` counts this instance — the
        // fallback test asserts on the diagnostics-covered set.
        let server_name = "mockls-event".to_string();
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string(), "--workspace-folders".to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                root_markers: Some(markers),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    /// Config with legacy (no workspace folders) server AND root markers.
    fn mockls_legacy_markers_config(markers: Vec<String>) -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}-lm");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                root_markers: Some(markers),
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
            linter: HashMap::new(),
            quarantined: crate::config::Quarantine::new(),
        })
    }

    #[tokio::test]
    #[allow(
        clippy::similar_names,
        reason = "root_a/root_b are intentionally parallel"
    )]
    async fn test_marker_workspace_capable_per_root() -> Result<()> {
        // Two roots with markers + workspace-folder-capable server →
        // two per-root instances.
        let dir = tempfile::tempdir().expect("tempdir");
        let root_a = dir.path().join("project_a");
        let root_b = dir.path().join("project_b");
        std::fs::create_dir_all(&root_a).expect("mkdir a");
        std::fs::create_dir_all(&root_b).expect("mkdir b");
        std::fs::write(root_a.join("Cargo.toml"), "").expect("marker a");
        std::fs::write(root_b.join("Cargo.toml"), "").expect("marker b");

        let config = mockls_workspace_folders_markers_config(vec!["Cargo.toml".into()]);
        let root_a_str = root_a.to_str().expect("path a");
        let root_b_str = root_b.to_str().expect("path b");
        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[root_a_str, root_b_str]),
        );

        let server_name = &manager
            .config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .first()
            .expect("binding")
            .name;

        // Spawn for both roots.
        manager
            .ensure_server(MOCK_LANG_A, server_name, &root_a)
            .await?;
        manager
            .ensure_server(MOCK_LANG_A, server_name, &root_b)
            .await?;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 2, "Each root should have its own instance");
        for key in clients.keys() {
            assert!(
                matches!(&key.scope, Scope::Root(_)),
                "Each instance should be Scope::Root, got {:?}",
                key.scope,
            );
        }

        Ok(())
    }

    #[tokio::test]
    #[allow(
        clippy::similar_names,
        reason = "root_a/root_b are intentionally parallel"
    )]
    async fn test_marker_no_workspace_folders_isolates() -> Result<()> {
        // Two roots with markers + legacy server (no workspace folders)
        // → two instances with Scope::Root each.
        let dir = tempfile::tempdir().expect("tempdir");
        let root_a = dir.path().join("project_a");
        let root_b = dir.path().join("project_b");
        std::fs::create_dir_all(&root_a).expect("mkdir a");
        std::fs::create_dir_all(&root_b).expect("mkdir b");
        std::fs::write(root_a.join("Cargo.toml"), "").expect("marker a");
        std::fs::write(root_b.join("Cargo.toml"), "").expect("marker b");

        let config = mockls_legacy_markers_config(vec!["Cargo.toml".into()]);
        let root_a_str = root_a.to_str().expect("path a");
        let root_b_str = root_b.to_str().expect("path b");
        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[root_a_str, root_b_str]),
        );

        let server_name = &manager
            .config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .first()
            .expect("binding")
            .name;

        // Spawn for both roots.
        manager
            .ensure_server(MOCK_LANG_A, server_name, &root_a)
            .await?;
        manager
            .ensure_server(MOCK_LANG_A, server_name, &root_b)
            .await?;

        let clients = manager.clients().await;
        assert_eq!(clients.len(), 2, "Legacy server should have two instances");
        for key in clients.keys() {
            assert!(
                matches!(&key.scope, Scope::Root(_)),
                "Each instance should be Scope::Root, got {:?}",
                key.scope,
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_sync_roots_marker_spawns_per_root() -> Result<()> {
        // Start with one root, add a second mid-session. Each root
        // gets its own per-root instance.
        let dir = tempfile::tempdir().expect("tempdir");
        let root_a = dir.path().join("project_a");
        let root_b = dir.path().join("project_b");
        std::fs::create_dir_all(&root_a).expect("mkdir a");
        std::fs::create_dir_all(&root_b).expect("mkdir b");
        std::fs::write(root_a.join("Cargo.toml"), "").expect("marker a");
        std::fs::write(root_b.join("Cargo.toml"), "").expect("marker b");
        // Create a file so detect_workspace_languages finds the language.
        std::fs::write(root_b.join(format!("file.{MOCK_LANG_A}")), "").expect("file b");

        let config = mockls_workspace_folders_markers_config(vec!["Cargo.toml".into()]);
        let root_a_str = root_a.to_str().expect("path a");
        let manager =
            LspClientManager::new(config, test_logging(), test_fs_with_roots(&[root_a_str]));

        let client = ensure_first_server(&manager, MOCK_LANG_A).await?;
        assert!(client.lock().await.is_alive());
        assert_eq!(manager.clients().await.len(), 1);

        // Add second root mid-session.
        manager
            .sync_roots(rich_bufs(vec![root_a.clone(), root_b.clone()]))
            .await?;

        // Two instances — one per root.
        let clients = manager.clients().await;
        assert_eq!(
            clients.len(),
            2,
            "Should have two per-root instances after adding root"
        );
        assert_eq!(count_scope(&clients, MOCK_LANG_A, "root"), 2);

        Ok(())
    }

    // ── Workspace folder marker tests (misc 103) ────────────────────

    /// Workspace-folder-capable server with markers: `ensure_clients_for_paths`
    /// should NOT spawn a redundant instance at the marker root when a
    /// workspace-root instance already exists. Instead it sends
    /// `didChangeWorkspaceFolders`.
    #[tokio::test]
    async fn test_ensure_clients_ws_folders_no_redundant_spawn() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("sub_crate");
        let src = sub.join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(sub.join("Cargo.toml"), "").expect("marker");
        let file = src.join(format!("lib.{MOCK_LANG_A}"));
        std::fs::write(&file, "").expect("file");

        let config = mockls_workspace_folders_markers_config(vec!["Cargo.toml".into()]);
        let ws_str = ws.to_str().expect("utf8");
        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&[ws_str]));

        // Spawn at workspace root (normal spawn_all behavior).
        let server_name = &manager
            .config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .first()
            .expect("binding")
            .name;
        manager.ensure_server(MOCK_LANG_A, server_name, &ws).await?;
        assert_eq!(manager.clients().await.len(), 1);

        // ensure_clients_for_paths for a file in the sub-crate should
        // NOT spawn a second instance.
        manager.ensure_clients_for_paths(&[file]).await;
        assert_eq!(
            manager.clients().await.len(),
            1,
            "Workspace-folder-capable server should not spawn redundant instance at marker root"
        );

        Ok(())
    }

    /// Legacy server (no workspace folders) with markers:
    /// `ensure_clients_for_paths` SHOULD spawn a per-marker-root instance.
    #[tokio::test]
    async fn test_ensure_clients_legacy_spawns_at_marker_root() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("sub_crate");
        let src = sub.join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(sub.join("Cargo.toml"), "").expect("marker");
        let file = src.join(format!("lib.{MOCK_LANG_A}"));
        std::fs::write(&file, "").expect("file");

        let config = mockls_legacy_markers_config(vec!["Cargo.toml".into()]);
        let ws_str = ws.to_str().expect("utf8");
        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&[ws_str]));

        // Spawn at workspace root.
        let server_name = &manager
            .config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .first()
            .expect("binding")
            .name;
        manager.ensure_server(MOCK_LANG_A, server_name, &ws).await?;
        assert_eq!(manager.clients().await.len(), 1);

        // ensure_clients_for_paths for a sub-crate file SHOULD spawn
        // a second instance (legacy server can't receive workspace folders).
        manager.ensure_clients_for_paths(&[file]).await;
        assert_eq!(
            manager.clients().await.len(),
            2,
            "Legacy server should spawn a separate instance at the marker root"
        );

        Ok(())
    }

    /// `get_servers` finds the workspace-root instance for files in
    /// sub-crate marker roots (workspace-folder-capable server).
    #[tokio::test]
    async fn test_get_servers_ws_folder_fallback() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("sub_crate");
        let src = sub.join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(sub.join("Cargo.toml"), "").expect("marker");
        let file = src.join(format!("lib.{MOCK_LANG_A}"));
        std::fs::write(&file, "").expect("file");

        let config = mockls_workspace_folders_markers_config(vec!["Cargo.toml".into()]);
        let ws_str = ws.to_str().expect("utf8");
        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&[ws_str]));

        // Only spawn at workspace root — no instance at marker root.
        let server_name = &manager
            .config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .first()
            .expect("binding")
            .name;
        manager.ensure_server(MOCK_LANG_A, server_name, &ws).await?;

        // get_servers should find the workspace-root instance for the
        // sub-crate file via workspace folder fallback.
        let servers = manager
            .get_servers(&file, LspServer::supports_diagnostics, None)
            .await;
        assert_eq!(
            servers.len(),
            1,
            "get_servers should find the workspace-root instance for sub-crate files"
        );

        Ok(())
    }

    /// `wait_ready_for_path` finds the workspace-root instance for
    /// sub-crate marker roots (workspace-folder-capable server).
    #[tokio::test]
    async fn test_wait_ready_ws_folder_fallback() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("workspace");
        let sub = ws.join("sub_crate");
        let src = sub.join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(sub.join("Cargo.toml"), "").expect("marker");
        let file = src.join(format!("lib.{MOCK_LANG_A}"));
        std::fs::write(&file, "").expect("file");

        let config = mockls_workspace_folders_markers_config(vec!["Cargo.toml".into()]);
        let ws_str = ws.to_str().expect("utf8");
        let manager = LspClientManager::new(config, test_logging(), test_fs_with_roots(&[ws_str]));

        // Only spawn at workspace root.
        let server_name = &manager
            .config
            .resolve_language(MOCK_LANG_A)
            .expect("lang config")
            .servers()
            .first()
            .expect("binding")
            .name;
        manager.ensure_server(MOCK_LANG_A, server_name, &ws).await?;

        // wait_ready_for_path should find the workspace-root instance.
        manager.wait_ready_for_path(&file).await;

        let clients = manager.clients().await;
        let (_, client) = clients.iter().next().expect("should have client");
        let lifecycle = client.lock().await.lifecycle();
        assert!(
            lifecycle == crate::lsp::state::ServerLifecycle::Probing
                || lifecycle == crate::lsp::state::ServerLifecycle::Healthy,
            "server should be Probing or Healthy after wait_ready, got {lifecycle:?}"
        );
        Ok(())
    }

    /// The changed-set router rebuilds the `file://` URI from the owning root
    /// plus the root-relative path the baseline stores (WS31 Consumer A). The
    /// join round-trips: `root` + `rel` reconstructs the original absolute path.
    #[test]
    fn relative_path_roundtrips_to_uri() {
        let root = PathBuf::from("/home/user/project");
        let rel = PathBuf::from("src/bridge/handler.rs");
        let uri = changed_file_uri(&root, &rel);
        assert_eq!(uri, "file:///home/user/project/src/bridge/handler.rs");

        // A nested relative path with no directory component also round-trips.
        let rel_top = PathBuf::from("Cargo.toml");
        assert_eq!(
            changed_file_uri(&root, &rel_top),
            "file:///home/user/project/Cargo.toml"
        );
    }

    /// Removing a root via `sync_roots` drops its changed-set baseline and
    /// generation counter; re-adding it yields a fresh first-walk snapshot.
    #[tokio::test]
    async fn baseline_dropped_on_root_removal() -> Result<()> {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let root_str = root.to_str().expect("root path");

        let fs = test_fs_with_roots(&[root_str]);
        // Seed a baseline + generation for the root (simulating a prior walk).
        let _ = fs.diff_and_update(&root, &[(PathBuf::from("a.rs"), 100)]);
        fs.bump_generation_for_test(&root);
        assert!(fs.has_baseline_for_test(&root));
        assert!(fs.has_generation_for_test(&root));

        let manager = LspClientManager::new(mockls_config(), test_logging(), Arc::clone(&fs));

        // Remove the root via sync_roots (new set excludes it).
        manager.sync_roots(vec![]).await?;

        assert!(
            !fs.has_baseline_for_test(&root),
            "last_seen entry should be dropped on root removal"
        );
        assert!(
            !fs.has_generation_for_test(&root),
            "root_generations entry should be dropped on root removal"
        );

        // Re-add the root and walk again ⇒ fresh cold-start full set.
        manager.sync_roots(rich_bufs(vec![root.clone()])).await?;
        let set = fs.diff_and_update(&root, &[(PathBuf::from("a.rs"), 100)]);
        assert_eq!(set.changes.len(), 1, "re-added root ⇒ fresh first walk");

        Ok(())
    }

    // ---- misc 191: per-key in-flight cold-spawn markers ----

    /// A mockls config whose one server for `MOCK_LANG_A` carries the given
    /// extra CLI args (e.g. `--response-delay`, `--request-log`). The first
    /// positional is always the language name mockls filters on.
    fn mockls_config_with_args(extra_args: &[&str]) -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = format!("mockls-{MOCK_LANG_A}");
        let mut args = vec![MOCK_LANG_A.to_string()];
        args.extend(extra_args.iter().map(ToString::to_string));
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args,
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            ..test_config_raw()
        })
    }

    /// The server name `mockls_config_with_args` binds for `MOCK_LANG_A`.
    fn mockls_server_name() -> String {
        format!("mockls-{MOCK_LANG_A}")
    }

    /// The `Scope::Root` instance key for the one mockls server at `root`.
    fn root_key(root: &Path) -> InstanceKey {
        InstanceKey::new(
            MOCK_LANG_A.to_string(),
            mockls_server_name(),
            Scope::Root(root.to_path_buf()),
        )
    }

    /// Yield-polls `cond` until it holds, cooperatively (no wall-clock
    /// assertion): a courtesy 1 ms spacing keeps the poll off a hot spin while
    /// the concurrent spawn tasks make progress on other worker threads. The
    /// bounded backstop turns a genuine wedge (the very regression under test)
    /// into a failure instead of a hang.
    async fn poll_until(mut cond: impl FnMut() -> bool) -> Result<()> {
        for _ in 0..2_000 {
            if cond() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        anyhow::bail!("condition never held within the poll backstop")
    }

    /// Concurrent requests for the SAME cold key spawn exactly one process —
    /// the anti-duplicate property, preserved keyed instead of registry-locked
    /// (misc 191). Every requester either owns the spawn or awaits its marker;
    /// none launches a second `initialize`. `--request-log --log-pid-suffix`
    /// makes each mockls process write its own file, so the file count is the
    /// process count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_same_key_spawns_exactly_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let log = dir.path().join("requests.jsonl");
        // A slow init widens the concurrent window so every requester arrives
        // while the owner's spawn is still in flight.
        let config = mockls_config_with_args(&[
            "--response-delay",
            "200",
            "--request-log",
            log.to_str().expect("log path"),
            "--log-pid-suffix",
        ]);
        let manager = Arc::new(LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[root.to_str().expect("root")]),
        ));

        let server = mockls_server_name();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let server = server.clone();
            let root = root.clone();
            handles.push(tokio::spawn(async move {
                manager.spawn(&server, MOCK_LANG_A, &root).await
            }));
        }

        let mut clients = Vec::new();
        for handle in handles {
            let (_key, client) = handle
                .await
                .expect("spawn task panicked")
                .expect("spawn succeeded");
            clients.push(client);
        }

        // Every requester got the SAME instance.
        let first = &clients[0];
        for client in &clients[1..] {
            assert!(
                Arc::ptr_eq(first, client),
                "all concurrent requesters must share one instance"
            );
        }
        assert_eq!(
            manager.clients().await.len(),
            1,
            "the registry holds exactly one instance for the key"
        );
        // Exactly one process handled a request ⇒ exactly one spawn.
        let log_files: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("requests.jsonl.")
            })
            .collect();
        assert_eq!(
            log_files.len(),
            1,
            "exactly one mockls process spawned (anti-duplicate); found {} request-log files",
            log_files.len()
        );
        // The marker cleared once the spawn landed.
        assert_eq!(manager.spawning_len(), 0, "marker cleared after spawn");

        manager.shutdown_all().await;
        Ok(())
    }

    /// A cold spawn of one key must not stall a DIFFERENT key's spawn — the
    /// whole point of misc 191. Proof is purely operation/state based: two
    /// different-key cold spawns are driven to be in flight *simultaneously*
    /// (both markers present at once). Under the pre-191 hold — the registry
    /// lock kept across spawn+`initialize` — the second spawn could not even
    /// reach its marker insert (it would block acquiring the registry lock),
    /// so both markers being live at once is impossible there and decisive here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cold_spawn_does_not_stall_a_different_key() -> Result<()> {
        let dir_a = tempfile::tempdir()?;
        let dir_b = tempfile::tempdir()?;
        let root_a = dir_a.path().to_path_buf();
        let root_b = dir_b.path().to_path_buf();
        // Slow init on both roots so each spawn dwells in its handshake window.
        let config = mockls_config_with_args(&["--response-delay", "400"]);
        let manager = Arc::new(LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[
                root_a.to_str().expect("root a"),
                root_b.to_str().expect("root b"),
            ]),
        ));
        let server = mockls_server_name();
        let key_a = root_key(&root_a);
        let key_b = root_key(&root_b);

        // Owner A: a cold spawn on root A, in flight in the background.
        let spawn_a = {
            let manager = Arc::clone(&manager);
            let server = server.clone();
            let root_a = root_a.clone();
            tokio::spawn(async move { manager.spawn(&server, MOCK_LANG_A, &root_a).await })
        };
        // Wait — by state, not by clock — until A is provably mid-handshake
        // (marker present, registry lock already dropped).
        poll_until(|| manager.is_spawning(&key_a)).await?;

        // Owner B: a cold spawn on root B, started while A is still in flight.
        let spawn_b = {
            let manager = Arc::clone(&manager);
            let server = server.clone();
            let root_b = root_b.clone();
            tokio::spawn(async move { manager.spawn(&server, MOCK_LANG_A, &root_b).await })
        };
        // B reaches its own marker while A's is still live: two cold spawns in
        // flight at once. The pre-191 registry hold makes this unreachable.
        poll_until(|| manager.is_spawning(&key_b) && manager.is_spawning(&key_a)).await?;

        // Both complete; both land distinct live instances.
        let (_ka, client_a) = spawn_a.await.expect("A task").expect("A spawned");
        let (_kb, client_b) = spawn_b.await.expect("B task").expect("B spawned");
        assert!(!Arc::ptr_eq(&client_a, &client_b), "distinct instances");
        assert_eq!(manager.clients().await.len(), 2, "two roots, two instances");
        assert_eq!(manager.spawning_len(), 0, "both markers cleared");

        manager.shutdown_all().await;
        Ok(())
    }

    /// A spawn failure clears the marker (the pinned failure semantic): the key
    /// is not wedged, and the next request retries fresh. A process that never
    /// starts (bogus program) fails before any tombstone insert, so the retry
    /// sees an empty registry and spawns anew rather than short-circuiting on a
    /// tombstone.
    #[tokio::test]
    async fn spawn_failure_clears_marker_and_retries_fresh() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let server_name = mockls_server_name();
        // A program that cannot start: `LspClient::spawn` errors before the
        // tombstone path, so no instance is inserted.
        let bogus = dir.path().join("does-not-exist-mockls");
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bogus.to_string_lossy().to_string()),
                args: vec![MOCK_LANG_A.to_string()],
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name.clone())]),
                ..LanguageConfig::default()
            },
        );
        let config = Arc::new(Config {
            language,
            server,
            ..test_config_raw()
        });
        let manager = LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[root.to_str().expect("root")]),
        );
        let key = root_key(&root);

        // First attempt fails (process won't start); the marker is cleared and
        // no instance is left behind.
        assert!(
            manager
                .spawn(&server_name, MOCK_LANG_A, &root)
                .await
                .is_err(),
            "a bogus program fails to spawn"
        );
        assert!(
            !manager.is_spawning(&key),
            "the marker is cleared on failure — the key is not wedged"
        );
        assert_eq!(manager.spawning_len(), 0, "no marker left in flight");
        assert!(
            manager.clients().await.is_empty(),
            "a process-spawn failure leaves no tombstone"
        );

        // Second attempt retries fresh (re-claims the marker, fails again the
        // same way) — the failure did not lock the key out. Two attempts stay
        // below the misc-167 strike cap.
        assert!(
            manager
                .spawn(&server_name, MOCK_LANG_A, &root)
                .await
                .is_err(),
            "the next request retries fresh and fails the same way"
        );
        assert_eq!(manager.spawning_len(), 0, "marker cleared after the retry");

        Ok(())
    }

    // ---- misc 208: the shared spawn-or-await gate covers the rootless tier ----

    /// Single-file variant of [`mockls_config_with_args`]: the one server for
    /// `MOCK_LANG_A` carries `single_file = true` plus the given extra CLI
    /// args.
    fn mockls_single_file_config_with_args(extra_args: &[&str]) -> Arc<Config> {
        let bin = mockls_bin();
        let server_name = mockls_server_name();
        let mut args = vec![MOCK_LANG_A.to_string()];
        args.extend(extra_args.iter().map(ToString::to_string));
        let mut server = HashMap::new();
        server.insert(
            server_name.clone(),
            ServerDef {
                path: Some(bin.to_string_lossy().to_string()),
                args,
                single_file: true,
                ..ServerDef::default()
            },
        );
        let mut language = HashMap::new();
        language.insert(
            MOCK_LANG_A.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(server_name)]),
                ..LanguageConfig::default()
            },
        );
        Arc::new(Config {
            language,
            server,
            ..test_config_raw()
        })
    }

    /// The rootless singleton key for the one mockls server.
    fn sf_key() -> InstanceKey {
        InstanceKey::new(
            MOCK_LANG_A.to_string(),
            mockls_server_name(),
            Scope::SingleFile,
        )
    }

    /// Concurrent requests for the SAME cold singleton key spawn exactly one
    /// process — misc 191's anti-duplicate property, carried to the rootless
    /// tier by the shared gate (misc 208). `--request-log --log-pid-suffix`
    /// makes each mockls process write its own file, so the file count is the
    /// process count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_single_file_spawns_exactly_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("requests.jsonl");
        // A slow init widens the concurrent window so every requester arrives
        // while the owner's handshake is still in flight.
        let config = mockls_single_file_config_with_args(&[
            "--response-delay",
            "200",
            "--request-log",
            log.to_str().expect("log path"),
            "--log-pid-suffix",
        ]);
        let manager = Arc::new(LspClientManager::new(config, test_logging(), test_fs()));

        let server = mockls_server_name();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let server = server.clone();
            handles.push(tokio::spawn(async move {
                manager.spawn_single_file(&server, MOCK_LANG_A).await
            }));
        }

        let mut clients = Vec::new();
        for handle in handles {
            clients.push(
                handle
                    .await
                    .expect("spawn task panicked")
                    .expect("spawn succeeded"),
            );
        }

        // Every requester got the SAME instance.
        let first = &clients[0];
        for client in &clients[1..] {
            assert!(
                Arc::ptr_eq(first, client),
                "all concurrent requesters must share one singleton"
            );
        }
        assert_eq!(
            manager.clients().await.len(),
            1,
            "the registry holds exactly one singleton for the key"
        );
        // Exactly one process handled a request ⇒ exactly one spawn.
        let log_files: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("requests.jsonl.")
            })
            .collect();
        assert_eq!(
            log_files.len(),
            1,
            "exactly one mockls process spawned (anti-duplicate); found {} request-log files",
            log_files.len()
        );
        assert_eq!(manager.spawning_len(), 0, "marker cleared after spawn");

        manager.shutdown_all().await;
        Ok(())
    }

    /// A cold singleton spawn must not stall a DIFFERENT key — the misc-208
    /// regression proof, state-based like misc 191's. Pre-208
    /// `spawn_single_file` held the clients registry lock across its whole
    /// spawn+`initialize` handshake: no marker ever existed for a singleton
    /// key, and a per-root cold spawn could not even reach its own marker
    /// insert while a singleton handshake was in flight. Both markers being
    /// live at once is therefore unreachable pre-208 and decisive here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn single_file_cold_spawn_does_not_stall_a_different_key() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        // Slow init on both spawns so each dwells in its handshake window.
        let config = mockls_single_file_config_with_args(&["--response-delay", "400"]);
        let manager = Arc::new(LspClientManager::new(
            config,
            test_logging(),
            test_fs_with_roots(&[root.to_str().expect("root")]),
        ));
        let server = mockls_server_name();
        let key_sf = sf_key();
        let key_root = root_key(&root);

        // Owner A: the singleton's cold spawn, in flight in the background.
        let spawn_sf = {
            let manager = Arc::clone(&manager);
            let server = server.clone();
            tokio::spawn(async move { manager.spawn_single_file(&server, MOCK_LANG_A).await })
        };
        // Wait — by state, not by clock — until the singleton spawn is
        // provably mid-handshake (marker present, registry lock dropped).
        poll_until(|| manager.is_spawning(&key_sf)).await?;

        // Owner B: a per-root cold spawn, started while A is still in flight.
        let spawn_root = {
            let manager = Arc::clone(&manager);
            let server = server.clone();
            let root = root.clone();
            tokio::spawn(async move { manager.spawn(&server, MOCK_LANG_A, &root).await })
        };
        // B reaches its own marker while A's is still live: two cold spawns
        // in flight at once across the rooted and rootless tiers.
        poll_until(|| manager.is_spawning(&key_root) && manager.is_spawning(&key_sf)).await?;

        // Both complete; both land distinct live instances.
        let sf_client = spawn_sf.await.expect("singleton task").expect("sf spawned");
        let (_k, root_client) = spawn_root.await.expect("root task").expect("root spawned");
        assert!(!Arc::ptr_eq(&sf_client, &root_client), "distinct instances");
        assert_eq!(
            manager.clients().await.len(),
            2,
            "one singleton, one per-root instance"
        );
        assert_eq!(manager.spawning_len(), 0, "both markers cleared");

        manager.shutdown_all().await;
        Ok(())
    }

    /// A failed singleton init fans out as ONE handshake across concurrent
    /// requesters: the owner negative-caches before its marker drops, and
    /// every woken waiter (or late arrival) honors the cache instead of
    /// retrying the spawn — the process count stays 1, the marker clears,
    /// and no tombstone is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_single_file_init_is_one_attempt_across_waiters() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let log = dir.path().join("requests.jsonl");
        let config = mockls_single_file_config_with_args(&[
            "--reject-null-workspace",
            "--response-delay",
            "200",
            "--request-log",
            log.to_str().expect("log path"),
            "--log-pid-suffix",
        ]);
        let manager = Arc::new(LspClientManager::new(config, test_logging(), test_fs()));

        let server = mockls_server_name();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let server = server.clone();
            handles.push(tokio::spawn(async move {
                manager.spawn_single_file(&server, MOCK_LANG_A).await
            }));
        }
        for handle in handles {
            assert!(
                handle.await.expect("spawn task panicked").is_err(),
                "every requester fails — the server rejects null-workspace init"
            );
        }

        assert!(
            manager.clients().await.is_empty(),
            "a failed singleton leaves no tombstone (the negative cache is the memory)"
        );
        assert_eq!(manager.spawning_len(), 0, "marker cleared on failure");
        assert!(
            manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&(MOCK_LANG_A.to_string(), mockls_server_name())),
            "the failure is negative-cached"
        );
        // Exactly one process received the (rejected) initialize — the owner's.
        let log_files: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("requests.jsonl.")
            })
            .collect();
        assert_eq!(
            log_files.len(),
            1,
            "one failed handshake total, not one per waiter; found {} request-log files",
            log_files.len()
        );
        Ok(())
    }

    // ── project-config forwarding: the init-options layering (misc 202) ──

    /// A convention-carrying server (rust-analyzer) with a present config file
    /// and NO user options forwards the translated file as the init options —
    /// the spawn path picks the file up from the workspace root.
    #[test]
    fn project_config_file_forwards_when_user_supplies_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("rust-analyzer.toml"),
            "check.command = \"clippy\"\ncargo.features = [\"mockls\"]\n",
        )
        .expect("write rust-analyzer.toml");

        let options =
            initialization_options_with_project_config(root.path(), "rust-analyzer", None)
                .expect("a present file with no user options forwards the file");
        assert_eq!(
            options,
            serde_json::json!({
                "check": { "command": "clippy" },
                "cargo": { "features": ["mockls"] },
            }),
        );
    }

    /// The merge order: the project file is the base, the user's Catenary-config
    /// options overlay ON TOP and win on conflict, unrelated keys from both
    /// survive.
    #[test]
    fn user_options_win_over_the_project_file() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("rust-analyzer.toml"),
            "check.command = \"clippy\"\ncargo.features = [\"from-file\"]\n",
        )
        .expect("write rust-analyzer.toml");

        // The user's machine-level Catenary config disagrees on check.command and
        // adds an unrelated key.
        let user = serde_json::json!({
            "check": { "command": "check" },
            "trace": { "server": "verbose" },
        });
        let options =
            initialization_options_with_project_config(root.path(), "rust-analyzer", Some(user))
                .expect("merged options");

        // User wins on the conflicting key…
        assert_eq!(options["check"]["command"], serde_json::json!("check"));
        // …the file's unrelated key survives…
        assert_eq!(
            options["cargo"]["features"],
            serde_json::json!(["from-file"]),
        );
        // …and the user's unrelated key survives.
        assert_eq!(options["trace"]["server"], serde_json::json!("verbose"));
    }

    /// A server with no project-config convention (gopls) is pure pass-through:
    /// the user's options are returned unchanged, no file is ever read.
    #[test]
    fn no_convention_server_is_identity_on_user_options() {
        let root = tempfile::tempdir().expect("tempdir");
        // Even a file named like a convention at the root is irrelevant here.
        std::fs::write(
            root.path().join("rust-analyzer.toml"),
            "check.command = \"clippy\"\n",
        )
        .expect("write a file");

        let user = serde_json::json!({ "buildFlags": ["-tags=x"] });
        assert_eq!(
            initialization_options_with_project_config(root.path(), "gopls", Some(user.clone()),),
            Some(user),
            "a no-convention server passes user options through unchanged",
        );
        // And with no user options either, nothing is forwarded.
        assert_eq!(
            initialization_options_with_project_config(root.path(), "gopls", None),
            None,
        );
    }

    /// An absent file leaves today's behavior unchanged — the user's options
    /// (present or absent) pass straight through.
    #[test]
    fn absent_file_leaves_user_options_unchanged() {
        let root = tempfile::tempdir().expect("tempdir");
        // No rust-analyzer.toml at the root.
        assert_eq!(
            initialization_options_with_project_config(root.path(), "rust-analyzer", None),
            None,
            "absent file + no user options → None (today's behavior)",
        );
        let user = serde_json::json!({ "cargo": { "features": ["x"] } });
        assert_eq!(
            initialization_options_with_project_config(
                root.path(),
                "rust-analyzer",
                Some(user.clone()),
            ),
            Some(user),
            "absent file → the user's options are untouched",
        );
    }
}
