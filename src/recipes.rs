// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! CI-internal install recipes and the blessed-manifest (tui-rework 07).
//!
//! Two disjoint data sets, both parsed here, both keyed by canonical server
//! name (the `[lsp.server.*]` keys in `defaults/servers.toml`):
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
//! - **The blessed-manifest** (`defaults/blessed-manifest.toml`) is the
//!   committed record of servers that have passed the CI conformance gate:
//!   server → conformed version / platform / date. A server earns an entry only
//!   by a green conformance matrix job, reviewed by a human on the PR (blessing
//!   is mechanized, not manual — see the tui-rework 06 design). It is the *only*
//!   recipe-derived data a user surface is ever allowed to consult.
//!
//! Neither data set is consumed by the running daemon; both exist for the
//! conformance harness (`tests/conformance_harness.rs`), the `refresh-recipes`
//! maintenance target, and the conformance CI workflow. This module is the
//! single parse/validate port for both.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Embedded default recipe drafts (`defaults/recipes.toml`).
pub const DEFAULT_RECIPES: &str = include_str!("../defaults/recipes.toml");

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
/// motivating case is a JDK for a JVM-based server (kotlin-ls, metals). The
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
        // for metals/kotlin-ls) the conformance workflow must provision before
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

    /// Borrowing serialize helper so the round-trip test can serialize without
    /// cloning the map into an owned [`RecipesDoc`] twice.
    #[derive(Serialize)]
    struct Wrap<'a> {
        recipe: &'a BTreeMap<String, InstallRecipe>,
    }
}
