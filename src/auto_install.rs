// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Background auto-install of missing blessed servers (ls-manager 05).
//!
//! Opt-in (`[servers] auto_install`, user-config only — default `false`): at
//! each `SessionStart` the daemon's hook dispatch detects, from the session's
//! roots ([`detect_missing`]), blessed servers that a served root wants but
//! that are **not resolvable for spawn** — no managed install at the blessed
//! pin and nothing on `PATH` — and kicks each one as a daemon-side
//! **background** task ([`AutoInstaller::kick`]). The dispatch is a spawn,
//! never an await: session start returns immediately whether or not an install
//! kicked, and even the success path never waits on registry latency.
//!
//! **Two legs, not one** (bug 148 ruling). *Eager*: the session-start detection
//! above — a root wants a marker language's servers when a `root_markers` hit
//! binds it, and a **markerless** (file-presence) language's servers when a
//! matching file sits at the root's own surface, one depth-0 directory read, no
//! walk. Before that leg existed, no server bound to a markerless language
//! (toml / json / yaml / …) could ever be detected as missing, under any flag.
//! *JIT*: the spawn path's not-installed classification
//! ([`crate::lsp::manager`]) is the ground-truth "wants but cannot spawn"
//! signal, and kicks through the same [`AutoInstaller`] — same announce, same
//! in-flight dedupe, same concurrency cap, same one-warn-per-lifetime failure
//! contract. The eligibility gate both legs ask is [`AutoInstaller::install_target`].
//!
//! The install itself is the exact engine `catenary`'s guided install runs —
//! [`BlessedRecipe::resolve`] → [`InstallPlan::resolve`] → [`install::execute`]
//! against the managed home ([`ManagedHome`]) — there is no second install
//! path. **Completion kicks a pre-warm**: the caller-supplied callback fires,
//! and the router runs the same `spawn_all` machinery a `catenary pin` uses, so
//! coverage arrives promptly for every live mounted root whose markers match
//! rather than lazily on the next query. A landed install also enacts the
//! warranty-renewal GC (lsm 06, [`ManagedHome::collect_stale_versions`] — see
//! the [`crate::managed_home`] docs): version dirs beyond the kept pair are
//! collected, each named on the landing's firehose event and snapshot record.
//!
//! Failure is skip-with-finding, never a retry loop: the failure is recorded on
//! the daemon snapshot ([`crate::state_snapshot::SnapshotWriter::record_auto_install`],
//! the doctor/TUI-visible record) and warned **once per server per daemon
//! lifetime** (`warn!` — a TUI health finding, never the `error!` desktop
//! interrupt: degraded enrichment is not urgent). The next session start's
//! detection retries naturally; a fresh daemon starts with a clean ledger.
//!
//! Every kick is announced: the router returns the [`announce_line`] to the
//! `SessionStart` CLI, which rides it on the host's user-visible
//! `systemMessage` surface, and the kick/landing/failure each emit a firehose
//! event plus the snapshot record — so the floor (an `info!` event + a
//! doctor/TUI-visible record) holds even for hosts with no `systemMessage`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tracing::{info, warn};

use crate::config::{Config, LanguageConfig};
use crate::install::{self, BlessedRecipe, CommandRunner, InstallPlan, TarballFetcher};
use crate::managed_home::ManagedHome;
use crate::recipes::{BlessedManifest, InstallClass, InstallRecipe, current_platform_token};
use crate::source::Source;
use crate::state_snapshot::SnapshotWriter;

/// Maximum background installs running concurrently, daemon-wide.
///
/// Concurrent *different* servers are fine, but a polyglot first session
/// should not hammer the network/toolchain with one install per language —
/// the rest queue behind the semaphore and run as permits free up.
const MAX_CONCURRENT_INSTALLS: usize = 2;

/// One detected missing blessed server: the name, the blessed pin the install
/// will land, and the install class for honest announcement wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingServer {
    /// Canonical server name (the `[lsp.server.*]` key — also the executable
    /// name, misc 162).
    pub server: String,
    /// The blessed pinned version the managed install lands at.
    pub version: String,
    /// Fetch-class (seconds) or compile-class (minutes) — drives the
    /// "this one takes minutes" announcement wording, nothing gates on it.
    pub class: InstallClass,
}

/// What the daemon's ONE shared installer is doing about a server right now —
/// the read-only state a diagnostics receipt renders in-band (bug 148 receipt
/// amendment).
///
/// Derived at read time from the installer's own dedupe and failure state; there
/// is no second store and nothing polls. A server with neither a running install
/// nor a recorded failure yields `None` — so a server whose install **lands**
/// mid-session stops producing any line at all: it just serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoInstallProgress {
    /// A background install of this server is running right now — kicked by the
    /// eager session-start leg or the JIT spawn-failure leg, possibly still
    /// queued behind [`MAX_CONCURRENT_INSTALLS`].
    InFlight,
    /// An install of this server already failed this daemon lifetime. Nothing
    /// retries on its own until the next session start re-detects it.
    Failed,
}

/// The one-line user-visible announcement for a kicked auto-install.
///
/// Ridden on the `SessionStart` `systemMessage` surface by the CLI; the
/// compile-class variant states the "takes minutes" expectation honestly
/// (lsm 04's machine-readable [`InstallClass`]).
#[must_use]
pub fn announce_line(missing: &MissingServer) -> String {
    match missing.class {
        InstallClass::Fetch => format!(
            "Catenary: auto-installing {} {} in the background; coverage arrives when it lands.",
            missing.server, missing.version,
        ),
        InstallClass::Compile => format!(
            "Catenary: auto-installing {} {} in the background (compiles from source — this can \
             take minutes); coverage arrives when it lands.",
            missing.server, missing.version,
        ),
    }
}

/// The install a server's blessing data actually supports, or `None` when
/// nothing is constructible — the shared eligibility gate (bug 148).
///
/// Both legs ask exactly this before kicking: the blessed-manifest row is
/// pinned ([`BlessedManifest::pinned_version`] — rust-analyzer reports no pin
/// and so never qualifies, by design), a shipped recipe carries **that exact**
/// pinned version (a recipe/manifest skew could install a version the pin would
/// never resolve — refuse), and the structural blessing gate resolves
/// ([`BlessedRecipe::resolve`], never bypassed). One definition, so "the
/// finding says auto-install will act" and "auto-install acts" cannot disagree.
#[must_use]
fn blessed_target(
    server: &str,
    manifest: &BlessedManifest,
    recipes: &BTreeMap<String, InstallRecipe>,
) -> Option<MissingServer> {
    let version = manifest.pinned_version(server)?;
    let recipe = recipes.get(server)?;
    if recipe.version != version {
        return None;
    }
    BlessedRecipe::resolve(server, recipe, manifest).map(|_blessed| MissingServer {
        server: server.to_owned(),
        version: version.to_owned(),
        class: recipe.install_class_on(current_platform_token()),
    })
}

/// The names of `root`'s own directory entries that are not directories — the
/// root's **surface** (bug 148's eager leg).
///
/// One `read_dir` at depth 0: no recursion, no walk, no `.gitignore` pass. An
/// unreadable root reads as an empty surface (detection nominates nothing
/// rather than guessing).
fn root_surface(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_dir()))
        .map(|entry| PathBuf::from(entry.file_name()))
        .collect()
}

/// Whether a **markerless** language is wanted at a root whose `surface` is
/// already read: a `filenames` (exact) or `extensions` hit at depth 0.
///
/// Mirrors the classification predicate the filesystem manager applies to a
/// path (filename first, then extension without the dot), asked per language
/// rather than through the winner-takes-the-extension global table — the
/// question here is "does THIS language claim a file at the surface", which is
/// what binds the root to the language's servers. A language with no
/// classification fields at all claims nothing.
fn surface_wants(surface: &[PathBuf], lang_config: &LanguageConfig) -> bool {
    let filenames = lang_config.filenames.as_deref().unwrap_or_default();
    let extensions = lang_config.extensions.as_deref().unwrap_or_default();
    if filenames.is_empty() && extensions.is_empty() {
        return false;
    }
    surface.iter().any(|name| {
        name.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| filenames.iter().any(|f| f == n))
            || name
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| extensions.iter().any(|e| e == ext))
    })
}

/// Detect the blessed servers `roots` need but cannot spawn (lsm 05).
///
/// For each root, a configured language nominates its bound servers when that
/// root **wants** it:
///
/// - a language with `root_markers` is wanted on a marker hit at the root (the
///   same [`crate::lsp::project_config::dir_has_marker`] predicate that binds
///   roots to languages everywhere else);
/// - a **markerless** language — the file-presence class: toml / json / yaml /
///   html / css / bash / markdown, which declare only `extensions` /
///   `filenames` — is wanted when a matching file sits at the root's own
///   surface ([`surface_wants`] over one depth-0 [`root_surface`] read). This
///   is bug 148's eager leg, and the ruling's own definition of eager: the file
///   is literally right there at the surface. Nothing recurses — a match only
///   below depth 0 does not nominate here; the demand-driven (JIT) leg at the
///   spawn-failure site covers that case when the file is actually opened.
///
/// A nominated server is **missing** — and only then eligible — when all of:
///
/// - it has no `[lsp.server.*]` `path` override (an explicitly configured
///   executable is the user's own resolution; auto-install never second-guesses
///   it);
/// - [`blessed_target`] resolves for it (pinned row, version-matched recipe,
///   blessing gate), so an install is actually constructible;
/// - the managed home has no install at the pin
///   ([`ManagedHome::pinned_executable`] — the lsm-02 resolution leg), and
/// - nothing resolves on `PATH`
///   ([`crate::health::servers::server_binary_installed`] — a PATH-managed
///   server is NOT missing).
///
/// Returns nothing when `prefer_managed` is off: with the managed home opted
/// out of spawn resolution, a completed install could never be picked up, so
/// kicking one would be dead weight. Deduped across roots and languages;
/// deterministic order (languages sorted by key, roots in caller order).
#[must_use]
pub fn detect_missing(
    roots: &[PathBuf],
    config: &Config,
    manifest: &BlessedManifest,
    recipes: &BTreeMap<String, InstallRecipe>,
    home: &ManagedHome,
) -> Vec<MissingServer> {
    if !config.prefer_managed() {
        // The user manages their own servers on PATH — a managed install would
        // never be consulted at spawn, so there is nothing useful to install.
        return Vec::new();
    }

    let mut languages: Vec<(&String, &LanguageConfig)> = config.language.iter().collect();
    languages.sort_by(|a, b| a.0.cmp(b.0));

    let mut seen: HashSet<&str> = HashSet::new();
    let mut missing = Vec::new();
    for root in roots {
        // The root's depth-0 listing, read at most once per root and only when
        // a markerless language actually asks for it (bug 148) — a
        // marker-only workspace costs exactly what it did before.
        let mut surface: Option<Vec<PathBuf>> = None;
        for (_lang, lang_config) in &languages {
            let wanted = match lang_config.marker_set() {
                Some((markers, compiled)) => {
                    crate::lsp::project_config::dir_has_marker(root, markers, compiled)
                }
                None => surface_wants(
                    surface.get_or_insert_with(|| root_surface(root)),
                    lang_config,
                ),
            };
            if !wanted {
                continue;
            }
            for binding in lang_config.servers() {
                let name = binding.name.as_str();
                if seen.contains(name) {
                    continue;
                }
                let def = config.server.get(name).cloned().unwrap_or_default();
                if def.path.is_some() {
                    // An explicit executable override is its own resolution.
                    continue;
                }
                let Some(target) = blessed_target(name, manifest, recipes) else {
                    continue; // unpinned, recipeless, skewed, or unblessed
                };
                if home
                    .pinned_executable(name, &target.version, name)
                    .is_some()
                {
                    continue; // the managed install already exists
                }
                if crate::health::servers::server_binary_installed(name, def.program(name)) {
                    continue; // PATH-managed is NOT missing
                }
                seen.insert(&binding.name);
                missing.push(target);
            }
        }
    }
    missing
}

/// The daemon-side background install manager (lsm 05).
///
/// Cheaply cloneable (`Arc` inner); one per daemon, held on the hook-dispatch
/// context. Owns the dedupe state: one in-flight install per server name
/// (concurrent sessions wanting the same server collapse to one task), at most
/// [`MAX_CONCURRENT_INSTALLS`] running at once, and one failure `warn!` per
/// server per daemon lifetime.
#[derive(Clone)]
pub struct AutoInstaller {
    inner: Arc<Inner>,
}

/// Shared state behind [`AutoInstaller`].
struct Inner {
    /// The managed home installs land in — production uses
    /// [`ManagedHome::resolve`]; tests inject a tempdir.
    home: ManagedHome,
    /// Pinned install recipes keyed by canonical server name (the seed set,
    /// matching the guided-install surface).
    recipes: BTreeMap<String, InstallRecipe>,
    /// Test-injected manifest; `None` resolves the live
    /// [`crate::recipes::active_manifest`] at each use so a registry re-pin is
    /// honored without a daemon bounce.
    manifest_override: Option<Arc<BlessedManifest>>,
    /// Install-command runner (production: [`install::ProcessRunner`]).
    runner: Box<dyn CommandRunner + Send + Sync>,
    /// Artifact fetcher (production: [`install::UreqFetcher`]).
    fetcher: Box<dyn TarballFetcher + Send + Sync>,
    /// Daemon snapshot writer for the doctor/TUI-visible install records;
    /// `None` outside daemon mode (transport-only tests).
    snapshot: Option<Arc<SnapshotWriter>>,
    /// Servers with an install task currently in flight.
    in_flight: Mutex<HashSet<String>>,
    /// Servers whose failure has already fired its one `warn!` this daemon
    /// lifetime — the dedupe the ticket rules (a failing server must not warn
    /// on every session start). Written only on the failure arm and never
    /// cleared, so it doubles as **the** failure ledger
    /// ([`AutoInstaller::progress`] reads it for the receipt's honest
    /// install-failed line — one ledger, no second store).
    warned: Mutex<HashSet<String>>,
    /// Concurrency cap across different servers.
    limiter: Arc<tokio::sync::Semaphore>,
}

/// RAII seat for an in-flight install: dropping it — on completion, a panic
/// inside the task, or a cancelled future — releases the server's dedupe slot,
/// so an unstable install can never wedge the key (the same failure doctrine
/// as the diag-round guard).
struct InFlightSeat {
    inner: Arc<Inner>,
    server: String,
}

impl Drop for InFlightSeat {
    fn drop(&mut self) {
        self.inner
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.server);
    }
}

impl AutoInstaller {
    /// The production installer: seed recipes, the live active manifest, the
    /// real managed home, and the real process/network seams.
    #[must_use]
    pub fn new(snapshot: Option<Arc<SnapshotWriter>>) -> Self {
        Self::with_parts(
            ManagedHome::resolve(),
            crate::recipes::default_recipes().unwrap_or_default(),
            None,
            Box::new(install::ProcessRunner),
            Box::new(install::UreqFetcher),
            snapshot,
        )
    }

    /// An installer with every seam injected — the test constructor (a stub
    /// runner/fetcher through the real background-task path).
    #[must_use]
    pub fn with_parts(
        home: ManagedHome,
        recipes: BTreeMap<String, InstallRecipe>,
        manifest_override: Option<Arc<BlessedManifest>>,
        runner: Box<dyn CommandRunner + Send + Sync>,
        fetcher: Box<dyn TarballFetcher + Send + Sync>,
        snapshot: Option<Arc<SnapshotWriter>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                home,
                recipes,
                manifest_override,
                runner,
                fetcher,
                snapshot,
                in_flight: Mutex::new(HashSet::new()),
                warned: Mutex::new(HashSet::new()),
                limiter: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_INSTALLS)),
            }),
        }
    }

    /// Detect the missing blessed servers for `roots` under this installer's
    /// recipes/manifest/home — [`detect_missing`] with the injected seams.
    #[must_use]
    pub fn detect(&self, roots: &[PathBuf], config: &Config) -> Vec<MissingServer> {
        detect_missing(
            roots,
            config,
            &self.manifest(),
            &self.inner.recipes,
            &self.inner.home,
        )
    }

    /// The install target for `server` under this installer's manifest and
    /// recipes — [`blessed_target`] with the injected seams, `None` when
    /// nothing is constructible (unpinned, recipeless, version-skewed, or
    /// unblessed).
    ///
    /// The demand-driven (JIT) leg's eligibility question (bug 148): the spawn
    /// path found the server missing and asks this before kicking, so it clears
    /// exactly the gates [`detect_missing`] applies at session start.
    #[must_use]
    pub fn install_target(&self, server: &str) -> Option<MissingServer> {
        blessed_target(server, &self.manifest(), &self.inner.recipes)
    }

    /// Kick a background install for `missing`, returning whether a task was
    /// actually spawned (`false` when the same server is already in flight —
    /// the caller then announces nothing, so a duplicate session start never
    /// double-announces).
    ///
    /// The dispatch is a spawn, never an await (the session-start latency
    /// pin): all install work — plan resolution included — runs on the spawned
    /// task, and the blocking engine runs under `spawn_blocking`. On success
    /// `on_installed` fires (the router's pre-warm); on failure the snapshot
    /// record and the once-per-lifetime `warn!` land and nothing retries until
    /// a later session start re-detects.
    pub fn kick(
        &self,
        missing: &MissingServer,
        on_installed: impl FnOnce() + Send + 'static,
    ) -> bool {
        {
            let mut in_flight = self
                .inner
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !in_flight.insert(missing.server.clone()) {
                return false;
            }
        }
        let seat = InFlightSeat {
            inner: self.inner.clone(),
            server: missing.server.clone(),
        };

        info!(
            source = Source::LspLifecycle.as_str(),
            server = %missing.server,
            version = %missing.version,
            class = missing.class.as_str(),
            "auto-install kicked: {} {} ({}-class)",
            missing.server,
            missing.version,
            missing.class.as_str(),
        );
        if let Some(snapshot) = &self.inner.snapshot {
            snapshot.record_auto_install(&missing.server, &missing.version, "installing", None);
        }

        let inner = self.inner.clone();
        let missing = missing.clone();
        tokio::spawn(async move {
            // The seat lives for the whole task — any exit path releases it.
            let _seat = seat;
            // Cap concurrency across different servers; queued kicks wait here,
            // in the background, never in a hook dispatch. `acquire_owned` only
            // errs on a closed semaphore, which this one never is.
            let Ok(_permit) = inner.limiter.clone().acquire_owned().await else {
                return;
            };
            let run_inner = inner.clone();
            let run_missing = missing.clone();
            let joined =
                tokio::task::spawn_blocking(move || run_install(&run_inner, &run_missing)).await;
            let result = match joined {
                Ok(result) => result,
                Err(e) => Err(format!("install task panicked: {e}")),
            };
            report_outcome(&inner, &missing, result, on_installed);
        });
        true
    }

    /// What this installer is doing about `server` right now — the receipt's
    /// read-only window onto the shared state (bug 148 receipt amendment).
    ///
    /// Two brief lock peeks, no I/O, no polling, nothing mutated: the answer is
    /// whatever the in-flight set and the failure ledger say at the moment of
    /// the call. In-flight **wins** over a recorded failure — a re-kicked server
    /// IS installing now, whatever an earlier attempt did, and the receipt
    /// speaks in the present tense.
    #[must_use]
    pub fn progress(&self, server: &str) -> Option<AutoInstallProgress> {
        if self
            .inner
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(server)
        {
            return Some(AutoInstallProgress::InFlight);
        }
        if self
            .inner
            .warned
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(server)
        {
            return Some(AutoInstallProgress::Failed);
        }
        None
    }

    /// The manifest detection and the blessing gate read: the injected
    /// override (tests) or the live process-wide active manifest.
    fn manifest(&self) -> Arc<BlessedManifest> {
        self.inner
            .manifest_override
            .clone()
            .unwrap_or_else(crate::recipes::active_manifest)
    }
}

/// Report a finished install task: announce the landing — naming any lsm-06
/// collected stale versions on the firehose event and the snapshot record —
/// or the failure, then fire the pre-warm callback on success.
fn report_outcome(
    inner: &Inner,
    missing: &MissingServer,
    result: Result<Vec<String>, String>,
    on_installed: impl FnOnce(),
) {
    match result {
        Ok(collected) => {
            info!(
                source = Source::LspLifecycle.as_str(),
                server = %missing.server,
                version = %missing.version,
                "auto-install landed: {} {} in the managed home",
                missing.server,
                missing.version,
            );
            // The lsm-06 renewal announcement: each collected stale version is
            // named — on the firehose event and on the snapshot record's
            // detail — never a silent sweep.
            let detail = if collected.is_empty() {
                None
            } else {
                let stale = collected.join(", ");
                info!(
                    source = Source::LspLifecycle.as_str(),
                    server = %missing.server,
                    version = %missing.version,
                    "warranty renewal collected stale managed versions of {}: {stale}",
                    missing.server,
                );
                Some(format!("collected stale versions: {stale}"))
            };
            if let Some(snapshot) = &inner.snapshot {
                snapshot.record_auto_install(
                    &missing.server,
                    &missing.version,
                    "installed",
                    detail.as_deref(),
                );
            }
            on_installed();
        }
        Err(reason) => {
            if let Some(snapshot) = &inner.snapshot {
                snapshot.record_auto_install(
                    &missing.server,
                    &missing.version,
                    "failed",
                    Some(&reason),
                );
            }
            // One warn! per server per daemon lifetime (TUI awareness, no
            // desktop interrupt); repeats stay firehose-only.
            let first = inner
                .warned
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(missing.server.clone());
            if first {
                warn!(
                    source = Source::LspLifecycle.as_str(),
                    server = %missing.server,
                    version = %missing.version,
                    "auto-install of {} {} failed: {reason} — see `catenary doctor`; \
                     the next session start retries",
                    missing.server,
                    missing.version,
                );
            } else {
                info!(
                    source = Source::LspLifecycle.as_str(),
                    server = %missing.server,
                    version = %missing.version,
                    "auto-install of {} {} failed again (already warned): {reason}",
                    missing.server,
                    missing.version,
                );
            }
        }
    }
}

/// Run one install to completion through the real engine (blocking; called
/// under `spawn_blocking`). `Ok` carries the stale versions the
/// warranty-renewal GC collected (lsm 06 — often empty) for the landing
/// announcement; `Err` carries the honest one-line reason for the snapshot
/// record and the warn.
fn run_install(inner: &Inner, missing: &MissingServer) -> Result<Vec<String>, String> {
    let manifest = inner
        .manifest_override
        .clone()
        .unwrap_or_else(crate::recipes::active_manifest);
    let recipe = inner
        .recipes
        .get(&missing.server)
        .ok_or_else(|| format!("no install recipe for `{}`", missing.server))?;
    let blessed = BlessedRecipe::resolve(&missing.server, recipe, &manifest).ok_or_else(|| {
        format!(
            "`{}` failed the blessing gate (recipe pins {}, manifest disagrees)",
            missing.server, recipe.version,
        )
    })?;
    let plan = InstallPlan::resolve(&blessed, &inner.home).map_err(|e| format!("{e:#}"))?;
    let outcome = install::execute(&plan, inner.runner.as_ref(), inner.fetcher.as_ref());
    if !outcome.success {
        return Err(outcome
            .log
            .last()
            .cloned()
            .unwrap_or_else(|| "install failed".to_owned()));
    }
    // The engine reported success — confirm the ruled invariant actually
    // landed (`<home>/<server>/<version>/bin/<server>` exists and is
    // executable), so a "successful" install the spawn path cannot resolve is
    // an honest failure, never a silent no-op pre-warm.
    if inner
        .home
        .pinned_executable(&missing.server, &missing.version, &missing.server)
        .is_none()
    {
        return Err(format!(
            "install completed but {}/{}/bin/{} is not an executable — the artifact does not \
             expose the server key",
            missing.server, missing.version, missing.server,
        ));
    }
    // Warranty-renewal GC (lsm 06): this successful, invariant-checked install
    // at the current pin IS the renewal's enactment on this machine — collect
    // version dirs beyond the kept pair. A GC hiccup never fails the install;
    // the next renewal retries the sweep naturally.
    let collected = inner
        .home
        .collect_stale_versions(&missing.server, &missing.version)
        .unwrap_or_else(|e| {
            info!(
                source = Source::LspLifecycle.as_str(),
                server = %missing.server,
                version = %missing.version,
                "managed-home GC after {} {} did not complete: {e:#}",
                missing.server,
                missing.version,
            );
            Vec::new()
        });
    Ok(collected)
}

/// Unit-test scaffolding for the auto-install seams, shared across modules.
///
/// Lives outside `mod tests` so the manager's demand-driven (JIT) leg tests
/// (bug 148) drive the *same* stub installer machinery — the synthetic blessed
/// manifest and recipe, the staging and failing runners — rather than inventing
/// a second stub set that could drift from this one.
#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test scaffolding uses expect for readable assertions"
)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::install::{CommandOutcome, CommandRunner, InstallCommand, TarballFetcher};
    use crate::managed_home::ManagedHome;
    use crate::recipes::{
        BlessedEntry, BlessedManifest, ConformanceProvision, Ecosystem, InstallRecipe,
        VerificationTier,
    };

    use super::{AutoInstaller, BTreeMap};

    /// The synthetic server name every stub-seamed test installs.
    pub const SERVER: &str = "lsm05-test-ls";
    /// The version the synthetic manifest pins and the recipe carries.
    pub const VERSION: &str = "1.2.3";

    /// A manifest with one blessed row for [`SERVER`] pinning [`VERSION`] under
    /// a synthetic platform token, so the preferred-row lookup falls back to it
    /// deterministically on every host.
    pub fn manifest() -> BlessedManifest {
        let mut rows = std::collections::BTreeMap::new();
        rows.insert(
            "synthetic".to_string(),
            BlessedEntry {
                version: VERSION.to_string(),
                platform: "synthetic".to_string(),
                date: "2026-07-14".to_string(),
                tier: None,
            },
        );
        let mut blessed = std::collections::BTreeMap::new();
        blessed.insert(SERVER.to_string(), rows);
        BlessedManifest {
            blessed,
            ..BlessedManifest::default()
        }
    }

    /// A cargo-class recipe for [`SERVER`] at [`VERSION`] (compile-class, no
    /// hash needed — cargo verifies via `--locked`).
    pub fn recipe() -> InstallRecipe {
        InstallRecipe {
            ecosystem: Ecosystem::Cargo,
            package: SERVER.to_string(),
            version: VERSION.to_string(),
            tier: VerificationTier::CargoLocked,
            draft: false,
            hash: None,
            note: None,
            conformance: true,
            provision: ConformanceProvision::Bash,
            co_install: Vec::new(),
            artifact: std::collections::BTreeMap::new(),
            runtime: None,
        }
    }

    /// The seed recipe map: [`SERVER`] at [`VERSION`], nothing else.
    pub fn recipes() -> BTreeMap<String, InstallRecipe> {
        let mut map = BTreeMap::new();
        map.insert(SERVER.to_string(), recipe());
        map
    }

    /// Stage an executable at `<home>/<SERVER>/<VERSION>/bin/<SERVER>`.
    pub fn stage_managed(home: &ManagedHome) {
        let bin = home.bin_dir(SERVER, VERSION).expect("bin dir");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let exe = bin.join(SERVER);
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    /// A runner that reports success and stages the managed executable —
    /// standing in for the real `cargo install --root <version-dir>` leg.
    pub struct StagingRunner {
        /// The managed home root the staged executable lands under.
        pub home_root: PathBuf,
        /// Blocks the install until released, so a test can assert `kick`
        /// returned while the work is still pending (the latency pin).
        pub gate: Option<Arc<tokio::sync::Semaphore>>,
        /// Counts the installs that actually ran.
        pub runs: Arc<AtomicUsize>,
    }

    impl CommandRunner for StagingRunner {
        fn run(&self, _command: &InstallCommand) -> anyhow::Result<CommandOutcome> {
            if let Some(gate) = &self.gate {
                // Blocking acquire on the blocking-task thread; `forget`
                // consumes the permit so the gate stays spent.
                loop {
                    if let Ok(permit) = gate.try_acquire() {
                        permit.forget();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            self.runs.fetch_add(1, Ordering::SeqCst);
            stage_managed(&ManagedHome::at(self.home_root.clone()));
            Ok(CommandOutcome {
                success: true,
                code: Some(0),
                output: String::new(),
            })
        }
    }

    /// A runner that fails every install — the simulated registry failure.
    pub struct FailingRunner {
        /// Counts the installs that actually ran (and failed).
        pub runs: Arc<AtomicUsize>,
    }

    impl CommandRunner for FailingRunner {
        fn run(&self, _command: &InstallCommand) -> anyhow::Result<CommandOutcome> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(CommandOutcome {
                success: false,
                code: Some(1),
                output: "registry unreachable".to_string(),
            })
        }
    }

    /// A fetcher no test path reaches (cargo-class plans never fetch).
    pub struct NoFetch;
    impl TarballFetcher for NoFetch {
        fn fetch(&self, url: &str) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("unexpected fetch of {url}")
        }
    }

    /// An installer over `home_root` with the synthetic manifest/recipes and
    /// the given runner — the stub-seamed constructor every test starts from.
    pub fn installer_with_runner(
        home_root: &std::path::Path,
        runner: Box<dyn CommandRunner + Send + Sync>,
    ) -> AutoInstaller {
        AutoInstaller::with_parts(
            ManagedHome::at(home_root.to_path_buf()),
            recipes(),
            Some(Arc::new(manifest())),
            runner,
            Box::new(NoFetch),
            None,
        )
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::config::{LanguageConfig, ServerBinding, ServerDef};

    use super::test_support::{
        FailingRunner, NoFetch, SERVER, StagingRunner, VERSION, installer_with_runner, manifest,
        recipes, stage_managed,
    };
    use super::*;

    const MARKER: &str = "lsm05.marker";
    /// The surface filename a markerless language claims exactly (the
    /// `Cargo.toml`-shaped case, bug 148).
    const SURFACE_FILE: &str = "Surface.tick";
    /// The surface extension a markerless language claims (without the dot).
    const SURFACE_EXT: &str = "tick";

    /// A config with one language whose marker is [`MARKER`], bound to
    /// [`SERVER`].
    fn config() -> Config {
        let mut config = Config::default();
        let mut lang = LanguageConfig {
            root_markers: Some(vec![MARKER.to_string()]),
            servers: Some(vec![ServerBinding::new(SERVER)]),
            ..LanguageConfig::default()
        };
        lang.compile_markers().expect("plain marker compiles");
        config.language.insert("lsm05-lang".to_string(), lang);
        config
    }

    /// A root dir carrying the language marker.
    fn marked_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(MARKER), b"").expect("write marker");
        root
    }

    /// A config with one **markerless** (file-presence) language bound to
    /// [`SERVER`] — the toml/json/yaml-shaped class bug 148 was blind to.
    /// `filenames` carries the exact-name leg, `extensions` the suffix leg;
    /// `root_markers` is absent, so `marker_set()` is `None`.
    fn markerless_config(filenames: bool, extensions: bool) -> Config {
        let mut config = Config::default();
        let lang = LanguageConfig {
            filenames: filenames.then(|| vec![SURFACE_FILE.to_string()]),
            extensions: extensions.then(|| vec![SURFACE_EXT.to_string()]),
            servers: Some(vec![ServerBinding::new(SERVER)]),
            ..LanguageConfig::default()
        };
        config.language.insert("lsm05-markerless".to_string(), lang);
        config
    }

    /// An empty root plus a fresh managed home — the standing detection fixture.
    fn empty_root_and_home() -> (tempfile::TempDir, tempfile::TempDir, ManagedHome) {
        let root = tempfile::tempdir().expect("tempdir");
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));
        (root, home_dir, home)
    }

    // ── detection ─────────────────────────────────────────────────────

    #[test]
    fn detects_a_marked_root_wanting_an_unresolvable_blessed_server() {
        let root = marked_root();
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &config(),
            &manifest(),
            &recipes(),
            &home,
        );
        assert_eq!(missing.len(), 1, "one missing server: {missing:?}");
        assert_eq!(missing[0].server, SERVER);
        assert_eq!(missing[0].version, VERSION);
        assert_eq!(
            missing[0].class,
            InstallClass::Compile,
            "a cargo recipe is compile-class"
        );
    }

    #[test]
    fn managed_install_at_the_pin_is_not_missing() {
        let root = marked_root();
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));
        stage_managed(&home);

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &config(),
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(missing.is_empty(), "resolvable at the pin: {missing:?}");
    }

    #[test]
    fn path_resolvable_server_is_not_missing() {
        // A `path` override that exists stands in for "resolvable on PATH"
        // without touching the test host's real PATH — and pins the explicit
        // override skip at the same time (an explicitly configured executable
        // is never auto-installed over).
        let root = marked_root();
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));

        let exe_dir = tempfile::tempdir().expect("tempdir");
        let exe = exe_dir.path().join(SERVER);
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let mut config = config();
        config.server.insert(
            SERVER.to_string(),
            ServerDef {
                path: Some(exe.display().to_string()),
                ..ServerDef::default()
            },
        );

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &config,
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(missing.is_empty(), "an overridden server never kicks");
    }

    #[test]
    fn unmarked_root_detects_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &config(),
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(missing.is_empty(), "no marker, no nomination");
    }

    // ── the eager depth-0 leg for markerless languages (bug 148) ──────

    #[test]
    fn markerless_language_with_a_surface_filename_detects_missing() {
        // The tombi case: a file-presence language (no `root_markers`) whose
        // exact filename sits at the root's surface — Cargo.toml-shaped. Before
        // bug 148 the language loop skipped it outright, so no server bound to
        // it could ever be detected as missing, under any flag.
        let (root, _home_dir, home) = empty_root_and_home();
        std::fs::write(root.path().join(SURFACE_FILE), b"").expect("write surface file");

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &markerless_config(true, false),
            &manifest(),
            &recipes(),
            &home,
        );
        assert_eq!(missing.len(), 1, "the surface hit nominates: {missing:?}");
        assert_eq!(missing[0].server, SERVER);
        assert_eq!(missing[0].version, VERSION);
    }

    #[test]
    fn markerless_language_extension_hit_at_depth_zero_detects_missing() {
        // The extension leg of the same surface read: `notes.tick` at depth 0.
        let (root, _home_dir, home) = empty_root_and_home();
        std::fs::write(root.path().join("notes.tick"), b"").expect("write surface file");

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &markerless_config(false, true),
            &manifest(),
            &recipes(),
            &home,
        );
        assert_eq!(missing.len(), 1, "the extension hit nominates: {missing:?}");
        assert_eq!(missing[0].server, SERVER);
    }

    #[test]
    fn markerless_language_without_a_surface_hit_detects_nothing() {
        // No matching file at the surface: the root does not want the language,
        // so nothing is nominated (a non-matching file present is still a miss).
        let (root, _home_dir, home) = empty_root_and_home();
        std::fs::write(root.path().join("unrelated.txt"), b"").expect("write");

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &markerless_config(true, true),
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(missing.is_empty(), "no surface hit, no kick: {missing:?}");
    }

    #[test]
    fn surface_detection_never_recurses_below_depth_zero() {
        // The ruling's eager definition is depth-0 visibility: ONE root-level
        // directory read. A match nested under the root must not nominate here
        // — that demand is the JIT leg's, at the spawn-failure site.
        let (root, _home_dir, home) = empty_root_and_home();
        let nested = root.path().join("sub");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(nested.join(SURFACE_FILE), b"").expect("write nested filename hit");
        std::fs::write(nested.join("notes.tick"), b"").expect("write nested extension hit");

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &markerless_config(true, true),
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(
            missing.is_empty(),
            "a hit below the surface must not nominate: {missing:?}"
        );
    }

    #[test]
    fn a_directory_named_like_a_surface_hit_does_not_nominate() {
        // The surface is files, not entries: a directory called `notes.tick`
        // (or `Surface.tick`) is not a file of the language.
        let (root, _home_dir, home) = empty_root_and_home();
        std::fs::create_dir_all(root.path().join(SURFACE_FILE)).expect("mkdir");
        std::fs::create_dir_all(root.path().join("notes.tick")).expect("mkdir");

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &markerless_config(true, true),
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(
            missing.is_empty(),
            "a directory is not a surface file: {missing:?}"
        );
    }

    #[test]
    fn a_classificationless_markerless_language_nominates_nothing() {
        // No markers AND no `extensions`/`filenames`: nothing can make a root
        // want it, so the surface read can never nominate.
        let (root, _home_dir, home) = empty_root_and_home();
        std::fs::write(root.path().join(SURFACE_FILE), b"").expect("write");

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &markerless_config(false, false),
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(missing.is_empty(), "nothing claims the file: {missing:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_markerless_surface_hit_kicks_the_install_through_the_shared_machinery() {
        // End-to-end for the eager leg: detection through `AutoInstaller` (its
        // own injected manifest/recipes/home), then the ordinary kick — every
        // gate downstream of the want-decision is shared, unchanged.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join(SURFACE_FILE), b"").expect("write surface file");
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let runs = Arc::new(AtomicUsize::new(0));
        let installer = installer_with_runner(
            &home_root,
            Box::new(StagingRunner {
                home_root: home_root.clone(),
                gate: None,
                runs: runs.clone(),
            }),
        );

        let config = markerless_config(true, false);
        let missing = installer.detect(&[root.path().to_path_buf()], &config);
        assert_eq!(missing.len(), 1, "the surface hit detects: {missing:?}");
        let landed = Arc::new(AtomicBool::new(false));
        let flag = landed.clone();
        assert!(
            installer.kick(&missing[0], move || flag.store(true, Ordering::SeqCst)),
            "the markerless nomination kicks like any other"
        );
        for _ in 0..200 {
            if landed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(landed.load(Ordering::SeqCst), "the install landed");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "exactly one install ran");
        assert!(
            installer
                .detect(&[root.path().to_path_buf()], &config)
                .is_empty(),
            "the landed install is no longer missing",
        );
    }

    #[test]
    fn unblessed_or_recipeless_server_never_qualifies() {
        let root = marked_root();
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));

        // No manifest row: unpinned — nothing to install toward.
        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &config(),
            &BlessedManifest::default(),
            &recipes(),
            &home,
        );
        assert!(missing.is_empty(), "unpinned server never qualifies");

        // Pinned but no recipe: nothing constructible.
        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &config(),
            &manifest(),
            &BTreeMap::new(),
            &home,
        );
        assert!(missing.is_empty(), "recipeless server never qualifies");
    }

    #[test]
    fn prefer_managed_off_detects_nothing() {
        // With the managed home opted out of spawn resolution, an install
        // could never be picked up — detection refuses to kick dead weight.
        let root = marked_root();
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));

        let mut config = config();
        config.servers = Some(crate::config::ServersConfig {
            prefer_managed: false,
            auto_install: true,
        });

        let missing = detect_missing(
            &[root.path().to_path_buf()],
            &config,
            &manifest(),
            &recipes(),
            &home,
        );
        assert!(missing.is_empty(), "prefer_managed = false detects nothing");
    }

    #[test]
    fn duplicate_roots_dedupe_to_one_missing_entry() {
        let root = marked_root();
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = ManagedHome::at(home_dir.path().join("servers"));

        let missing = detect_missing(
            &[root.path().to_path_buf(), root.path().to_path_buf()],
            &config(),
            &manifest(),
            &recipes(),
            &home,
        );
        assert_eq!(missing.len(), 1, "deduped across roots: {missing:?}");
    }

    // ── the background task path ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kick_returns_before_the_install_runs_and_completion_fires_prewarm() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let installer = installer_with_runner(
            &home_root,
            Box::new(StagingRunner {
                home_root: home_root.clone(),
                gate: Some(gate.clone()),
                runs: runs.clone(),
            }),
        );

        let missing = MissingServer {
            server: SERVER.to_string(),
            version: VERSION.to_string(),
            class: InstallClass::Compile,
        };
        let prewarmed = Arc::new(AtomicBool::new(false));
        let flag = prewarmed.clone();

        // The latency pin: kick returns while the runner is still gated —
        // the dispatch is a spawn, never an await.
        assert!(installer.kick(&missing, move || {
            flag.store(true, Ordering::SeqCst);
        }));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "no install ran yet");
        assert!(!prewarmed.load(Ordering::SeqCst));

        // A duplicate kick while in flight is deduped (no double announce).
        assert!(
            !installer.kick(&missing, || {}),
            "same server in flight dedupes"
        );

        // Release the gate; the install lands and the pre-warm fires.
        gate.add_permits(1);
        for _ in 0..200 {
            if prewarmed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            prewarmed.load(Ordering::SeqCst),
            "completion fires pre-warm"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "exactly one install ran");
        assert!(
            ManagedHome::at(home_root.clone())
                .pinned_executable(SERVER, VERSION, SERVER)
                .is_some(),
            "the install landed in the managed home at the pin",
        );

        // With the install landed, a re-kick is possible again (the in-flight
        // seat was released) — and detection would no longer nominate it.
        let root = marked_root();
        let missing_now = installer.detect(&[root.path().to_path_buf()], &config());
        assert!(
            missing_now.is_empty(),
            "landed install is no longer missing"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failure_records_no_prewarm_and_warns_once_per_lifetime() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let runs = Arc::new(AtomicUsize::new(0));
        let snapshot = SnapshotWriter::with_coalesce(
            &tokio::runtime::Handle::current(),
            home_dir.path(),
            crate::state_snapshot::DaemonInfo::current(
                "daemon:test".to_string(),
                1,
                crate::state_snapshot::now_iso(),
            ),
            std::time::Duration::from_millis(10),
        );
        let installer = AutoInstaller::with_parts(
            ManagedHome::at(home_root.clone()),
            recipes(),
            Some(Arc::new(manifest())),
            Box::new(FailingRunner { runs: runs.clone() }),
            Box::new(NoFetch),
            Some(snapshot.clone()),
        );

        let missing = MissingServer {
            server: SERVER.to_string(),
            version: VERSION.to_string(),
            class: InstallClass::Compile,
        };
        let prewarmed = Arc::new(AtomicBool::new(false));
        let flag = prewarmed.clone();
        assert!(installer.kick(&missing, move || {
            flag.store(true, Ordering::SeqCst);
        }));

        // Wait for the failure to land on the snapshot record.
        let mut recorded = None;
        for _ in 0..200 {
            snapshot.flush_now();
            let parsed = std::fs::read_to_string(snapshot.path())
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
            if let Some(entry) = parsed
                .as_ref()
                .and_then(|v| v.get("auto_installs"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .filter(|e| e.get("status").and_then(|s| s.as_str()) == Some("failed"))
            {
                recorded = Some(entry.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let entry = recorded.expect("the failed install is recorded on the snapshot");
        assert_eq!(entry.get("server").and_then(|v| v.as_str()), Some(SERVER));
        assert!(
            entry
                .get("detail")
                .and_then(|v| v.as_str())
                .is_some_and(|d| !d.is_empty()),
            "the record carries the failure reason: {entry}"
        );
        assert!(!prewarmed.load(Ordering::SeqCst), "no pre-warm on failure");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no retry loop");

        // The warn-dedupe ledger: the first failure consumed the server's one
        // warn; a re-kick (the natural next-session-start retry) must find it
        // already marked, so it can never warn twice this daemon lifetime.
        assert!(
            installer
                .inner
                .warned
                .lock()
                .expect("ledger lock")
                .contains(SERVER),
            "the first failure consumed the one warn"
        );

        // Retry is allowed (the seat was released) — kick succeeds again and
        // fails again without a second warn (ledger already holds the server).
        let flag2 = prewarmed.clone();
        assert!(
            installer.kick(&missing, move || {
                flag2.store(true, Ordering::SeqCst);
            }),
            "a later session start may retry"
        );
        for _ in 0..200 {
            if runs.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the retry ran once");
        assert!(!prewarmed.load(Ordering::SeqCst));
    }

    // ── the receipt's read-only progress window (bug 148 amendment) ───

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progress_reads_in_flight_then_falls_silent_once_the_install_lands() {
        // The receipt's state source: while the gated install runs, `progress`
        // reports it in the present tense; the moment it LANDS the server stops
        // producing any line at all — it just serves.
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let installer = installer_with_runner(
            &home_root,
            Box::new(StagingRunner {
                home_root: home_root.clone(),
                gate: Some(gate.clone()),
                runs: runs.clone(),
            }),
        );
        assert_eq!(
            installer.progress(SERVER),
            None,
            "an untouched server has nothing to report"
        );

        let missing = MissingServer {
            server: SERVER.to_string(),
            version: VERSION.to_string(),
            class: InstallClass::Compile,
        };
        let landed = Arc::new(AtomicBool::new(false));
        let flag = landed.clone();
        assert!(installer.kick(&missing, move || {
            flag.store(true, Ordering::SeqCst);
        }));
        assert_eq!(
            installer.progress(SERVER),
            Some(AutoInstallProgress::InFlight),
            "the kicked install reads as running while it is gated",
        );

        gate.add_permits(1);
        for _ in 0..200 {
            if landed.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(landed.load(Ordering::SeqCst), "the install landed");
        // The seat drops with the task; poll the release so the assert reads a
        // settled state rather than racing the completion callback.
        for _ in 0..200 {
            if installer.progress(SERVER).is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            installer.progress(SERVER),
            None,
            "a landed install reports nothing — the server simply serves",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn progress_reads_failed_off_the_one_failure_ledger() {
        // The failure arm reads the SAME one-failure-per-server-per-lifetime
        // ledger the warn dedupe uses — no second store.
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        let runs = Arc::new(AtomicUsize::new(0));
        let installer =
            installer_with_runner(&home_root, Box::new(FailingRunner { runs: runs.clone() }));

        let missing = MissingServer {
            server: SERVER.to_string(),
            version: VERSION.to_string(),
            class: InstallClass::Compile,
        };
        assert!(installer.kick(&missing, || {}));
        for _ in 0..200 {
            if installer.progress(SERVER) == Some(AutoInstallProgress::Failed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            installer.progress(SERVER),
            Some(AutoInstallProgress::Failed),
            "the failed install is reported honestly",
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no retry loop");
        assert_eq!(
            installer.progress("some-other-server"),
            None,
            "the ledger is per-server",
        );
    }

    // ── warranty-renewal GC on landing (lsm 06) ───────────────────────

    /// Create a bare version dir under the home (a stale previous install),
    /// optionally pinning its mtime for deterministic install-time ordering.
    fn stage_stale_version(home_root: &std::path::Path, version: &str, at_secs: Option<u64>) {
        let home = ManagedHome::at(home_root.to_path_buf());
        let dir = home.version_dir(SERVER, version).expect("derives");
        std::fs::create_dir_all(&dir).expect("mkdir");
        if let Some(secs) = at_secs {
            std::fs::File::open(&dir)
                .expect("open dir")
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .expect("set mtime");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_install_never_collects_stale_versions() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        stage_stale_version(&home_root, "0.8.0", None);
        stage_stale_version(&home_root, "0.9.0", None);
        let runs = Arc::new(AtomicUsize::new(0));
        let installer =
            installer_with_runner(&home_root, Box::new(FailingRunner { runs: runs.clone() }));

        let missing = MissingServer {
            server: SERVER.to_string(),
            version: VERSION.to_string(),
            class: InstallClass::Compile,
        };
        assert!(installer.kick(&missing, || {}));

        // The failure arm marks the warn ledger — the reliable completion
        // signal for the failed task.
        for _ in 0..200 {
            if installer
                .inner
                .warned
                .lock()
                .expect("ledger lock")
                .contains(SERVER)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the install ran and failed");
        let home = ManagedHome::at(home_root);
        for version in ["0.8.0", "0.9.0"] {
            assert!(
                home.version_dir(SERVER, version).expect("derives").is_dir(),
                "a failed install must not GC: {version} survives",
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn landed_install_collects_stale_versions_and_records_them() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home_root = home_dir.path().join("servers");
        // Two stale versions with explicit install times; the runner stages
        // VERSION at "now", so the ordering is oldest → 0.8.0.
        stage_stale_version(&home_root, "0.8.0", Some(100));
        stage_stale_version(&home_root, "0.9.0", Some(200));
        let runs = Arc::new(AtomicUsize::new(0));
        let snapshot = SnapshotWriter::with_coalesce(
            &tokio::runtime::Handle::current(),
            home_dir.path(),
            crate::state_snapshot::DaemonInfo::current(
                "daemon:test".to_string(),
                1,
                crate::state_snapshot::now_iso(),
            ),
            std::time::Duration::from_millis(10),
        );
        let installer = AutoInstaller::with_parts(
            ManagedHome::at(home_root.clone()),
            recipes(),
            Some(Arc::new(manifest())),
            Box::new(StagingRunner {
                home_root: home_root.clone(),
                gate: None,
                runs: runs.clone(),
            }),
            Box::new(NoFetch),
            Some(snapshot.clone()),
        );

        let missing = MissingServer {
            server: SERVER.to_string(),
            version: VERSION.to_string(),
            class: InstallClass::Compile,
        };
        assert!(installer.kick(&missing, || {}));

        // Wait for the landing to reach the snapshot record.
        let mut recorded = None;
        for _ in 0..200 {
            snapshot.flush_now();
            let parsed = std::fs::read_to_string(snapshot.path())
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
            if let Some(entry) = parsed
                .as_ref()
                .and_then(|v| v.get("auto_installs"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .filter(|e| e.get("status").and_then(|s| s.as_str()) == Some("installed"))
            {
                recorded = Some(entry.clone());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let entry = recorded.expect("the landed install is recorded on the snapshot");
        let detail = entry
            .get("detail")
            .and_then(|v| v.as_str())
            .expect("the record names the collected versions");
        assert!(
            detail.contains("0.8.0"),
            "the oldest stale version is named on the record: {detail}",
        );
        assert!(
            !detail.contains("0.9.0"),
            "the kept previous version is not named as collected: {detail}",
        );

        let home = ManagedHome::at(home_root);
        assert!(
            !home.version_dir(SERVER, "0.8.0").expect("derives").exists(),
            "the oldest stale version dir is gone",
        );
        assert!(
            home.version_dir(SERVER, "0.9.0").expect("derives").is_dir(),
            "the most recent other version is kept (the rollback target)",
        );
        assert!(
            home.pinned_executable(SERVER, VERSION, SERVER).is_some(),
            "the pin just installed is kept and resolvable",
        );
    }

    #[test]
    fn announce_lines_state_the_class_honestly() {
        let fetch = MissingServer {
            server: "srv".to_string(),
            version: "1.0.0".to_string(),
            class: InstallClass::Fetch,
        };
        let line = announce_line(&fetch);
        assert!(line.contains("auto-installing srv 1.0.0"), "{line}");
        assert!(!line.contains("minutes"), "fetch-class is quick: {line}");

        let compile = MissingServer {
            class: InstallClass::Compile,
            ..fetch
        };
        let line = announce_line(&compile);
        assert!(line.contains("take minutes"), "compile-class warns: {line}");
    }
}
