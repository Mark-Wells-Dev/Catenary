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

/// Embedded mockls-persona rows (`defaults/mockls-personas.toml`), concatenated
/// onto the seed manifest **only** under `feature = "mockls"` (diagnostics-debt
/// 04c).
///
/// One blessed+discipline row per persona — the publisher-discipline taxonomy
/// made flesh, so the conformance/integration harness's mock stand-ins are
/// diagnostics sources by manifest membership rather than an env lever. The
/// production build never enables the feature, so it carries zero persona rows:
/// no name-spoof surface, nothing shipped, and the seed parse is byte-identical
/// to `DEFAULT_BLESSED_MANIFEST` alone.
#[cfg(feature = "mockls")]
pub const MOCKLS_PERSONAS: &str = include_str!("../defaults/mockls-personas.toml");

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

/// A pinned npm package co-installed **alongside** a recipe's own server.
///
/// The motivating case (misc 195) is a server that needs an npm package
/// resolvable at runtime but does not bundle it — typescript-language-server →
/// the `typescript` package that supplies `tsserver`.
///
/// It is NOT a separate recipe (it keys no `[lsp.server.*]` and is never
/// installed-and-conformed on its own) and NOT a `runtime` toolchain (node is
/// already the npm host); it is an extra pinned npm artifact the conformance
/// install step fetches, verifies, and installs by the SAME
/// `npm-tarball-sha512` mechanics as the server itself — exact `version` plus
/// the registry `dist.integrity` (SRI sha512), installed `--ignore-scripts`. So
/// the gate verifies against a KNOWN co-installed version on both platforms
/// instead of riding whatever the runner image happens to ship. The schema is
/// deliberately generic (any recipe may carry a `co_install` list): the exempt
/// `vscode-eslint-language-server` case, which needs an `eslint` co-install,
/// could later reuse this mechanism (it stays exempt for now — it also needs a
/// settings block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoInstall {
    /// The npm package identifier (e.g. `typescript`).
    pub package: String,
    /// The exact pinned version (never a range).
    pub version: String,
    /// The registry `dist.integrity` SRI sha512 for the pinned version, in the
    /// same `sha512-<base64>` form the recipe `hash` carries. Absent only at
    /// draft stage (filled mechanically by `refresh-recipes`), never fabricated;
    /// [`recipes_missing_hash`] reports a co-install still lacking one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
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
    /// Pinned npm packages co-installed alongside this server because it needs
    /// them resolvable at runtime but does not bundle them (misc 195). Empty for
    /// the common case. TOML serializes a `Vec<struct>` as `[[recipe.<name>.co_install]]`
    /// array-of-tables, which — like [`Self::runtime`] — must follow every scalar
    /// key of the parent table, so it is declared among the trailing composite
    /// fields (an array-of-tables precedes a sub-table, so `co_install` sits
    /// before `runtime`, and any recipe round-trips through `toml::to_string`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub co_install: Vec<CoInstall>,
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
        for co in &recipe.co_install {
            if co.package.trim().is_empty() {
                errors.push(format!("recipe `{name}` co-install has an empty package"));
            }
            if co.version.trim().is_empty() {
                errors.push(format!(
                    "recipe `{name}` co-install `{}` has an empty version",
                    co.package
                ));
            }
        }
    }
    errors
}

/// Names of hash-carrying-tier recipes that have no `hash` yet — including a
/// recipe whose npm co-install is still hashless.
///
/// These are the drafts `refresh-recipes` still needs to resolve a hash for. A
/// co-install always pins by npm SRI sha512 (its `hash`), so a recipe with a
/// hashless co-install is reported here too. Reported (not errored) so the draft
/// set can ship version-pinned but hashless without a fabricated hash.
#[must_use]
pub fn recipes_missing_hash(recipes: &BTreeMap<String, InstallRecipe>) -> Vec<String> {
    recipes
        .iter()
        .filter(|(_, r)| {
            (r.tier.carries_hash() && r.hash.is_none())
                || r.co_install.iter().any(|co| co.hash.is_none())
        })
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

/// The publisher discipline of a blessed server — the bug-82 taxonomy, made data
/// (diagnostics-debt 04 / DESIGN §"Publisher-discipline metadata").
///
/// A server's discipline states what silence and emptiness *mean* on its
/// diagnostics channel, so the ledger knows how to settle a debt. It is a
/// conformance-verified property that rides the manifest per-pin, not a measured
/// guess. A server absent from the manifest (an unverified custom def) has **no**
/// discipline — it is enrichment-only and never a diagnostics source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Discipline {
    /// The server answers `textDocument/diagnostic` pulls (e.g. gopls in pull
    /// mode) — a pull settles the debt directly.
    Pull,
    /// The server publishes on document events, echoing the version (e.g.
    /// rust-analyzer, lattice) — a versioned publish settles; a versioned empty
    /// publish is an authoritative clean.
    Event,
    /// The server publishes on an internal debounce timer bounded by a declared
    /// constant (e.g. ts-ls) — the ledger awaits the version echo, bounded by
    /// [`DisciplineRecord::debounce_ms`], never interpreting silence.
    Debounce,
    /// The server scans once and does not re-publish (marksman-class) — silence
    /// is not clean; a missing answer is a fault, not evidence.
    Scan,
    /// The server publishes only on diff/never (marksman diff-only) — like
    /// [`Self::Scan`], silence is not clean.
    Diff,
}

impl Discipline {
    /// The lowercase token used in the manifest TOML and in tracing.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pull => "pull",
            Self::Event => "event",
            Self::Debounce => "debounce",
            Self::Scan => "scan",
            Self::Diff => "diff",
        }
    }
}

/// One line-strip rule for the message compressor (diagnostics-debt 04 / misc
/// 165's rider / DESIGN §"The manifest").
///
/// The compressor's rules are the declared-constants species — regexes against an
/// output format Catenary does not own — so they ride the manifest per-pin
/// instead of lagging a binary release. The engine that consumes these
/// ([`crate::filter`]) has fixed, narrow semantics: a message *line* whose
/// trimmed text matches this rule is deleted whole; a message left empty after
/// stripping drops entirely; nothing is ever rewritten or injected. A rule that
/// ate verdict text would fail the strip+preserve conformance fixtures before it
/// shipped. The mandate is **compression, never verdict policy**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripRule {
    /// A required literal prefix on the line's trimmed text. A line must start
    /// with this to be a candidate for the rule (the cheap, unambiguous anchor
    /// the hand-coded rules used — e.g. `` `#[ ``, `for further information
    /// visit`).
    pub prefix: String,
    /// Optional substrings that must **all** be present on the line for the rule
    /// to fire. Empty ⇒ the prefix alone is sufficient. Narrows a prefix that
    /// would otherwise be too broad (e.g. anchor on `` `#[ `` but only strip when
    /// the line also says `on by default` / `implied by` / `to override`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains_all: Vec<String>,
    /// Optional substrings of which **any** one present makes the rule fire
    /// (together with the prefix and every `contains_all`). Empty ⇒ no
    /// any-of constraint. Models the `on by default` / `implied by` / `to
    /// override` alternation the hand-coded rustc-attribution rule carried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains_any: Vec<String>,
}

impl StripRule {
    /// Whether `trimmed` (a message line, already trimmed) matches this rule and
    /// should therefore be deleted.
    ///
    /// A line matches when it starts with [`Self::prefix`], contains every
    /// [`Self::contains_all`] substring, and — when [`Self::contains_any`] is
    /// non-empty — contains at least one of those. The semantics are deliberately
    /// narrow (delete whole matching lines; never rewrite) so a manifest rule can
    /// only ever compress boilerplate, never rule on a verdict.
    #[must_use]
    pub fn matches(&self, trimmed: &str) -> bool {
        trimmed.starts_with(&self.prefix)
            && self.contains_all.iter().all(|s| trimmed.contains(s))
            && (self.contains_any.is_empty()
                || self.contains_any.iter().any(|s| trimmed.contains(s)))
    }
}

/// The per-server discipline record — the misc-157 profile table, made manifest
/// data (diagnostics-debt 04 §"Discipline metadata as data").
///
/// One record per blessed server carries what the daemon needs to run the full
/// adapter against it: its [`Discipline`], its declared constants (the debounce
/// window), its capability casing (whether to withhold the pull capability,
/// whether it contractually publishes), and its message-compression
/// [`StripRule`]s. The [`crate::lsp::server_behavior::ServerProfile`] projects
/// from this record at build time — the table is generated from the manifest,
/// not a rival home for the same knowledge.
///
/// Every field defaults, so a discipline row may carry only what a server needs
/// (rust-analyzer withholds pull; gopls forces init options; lattice declares
/// push) and the manifest stays terse. A server with **no** discipline row is
/// unverified — enrichment-only, never a diagnostics source.
///
/// Does not derive `Eq`: [`Self::forced_init_options`] is a [`toml::Value`],
/// which cannot implement `Eq` (it may hold a float). `PartialEq` suffices for
/// the round-trip and projection tests; nothing keys a `DisciplineRecord`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DisciplineRecord {
    /// The server's publisher discipline. `None` ⇒ the record carries only
    /// casing/compression (the common case is a discipline is always stated for a
    /// blessed diagnostics server; `None` tolerates a casing-only row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discipline: Option<Discipline>,
    /// The declared debounce window in milliseconds, for a [`Discipline::Debounce`]
    /// server (ts-ls's declared 300–800 ms + 50 ms). Declared in the pinned
    /// source and re-verified at every re-pin; the ledger awaits the version echo
    /// bounded by this constant rather than interpreting silence. Absent for
    /// non-debounce disciplines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_ms: Option<u64>,
    /// Capability casing: withhold the `textDocument.diagnostic` client capability
    /// and never issue a pull for this server — its native pushes are the sole
    /// diagnostic channel (rust-analyzer, RA #18709). Defaults to `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub suppress_pull: bool,
    /// Capability casing: the server contractually publishes for every opened
    /// document, an explicit `[]` for clean (lattice, misc 187). Arms the
    /// retrieval evidence bar by declaration. Defaults to `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub declares_push: bool,
    /// Evidence class: the server is verified to announce work-done progress
    /// tokens via `window/workDoneProgress/create` **before** it opens the
    /// `$/progress` bracket — the created-token-pending shape (misc 200,
    /// grounded in the conformance-29312867815-elm trace: `create` observed at
    /// 06:57:50.729, pre-download, the `begin` bracket only at 06:57:53.077).
    /// The token-pending settle hold that covers that gap is **identity-blind**
    /// (it fires for any server that creates a token, keyed on nothing), so
    /// this leg records the verified evidence rather than gating behaviour — an
    /// honesty marker in the discipline row, like the verified `declares_push`
    /// leg. Defaults to `false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub declares_progress: bool,
    /// Forced `initializationOptions` overlaid onto — and winning over — the
    /// user's options at initialize time. A raw TOML value serialized as an inline
    /// table / sub-table. Absent ⇒ no forced options. (gopls's `pullDiagnostics:
    /// false` was the motivating case for bug 87, retired in diagnostics-debt 05
    /// when its pull was re-enabled; the mechanism stands for any future forced
    /// lever.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_init_options: Option<toml::Value>,
    /// Top-level `initializationOptions` keys enforced absent — stripped after the
    /// user/forced merge so the server's own default is the only value that can
    /// apply (gopls's `diagnosticsDelay`, run 9). Empty ⇒ none forbidden.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden_init_options: Vec<String>,
    /// The message-compression line-strip rules for this server's diagnostic
    /// output (rust-analyzer's clippy/rustc boilerplate). Empty ⇒ the server's
    /// messages pass through unchanged. Version-scoped via
    /// [`Self::compress_versions`]: a version outside the pinned set is
    /// pass-through, matching the hand-coded rules' version safety.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compress: Vec<StripRule>,
    /// Version prefixes the [`Self::compress`] rules are pinned against (e.g.
    /// `"1."` for rust-analyzer 1.x). A reported server version that starts with
    /// one of these is in-scope for the rules; any other version — or an unknown
    /// version — is pass-through. Empty ⇒ the rules apply to any version (used
    /// only when a rule is version-agnostic).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compress_versions: Vec<String>,
}

impl DisciplineRecord {
    /// Whether this server's reported `version` is in scope for its compression
    /// rules.
    ///
    /// A version is in scope when it starts with one of [`Self::compress_versions`]
    /// (an empty pin list means version-agnostic ⇒ always in scope). An absent
    /// version is out of scope whenever a pin list is present — matching the
    /// hand-coded rules' rule that an unknown version is pass-through.
    #[must_use]
    pub fn compress_applies(&self, version: Option<&str>) -> bool {
        if self.compress_versions.is_empty() {
            return true;
        }
        version.is_some_and(|v| self.compress_versions.iter().any(|p| v.starts_with(p)))
    }
}

/// One blessed server entry: a server that passed the CI conformance gate on one
/// platform.
///
/// Rows are platform-qualified (`[blessed.<server>.<platform>]`; misc 164): the
/// same server list blesses on Linux and macOS, so a server can carry one entry
/// per platform it conformed on. The [`Self::platform`] field is redundant with
/// the row's platform key and kept as the row's self-description (the CI emit
/// jobs write it verbatim).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlessedEntry {
    /// The exact version that conformed.
    pub version: String,
    /// The platform the conformance job ran on (e.g. `linux-x86_64`,
    /// `macos-arm64`) — equal to the row's platform key.
    pub platform: String,
    /// The ISO-8601 date the conformance job passed.
    pub date: String,
    /// The honesty tier of the claim (e.g. `verified-on-linux`,
    /// `verified-on-macos`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
}

/// The manifest schema version this binary understands (diagnostics-debt 04).
///
/// Bumped when a manifest change is **not** backward-compatible with an older
/// binary — a new field an old binary can ignore does not bump it (serde's
/// `#[serde(default)]` already tolerates unknown-to-old fields). A fetched
/// manifest whose [`ManifestMeta::min_schema`] exceeds this constant is refused
/// by [`BlessedManifest::engine_supports`] and the loader degrades to its
/// embedded snapshot: directional safety, an unreadable manifest never makes
/// Catenary more trusting.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Schema/min-engine metadata for the manifest (`[manifest]`; diagnostics-debt
/// 04).
///
/// Lets an old binary meeting a newer manifest degrade to its snapshot rather
/// than misread data it does not understand. Both fields default, so the shipped
/// seed and every existing manifest parse without the table present (an absent
/// `[manifest]` means "schema 0, no engine floor" — always supported).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestMeta {
    /// The minimum [`MANIFEST_SCHEMA_VERSION`] a binary must implement to read
    /// this manifest safely. Absent ⇒ `0` (any binary). A binary whose
    /// [`MANIFEST_SCHEMA_VERSION`] is below this must degrade to its snapshot.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub min_schema: u32,
    /// The minimum engine (Catenary) version this manifest targets, for the
    /// human-readable finding when a degrade fires. Advisory — the machine gate
    /// is [`Self::min_schema`]. Absent ⇒ no floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_engine: Option<String>,
}

/// serde `skip_serializing_if` for a defaults-zero `u32` — omit it from TOML when
/// it holds its default so an absent `[manifest]` table stays absent.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires an fn(&T) -> bool"
)]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// serde `skip_serializing_if` for a default [`ManifestMeta`] — omit the
/// `[manifest]` table when it carries no floor, so an unversioned manifest stays
/// unversioned on round-trip.
fn meta_is_default(meta: &ManifestMeta) -> bool {
    *meta == ManifestMeta::default()
}

/// The committed blessed-manifest: server → platform → conformed record.
///
/// Rows are platform-qualified (`[blessed.<server>.<platform>]`; misc 164), so
/// one server may carry an entry per platform it conformed on (e.g. a
/// `linux-x86_64` and a `macos-arm64` row). The one committed manifest holds both
/// platforms — there is no per-platform manifest file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BlessedManifest {
    /// Schema/min-engine metadata (`[manifest]`). Default (schema 0, no floor)
    /// when the table is absent — every existing manifest and the seed.
    #[serde(default, rename = "manifest", skip_serializing_if = "meta_is_default")]
    pub meta: ManifestMeta,
    /// Blessed entries keyed by canonical server name, then by platform token.
    #[serde(default)]
    pub blessed: BTreeMap<String, BTreeMap<String, BlessedEntry>>,
    /// Per-server publisher-discipline records — the misc-157 profile table made
    /// manifest data (diagnostics-debt 04). Keyed by canonical server name,
    /// platform-agnostic (discipline is a behavior of the server binary, not of
    /// the host it ran on). A server present in [`Self::blessed`] but absent here
    /// carries the default (empty) discipline. Written as `[discipline.<server>]`
    /// tables in the manifest TOML.
    #[serde(default)]
    pub discipline: BTreeMap<String, DisciplineRecord>,
}

impl BlessedManifest {
    /// A blessed entry for `server` at exactly `version`, on any platform, or
    /// `None`.
    ///
    /// Offerability is version-matched, not platform-matched: a recipe pin that
    /// conformed on *any* platform clears the blessing gate (misc 164 — the same
    /// server list blesses on both platforms). When several platform rows share
    /// the version, the first by platform-key order is returned (deterministic;
    /// the entries differ only in `platform`/`date`/`tier`, all honest).
    #[must_use]
    pub fn entry_at_version(&self, server: &str, version: &str) -> Option<&BlessedEntry> {
        self.blessed
            .get(server)?
            .values()
            .find(|entry| entry.version == version)
    }

    /// Whether `server` is **blessed** — present in the manifest's blessed set on
    /// any platform (diagnostics-debt 04 §"Classification").
    ///
    /// Manifest membership is the classifier: a blessed server earns the full
    /// diagnostics adapter; a server absent here is an unverified custom def and
    /// is enrichment-only (no diagnostics advertisement, publishes ignored, no
    /// batch sync lifecycle). Blessing is any-platform, matching
    /// [`Self::entry_at_version`]: a server that conformed on Linux is blessed for
    /// classification even when running on macOS.
    #[must_use]
    pub fn is_blessed(&self, server: &str) -> bool {
        self.blessed
            .get(server)
            .is_some_and(|rows| !rows.is_empty())
    }

    /// The discipline record for `server`, or the default (empty) record when the
    /// server carries no discipline row.
    ///
    /// The single lookup the [`crate::lsp::server_behavior::ServerProfile`]
    /// projection and the message compressor consult. Returning the default for
    /// an absent server keeps the callers server-name-blind — an unknown server
    /// naturally resolves to no casing and no compression, which is the
    /// directional-safety default (noisier, never more trusting).
    #[must_use]
    pub fn discipline_for(&self, server: &str) -> DisciplineRecord {
        self.discipline.get(server).cloned().unwrap_or_default()
    }

    /// Whether this binary understands the manifest's schema
    /// (diagnostics-debt 04).
    ///
    /// True when the manifest's [`ManifestMeta::min_schema`] is at or below the
    /// binary's [`MANIFEST_SCHEMA_VERSION`]. A newer manifest that requires a
    /// schema this binary does not implement returns `false`, and the loader
    /// degrades to its embedded snapshot rather than misreading fields — an old
    /// binary meeting a new manifest is noisier (seed-only), never more trusting.
    #[must_use]
    pub const fn engine_supports(&self) -> bool {
        self.meta.min_schema <= MANIFEST_SCHEMA_VERSION
    }
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
/// Under `feature = "mockls"` the mockls-persona rows
/// ([`MOCKLS_PERSONAS`]) are concatenated onto the committed manifest before the
/// parse, so the conformance/integration harness's mock stand-ins classify as
/// diagnostics sources by manifest membership (diagnostics-debt 04c). Without the
/// feature the parse is byte-identical to [`DEFAULT_BLESSED_MANIFEST`] alone — a
/// production binary carries zero persona rows.
///
/// # Errors
///
/// Returns an error if the embedded `defaults/blessed-manifest.toml` (or, under
/// the feature, the concatenated persona fragment) is malformed.
pub fn default_blessed_manifest() -> Result<BlessedManifest> {
    // Under the feature the persona fragment is concatenated onto the committed
    // manifest before the single parse. The fragment uses only distinct
    // `[blessed.mockls-*.*]` / `[discipline.mockls-*]` tables, so the concat
    // merges into the committed manifest's maps without a key clash; the leading
    // newline guards against the fragment landing on the committed file's last
    // line. Without the feature the source is `DEFAULT_BLESSED_MANIFEST` verbatim
    // — a `Cow` so the no-feature path never allocates and the parse is
    // byte-identical.
    #[cfg(feature = "mockls")]
    let source = std::borrow::Cow::Owned(format!("{DEFAULT_BLESSED_MANIFEST}\n{MOCKLS_PERSONAS}"));
    #[cfg(not(feature = "mockls"))]
    let source = std::borrow::Cow::Borrowed(DEFAULT_BLESSED_MANIFEST);

    parse_blessed_manifest(&source)
}

/// The embedded seed manifest, parsed once and cached — the offline
/// directional-safety **floor** under [`active_manifest`] (diagnostics-debt 04).
///
/// Always available offline: a server absent from the seed carries no casing and
/// no compression, which only ever makes Catenary noisier. A malformed embedded
/// manifest (its own tests forbid it) degrades to an empty manifest — every
/// server then classifies enrichment-only, never more trusting. This is the value
/// [`active_manifest`] is seeded with before any registry refresh threads a
/// fetched manifest in.
#[must_use]
pub fn seed_manifest() -> &'static BlessedManifest {
    static SEED: std::sync::OnceLock<BlessedManifest> = std::sync::OnceLock::new();
    SEED.get_or_init(|| default_blessed_manifest().unwrap_or_default())
}

/// The process-wide **active** blessed manifest (diagnostics-debt 04b).
///
/// The projection source for [`crate::lsp::server_behavior::ServerProfile`], the
/// [`crate::filter`] compressor, and the blessed/unverified classification.
///
/// The synchronous LSP construction seams (`LspServer::new`, `params::initialize`,
/// `compress_message`) are server-name-blind and cannot thread the resolved
/// registry through their signatures, so they consult this holder. It is seeded
/// with the embedded [`seed_manifest`] — the offline floor — and upgraded in
/// place by [`install_active_manifest`] when the daemon's registry refresh
/// resolves a fetched or cached manifest (the registry chain's output). A re-pin
/// therefore ships updated discipline/casing/compression **without a binary
/// release**: the projection shape is unchanged; only the source upgrades.
///
/// **Directional safety, preserved:** the registry loader already degrades a
/// fetch failure, a bad signature, or an unreadable schema down to the seed
/// (`RegistrySource::Seed`), so an install only ever replaces the floor with an
/// equal-or-more-verified manifest — fetched-absent stays seed-only (noisier),
/// never more trusting.
#[must_use]
pub fn active_manifest() -> std::sync::Arc<BlessedManifest> {
    std::sync::Arc::clone(
        &active_slot()
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Installs a resolved manifest as the process-wide [`active_manifest`].
///
/// Called by the daemon's registry-refresh task after a resolution, so live
/// daemons pick up a re-pin's discipline without a binary release. Overwrites the
/// current active manifest wholesale — the registry chain has already resolved
/// the best rung available (verified → cache → seed) and degraded a failed fetch
/// to the seed, so the installed value is never *less* verified than the floor
/// (directional safety).
pub fn install_active_manifest(manifest: std::sync::Arc<BlessedManifest>) {
    *active_slot()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = manifest;
}

/// The swappable slot backing [`active_manifest`], seeded with the offline floor.
fn active_slot() -> &'static std::sync::RwLock<std::sync::Arc<BlessedManifest>> {
    static SLOT: std::sync::OnceLock<std::sync::RwLock<std::sync::Arc<BlessedManifest>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::RwLock::new(std::sync::Arc::new(seed_manifest().clone())))
}

/// Whether `server_name` is **blessed** — a diagnostics source — per the active
/// manifest (diagnostics-debt 04, 04c).
///
/// The single classification predicate every seam consults
/// ([`crate::lsp::server_behavior::ServerProfile::for_server`], the manager's
/// coverage gate, the doctor disclosure), so manifest membership stays the one
/// classifier. A server absent from the manifest is unverified — enrichment-only,
/// never a diagnostics source.
///
/// Blessing is purely manifest membership now: the operator bless-list env lever
/// and the `cfg(test)` mock-prefix rule (both diagnostics-debt 04b) retired in
/// 04c. Under `feature = "mockls"` the seed manifest carries the mockls-persona
/// rows (see [`MOCKLS_PERSONAS`]), so a harness mock spawned under a persona
/// server key is a diagnostics source by membership, and a mock spawned under any
/// other name classifies enrichment-only — the strictness the classification
/// tests pin.
#[must_use]
pub fn is_server_blessed(server_name: &str) -> bool {
    active_manifest().is_blessed(server_name)
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
        // the type, asserted here for the honesty tier. Rows are
        // platform-qualified (server → platform → entry; misc 164), and each
        // entry's `platform` field equals its row's platform key.
        for (name, per_platform) in &manifest.blessed {
            for (platform, entry) in per_platform {
                assert!(!entry.version.is_empty(), "`{name}` [{platform}] version");
                assert!(!entry.platform.is_empty(), "`{name}` [{platform}] platform");
                assert!(!entry.date.is_empty(), "`{name}` [{platform}] date");
                assert_eq!(
                    &entry.platform, platform,
                    "`{name}` row key must equal its `platform` field"
                );
            }
        }
    }

    // ── discipline metadata as manifest data (diagnostics-debt 04) ───────

    #[test]
    fn discipline_table_parses_and_roundtrips() {
        // The `[discipline.<server>]` tables (discipline, declared constants,
        // capability casing, compression rules) parse from the shipped manifest
        // and survive a TOML round-trip byte-for-byte.
        let manifest = default_blessed_manifest().expect("manifest parses");
        assert!(
            !manifest.discipline.is_empty(),
            "the shipped manifest carries discipline rows"
        );
        let serialized = toml::to_string(&manifest).expect("serialize");
        let reparsed = parse_blessed_manifest(&serialized).expect("reparse");
        assert_eq!(
            manifest.discipline, reparsed.discipline,
            "the discipline table round-trips through TOML"
        );
    }

    #[test]
    fn every_discipline_row_is_a_blessed_server() {
        // A discipline row grants a server the full diagnostics adapter. A row for
        // a non-blessed server would be an unverified server masquerading as a
        // diagnostics source — the classification the design forbids.
        let manifest = default_blessed_manifest().expect("manifest parses");
        for name in manifest.discipline.keys() {
            assert!(
                manifest.is_blessed(name),
                "discipline row `{name}` names a server that is not in the blessed set"
            );
        }
    }

    #[test]
    fn is_blessed_classifies_membership() {
        // Manifest membership is the classifier: a blessed server (rust-analyzer)
        // is blessed; a name absent from the blessed set (a custom def) is not.
        let manifest = default_blessed_manifest().expect("manifest parses");
        assert!(
            manifest.is_blessed("rust-analyzer"),
            "rust-analyzer is blessed"
        );
        assert!(manifest.is_blessed("lattice"), "lattice is blessed");
        assert!(
            !manifest.is_blessed("some-custom-server"),
            "a custom def is not blessed"
        );
        // The default (empty) manifest blesses nothing — directional safety.
        assert!(!BlessedManifest::default().is_blessed("rust-analyzer"));
    }

    #[test]
    fn discipline_projects_the_cased_servers() {
        // The three servers the misc-157 profile table cased are now manifest
        // data, and `discipline_for` projects exactly those settings.
        let manifest = default_blessed_manifest().expect("manifest parses");

        // rust-analyzer: withholds the pull capability, event discipline.
        let ra = manifest.discipline_for("rust-analyzer");
        assert!(ra.suppress_pull, "rust-analyzer suppresses pull");
        assert!(!ra.declares_push);
        assert_eq!(ra.discipline, Some(Discipline::Event));
        assert!(!ra.compress.is_empty(), "rust-analyzer carries strip rules");
        assert_eq!(ra.compress_versions, vec!["1.".to_owned()]);

        // gopls: PULL discipline (diagnostics-debt 05 re-enabled pull — bug 87's
        // `pullDiagnostics = false` override is retired). Still forbids
        // diagnosticsDelay (run 9), but forces no options.
        let gopls = manifest.discipline_for("gopls");
        assert!(!gopls.suppress_pull);
        assert_eq!(gopls.discipline, Some(Discipline::Pull));
        assert_eq!(
            gopls.forbidden_init_options,
            vec!["diagnosticsDelay".to_owned()]
        );
        assert!(
            gopls.forced_init_options.is_none(),
            "gopls no longer forces pullDiagnostics off (bug 87 re-enabled)",
        );

        // lattice: declares push.
        let lattice = manifest.discipline_for("lattice");
        assert!(lattice.declares_push, "lattice declares push");
        assert!(
            !lattice.declares_progress,
            "declares_progress defaults off — only elm's verified leg sets it"
        );

        // elm-language-server: the misc-200 verified `declares_progress` leg —
        // the conformance-29312867815-elm trace observed
        // `window/workDoneProgress/create` pre-download. It is an evidence class
        // (identity-blind hold), not a behaviour gate, so the discipline stays
        // bare `event`.
        let elm = manifest.discipline_for("elm-language-server");
        assert_eq!(elm.discipline, Some(Discipline::Event));
        assert!(
            elm.declares_progress,
            "elm declares the created-token-pending progress shape (misc 200)"
        );
        assert!(
            !elm.declares_push,
            "elm's stronger push contract stays unverified/unarmed (misc 196)"
        );

        // An unruled/unverified server projects the empty record.
        let unknown = manifest.discipline_for("some-custom-server");
        assert_eq!(unknown, DisciplineRecord::default());
    }

    #[test]
    fn mockls_personas_incarnate_the_discipline_taxonomy() {
        // Under `feature = "mockls"` (which every `make check`/`make test` run
        // enables) the seed manifest carries one blessed persona per discipline
        // — the taxonomy made flesh (diagnostics-debt 04c). Each persona is
        // blessed AND carries the discipline row its name advertises, so a
        // harness mock spawned under the key is a diagnostics source by
        // membership, no env lever.
        let manifest = default_blessed_manifest().expect("manifest parses");

        for name in [
            "mockls-pull",
            "mockls-event",
            "mockls-declared",
            "mockls-debounce",
            "mockls-scan",
            "mockls-diff",
            "mockls-violator",
            "mockls-pending",
        ] {
            assert!(manifest.is_blessed(name), "`{name}` must be blessed");
        }

        assert_eq!(
            manifest.discipline_for("mockls-pull").discipline,
            Some(Discipline::Pull)
        );
        assert_eq!(
            manifest.discipline_for("mockls-event").discipline,
            Some(Discipline::Event)
        );

        // The declared-push persona (the lattice shape): event discipline PLUS
        // the contractual publish declaration.
        let declared = manifest.discipline_for("mockls-declared");
        assert_eq!(declared.discipline, Some(Discipline::Event));
        assert!(declared.declares_push, "`mockls-declared` declares push");

        // The debounce persona carries its declared constant (ledger 05's gate).
        let debounce = manifest.discipline_for("mockls-debounce");
        assert_eq!(debounce.discipline, Some(Discipline::Debounce));
        assert_eq!(debounce.debounce_ms, Some(300));

        assert_eq!(
            manifest.discipline_for("mockls-scan").discipline,
            Some(Discipline::Scan)
        );
        assert_eq!(
            manifest.discipline_for("mockls-diff").discipline,
            Some(Discipline::Diff)
        );

        // The violating twin DECLARES the push contract (its manifest row) — the
        // binary is what breaks it (`persona_bundle` withholds), so the row here
        // mirrors `mockls-declared` and the conformance leg proves the violation.
        let violator = manifest.discipline_for("mockls-violator");
        assert!(
            violator.declares_push,
            "`mockls-violator` DECLARES push — the binary breaks the contract"
        );

        // The created-token-pending persona (the elm shape, misc 200): event
        // discipline PLUS the verified `declares_progress` leg, mirroring elm's
        // real row. The binary announces its token at init and holds settle
        // across the gap.
        let pending = manifest.discipline_for("mockls-pending");
        assert_eq!(pending.discipline, Some(Discipline::Event));
        assert!(
            pending.declares_progress,
            "`mockls-pending` declares the created-token-pending progress shape"
        );
    }

    #[test]
    fn blessed_servers_all_rowed() {
        // The misc-196 invariant, structural: EVERY blessed server carries a
        // `[discipline.<server>]` row. A blessed-but-rowless server used to get the
        // most-trusting treatment there is — its silence read `[clean]` through the
        // misc-153 residual — inverting the manifest's directional-safety doctrine
        // ("missing data only ever makes Catenary noisier, never more trusting").
        // The ruling retires that residual by making the shipped data structurally
        // incapable of holding a rowless blessed server: this test fails CI the
        // moment a future blessing lands without a row.
        //
        // The parse is `default_blessed_manifest()`, so this covers BOTH the
        // committed seed AND — under `feature = "mockls"` (the test build) — the
        // concatenated persona fragment (`defaults/mockls-personas.toml`): every
        // `[blessed.mockls-*.*]` persona must carry its `[discipline.mockls-*]` row
        // too, so a future persona cannot ship blessed-but-rowless either.
        let manifest = default_blessed_manifest().expect("manifest parses");
        let rowless: Vec<&String> = manifest
            .blessed
            .keys()
            .filter(|server| !manifest.discipline.contains_key(*server))
            .collect();
        assert!(
            rowless.is_empty(),
            "every blessed server must carry a `[discipline.<server>]` row \
             (misc 196 — blessed ⊆ rowed); rowless: {rowless:?}",
        );
    }

    #[test]
    fn removing_a_blessed_row_breaks_the_invariant() {
        // The invariant's teeth, demonstrated: a manifest with a blessed server but
        // NO discipline row for it is exactly the rowless residual misc 196 retires,
        // so the `blessed ⊆ rowed` check must catch it. This is the "demonstrably
        // red if a row is removed" leg — it proves the guard fails on the condition
        // it exists to forbid, not merely passes on the shipped data.
        let mut manifest = default_blessed_manifest().expect("manifest parses");
        // Drop rust-analyzer's discipline row while leaving it blessed.
        assert!(
            manifest.discipline.remove("rust-analyzer").is_some(),
            "rust-analyzer must have had a row to remove",
        );
        assert!(
            manifest.is_blessed("rust-analyzer"),
            "rust-analyzer stays blessed after only its discipline row is removed",
        );
        let rowless: Vec<&String> = manifest
            .blessed
            .keys()
            .filter(|server| !manifest.discipline.contains_key(*server))
            .collect();
        assert!(
            rowless.iter().any(|s| s.as_str() == "rust-analyzer"),
            "a blessed server with its row removed must trip the blessed ⊆ rowed \
             check — the invariant is demonstrably red when a row is missing",
        );
    }

    #[test]
    fn engine_supports_gates_on_min_schema() {
        // The shipped seed carries no `[manifest]` floor ⇒ schema 0 ⇒ always
        // supported. A manifest requiring a schema this binary implements is
        // supported; one requiring a newer schema is refused (degrade-to-snapshot).
        let seed = default_blessed_manifest().expect("manifest parses");
        assert_eq!(seed.meta, ManifestMeta::default(), "seed carries no floor");
        assert!(
            seed.engine_supports(),
            "the shipped seed is always readable"
        );

        let at_floor = BlessedManifest {
            meta: ManifestMeta {
                min_schema: MANIFEST_SCHEMA_VERSION,
                min_engine: None,
            },
            ..BlessedManifest::default()
        };
        assert!(
            at_floor.engine_supports(),
            "a manifest at the floor is readable"
        );

        let too_new = BlessedManifest {
            meta: ManifestMeta {
                min_schema: MANIFEST_SCHEMA_VERSION + 1,
                min_engine: Some("99.0.0".to_owned()),
            },
            ..BlessedManifest::default()
        };
        assert!(
            !too_new.engine_supports(),
            "a manifest requiring a newer schema must be refused"
        );
    }

    #[test]
    fn manifest_meta_roundtrips_through_toml() {
        // The `[manifest]` table parses and round-trips; an absent table stays
        // absent (schema 0).
        let toml = "[manifest]\nmin_schema = 1\nmin_engine = \"2.1.0\"\n\
                    [blessed.taplo.linux-x86_64]\nversion = \"0.10.0\"\n\
                    platform = \"linux-x86_64\"\ndate = \"2026-07-07\"\n";
        let manifest = parse_blessed_manifest(toml).expect("parses");
        assert_eq!(manifest.meta.min_schema, 1);
        assert_eq!(manifest.meta.min_engine.as_deref(), Some("2.1.0"));
        let serialized = toml::to_string(&manifest).expect("serialize");
        let reparsed = parse_blessed_manifest(&serialized).expect("reparse");
        assert_eq!(manifest.meta, reparsed.meta, "the meta table round-trips");
    }

    #[test]
    fn compress_rules_strip_boilerplate_and_preserve_verdicts() {
        // Strip AND preserve against the PIN (misc 165's rider): the manifest's
        // real rust-analyzer rules compress boilerplate to shape, and a clean
        // multi-line verdict survives byte-exact. A rule that ate verdict text
        // would fail here before it shipped.
        let manifest = default_blessed_manifest().expect("manifest parses");
        let ra = manifest.discipline_for("rust-analyzer");

        // A helper mirroring the engine's whole-line-delete semantics.
        let strip = |message: &str| -> String {
            message
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    !ra.compress.iter().any(|rule| rule.matches(trimmed))
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim_end()
                .to_string()
        };

        // STRIP: a real clippy warning loses its URL and attribution lines.
        let clippy = "unused variable: `x`\n\
                      for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#unused_variables\n\
                      `#[warn(unused_variables)]` on by default";
        assert_eq!(strip(clippy), "unused variable: `x`");

        // PRESERVE: a clean multi-line rustc verdict survives byte-exact — no
        // line here is boilerplate, so the compressor is the identity.
        let verdict = "mismatched types\n\
                       expected struct `String`, found `&str`\n\
                       note: expected because of the `let` binding";
        assert_eq!(strip(verdict), verdict, "a verdict body is never eaten");

        // PRESERVE: the `compress_versions` pin gates the rules — an unknown
        // version is pass-through even for boilerplate (version safety).
        assert!(!ra.compress_applies(Some("2.0.0")));
        assert!(!ra.compress_applies(None));
        assert!(ra.compress_applies(Some("1.95.0")));
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
    fn recipe_parses_and_roundtrips_a_co_install() {
        // The `co_install` field (misc 195) pins an extra npm artifact the server
        // needs at runtime but does not bundle. It parses from the inline
        // array-of-tables form and round-trips as `[[recipe.<name>.co_install]]`,
        // and — with a `runtime` also set — the array-of-tables serializes BEFORE
        // the `runtime` sub-table (both trailing composites), so the whole recipe
        // still round-trips through `toml::to_string`.
        let toml = "[recipe.demo]\necosystem = \"npm\"\npackage = \"x\"\n\
                    version = \"1.0.0\"\ntier = \"npm-tarball-sha512\"\ndraft = true\n\
                    co_install = [{ package = \"typescript\", version = \"6.0.3\", \
                    hash = \"sha512-abc==\" }]\n\
                    runtime = { name = \"jdk\", version = \">=17\" }\n";
        let recipes = parse_recipes(toml).expect("parses a co-install");
        let co = &recipes["demo"].co_install;
        assert_eq!(co.len(), 1, "one co-install");
        assert_eq!(co[0].package, "typescript");
        assert_eq!(co[0].version, "6.0.3");
        assert_eq!(co[0].hash.as_deref(), Some("sha512-abc=="));

        let serialized = toml::to_string(&Wrap { recipe: &recipes }).expect("serialize");
        let reparsed = parse_recipes(&serialized).expect("reparse");
        assert_eq!(recipes, reparsed, "a co-install-bearing recipe round-trips");
    }

    #[test]
    fn co_install_with_empty_package_or_version_fails_validation() {
        // A co-install pins exactly like a recipe: an empty package or version is a
        // validation error, caught structurally rather than at install time.
        let toml = "[recipe.demo]\necosystem = \"npm\"\npackage = \"x\"\n\
                    version = \"1.0.0\"\ntier = \"npm-tarball-sha512\"\ndraft = true\n\
                    co_install = [{ package = \"\", version = \"\" }]\n";
        let recipes = parse_recipes(toml).expect("parses");
        let errors = validate_recipes(&recipes);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("co-install") && e.contains("empty package")),
            "an empty co-install package must fail validation: {errors:?}"
        );
    }

    #[test]
    fn a_hashless_co_install_is_reported_missing_hash() {
        // A co-install pins by npm SRI sha512, so a hashless one is a draft the
        // refresh tooling still owes a hash — reported (never fabricated).
        let toml = "[recipe.demo]\necosystem = \"npm\"\npackage = \"x\"\n\
                    version = \"1.0.0\"\ntier = \"npm-tarball-sha512\"\n\
                    hash = \"sha512-present==\"\ndraft = true\n\
                    co_install = [{ package = \"typescript\", version = \"6.0.3\" }]\n";
        let recipes = parse_recipes(toml).expect("parses");
        assert!(
            recipes_missing_hash(&recipes).contains(&"demo".to_owned()),
            "a recipe whose co-install lacks a hash is reported missing a hash"
        );
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
        // A co-install always pins by npm SRI sha512 (regardless of the parent
        // recipe's tier), so any present co-install hash must be well-formed too.
        for (name, recipe) in &recipes {
            for co in &recipe.co_install {
                if let Some(hash) = co.hash.as_deref() {
                    assert!(
                        hash.starts_with("sha512-") && hash.len() > "sha512-".len(),
                        "co-install `{}` of recipe `{name}` hash must be SRI sha512: {hash}",
                        co.package
                    );
                }
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
        // The shipped exemptions: cmake-language-server /
        // vscode-eslint-language-server recipes and the marksman provision
        // (tui-rework 13). typescript-language-server was in this set until
        // diagnostics-debt 05 un-exempted it behind the declared-constant gate.
        // Everything else that is not pending is conformed. The two sets are
        // disjoint and neither contains a pending server.
        let recipes = default_recipes().expect("recipes parse");
        let provisions = default_provisioning().expect("provisioning parses");

        let conformed = conformed_server_names(&recipes, &provisions);
        let exempt = conformance_exempt_names(&recipes, &provisions);

        for name in [
            "cmake-language-server",
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
        // typescript-language-server is now CONFORMED (diagnostics-debt 05
        // un-exempted it behind the declared-constant gate), not exempt.
        assert!(
            conformed.iter().any(|c| c == "typescript-language-server"),
            "typescript-language-server conforms after the ledger-05 un-exemption"
        );
        assert!(
            !exempt.iter().any(|e| e == "typescript-language-server"),
            "typescript-language-server is no longer conformance-exempt"
        );
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
