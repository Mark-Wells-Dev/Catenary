// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CI-internal install recipes, CI provisioning, and the blessed-manifest
//! (tui-rework 07/10).
//!
//! Three disjoint data sets, all parsed here, all keyed by canonical server name
//! (the `[lsp.server.*]` keys in `defaults/servers.toml`):
//!
//! - **Recipes** (`defaults/recipes.toml`) are *CI-internal install
//!   instructions*: how a conformance matrix job installs a pinned language
//!   server before running the harness against it. Every recipe is a **draft**
//!   — a draft is NOT blessable, NOT user-visible, and nothing may render it as
//!   a recommendation. That invariant is enforced structurally: recipes live in
//!   their own file, are parsed by this module alone, and are **never wired into
//!   [`crate::config`]'s load path**, so no user surface (doctor, TUI, fix-it
//!   text) can reach them. Catenary never prints an unpinned recommendation, and
//!   a draft never even reaches the pinned-recommendation surface — only the
//!   blessed-manifest feeds any user surface (that surface ships later, in
//!   tickets 03/06).
//!
//! - **Provisioning** (`defaults/ci-provision.toml`) records how a conformance
//!   matrix job *obtains* a server that no user-grade ecosystem recipe can carry
//!   — a toolchain component, a system package, a checksummed GitHub-release
//!   binary, a git-pinned source build, a gem. A provision is **not** a recipe
//!   and is structurally unable to become an install offer: it lives in its own
//!   file, is parsed by this module alone, and its key set is asserted disjoint
//!   from the recipe set ([`validate_provisioning`] plus the test module). A
//!   provisioned server still enters the *blessed*-manifest (blessing = conforms
//!   with the shipped default config), but blessing is not offerability —
//!   offerability additionally requires a recipe by construction
//!   (`crate::install::BlessedRecipe`).
//!
//! - **The blessed-manifest** (`defaults/blessed-manifest.toml`) is the
//!   committed record of servers that have passed the CI conformance gate:
//!   server → conformed version / platform / date. A server earns an entry only
//!   by a green conformance matrix job, reviewed by a human on the PR (blessing
//!   is mechanized, not manual — see the tui-rework 06 design). It is the *only*
//!   recipe-derived data a user surface is ever allowed to consult.
//!
//! None of the three is consumed by the running daemon; they exist for the
//! conformance harness (`tests/conformance_harness.rs`), the `refresh-recipes`
//! maintenance target, and the conformance CI workflow. This module is the
//! single parse/validate port for all three.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Embedded default recipe drafts (`defaults/recipes.toml`).
pub const DEFAULT_RECIPES: &str = include_str!("../defaults/recipes.toml");

/// Embedded CI provisioning stanzas (`defaults/ci-provision.toml`).
pub const DEFAULT_CI_PROVISION: &str = include_str!("../defaults/ci-provision.toml");

/// Embedded committed blessed-manifest (`defaults/blessed-manifest.toml`).
pub const DEFAULT_BLESSED_MANIFEST: &str = include_str!("../defaults/blessed-manifest.toml");

/// The package ecosystem a recipe installs from.
///
/// The four ecosystems whose native verification the recipe schema records
/// (tui-rework 06 §"Recipes as data"). GitHub-release binary distributions are
/// deliberately out of scope — placing binaries is the full-mason boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    /// npm registry (Node-based servers).
    Npm,
    /// crates.io (Rust servers).
    Cargo,
    /// `PyPI` (Python servers).
    Pip,
    /// Go module proxy (`go install`).
    Go,
}

impl Ecosystem {
    /// The lowercase token used in TOML and matrix jobs (`npm`, `cargo`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Cargo => "cargo",
            Self::Pip => "pip",
            Self::Go => "go",
        }
    }
}

/// How the pinned artifact is verified at install time.
///
/// The tier is recorded so a renderer can state the guarantee honestly, and so
/// the conformance workflow knows which verifying install form to run. Each tier
/// names the mechanism the ecosystem provides (tui-rework 06 §"Recipes as
/// data").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationTier {
    /// go — module version pin; the Go checksum database verifies the download
    /// transparently, so no explicit hash is carried.
    GoChecksumdb,
    /// cargo — exact version installed with `--locked`; cargo verifies the
    /// registry index checksums.
    CargoLocked,
    /// pip — installed with `--require-hashes`; every artifact's sha256 is
    /// pinned in the recipe.
    PipHashes,
    /// npm — exact version plus the expected tarball sha512; the engine fetches,
    /// verifies the tarball, then installs from the verified artifact with
    /// `--ignore-scripts` (install scripts never run).
    NpmTarballSha512,
}

impl VerificationTier {
    /// The kebab-case token used in TOML.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GoChecksumdb => "go-checksumdb",
            Self::CargoLocked => "cargo-locked",
            Self::PipHashes => "pip-hashes",
            Self::NpmTarballSha512 => "npm-tarball-sha512",
        }
    }

    /// Whether this tier carries an explicit content hash in the recipe.
    ///
    /// `pip`/`npm` pin an artifact hash; `go`/`cargo` delegate verification to
    /// the ecosystem (checksum DB / `--locked` index checksums), so a missing
    /// hash is expected and correct for them.
    #[must_use]
    pub const fn carries_hash(self) -> bool {
        matches!(self, Self::PipHashes | Self::NpmTarballSha512)
    }
}

/// A runtime dependency beyond the ecosystem's own host toolchain.
///
/// The npm/cargo/pip/go ecosystems each imply their own host (node, cargo,
/// python, go); this field records an *additional* runtime a server needs — the
/// motivating case is a JDK for a JVM-based server (kotlin-language-server, metals). The
/// conformance workflow provisions it before running the harness, and a later
/// suggestion surface needs it in its fix-it text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    /// The runtime's canonical name (e.g. `jdk`, `dotnet`).
    pub name: String,
    /// An optional version constraint (e.g. `>=17`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// One CI-internal install recipe for a shipped server.
///
/// `version` and `tier` are required by the schema: a recipe missing either
/// fails to deserialize (the schema test relies on this). `hash` is optional at
/// draft stage — the `refresh-recipes` tooling fills it mechanically for the
/// hash-carrying tiers; a fabricated hash is never shipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRecipe {
    /// The ecosystem the package is installed from.
    pub ecosystem: Ecosystem,
    /// The package/module identifier within the ecosystem.
    pub package: String,
    /// The exact pinned version (never a range).
    pub version: String,
    /// How the pinned artifact is verified.
    pub tier: VerificationTier,
    /// Always `true` for shipped recipes. A draft is CI-internal and never
    /// blessable/user-visible; defaults to `true` so an omitted flag can never
    /// silently produce a non-draft recipe.
    #[serde(default = "default_true")]
    pub draft: bool,
    /// The pinned content hash for hash-carrying tiers (`pip` sha256, `npm`
    /// sha512 in SRI form). Absent when not yet resolved (drafts) or when the
    /// tier delegates verification to the ecosystem (`go`/`cargo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// A free-text note stating residual supply-chain risk honestly (e.g. an npm
    /// package that resolves transitive deps at install time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Whether this server is exercised by the conformance harness (default
    /// `true`).
    ///
    /// A few shipped servers cannot satisfy the intentional-diagnostic contract
    /// (07 §fixtures) through Catenary's shipped lifecycle — a server that
    /// implements no diagnostics at all, or whose debounced/scan-based publish
    /// never lands inside the settle-then-collect window even on a warm
    /// re-diagnose. Marking `conformance = false` **excludes** the server from
    /// both the CI matrix (`tools/conformance_matrix.py` drops it, like a
    /// `pending` provision) and the harness `CASES` list — the matrix↔CASES
    /// drift guard requires exactly this: a non-exempt entry MUST have a case, an
    /// exempt one must NOT. The `note` records the honest reason (tui-rework 13).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub conformance: bool,
    /// An additional runtime dependency (e.g. a JDK), if any.
    ///
    /// A struct-valued field, so it is declared **last**: TOML serializes a
    /// nested struct as a `[recipe.<name>.runtime]` sub-table, which must follow
    /// every scalar key of the parent table — keeping it last makes any recipe
    /// round-trip through `toml::to_string` regardless of which scalars are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
}

/// serde default for [`InstallRecipe::draft`] — drafts are the safe default.
const fn default_true() -> bool {
    true
}

/// serde `skip_serializing_if` for a defaults-false bool (e.g.
/// [`ProvisionStanza::pending`]) — omit it from TOML when unset. The `&bool`
/// signature is dictated by serde's `skip_serializing_if` contract.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires an fn(&T) -> bool"
)]
const fn is_false(b: &bool) -> bool {
    !*b
}

/// serde `skip_serializing_if` for a defaults-true bool (the `conformance`
/// fields) — omit it from TOML when it holds its default, so only an explicit
/// `conformance = false` exemption is written. `&bool` per serde's contract.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires an fn(&T) -> bool"
)]
const fn is_true(b: &bool) -> bool {
    *b
}

/// Parse target for `defaults/recipes.toml`: a `[recipe.<name>]` table map.
#[derive(Debug, Default, Deserialize)]
struct RecipesDoc {
    #[serde(default)]
    recipe: BTreeMap<String, InstallRecipe>,
}

/// Parse a `[recipe.*]` TOML document into a map of recipes keyed by server
/// name.
///
/// # Errors
///
/// Returns an error if the TOML is malformed or any recipe is missing a
/// required field (`ecosystem`, `package`, `version`, `tier`).
pub fn parse_recipes(contents: &str) -> Result<BTreeMap<String, InstallRecipe>> {
    let doc: RecipesDoc = toml::from_str(contents).context("Failed to parse recipes TOML")?;
    Ok(doc.recipe)
}

/// Parse the embedded default recipe drafts.
///
/// # Errors
///
/// Returns an error if the embedded `defaults/recipes.toml` is malformed.
pub fn default_recipes() -> Result<BTreeMap<String, InstallRecipe>> {
    parse_recipes(DEFAULT_RECIPES)
}

/// Validate a recipe set, returning one human-readable message per problem.
///
/// Enforces the CI-internal invariants: every recipe carries a non-empty
/// `package` and `version`, and every recipe is a draft (a non-draft recipe
/// could otherwise reach a user surface, which the draft/blessed split forbids).
/// A missing hash is *not* an error — hash-carrying tiers may ship version-pinned
/// but hashless at draft stage, with the hash filled mechanically by
/// `refresh-recipes`. Use [`recipes_missing_hash`] to report those.
#[must_use]
pub fn validate_recipes(recipes: &BTreeMap<String, InstallRecipe>) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, recipe) in recipes {
        if recipe.package.trim().is_empty() {
            errors.push(format!("recipe `{name}` has an empty package"));
        }
        if recipe.version.trim().is_empty() {
            errors.push(format!("recipe `{name}` has an empty version"));
        }
        if !recipe.draft {
            errors.push(format!(
                "recipe `{name}` is not marked draft — recipes are CI-internal \
                 install instructions and are never blessable or user-visible"
            ));
        }
        if !recipe.conformance && recipe.note.as_ref().is_none_or(|n| n.trim().is_empty()) {
            errors.push(format!(
                "recipe `{name}` is `conformance = false` but carries no `note` — an \
                 exemption must state honestly why the shipped lifecycle cannot conform it"
            ));
        }
    }
    errors
}

/// Names of hash-carrying-tier recipes that have no `hash` yet.
///
/// These are the drafts `refresh-recipes` still needs to resolve a hash for.
/// Reported (not errored) so the draft set can ship version-pinned but hashless
/// without a fabricated hash.
#[must_use]
pub fn recipes_missing_hash(recipes: &BTreeMap<String, InstallRecipe>) -> Vec<String> {
    recipes
        .iter()
        .filter(|(_, r)| r.tier.carries_hash() && r.hash.is_none())
        .map(|(name, _)| name.clone())
        .collect()
}

// ── CI provisioning (tui-rework 10) ──────────────────────────────────

/// How a conformance CI matrix job obtains a server that no user-grade ecosystem
/// recipe can carry.
///
/// Each variant names the mechanism `.github/workflows/conformance.yml` runs; the
/// kebab-case token is the `kind` key in `defaults/ci-provision.toml`. Unlike
/// [`Ecosystem`], these are deliberately CI-only — a provisioned server is never
/// an install offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvisionKind {
    /// `rustup component add <component>` — the pin rides the pinned toolchain,
    /// so no explicit version is carried.
    RustupComponent,
    /// The runner's apt package (`apt-get install <apt>`); apt's signed
    /// repositories verify, so no explicit hash is carried.
    SystemApt,
    /// A pinned GitHub release `asset` from `repo`, verified against `sha256` and
    /// unpacked; `bin` is the launcher path within the asset.
    GithubReleaseSha256,
    /// `cargo install --git <git> --rev <rev> --locked` — a reproducible source
    /// build pinned to an exact commit.
    CargoGitLocked,
    /// A pinned `url` tarball verified against `sha256` and unpacked (for a
    /// release hosted off GitHub, e.g. an Eclipse milestone build).
    TarballSha256,
    /// `gem install <gem> --version <version>` — the gem registry carries no
    /// npm-grade per-artifact integrity, so verification is the exact-version pin
    /// plus the registry index.
    Gem,
}

impl ProvisionKind {
    /// The kebab-case token used in TOML and matrix jobs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RustupComponent => "rustup-component",
            Self::SystemApt => "system-apt",
            Self::GithubReleaseSha256 => "github-release-sha256",
            Self::CargoGitLocked => "cargo-git-locked",
            Self::TarballSha256 => "tarball-sha256",
            Self::Gem => "gem",
        }
    }
}

/// One CI provisioning stanza for a shipped server.
///
/// The kind-specific fields are all optional at the type level (one struct serves
/// every kind); [`validate_provisioning`] enforces that the fields a given `kind`
/// requires are present, unless the stanza is `pending`. `runtime` is declared
/// last so a nested struct serializes as a trailing `[provision.<name>.runtime]`
/// sub-table (same reason as [`InstallRecipe::runtime`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvisionStanza {
    /// The provisioning mechanism.
    pub kind: ProvisionKind,
    /// The exact pin, where the kind carries one (git tag / release tag / gem
    /// version). Absent for kinds that ride a host (`rustup-component`,
    /// `system-apt`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `rustup-component`: the component name (e.g. `rust-analyzer`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// `system-apt`: the apt package name (e.g. `clangd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apt: Option<String>,
    /// `github-release-sha256`: the `owner/name` repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// `github-release-sha256`: the release asset file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// `github-release-sha256` / `tarball-sha256`: the pinned asset sha256 (hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// `github-release-sha256` / `tarball-sha256`: the launcher path within the
    /// unpacked asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// `cargo-git-locked`: the git repository URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    /// `cargo-git-locked`: a human-readable tag for the pinned rev (documentation
    /// only; `rev` is what `cargo install` pins on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// `cargo-git-locked`: the exact commit sha the build pins to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// `tarball-sha256`: the pinned tarball URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `gem`: the gem name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gem: Option<String>,
    /// `true` when a pin this kind requires could not be resolved mechanically
    /// from the authoring environment. A pending stanza is exempt from the
    /// required-field check (so it round-trips and ships) but its matrix job
    /// fails loudly until a maintainer fills the pin — never invented.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending: bool,
    /// Whether this server is exercised by the conformance harness (default
    /// `true`). Same semantics as [`InstallRecipe::conformance`]: `false`
    /// excludes the server from both the CI matrix and the harness `CASES`, with
    /// the `note` recording why the shipped lifecycle cannot conform it
    /// (tui-rework 13).
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub conformance: bool,
    /// Free-text note stating residual risk or what a pending pin still needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// An additional runtime dependency (e.g. a JDK for jdtls), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<Runtime>,
}

/// Parse target for `defaults/ci-provision.toml`: a `[provision.<name>]` map.
#[derive(Debug, Default, Deserialize)]
struct ProvisionDoc {
    #[serde(default)]
    provision: BTreeMap<String, ProvisionStanza>,
}

/// Parse a `[provision.*]` TOML document into a map keyed by server name.
///
/// # Errors
///
/// Returns an error if the TOML is malformed or a stanza has no `kind`.
pub fn parse_provisioning(contents: &str) -> Result<BTreeMap<String, ProvisionStanza>> {
    let doc: ProvisionDoc =
        toml::from_str(contents).context("Failed to parse provisioning TOML")?;
    Ok(doc.provision)
}

/// Parse the embedded default provisioning stanzas.
///
/// # Errors
///
/// Returns an error if the embedded `defaults/ci-provision.toml` is malformed.
pub fn default_provisioning() -> Result<BTreeMap<String, ProvisionStanza>> {
    parse_provisioning(DEFAULT_CI_PROVISION)
}

/// Validate a provisioning set against a recipe set, returning one message per
/// problem.
///
/// Enforces two CI-internal invariants: the kind-required pins are present (a
/// `pending` stanza is exempt — it ships with the pin unresolved and fails its
/// matrix job loudly instead), and — the structural boundary — the provisioning
/// and recipe key sets are **disjoint**, so a provisioned server can never leak
/// into the offerable recipe set.
#[must_use]
pub fn validate_provisioning(
    provisions: &BTreeMap<String, ProvisionStanza>,
    recipes: &BTreeMap<String, InstallRecipe>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, p) in provisions {
        if recipes.contains_key(name) {
            errors.push(format!(
                "server `{name}` is BOTH provisioned and a recipe — the unofferability \
                 boundary requires the provision and recipe key sets be disjoint"
            ));
        }
        if !p.conformance && p.note.as_ref().is_none_or(|n| n.trim().is_empty()) {
            errors.push(format!(
                "provision `{name}` is `conformance = false` but carries no `note` — an \
                 exemption must state honestly why the shipped lifecycle cannot conform it"
            ));
        }
        if p.pending {
            continue; // a pending stanza may omit the pins it is pending on.
        }
        for field in missing_required_fields(p) {
            errors.push(format!(
                "provision `{name}` (kind `{}`) is missing required field `{field}`",
                p.kind.as_str()
            ));
        }
    }
    errors
}

/// The kind-required fields absent on `p` (the `gem` tier carries no sha256 by
/// design, so it is not required; a `pending` stanza is checked by the caller).
fn missing_required_fields(p: &ProvisionStanza) -> Vec<&'static str> {
    let mut missing = Vec::new();
    let mut require = |present: bool, field: &'static str| {
        if !present {
            missing.push(field);
        }
    };
    match p.kind {
        ProvisionKind::RustupComponent => require(p.component.is_some(), "component"),
        ProvisionKind::SystemApt => require(p.apt.is_some(), "apt"),
        ProvisionKind::GithubReleaseSha256 => {
            require(p.repo.is_some(), "repo");
            require(p.asset.is_some(), "asset");
            require(p.sha256.is_some(), "sha256");
            require(p.bin.is_some(), "bin");
        }
        ProvisionKind::CargoGitLocked => {
            require(p.git.is_some(), "git");
            require(p.rev.is_some(), "rev");
        }
        ProvisionKind::TarballSha256 => {
            require(p.url.is_some(), "url");
            require(p.sha256.is_some(), "sha256");
        }
        ProvisionKind::Gem => {
            require(p.gem.is_some(), "gem");
            require(p.version.is_some(), "version");
        }
    }
    missing
}

/// Names of provisioning stanzas marked `pending` (a required pin unresolved).
///
/// Reported, not errored — a pending stanza ships (so the mechanism and the rest
/// of its pins are recorded) and its matrix job fails until the pin is filled by
/// a maintainer, never invented.
#[must_use]
pub fn provisioning_pending(provisions: &BTreeMap<String, ProvisionStanza>) -> Vec<String> {
    provisions
        .iter()
        .filter(|(_, p)| p.pending)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The server names the conformance matrix runs.
///
/// Every recipe or provision that is neither `pending` (an unresolved pin) nor
/// `conformance = false` (a server the shipped lifecycle cannot conform;
/// tui-rework 13).
///
/// This is the single source of truth the matrix↔`CASES` drift guard checks
/// against: every name here MUST have a harness `CASES` entry, and no exempt
/// name may. `tools/conformance_matrix.py` applies the identical filter (drop
/// `pending`, drop `conformance = false`) so the guard and the CI matrix agree
/// by construction.
#[must_use]
pub fn conformed_server_names(
    recipes: &BTreeMap<String, InstallRecipe>,
    provisions: &BTreeMap<String, ProvisionStanza>,
) -> Vec<String> {
    let from_recipes = recipes
        .iter()
        .filter(|(_, r)| r.conformance)
        .map(|(name, _)| name.clone());
    let from_provisions = provisions
        .iter()
        .filter(|(_, p)| !p.pending && p.conformance)
        .map(|(name, _)| name.clone());
    from_recipes.chain(from_provisions).collect()
}

/// The server names explicitly exempt from conformance (`conformance = false`)
/// across both sources — the complement of [`conformed_server_names`] among
/// non-pending entries.
///
/// The drift guard asserts none of these has a `CASES` entry, so an exemption is
/// a deliberate, reviewable data edit rather than a silently-dropped case.
#[must_use]
pub fn conformance_exempt_names(
    recipes: &BTreeMap<String, InstallRecipe>,
    provisions: &BTreeMap<String, ProvisionStanza>,
) -> Vec<String> {
    let from_recipes = recipes
        .iter()
        .filter(|(_, r)| !r.conformance)
        .map(|(name, _)| name.clone());
    let from_provisions = provisions
        .iter()
        .filter(|(_, p)| !p.pending && !p.conformance)
        .map(|(name, _)| name.clone());
    from_recipes.chain(from_provisions).collect()
}

/// One blessed server entry: a server that passed the CI conformance gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlessedEntry {
    /// The exact version that conformed.
    pub version: String,
    /// The platform the conformance job ran on (e.g. `linux-x86_64`).
    pub platform: String,
    /// The ISO-8601 date the conformance job passed.
    pub date: String,
    /// The honesty tier of the claim (initial tier: `verified-on-linux`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

/// The committed blessed-manifest: server → conformed record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlessedManifest {
    /// Blessed entries keyed by canonical server name.
    #[serde(default)]
    pub blessed: BTreeMap<String, BlessedEntry>,
}

/// Parse a blessed-manifest TOML document.
///
/// # Errors
///
/// Returns an error if the TOML is malformed or an entry is missing a required
/// field (`version`, `platform`, `date`).
pub fn parse_blessed_manifest(contents: &str) -> Result<BlessedManifest> {
    toml::from_str(contents).context("Failed to parse blessed-manifest TOML")
}

/// Parse the embedded default blessed-manifest.
///
/// # Errors
///
/// Returns an error if the embedded `defaults/blessed-manifest.toml` is
/// malformed.
pub fn default_blessed_manifest() -> Result<BlessedManifest> {
    parse_blessed_manifest(DEFAULT_BLESSED_MANIFEST)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::default_server_names;

    #[test]
    fn default_recipes_parse_and_validate() {
        let recipes = default_recipes().expect("default recipes parse");
        assert!(!recipes.is_empty(), "at least one recipe ships");
        let errors = validate_recipes(&recipes);
        assert!(
            errors.is_empty(),
            "shipped recipes must validate: {errors:?}"
        );
    }

    #[test]
    fn every_shipped_recipe_is_a_draft() {
        // The structural invariant: nothing in the recipe file is blessable.
        let recipes = default_recipes().expect("default recipes parse");
        for (name, recipe) in &recipes {
            assert!(recipe.draft, "recipe `{name}` must be a draft");
        }
    }

    #[test]
    fn every_recipe_names_a_real_server() {
        // A recipe keyed by a name absent from defaults/servers.toml could never
        // be installed-and-conformed for a real server.
        let recipes = default_recipes().expect("default recipes parse");
        let servers = default_server_names();
        for name in recipes.keys() {
            assert!(
                servers.contains(name),
                "recipe `{name}` does not name a [lsp.server.*] in defaults/servers.toml"
            );
        }
    }

    #[test]
    fn every_recipe_has_version_and_tier() {
        // Redundant with the type system (both are required fields) but pins the
        // guarantee the ticket names explicitly.
        let recipes = default_recipes().expect("default recipes parse");
        for (name, recipe) in &recipes {
            assert!(!recipe.version.trim().is_empty(), "`{name}` version");
            // `tier` presence is enforced at parse time (a required field); pin
            // that every variant is one of the known four so a new tier can't
            // slip through untested.
            assert!(
                matches!(
                    recipe.tier,
                    VerificationTier::NpmTarballSha512
                        | VerificationTier::CargoLocked
                        | VerificationTier::PipHashes
                        | VerificationTier::GoChecksumdb
                ),
                "`{name}` tier"
            );
        }
    }

    #[test]
    fn recipe_missing_version_fails_to_parse() {
        let toml =
            "[recipe.demo]\necosystem = \"npm\"\npackage = \"x\"\ntier = \"npm-tarball-sha512\"\n";
        assert!(
            parse_recipes(toml).is_err(),
            "a recipe with no version must fail the schema"
        );
    }

    #[test]
    fn recipe_missing_tier_fails_to_parse() {
        let toml = "[recipe.demo]\necosystem = \"npm\"\npackage = \"x\"\nversion = \"1.0.0\"\n";
        assert!(
            parse_recipes(toml).is_err(),
            "a recipe with no tier must fail the schema"
        );
    }

    #[test]
    fn recipe_empty_version_fails_validation() {
        let toml = "[recipe.demo]\necosystem = \"npm\"\npackage = \"x\"\nversion = \"\"\ntier = \"npm-tarball-sha512\"\ndraft = true\n";
        let recipes = parse_recipes(toml).expect("parses (empty string is a string)");
        let errors = validate_recipes(&recipes);
        assert!(
            errors.iter().any(|e| e.contains("empty version")),
            "empty version must be a validation error: {errors:?}"
        );
    }

    #[test]
    fn non_draft_recipe_fails_validation() {
        let toml = "[recipe.demo]\necosystem = \"cargo\"\npackage = \"x\"\nversion = \"1.0.0\"\ntier = \"cargo-locked\"\ndraft = false\n";
        let recipes = parse_recipes(toml).expect("parses");
        let errors = validate_recipes(&recipes);
        assert!(
            errors.iter().any(|e| e.contains("not marked draft")),
            "a non-draft recipe must fail validation: {errors:?}"
        );
    }

    #[test]
    fn recipe_defaults_to_draft_when_flag_omitted() {
        let toml = "[recipe.demo]\necosystem = \"go\"\npackage = \"x\"\nversion = \"v1.0.0\"\ntier = \"go-checksumdb\"\n";
        let recipes = parse_recipes(toml).expect("parses");
        assert!(recipes["demo"].draft, "omitted draft flag defaults to true");
    }

    #[test]
    fn recipe_roundtrips_through_toml() {
        let recipes = default_recipes().expect("default recipes parse");
        let doc = RecipesDoc {
            recipe: recipes.clone(),
        };
        let serialized = toml::to_string(&Wrap {
            recipe: &doc.recipe,
        })
        .expect("serialize");
        let reparsed = parse_recipes(&serialized).expect("reparse");
        assert_eq!(recipes, reparsed, "recipes round-trip through TOML");
    }

    #[test]
    fn default_blessed_manifest_parses_and_roundtrips() {
        let manifest = default_blessed_manifest().expect("manifest parses");
        let serialized = toml::to_string(&manifest).expect("serialize");
        let reparsed = parse_blessed_manifest(&serialized).expect("reparse");
        assert_eq!(
            manifest.blessed.len(),
            reparsed.blessed.len(),
            "manifest round-trips"
        );
        // Every present entry (if any) carries the required fields — enforced by
        // the type, asserted here for the honesty tier.
        for (name, entry) in &manifest.blessed {
            assert!(!entry.version.is_empty(), "`{name}` version");
            assert!(!entry.platform.is_empty(), "`{name}` platform");
            assert!(!entry.date.is_empty(), "`{name}` date");
        }
    }

    #[test]
    fn recipes_span_all_four_ecosystems() {
        let recipes = default_recipes().expect("default recipes parse");
        for eco in [
            Ecosystem::Npm,
            Ecosystem::Cargo,
            Ecosystem::Pip,
            Ecosystem::Go,
        ] {
            assert!(
                recipes.values().any(|r| r.ecosystem == eco),
                "at least one {} recipe ships",
                eco.as_str()
            );
        }
    }

    #[test]
    fn recipe_parses_and_roundtrips_a_runtime_dependency() {
        // The `runtime` field is the schema hook for the JVM/dotnet tail (a JDK
        // for metals/kotlin-language-server) the conformance workflow must provision before
        // running the harness. No shipped draft sets it yet (the runtime tail is
        // out of tranche 1/2), so exercise it directly: inline-table on parse,
        // sub-table on serialize (it is declared last for exactly this).
        let toml = "[recipe.demo]\necosystem = \"npm\"\npackage = \"x\"\n\
                    version = \"1.0.0\"\ntier = \"npm-tarball-sha512\"\ndraft = true\n\
                    runtime = { name = \"jdk\", version = \">=17\" }\n";
        let recipes = parse_recipes(toml).expect("parses a runtime dependency");
        let runtime = recipes["demo"].runtime.as_ref().expect("runtime present");
        assert_eq!(runtime.name, "jdk");
        assert_eq!(runtime.version.as_deref(), Some(">=17"));

        let serialized = toml::to_string(&Wrap { recipe: &recipes }).expect("serialize");
        let reparsed = parse_recipes(&serialized).expect("reparse");
        assert_eq!(recipes, reparsed, "a runtime-bearing recipe round-trips");
    }

    #[test]
    fn every_present_hash_is_well_formed_for_its_tier() {
        // A missing hash is allowed (drafts may ship version-pinned but hashless),
        // but any hash that IS present must match its tier's canonical form so a
        // truncated or mis-pasted value is caught structurally rather than only at
        // install time. npm carries an SRI sha512 (`sha512-<base64>`); pip carries
        // a sha256 (`sha256:<hex>` or bare 64-hex). go/cargo carry none.
        let recipes = default_recipes().expect("default recipes parse");
        for (name, recipe) in &recipes {
            let Some(hash) = recipe.hash.as_deref() else {
                continue;
            };
            match recipe.tier {
                VerificationTier::NpmTarballSha512 => assert!(
                    hash.starts_with("sha512-") && hash.len() > "sha512-".len(),
                    "npm recipe `{name}` hash must be SRI sha512: {hash}"
                ),
                VerificationTier::PipHashes => {
                    let hex = hash.strip_prefix("sha256:").unwrap_or(hash);
                    assert!(
                        hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
                        "pip recipe `{name}` hash must be a sha256 hex digest: {hash}"
                    );
                }
                VerificationTier::GoChecksumdb | VerificationTier::CargoLocked => assert!(
                    recipe.hash.is_none(),
                    "recipe `{name}` tier delegates verification and must carry no hash"
                ),
            }
        }
    }

    #[test]
    fn default_provisioning_parses_and_validates() {
        let provisions = default_provisioning().expect("default provisioning parses");
        assert!(!provisions.is_empty(), "at least one provision ships");
        let recipes = default_recipes().expect("default recipes parse");
        let errors = validate_provisioning(&provisions, &recipes);
        assert!(
            errors.is_empty(),
            "shipped provisioning must validate: {errors:?}"
        );
    }

    #[test]
    fn provisioning_and_recipes_are_disjoint() {
        // The unofferability boundary: a provisioned server lives ONLY in
        // ci-provision.toml and can never appear in the offerable recipe set.
        let provisions = default_provisioning().expect("provisioning parses");
        let recipes = default_recipes().expect("recipes parse");
        for name in provisions.keys() {
            assert!(
                !recipes.contains_key(name),
                "server `{name}` is both provisioned and a recipe — the boundary is broken"
            );
        }
    }

    #[test]
    fn every_provision_names_a_real_server() {
        let provisions = default_provisioning().expect("provisioning parses");
        let servers = default_server_names();
        for name in provisions.keys() {
            assert!(
                servers.contains(name),
                "provision `{name}` does not name a [lsp.server.*] in defaults/servers.toml"
            );
        }
    }

    #[test]
    fn provisioning_covers_the_ticket_servers_and_kinds() {
        // tui-rework 10's headline coverage: the servers everyone actually wants
        // plus the lattice dogfood, spanning every provisioning kind.
        let provisions = default_provisioning().expect("provisioning parses");
        for server in [
            "rust-analyzer",
            "clangd",
            "lua-language-server",
            "marksman",
            "lattice",
            "jdtls",
            "ruby-lsp",
        ] {
            assert!(provisions.contains_key(server), "provision for `{server}`");
        }
        for kind in [
            ProvisionKind::RustupComponent,
            ProvisionKind::SystemApt,
            ProvisionKind::GithubReleaseSha256,
            ProvisionKind::CargoGitLocked,
            ProvisionKind::TarballSha256,
            ProvisionKind::Gem,
        ] {
            assert!(
                provisions.values().any(|p| p.kind == kind),
                "at least one `{}` provision ships",
                kind.as_str()
            );
        }
    }

    #[test]
    fn lattice_dogfood_is_a_pinned_rev_build() {
        // The contract pin (tui-rework 10 §4): lattice built from the repo at an
        // exact rev with --locked. The pin is fully resolvable from the authoring
        // environment (public repo, rev = the v0.4.0 tag commit), so it is NOT
        // pending.
        let provisions = default_provisioning().expect("provisioning parses");
        let lattice = &provisions["lattice"];
        assert_eq!(lattice.kind, ProvisionKind::CargoGitLocked);
        assert!(!lattice.pending, "the lattice rev is resolved, not pending");
        assert!(
            lattice.git.is_some() && lattice.rev.is_some(),
            "git + rev pinned"
        );
    }

    #[test]
    fn provisioning_pending_is_exactly_the_unresolved_pins() {
        // jdtls's Eclipse-hosted tarball URL + sha256 are not resolvable from
        // GitHub metadata, so it ships pending; ruby-lsp's gem tier carries no
        // sha256 BY DESIGN (not a missing pin), so it is not pending.
        let provisions = default_provisioning().expect("provisioning parses");
        let pending = provisioning_pending(&provisions);
        assert!(pending.contains(&"jdtls".to_owned()), "jdtls is pending");
        assert!(
            !pending.contains(&"ruby-lsp".to_owned()),
            "ruby-lsp is not pending — the gem tier carries no sha256 by design"
        );
    }

    #[test]
    fn conformance_defaults_true_and_omitting_it_is_conformed() {
        // An omitted `conformance` flag defaults to true (the server IS conformed),
        // so a recipe/provision author opts OUT explicitly — never silently.
        let recipe = parse_recipes(
            "[recipe.demo]\necosystem = \"go\"\npackage = \"x\"\n\
             version = \"v1.0.0\"\ntier = \"go-checksumdb\"\n",
        )
        .expect("parses");
        assert!(
            recipe["demo"].conformance,
            "omitted conformance defaults true"
        );
    }

    #[test]
    fn conformed_and_exempt_partition_the_shipped_data() {
        // The shipped exemptions (tui-rework 13): cmake-language-server /
        // typescript-language-server / vscode-eslint-language-server recipes and
        // the marksman provision. Everything else that is not pending is
        // conformed. The two sets are disjoint and neither contains a pending
        // server.
        let recipes = default_recipes().expect("recipes parse");
        let provisions = default_provisioning().expect("provisioning parses");

        let conformed = conformed_server_names(&recipes, &provisions);
        let exempt = conformance_exempt_names(&recipes, &provisions);

        for name in [
            "cmake-language-server",
            "typescript-language-server",
            "vscode-eslint-language-server",
            "marksman",
        ] {
            assert!(
                exempt.iter().any(|e| e == name),
                "`{name}` must be conformance-exempt"
            );
            assert!(
                !conformed.iter().any(|c| c == name),
                "`{name}` must not be in the conformed set"
            );
        }
        // A representative conformed server (ansible-language-server,
        // class-D-fixed) is present.
        assert!(
            conformed.iter().any(|c| c == "ansible-language-server"),
            "ansible-language-server conforms after the class D fixture fix"
        );
        // jdtls is pending, so it is in neither set (the matrix skips it).
        assert!(
            !conformed.iter().any(|c| c == "jdtls") && !exempt.iter().any(|e| e == "jdtls"),
            "a pending server is neither conformed nor exempt"
        );
    }

    #[test]
    fn conformance_false_without_a_note_fails_validation() {
        // An exemption must state honestly why the shipped lifecycle cannot
        // conform the server — a noteless exemption is a validation error.
        let recipes = parse_recipes(
            "[recipe.demo]\necosystem = \"go\"\npackage = \"x\"\n\
             version = \"v1.0.0\"\ntier = \"go-checksumdb\"\nconformance = false\n",
        )
        .expect("parses");
        let errors = validate_recipes(&recipes);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("conformance = false") && e.contains("note")),
            "a noteless conformance exemption must fail validation: {errors:?}"
        );
    }

    #[test]
    fn provisioning_roundtrips_through_toml() {
        let provisions = default_provisioning().expect("provisioning parses");
        let serialized = toml::to_string(&ProvisionWrap {
            provision: &provisions,
        })
        .expect("serialize");
        let reparsed = parse_provisioning(&serialized).expect("reparse");
        assert_eq!(
            provisions, reparsed,
            "provisioning round-trips through TOML"
        );
    }

    #[test]
    fn non_pending_provision_missing_a_required_field_fails_validation() {
        // A github-release-sha256 stanza with no sha256, not marked pending, is a
        // validation error — an unverified download would slip through otherwise.
        let toml = "[provision.demo]\nkind = \"github-release-sha256\"\n\
                    repo = \"o/r\"\nasset = \"a.tar.gz\"\nbin = \"bin/x\"\n";
        let provisions = parse_provisioning(toml).expect("parses");
        let errors = validate_provisioning(&provisions, &BTreeMap::new());
        assert!(
            errors.iter().any(|e| e.contains("sha256")),
            "a missing sha256 must fail validation: {errors:?}"
        );
    }

    #[test]
    fn a_server_in_both_provision_and_recipe_fails_validation() {
        let provisions = parse_provisioning(
            "[provision.taplo]\nkind = \"cargo-git-locked\"\n\
             git = \"https://example/x\"\nrev = \"abc\"\n",
        )
        .expect("parses");
        let recipes = default_recipes().expect("recipes parse"); // taplo is a recipe
        let errors = validate_provisioning(&provisions, &recipes);
        assert!(
            errors.iter().any(|e| e.contains("disjoint")),
            "a server that is both provisioned and a recipe must fail: {errors:?}"
        );
    }

    /// Borrowing serialize helper so the round-trip test can serialize without
    /// cloning the map into an owned [`RecipesDoc`] twice.
    #[derive(Serialize)]
    struct Wrap<'a> {
        recipe: &'a BTreeMap<String, InstallRecipe>,
    }

    /// Borrowing serialize helper for the provisioning round-trip test.
    #[derive(Serialize)]
    struct ProvisionWrap<'a> {
        provision: &'a BTreeMap<String, ProvisionStanza>,
    }
}
