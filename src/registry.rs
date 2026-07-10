// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The external signed-registry loader (tui-rework 08).
//!
//! The install recipes and blessed-manifest ([`crate::recipes`]) move out of the
//! binary into a **published, signed artifact** (a single payload carrying both
//! the `[recipe.*]` and `[blessed.*]` tables). This module resolves that artifact
//! with a strict degrade-down chain and never falls through to unpinned
//! behaviour:
//!
//! 1. **fetched-and-verified registry** — the payload fetched from the configured
//!    URL, its detached ed25519 signature verified against the **in-binary trust
//!    root** *before* the bytes are ever parsed-for-use;
//! 2. **cached copy** — the last verified payload, persisted under
//!    [`crate::paths::cache_dir`] (the regenerable class), re-verified on read;
//! 3. **in-binary seed** — the shipped defaults ([`crate::recipes::DEFAULT_RECIPES`]
//!    / [`crate::recipes::DEFAULT_BLESSED_MANIFEST`]), always available offline.
//!
//! A fetch failure or a bad signature is a **loud finding** (the health model's
//! [`FindingCode::RegistryStale`] / [`FindingCode::RegistryBadSignature`]) and
//! degrades one rung down the chain — it is never a hard failure and never
//! reverts a user to unpinned installs. The registry leaves the binary; the key
//! that vouches for it does not.
//!
//! ## Trust model, stated honestly
//!
//! A signature proves **immutability** (the tested bytes are the installed bytes;
//! no substitution, no MITM), never **benignity** — benignity comes from the
//! registry repo's mechanical gates (cooling-off, OSV advisories, provenance,
//! anomaly bounds; see `registry/` in this repo). Signature verification is
//! **never hand-rolled** (a hard rule of the ticket): it delegates to the audited
//! reference `ed25519-dalek` implementation.
//!
//! ## Seams
//!
//! Fetching ([`RegistryFetcher`]) and the wall clock ([`Clock`]) are injected, so
//! the whole chain — verification, caching, staleness, fallback — is exercised
//! offline against a deterministic test keypair with no network and no real time.
//! The production wiring ([`refresh_once`], the daemon's
//! [`crate::router::SessionManager::spawn_registry_refresh`] task) supplies the
//! HTTP fetcher and the system clock.

#![allow(
    clippy::duration_suboptimal_units,
    reason = "cadence and staleness are reasoned in seconds throughout (the arithmetic \
              `6 * 60 * 60` reads as the intended hours), and the test clock uses \
              second-granular offsets"
)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use ed25519_dalek::{Signature, VerifyingKey};

use crate::config::RegistryConfig;
use crate::health::{Finding, FindingCode, Severity};
use crate::recipes::{BlessedEntry, BlessedManifest, InstallRecipe};

/// The default registry artifact URL — a **placeholder** the maintainer will
/// stand up at go-live.
///
/// It is *not* wired as the active default: the shipped [`RegistryConfig`] leaves
/// `url` absent, so the loader is seed-only until the maintainer sets `url` (see
/// `registry/README.md`). This constant records the intended endpoint so the
/// go-live diff is a single, obvious change.
pub const DEFAULT_REGISTRY_URL: &str = "https://registry.catenary.dev/registry.toml";

/// The **in-binary trust root**: the ed25519 public key that must have signed a
/// fetched registry artifact for it to be accepted.
///
/// This is a **placeholder** (all-zero). No real key is generated or embedded in
/// the repository. The maintainer mints the registry keypair offline, keeps the
/// secret half in the registry repo's CI secret store, and embeds the 32-byte
/// public half here at go-live (see `registry/README.md`). Until then the shipped
/// config is seed-only, so this key is never consulted in production; tests
/// exercise verification with an ephemeral, deterministically-seeded test keypair
/// instead.
pub const PRODUCTION_TRUST_ROOT: [u8; 32] = [0u8; 32];

/// The slow refresh cadence for the daemon's background registry task (hours-class).
///
/// A pinned, verified artifact changes only when the registry repo republishes (a
/// pin bump or an advisory rollback), so a fast poll would prove nothing; the
/// revocation latency is fetch-cadence, not release-cadence.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// How old a cached copy may be before a refresh failure flags it stale.
///
/// Beyond this horizon a failed refresh raises [`FindingCode::RegistryStale`].
/// Generous relative to the refresh cadence — a single missed refresh is not
/// stale; a persistent multi-day outage is.
pub const CACHE_STALE_AFTER: Duration = Duration::from_secs(48 * 60 * 60);

/// A ceiling on a fetched artifact so a hostile or misdirected response cannot
/// exhaust memory. The recipes + manifest payload is a few KiB; 8 MiB is orders
/// of magnitude of headroom.
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;

/// The cached payload file name under the registry cache directory.
const PAYLOAD_FILE: &str = "registry.toml";
/// The cached detached-signature file name.
const SIGNATURE_FILE: &str = "registry.toml.sig";
/// The cached fetch-timestamp file name (decimal Unix seconds).
const FETCHED_AT_FILE: &str = "fetched_at";

// ── the resolved payload ─────────────────────────────────────────────

/// The registry payload: the recipes and the blessed-manifest, together.
///
/// On the wire this is a single TOML document with `[recipe.<name>]` and
/// `[blessed.<name>.<platform>]` tables — the two data sets [`crate::recipes`]
/// ships as separate embedded files, bundled into one signed artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryPayload {
    /// CI-internal install recipes keyed by canonical server name.
    pub recipes: BTreeMap<String, InstallRecipe>,
    /// The blessed-manifest: servers that passed the conformance gate.
    pub manifest: BlessedManifest,
}

impl RegistryPayload {
    /// An empty payload — the last-resort value if even the embedded seed fails
    /// to parse (which the seed's own tests forbid, so this is defensive).
    fn empty() -> Self {
        Self {
            recipes: BTreeMap::new(),
            manifest: BlessedManifest::default(),
        }
    }
}

/// Which rung of the resolution chain produced the served payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrySource {
    /// Fetched from the registry URL and verified against the trust root.
    Verified,
    /// The last verified payload, read back from the on-disk cache.
    Cache,
    /// The in-binary shipped defaults (offline seed).
    Seed,
}

impl RegistrySource {
    /// A stable lowercase token for logging and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Cache => "cache",
            Self::Seed => "seed",
        }
    }
}

/// The outcome of a resolution: the served payload, its provenance, and any
/// findings a renderer (or the daemon's tracing bridge) should surface.
#[derive(Debug, Clone)]
pub struct ResolvedRegistry {
    /// The payload that was resolved (always present — resolution never fails).
    pub payload: RegistryPayload,
    /// Which rung of the chain it came from.
    pub source: RegistrySource,
    /// Zero or more findings ([`FindingCode::RegistryStale`] /
    /// [`FindingCode::RegistryBadSignature`]) describing any degradation.
    pub findings: Vec<Finding>,
}

// ── the in-binary seed ───────────────────────────────────────────────

/// The in-binary seed payload: the shipped default recipes + blessed-manifest.
///
/// # Errors
///
/// Returns an error if the embedded `defaults/recipes.toml` or
/// `defaults/blessed-manifest.toml` fails to parse (guarded by their own tests).
pub fn seed_payload() -> Result<RegistryPayload> {
    Ok(RegistryPayload {
        recipes: crate::recipes::default_recipes()?,
        manifest: crate::recipes::default_blessed_manifest()?,
    })
}

// ── payload parsing ──────────────────────────────────────────────────

/// Parse target for the combined registry document (`[recipe.*]` +
/// `[blessed.*.<platform>]`).
///
/// Blessed rows are platform-qualified (server → platform → entry; misc 164),
/// matching [`crate::recipes::BlessedManifest`].
#[derive(Debug, Default, serde::Deserialize)]
struct RegistryDoc {
    #[serde(default)]
    recipe: BTreeMap<String, InstallRecipe>,
    #[serde(default)]
    blessed: BTreeMap<String, BTreeMap<String, BlessedEntry>>,
}

/// Parse a signed registry payload's bytes into a [`RegistryPayload`].
///
/// This is `parse-for-use`; a caller must have verified the signature first.
///
/// # Errors
///
/// Returns an error if the bytes are not UTF-8, are not valid TOML, or any recipe
/// / blessed entry is missing a required field.
pub fn parse_payload(bytes: &[u8]) -> Result<RegistryPayload> {
    let text = std::str::from_utf8(bytes).context("registry payload is not valid UTF-8")?;
    let doc: RegistryDoc = toml::from_str(text).context("registry payload is not valid TOML")?;
    Ok(RegistryPayload {
        recipes: doc.recipe,
        manifest: BlessedManifest {
            blessed: doc.blessed,
        },
    })
}

// ── signature verification ───────────────────────────────────────────

/// Verify a detached ed25519 signature over `payload` against `trust_root`.
///
/// The verification is delegated to the audited `ed25519-dalek` implementation —
/// never hand-rolled — and uses the strict check ([`VerifyingKey::verify_strict`])
/// so a low-order-point malleability does not pass.
///
/// # Errors
///
/// Returns an error when `trust_root` is not a valid ed25519 public key, when
/// `signature` is not exactly 64 bytes, or when the signature does not verify. In
/// every error case the caller MUST NOT parse the payload for use.
pub fn verify_signature(payload: &[u8], signature: &[u8], trust_root: &[u8; 32]) -> Result<()> {
    let verifying_key = VerifyingKey::from_bytes(trust_root)
        .map_err(|e| anyhow!("registry trust root is not a valid ed25519 public key: {e}"))?;
    let sig_bytes: [u8; 64] = signature.try_into().map_err(|_| {
        anyhow!(
            "registry signature must be 64 bytes, got {}",
            signature.len()
        )
    })?;
    let sig = Signature::from_bytes(&sig_bytes);
    verifying_key
        .verify_strict(payload, &sig)
        .map_err(|e| anyhow!("registry signature verification failed: {e}"))?;
    Ok(())
}

// ── fetch + clock seams ──────────────────────────────────────────────

/// A fetched artifact: the payload bytes and the detached-signature bytes.
#[derive(Debug, Clone)]
pub struct FetchedArtifact {
    /// The signed payload bytes (the combined TOML document).
    pub payload: Vec<u8>,
    /// The detached ed25519 signature bytes (64 bytes when well-formed).
    pub signature: Vec<u8>,
}

/// Fetches the registry payload and its detached signature. Injectable so the
/// resolution chain is tested offline.
pub trait RegistryFetcher {
    /// Fetch the payload at `url` and the detached signature at `<url>.sig`.
    ///
    /// # Errors
    ///
    /// Returns an error when either request fails or a body cannot be read.
    fn fetch(&self, url: &str) -> Result<FetchedArtifact>;
}

/// The wall clock. Injectable so staleness and cadence are tested without real
/// time.
pub trait Clock {
    /// The current wall-clock time.
    fn now(&self) -> SystemTime;
}

/// The production [`Clock`]: [`SystemTime::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The production [`RegistryFetcher`]: two bounded HTTP GETs via `ureq`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HttpFetcher;

impl RegistryFetcher for HttpFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedArtifact> {
        let payload = http_get(url).with_context(|| format!("GET {url}"))?;
        let sig_url = format!("{url}.sig");
        let signature = http_get(&sig_url).with_context(|| format!("GET {sig_url}"))?;
        Ok(FetchedArtifact { payload, signature })
    }
}

/// A single bounded HTTP GET returning the response body bytes.
fn http_get(url: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let resp = ureq::get(url)
        .set("User-Agent", "catenary-registry")
        .call()
        .with_context(|| format!("request to {url} failed"))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_ARTIFACT_BYTES)
        .read_to_end(&mut bytes)
        .context("reading registry response body")?;
    Ok(bytes)
}

// ── the loader ───────────────────────────────────────────────────────

/// A cached artifact read back from disk, with its fetch timestamp.
struct CachedArtifact {
    payload: Vec<u8>,
    signature: Vec<u8>,
    fetched_at: SystemTime,
}

/// Resolves the registry through the fetched → cache → seed chain.
///
/// Constructed with the effective config, a [`RegistryFetcher`], and a [`Clock`].
/// The trust root, cache directory, and staleness horizon default to the
/// production values ([`PRODUCTION_TRUST_ROOT`], [`registry_cache_dir`],
/// [`CACHE_STALE_AFTER`]) and are overridable for tests.
pub struct RegistryLoader<'a> {
    url: Option<String>,
    trust_root: [u8; 32],
    cache_dir: PathBuf,
    stale_after: Duration,
    fetcher: &'a dyn RegistryFetcher,
    clock: &'a dyn Clock,
}

impl<'a> RegistryLoader<'a> {
    /// Build a loader from the resolved [`RegistryConfig`], a fetcher, and a
    /// clock. Seed-only (`url` absent or `disable = true`) short-circuits to the
    /// seed on [`Self::resolve`].
    #[must_use]
    pub fn new(
        config: &RegistryConfig,
        fetcher: &'a dyn RegistryFetcher,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            url: config.effective_url().map(ToOwned::to_owned),
            trust_root: PRODUCTION_TRUST_ROOT,
            cache_dir: registry_cache_dir(),
            stale_after: CACHE_STALE_AFTER,
            fetcher,
            clock,
        }
    }

    /// Override the trust root (tests supply an ephemeral test public key).
    #[must_use]
    pub const fn with_trust_root(mut self, trust_root: [u8; 32]) -> Self {
        self.trust_root = trust_root;
        self
    }

    /// Override the cache directory (tests point it at a tempdir).
    #[must_use]
    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = dir.into();
        self
    }

    /// Override the staleness horizon (tests use a short one).
    #[must_use]
    pub const fn with_stale_after(mut self, stale_after: Duration) -> Self {
        self.stale_after = stale_after;
        self
    }

    /// Resolve the registry, degrading down the chain and never hard-failing.
    #[must_use]
    pub fn resolve(&self) -> ResolvedRegistry {
        let mut findings = Vec::new();

        // Seed-only: no URL configured (or `disable = true`). No network, no
        // findings — this is the shipped default.
        let Some(url) = self.url.as_deref() else {
            return seed_resolution(findings);
        };

        // 1. Fetch + verify-before-parse.
        match self.fetcher.fetch(url) {
            Ok(artifact) => {
                match verify_signature(&artifact.payload, &artifact.signature, &self.trust_root) {
                    Ok(()) => match parse_payload(&artifact.payload) {
                        Ok(payload) => {
                            // Persist the verified artifact best-effort (the cache
                            // is regenerable — a write failure is not fatal).
                            let _ = self.store_cache(&artifact, self.clock.now());
                            return ResolvedRegistry {
                                payload,
                                source: RegistrySource::Verified,
                                findings,
                            };
                        }
                        Err(e) => findings.push(stale_finding(format!(
                            "the verified registry artifact did not parse ({e:#}); \
                             serving the cached or seed copy"
                        ))),
                    },
                    Err(e) => findings.push(bad_signature_finding(format!(
                        "{e:#}; falling back to the cached or seed registry"
                    ))),
                }
            }
            Err(e) => findings.push(stale_finding(format!(
                "could not refresh the registry ({e:#}); serving the last cached or seed copy"
            ))),
        }

        // 2. Cache fallback — re-verify the cached bytes (the cache is on disk and
        // could have been tampered with) before use.
        if let Ok(cached) = self.read_cache() {
            match verify_signature(&cached.payload, &cached.signature, &self.trust_root) {
                Ok(()) => match parse_payload(&cached.payload) {
                    Ok(payload) => {
                        let age = self
                            .clock
                            .now()
                            .duration_since(cached.fetched_at)
                            .unwrap_or_default();
                        if age > self.stale_after
                            && !has_code(&findings, FindingCode::RegistryStale)
                        {
                            findings.push(stale_finding(format!(
                                "the cached registry is {} old (older than the {} freshness \
                                 horizon) and could not be refreshed",
                                humanize(age),
                                humanize(self.stale_after),
                            )));
                        }
                        return ResolvedRegistry {
                            payload,
                            source: RegistrySource::Cache,
                            findings,
                        };
                    }
                    Err(e) => findings.push(stale_finding(format!(
                        "the cached registry did not parse ({e:#}); serving the seed"
                    ))),
                },
                Err(e) => findings.push(bad_signature_finding(format!(
                    "the cached registry failed verification ({e:#}); serving the seed"
                ))),
            }
        }

        // 3. Seed — always available.
        seed_resolution(findings)
    }

    /// Read the cached artifact from disk.
    fn read_cache(&self) -> Result<CachedArtifact> {
        let payload = std::fs::read(self.cache_dir.join(PAYLOAD_FILE))
            .context("reading cached registry payload")?;
        let signature = std::fs::read(self.cache_dir.join(SIGNATURE_FILE))
            .context("reading cached registry signature")?;
        let stamp = std::fs::read_to_string(self.cache_dir.join(FETCHED_AT_FILE))
            .context("reading cached registry timestamp")?;
        let secs: u64 = stamp
            .trim()
            .parse()
            .context("cached registry timestamp is not an integer")?;
        Ok(CachedArtifact {
            payload,
            signature,
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        })
    }

    /// Persist a verified artifact to the cache directory.
    fn store_cache(&self, artifact: &FetchedArtifact, now: SystemTime) -> Result<()> {
        std::fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("creating registry cache dir {}", self.cache_dir.display()))?;
        std::fs::write(self.cache_dir.join(PAYLOAD_FILE), &artifact.payload)
            .context("writing cached registry payload")?;
        std::fs::write(self.cache_dir.join(SIGNATURE_FILE), &artifact.signature)
            .context("writing cached registry signature")?;
        let secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        std::fs::write(self.cache_dir.join(FETCHED_AT_FILE), secs.to_string())
            .context("writing cached registry timestamp")?;
        Ok(())
    }
}

/// Build a seed-sourced resolution, carrying forward any findings.
///
/// The last-resort rung of the chain — the in-binary defaults, always available
/// offline. If even the embedded seed fails to parse (its own tests forbid this),
/// an empty payload plus a stale finding is the honest degrade — never a panic.
fn seed_resolution(mut findings: Vec<Finding>) -> ResolvedRegistry {
    let payload = seed_payload().unwrap_or_else(|e| {
        findings.push(stale_finding(format!(
            "the in-binary seed registry failed to parse ({e:#})"
        )));
        RegistryPayload::empty()
    });
    ResolvedRegistry {
        payload,
        source: RegistrySource::Seed,
        findings,
    }
}

/// The registry cache directory: `<cache_dir>/catenary/registry/`.
///
/// The regenerable class ([`crate::paths::cache_dir`]) — the verified artifact is
/// a cache of the published registry, safe to delete and re-fetch.
#[must_use]
pub fn registry_cache_dir() -> PathBuf {
    crate::paths::cache_dir().join("catenary").join("registry")
}

// ── cadence + production entry point ─────────────────────────────────

/// Whether a refresh is due, given the last successful fetch time and the
/// interval. Injectable-clock friendly (all inputs are explicit).
///
/// A registry never fetched (`last_fetch == None`) is always due — this is the
/// on-daemon-start fetch. Otherwise it is due once `interval` has elapsed.
#[must_use]
pub fn refresh_due(now: SystemTime, last_fetch: Option<SystemTime>, interval: Duration) -> bool {
    // Never fetched (`None`) ⇒ due (the daemon-start fetch). A clock that ran
    // backwards (`Err` from `duration_since`) is likewise treated as due —
    // refresh rather than trust a negative age.
    last_fetch.is_none_or(|last| now.duration_since(last).map_or(true, |e| e >= interval))
}

/// Resolve the registry once with the production fetcher + system clock.
///
/// The daemon's background task ([`crate::router::SessionManager::spawn_registry_refresh`])
/// calls this on start and on the slow interval; the shipped seed-only config
/// makes it a cheap, network-free no-op until the registry is turned on.
#[must_use]
pub fn refresh_once(config: &RegistryConfig) -> ResolvedRegistry {
    let fetcher = HttpFetcher;
    let clock = SystemClock;
    RegistryLoader::new(config, &fetcher, &clock).resolve()
}

/// Emit tracing events for a resolution.
///
/// A `debug` names the served source; one `warn` per problem finding (bad
/// signature / stale) so a persistent misconfiguration reaches the
/// user-notification surface and the firehose.
pub fn log_resolution(resolved: &ResolvedRegistry) {
    tracing::debug!(
        source = crate::source::Source::DaemonLifecycle.as_str(),
        registry_source = resolved.source.as_str(),
        recipes = resolved.payload.recipes.len(),
        blessed = resolved.payload.manifest.blessed.len(),
        "registry resolved",
    );
    for finding in &resolved.findings {
        if finding.is_problem() {
            tracing::warn!(
                source = crate::source::Source::DaemonLifecycle.as_str(),
                finding = finding.code.as_str(),
                "{}",
                finding.message,
            );
        }
    }
}

// ── finding helpers ──────────────────────────────────────────────────

/// Build a [`FindingCode::RegistryStale`] warning.
fn stale_finding(message: impl Into<String>) -> Finding {
    Finding::new(FindingCode::RegistryStale, Severity::Warning, message).with_fix_it(
        "pins already installed stay safe (immutable, signature-verified); the served set may \
         lag the published registry until the next successful refresh — check network access to \
         the registry URL.",
    )
}

/// Build a [`FindingCode::RegistryBadSignature`] error.
fn bad_signature_finding(message: impl Into<String>) -> Finding {
    Finding::new(FindingCode::RegistryBadSignature, Severity::Error, message).with_fix_it(
        "the fetched artifact is unsigned, tampered, or signed by a key this binary does not \
         trust — the maintainer must re-sign the registry or ship the matching in-binary trust \
         root. Catenary refuses the artifact and serves the cached or seed registry.",
    )
}

/// Whether `findings` already carries a finding of `code`.
fn has_code(findings: &[Finding], code: FindingCode) -> bool {
    findings.iter().any(|f| f.code == code)
}

/// A coarse human-readable duration (hours/days) for finding messages.
fn humanize(d: Duration) -> String {
    let hours = d.as_secs() / 3600;
    if hours >= 48 {
        format!("{} days", hours / 24)
    } else {
        format!("{hours} hours")
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests use expect/unwrap for readable assertions"
)]
mod tests {
    use std::cell::Cell;

    use ed25519_dalek::{Signer, SigningKey};

    use super::*;

    /// A deterministic test keypair from a fixed seed — no RNG, no real key.
    fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn public_bytes(key: &SigningKey) -> [u8; 32] {
        key.verifying_key().to_bytes()
    }

    fn sign(key: &SigningKey, payload: &[u8]) -> Vec<u8> {
        key.sign(payload).to_bytes().to_vec()
    }

    /// A payload document with one recipe and one platform-qualified blessed row.
    const SAMPLE: &str = "[recipe.taplo]\n\
                          ecosystem = \"cargo\"\n\
                          package = \"taplo-cli\"\n\
                          version = \"0.10.0\"\n\
                          tier = \"cargo-locked\"\n\
                          draft = true\n\n\
                          [blessed.taplo.linux-x86_64]\n\
                          version = \"0.10.0\"\n\
                          platform = \"linux-x86_64\"\n\
                          date = \"2026-07-07\"\n";

    /// A fetcher returning a fixed artifact, or a failure when `None`.
    struct FakeFetcher {
        artifact: Option<FetchedArtifact>,
    }

    impl FakeFetcher {
        fn serving(artifact: FetchedArtifact) -> Self {
            Self {
                artifact: Some(artifact),
            }
        }

        fn failing() -> Self {
            Self { artifact: None }
        }
    }

    impl RegistryFetcher for FakeFetcher {
        fn fetch(&self, _url: &str) -> Result<FetchedArtifact> {
            self.artifact
                .clone()
                .ok_or_else(|| anyhow!("simulated fetch failure"))
        }
    }

    /// A settable fake clock.
    struct FakeClock {
        now: Cell<SystemTime>,
    }

    impl FakeClock {
        fn at(secs: u64) -> Self {
            Self {
                now: Cell::new(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            }
        }

        fn advance(&self, by: Duration) {
            self.now.set(self.now.get() + by);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> SystemTime {
            self.now.get()
        }
    }

    fn enabled_config() -> RegistryConfig {
        RegistryConfig {
            url: Some("https://example.test/registry.toml".to_owned()),
            disable: false,
        }
    }

    #[test]
    fn seed_payload_parses_the_shipped_defaults() {
        let seed = seed_payload().expect("seed payload parses");
        assert!(!seed.recipes.is_empty(), "the seed ships recipes");
        // First blessing landed 2026-07-08 (conformance run 28948357654):
        // the seed carries the committed blessed set from then on.
        assert!(
            !seed.manifest.blessed.is_empty(),
            "the shipped manifest carries the blessed set"
        );
    }

    #[test]
    fn parse_payload_reads_both_tables() {
        let payload = parse_payload(SAMPLE.as_bytes()).expect("sample parses");
        assert!(payload.recipes.contains_key("taplo"));
        assert!(payload.manifest.blessed.contains_key("taplo"));
        assert_eq!(
            payload.manifest.blessed["taplo"]["linux-x86_64"].version,
            "0.10.0"
        );
    }

    #[test]
    fn verify_accepts_good_and_rejects_tampered_or_wrong_key() {
        let key = test_key(7);
        let sig = sign(&key, SAMPLE.as_bytes());
        // A good signature under the right key verifies.
        assert!(verify_signature(SAMPLE.as_bytes(), &sig, &public_bytes(&key)).is_ok());
        // A single flipped payload byte fails.
        let mut tampered = SAMPLE.as_bytes().to_vec();
        tampered[0] ^= 0x01;
        assert!(verify_signature(&tampered, &sig, &public_bytes(&key)).is_err());
        // The right signature under a different trust root fails.
        let other = test_key(9);
        assert!(verify_signature(SAMPLE.as_bytes(), &sig, &public_bytes(&other)).is_err());
        // A malformed (non-64-byte) signature fails.
        assert!(verify_signature(SAMPLE.as_bytes(), b"short", &public_bytes(&key)).is_err());
    }

    #[test]
    fn placeholder_trust_root_is_the_zero_placeholder() {
        // A guard so swapping in the real minted key is a visible, intentional
        // diff — no real key ships in the repo.
        assert_eq!(PRODUCTION_TRUST_ROOT, [0u8; 32]);
        assert!(DEFAULT_REGISTRY_URL.starts_with("https://"));
    }

    #[test]
    fn disabled_config_resolves_seed_with_no_findings() {
        // The shipped default: url absent → seed-only, no network.
        let fetcher = FakeFetcher::failing();
        let clock = FakeClock::at(1_000_000);
        let resolved = RegistryLoader::new(&RegistryConfig::default(), &fetcher, &clock).resolve();
        assert_eq!(resolved.source, RegistrySource::Seed);
        assert!(resolved.findings.is_empty(), "{:?}", resolved.findings);

        // `disable = true` with a url is likewise seed-only.
        let disabled = RegistryConfig {
            url: Some("https://example.test/registry.toml".to_owned()),
            disable: true,
        };
        let resolved = RegistryLoader::new(&disabled, &fetcher, &clock).resolve();
        assert_eq!(resolved.source, RegistrySource::Seed);
        assert!(resolved.findings.is_empty());
    }

    #[test]
    fn verified_fetch_resolves_and_populates_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let key = test_key(3);
        let artifact = FetchedArtifact {
            payload: SAMPLE.as_bytes().to_vec(),
            signature: sign(&key, SAMPLE.as_bytes()),
        };
        let fetcher = FakeFetcher::serving(artifact);
        let clock = FakeClock::at(2_000_000);
        let resolved = RegistryLoader::new(&enabled_config(), &fetcher, &clock)
            .with_trust_root(public_bytes(&key))
            .with_cache_dir(tmp.path())
            .resolve();
        assert_eq!(resolved.source, RegistrySource::Verified);
        assert!(resolved.findings.is_empty(), "{:?}", resolved.findings);
        assert!(resolved.payload.recipes.contains_key("taplo"));
        // The verified artifact was cached.
        assert!(tmp.path().join(PAYLOAD_FILE).exists());
        assert!(tmp.path().join(SIGNATURE_FILE).exists());
        assert!(tmp.path().join(FETCHED_AT_FILE).exists());
    }

    #[test]
    fn fetch_failure_flags_stale_and_serves_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let key = test_key(4);
        let good = FetchedArtifact {
            payload: SAMPLE.as_bytes().to_vec(),
            signature: sign(&key, SAMPLE.as_bytes()),
        };
        let clock = FakeClock::at(3_000_000);
        // First, a good fetch populates the cache.
        let _ = RegistryLoader::new(&enabled_config(), &FakeFetcher::serving(good), &clock)
            .with_trust_root(public_bytes(&key))
            .with_cache_dir(tmp.path())
            .resolve();
        // Then, a failing fetch degrades to the (fresh) cache with a stale finding.
        let resolved = RegistryLoader::new(&enabled_config(), &FakeFetcher::failing(), &clock)
            .with_trust_root(public_bytes(&key))
            .with_cache_dir(tmp.path())
            .resolve();
        assert_eq!(resolved.source, RegistrySource::Cache);
        assert_eq!(resolved.findings.len(), 1);
        assert_eq!(resolved.findings[0].code, FindingCode::RegistryStale);
        assert!(resolved.payload.recipes.contains_key("taplo"));
    }

    #[test]
    fn bad_signature_flags_and_falls_back_to_seed() {
        let key = test_key(5);
        let wrong = test_key(6);
        // The payload is signed by `wrong`, but the loader trusts `key`.
        let artifact = FetchedArtifact {
            payload: SAMPLE.as_bytes().to_vec(),
            signature: sign(&wrong, SAMPLE.as_bytes()),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let clock = FakeClock::at(4_000_000);
        let resolved =
            RegistryLoader::new(&enabled_config(), &FakeFetcher::serving(artifact), &clock)
                .with_trust_root(public_bytes(&key))
                .with_cache_dir(tmp.path())
                .resolve();
        // No cache exists, so it falls all the way to the seed — never unpinned.
        assert_eq!(resolved.source, RegistrySource::Seed);
        assert!(
            resolved
                .findings
                .iter()
                .any(|f| f.code == FindingCode::RegistryBadSignature),
            "{:?}",
            resolved.findings
        );
        assert!(!resolved.payload.recipes.is_empty(), "seed still served");
    }

    #[test]
    fn stale_cache_beyond_horizon_is_flagged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let key = test_key(8);
        let good = FetchedArtifact {
            payload: SAMPLE.as_bytes().to_vec(),
            signature: sign(&key, SAMPLE.as_bytes()),
        };
        let clock = FakeClock::at(5_000_000);
        let _ = RegistryLoader::new(&enabled_config(), &FakeFetcher::serving(good), &clock)
            .with_trust_root(public_bytes(&key))
            .with_cache_dir(tmp.path())
            .with_stale_after(Duration::from_secs(3600))
            .resolve();
        // Advance well past the (1-hour) horizon, then fail the fetch.
        clock.advance(Duration::from_secs(10 * 3600));
        let resolved = RegistryLoader::new(&enabled_config(), &FakeFetcher::failing(), &clock)
            .with_trust_root(public_bytes(&key))
            .with_cache_dir(tmp.path())
            .with_stale_after(Duration::from_secs(3600))
            .resolve();
        assert_eq!(resolved.source, RegistrySource::Cache);
        // One stale finding — the fetch-failure and the age check do not double up.
        assert_eq!(
            resolved
                .findings
                .iter()
                .filter(|f| f.code == FindingCode::RegistryStale)
                .count(),
            1,
            "{:?}",
            resolved.findings
        );
    }

    #[test]
    fn tampered_cache_fails_reverification_and_serves_seed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let key = test_key(2);
        let good = FetchedArtifact {
            payload: SAMPLE.as_bytes().to_vec(),
            signature: sign(&key, SAMPLE.as_bytes()),
        };
        let clock = FakeClock::at(6_000_000);
        let _ = RegistryLoader::new(&enabled_config(), &FakeFetcher::serving(good), &clock)
            .with_trust_root(public_bytes(&key))
            .with_cache_dir(tmp.path())
            .resolve();
        // Corrupt the cached payload on disk.
        std::fs::write(tmp.path().join(PAYLOAD_FILE), b"tampered bytes").expect("overwrite");
        let resolved = RegistryLoader::new(&enabled_config(), &FakeFetcher::failing(), &clock)
            .with_trust_root(public_bytes(&key))
            .with_cache_dir(tmp.path())
            .resolve();
        assert_eq!(resolved.source, RegistrySource::Seed);
        assert!(
            resolved
                .findings
                .iter()
                .any(|f| f.code == FindingCode::RegistryBadSignature),
            "a tampered cache is a bad-signature finding: {:?}",
            resolved.findings
        );
    }

    #[test]
    fn refresh_due_predicate() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let interval = Duration::from_secs(3600);
        // Never fetched → due (daemon start).
        assert!(refresh_due(now, None, interval));
        // Just fetched → not due.
        assert!(!refresh_due(now, Some(now), interval));
        // A minute ago → not due.
        assert!(!refresh_due(
            now,
            Some(now - Duration::from_secs(60)),
            interval
        ));
        // Past the interval → due.
        assert!(refresh_due(
            now,
            Some(now - Duration::from_secs(7200)),
            interval
        ));
    }

    #[test]
    fn round_trip_seed_through_payload_bytes() {
        // The seed serializes and re-parses through the same combined-doc form the
        // wire uses (recipes + an empty blessed table).
        #[derive(serde::Serialize)]
        struct Doc<'a> {
            recipe: &'a BTreeMap<String, InstallRecipe>,
            blessed: &'a BTreeMap<String, BTreeMap<String, BlessedEntry>>,
        }
        let seed = seed_payload().expect("seed");
        let text = toml::to_string(&Doc {
            recipe: &seed.recipes,
            blessed: &seed.manifest.blessed,
        })
        .expect("serialize");
        let reparsed = parse_payload(text.as_bytes()).expect("reparse");
        assert_eq!(seed.recipes, reparsed.recipes);
    }
}
