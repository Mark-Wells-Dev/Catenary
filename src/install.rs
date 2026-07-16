// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The guided-install engine (tui-rework 06): a suggestion's fix-it becomes a
//! consented, verified install.
//!
//! Given a pinned [`InstallRecipe`] that has cleared the **blessing gate**, this
//! module produces and executes the per-ecosystem install — into the **managed
//! server home** ([`ManagedHome`], ls-manager 01):
//! `<data_dir>/catenary/servers/<name>/<version>/…`, a Catenary-owned prefix
//! that a `pacman -Syu` or `npm update -g` can never rewrite out from under the
//! version-pinned warranty. Every version dir is self-contained and exposes
//! executables at `bin/` (the ruled invariant). Recipes stay declarative — they
//! never name destinations; the containment below is keyed off the recipe's
//! ecosystem field alone:
//!
//! - **npm** — fetch the pinned registry tarballs — the server *and* every
//!   pinned `co_install` companion — verify each sha512 against the recipe
//!   (FAIL on any mismatch — never install an unverified artifact, never
//!   install half the set), then `npm install -g --prefix <version-dir>` the
//!   *verified* files in one invocation with `--ignore-scripts` (install
//!   scripts never run). Executables land in `bin/`, packages side by side in
//!   `lib/node_modules/`, where Node's require walk resolves a companion from
//!   inside the same version dir — the pinned pair travels as one unit and
//!   cannot drift independently.
//! - **cargo** — `cargo install <pkg> --version =<v> --locked --root
//!   <version-dir>` (cargo verifies the registry index checksums and lands
//!   binaries in `<root>/bin` — conforms natively).
//! - **pip** — a venv per server version (`python3 -m venv <version-dir>`),
//!   then the venv's own pip runs the `--require-hashes` form when the recipe
//!   carries a digest; refuses politely when it does not (the recipe explains
//!   why). Entry-point scripts land in the venv's `bin/`, which *is* the
//!   version dir's `bin/` — `pip --target` can't generate entry-point scripts,
//!   so the venv is the containment vehicle. **Known residual:** the venv
//!   couples to the system python it was created from — a python minor bump can
//!   break it; doctor detects the broken server, a recipe reinstall (recreating
//!   the venv) cures.
//! - **go** — `GOBIN=<version-dir>/bin go install <pkg>@v<v>` (the Go checksum
//!   DB verifies transparently; `GOBIN` drops the binary in `bin/` — conforms
//!   natively).
//!
//! Every command is spawned as **argv** ([`InstallCommand`]) — never a shell
//! string — and there is no `curl | bash` anywhere. Execution runs through two
//! injectable seams, the [`CommandRunner`] (spawns argv, captures output) and the
//! [`TarballFetcher`] (fetches the npm tarballs), so the engine's logic is tested
//! against fixtures without touching the network or installing anything global.
//!
//! **Blessing is structural, not a runtime filter.** The install action takes a
//! [`BlessedRecipe`], whose only constructor ([`BlessedRecipe::resolve`]) demands
//! a `blessed-manifest` entry whose version matches the recipe's pin. An
//! unblessed recipe cannot be turned into a value of that type, so an unblessed
//! install is unreachable at the type level rather than skipped by a check. The
//! shipped manifest has zero entries, so in production nothing is offerable yet —
//! blessing arrives via the CI conformance matrix (tui-rework 07/08).
//!
//! This is a **user-surface engine**: it is driven from the TUI. No agent-facing
//! verb reaches it — there is no CLI subcommand and no command-filter recognition.

#![allow(
    clippy::module_name_repetitions,
    reason = "InstallCommand/InstallPlan/InstallOutcome read clearest with the Install prefix despite the module being `install`"
)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::managed_home::ManagedHome;
use crate::recipes::{BlessedEntry, BlessedManifest, Ecosystem, InstallRecipe, VerificationTier};

/// The npm registry the tarball URL is derived against.
const NPM_REGISTRY: &str = "https://registry.npmjs.org";

/// A ceiling on a fetched tarball so a hostile or misdirected response cannot
/// exhaust memory (128 MiB — orders of magnitude above any language-server
/// package).
const MAX_TARBALL_BYTES: u64 = 128 * 1024 * 1024;

// ── The blessing gate (structural) ───────────────────────────────────

/// A recipe that has cleared the blessing gate.
///
/// The **only** way to obtain one is [`BlessedRecipe::resolve`], which requires a
/// `blessed-manifest` entry for the server whose version equals the recipe's
/// pin. A value of this type is therefore proof that the server is blessed *at
/// the recipe's exact version* — an unblessed or version-skewed recipe simply
/// cannot be represented. The guided-install action consumes a `BlessedRecipe`,
/// so an unblessed install is impossible to construct, not merely filtered.
#[derive(Debug, Clone)]
pub struct BlessedRecipe {
    /// Canonical server name (the `[lsp.server.*]` key).
    server: String,
    /// The pinned install recipe for the server.
    recipe: InstallRecipe,
    /// The matching blessed-manifest entry that unlocked it.
    blessed: BlessedEntry,
}

impl BlessedRecipe {
    /// Resolve a blessed recipe for `server`, or `None` when the blessing gate is
    /// not satisfied.
    ///
    /// Returns `Some` only when `manifest` carries a `[blessed.<server>.<platform>]`
    /// entry — on ANY platform — whose `version` equals `recipe.version`. Any other
    /// case — no entry, or every entry pinning a different version than the recipe
    /// — yields `None`, so the action is never offered for an unblessed or drifted
    /// recipe. Offerability is version-matched, not platform-matched (misc 164): a
    /// pin that conformed on any platform clears the gate.
    #[must_use]
    pub fn resolve(
        server: &str,
        recipe: &InstallRecipe,
        manifest: &BlessedManifest,
    ) -> Option<Self> {
        let blessed = manifest.entry_at_version(server, &recipe.version)?;
        Some(Self {
            server: server.to_owned(),
            recipe: recipe.clone(),
            blessed: blessed.clone(),
        })
    }

    /// The canonical server name.
    #[must_use]
    pub fn server(&self) -> &str {
        &self.server
    }

    /// The pinned install recipe.
    #[must_use]
    pub const fn recipe(&self) -> &InstallRecipe {
        &self.recipe
    }

    /// The matching blessed-manifest entry.
    #[must_use]
    pub const fn blessed(&self) -> &BlessedEntry {
        &self.blessed
    }

    /// The pinned, ecosystem-honest fix-it text (a copy-paste command).
    ///
    /// Catenary never prints an *unpinned* install command; where a blessed
    /// recipe exists the suggestion may show the exact pinned form. The copy-paste
    /// form carries only what a CLI accepts (pip/cargo/go express their pins; npm
    /// carries the version pin) — full sha512 verification is the guided engine's
    /// job, stated in the trailing note.
    #[must_use]
    pub fn pinned_fix_it(&self) -> String {
        let r = &self.recipe;
        let command = match r.ecosystem {
            Ecosystem::Npm => format!("npm install -g {}@{}", r.package, r.version),
            Ecosystem::Cargo => {
                format!(
                    "cargo install {} --version ={} --locked",
                    r.package, r.version
                )
            }
            Ecosystem::Pip => format!("pip install {}=={}", r.package, r.version),
            Ecosystem::Go => format!("go install {}@{}", r.package, go_version(&r.version)),
        };
        let note = match r.ecosystem {
            Ecosystem::Npm => {
                " — press `a` for the guided install (fetches + verifies the tarball sha512, --ignore-scripts)."
            }
            Ecosystem::Pip => " — press `a` for the guided --require-hashes install.",
            Ecosystem::Cargo | Ecosystem::Go => " — press `a` to run the guided, verified install.",
        };
        format!("Pinned: {command}{note}")
    }
}

// ── argv commands (never a shell string) ─────────────────────────────

/// A command spawned directly as an argv vector — never a shell string, so no
/// argument is ever interpreted by a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallCommand {
    /// The program to execute (resolved on `$PATH` by the OS).
    program: String,
    /// The literal argument vector.
    args: Vec<String>,
    /// Environment variables set for this command alone (the go containment's
    /// `GOBIN`). Derived by the managed home, never recipe data.
    envs: Vec<(String, String)>,
}

impl InstallCommand {
    /// Build a command from a program and its argument vector.
    fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            envs: Vec::new(),
        }
    }

    /// Add an environment variable to the command's own environment.
    fn env(mut self, key: &str, value: &str) -> Self {
        self.envs.push((key.to_owned(), value.to_owned()));
        self
    }

    /// The program name.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The argument vector.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The per-command environment variables.
    #[must_use]
    pub fn envs(&self) -> &[(String, String)] {
        &self.envs
    }

    /// The command rendered as a single display line (for the consent overlay
    /// only — it is never executed as a string). Per-command env vars render as
    /// a `KEY=value` prefix, the way a user would state them.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::new();
        for (key, value) in &self.envs {
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push(' ');
        }
        out.push_str(&self.program);
        for a in &self.args {
            out.push(' ');
            out.push_str(a);
        }
        out
    }
}

/// `cargo install <package> --version =<version> --locked --root <version-dir>`
/// — `--root` lands the binaries in `<version-dir>/bin`, conforming to the
/// managed-home invariant natively.
fn cargo_install_command(package: &str, version: &str, version_dir: &Path) -> InstallCommand {
    let version_arg = format!("={version}");
    let root = version_dir.to_string_lossy();
    InstallCommand::new(
        "cargo",
        &[
            "install",
            package,
            "--version",
            &version_arg,
            "--locked",
            "--root",
            &root,
        ],
    )
}

/// `GOBIN=<version-dir>/bin go install <package>@v<version>` (the version's `v`
/// prefix is normalized) — `GOBIN` drops the built binary in `bin/`, conforming
/// to the managed-home invariant natively; go's build cache stays go's own.
fn go_install_command(package: &str, version: &str, version_dir: &Path) -> InstallCommand {
    let module = format!("{package}@{}", go_version(version));
    let gobin = version_dir.join("bin");
    InstallCommand::new("go", &["install", &module]).env("GOBIN", &gobin.to_string_lossy())
}

/// `npm install -g --prefix <version-dir> <artifacts…> --ignore-scripts` —
/// `artifacts` are paths to already-fetched-and-verified local tarballs (the
/// server plus its pinned co-install companions), so install scripts never run
/// and no unverified bytes are installed. One invocation installs the whole
/// pinned set: with `--prefix`, executables land in `<version-dir>/bin` and
/// packages side by side in `<version-dir>/lib/node_modules/`, where Node's
/// require walk resolves a companion from inside the same version dir.
fn npm_install_command(version_dir: &Path, artifacts: &[&str]) -> InstallCommand {
    let prefix = version_dir.to_string_lossy();
    let mut args = vec!["install", "-g", "--prefix", prefix.as_ref()];
    args.extend_from_slice(artifacts);
    args.push("--ignore-scripts");
    InstallCommand::new("npm", &args)
}

/// `python3 -m venv <version-dir>` — the pip containment vehicle.
///
/// pip has no npm-`--prefix` equivalent that keeps a server runnable:
/// `--target` installs libraries but generates no entry-point scripts. So each
/// pip server gets a venv **at** the version dir — the venv's `bin/` *is* the
/// invariant's `bin/`, entry points included.
///
/// **Known residual** (stated, not hidden): the venv couples to the system
/// python it was created from — a python minor bump can break it. doctor
/// detects the broken server; a recipe reinstall (recreating the venv) cures.
fn venv_create_command(version_dir: &Path) -> InstallCommand {
    InstallCommand::new("python3", &["-m", "venv", &version_dir.to_string_lossy()])
}

/// `<version-dir>/bin/pip install --require-hashes --no-deps -r <requirements>`
/// — the hash-checked install, run through the venv's **own** pip so every
/// artifact lands inside the version dir. `--no-deps` keeps `--require-hashes`
/// self-consistent: the recipe pins one artifact's sha256, so the pinned
/// package is the whole (hashed) set.
fn pip_install_command(version_dir: &Path, requirements_path: &str) -> InstallCommand {
    let pip = version_dir.join("bin").join("pip");
    InstallCommand::new(
        &pip.to_string_lossy(),
        &[
            "install",
            "--require-hashes",
            "--no-deps",
            "-r",
            requirements_path,
        ],
    )
}

/// A `--require-hashes` requirements-file line: `<package>==<version> --hash=sha256:<hex>`.
fn pip_requirement_line(package: &str, version: &str, hash: &str) -> String {
    let digest = hash.strip_prefix("sha256:").unwrap_or(hash);
    format!("{package}=={version} --hash=sha256:{digest}")
}

/// The npm registry tarball URL for a pinned package version.
///
/// Handles scoped packages: `@scope/name` publishes its tarball under the
/// *unscoped* file name (`.../@scope/name/-/name-<version>.tgz`).
fn npm_tarball_url(package: &str, version: &str) -> String {
    let file_name = package.rsplit('/').next().unwrap_or(package);
    format!("{NPM_REGISTRY}/{package}/-/{file_name}-{version}.tgz")
}

/// Normalize a go module version to its `vX.Y.Z` form (recipes may store it with
/// or without the leading `v`).
fn go_version(version: &str) -> String {
    if version.starts_with('v') {
        version.to_owned()
    } else {
        format!("v{version}")
    }
}

// ── the resolved install plan ────────────────────────────────────────

/// One pinned npm artifact of a plan: the server package itself or a
/// `co_install` companion. Both install by the same mechanics — fetch, verify
/// the SRI sha512, install the verified file `--ignore-scripts`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NpmArtifact {
    /// `<package>@<version>`, for log and consent-overlay lines.
    label: String,
    /// The pinned registry tarball URL.
    url: String,
    /// The expected SRI sha512 (`sha512-<base64>`) from the recipe.
    sri: String,
}

/// The per-ecosystem step a plan runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlanKind {
    /// npm — fetch every pinned tarball (the server plus its co-install
    /// companions), verify each against its SRI sha512, then install the
    /// verified set into the version dir in one `--prefix`ed,
    /// `--ignore-scripts` invocation.
    NpmTarball {
        /// The pinned artifacts; the server's own package is first.
        artifacts: Vec<NpmArtifact>,
    },
    /// cargo/go — a single self-verifying command, already contained
    /// (`--root` / `GOBIN` name the version dir).
    Direct {
        /// The pinned install command.
        command: InstallCommand,
    },
    /// pip — a venv at the version dir, then a `--require-hashes` requirements
    /// install through the venv's own pip; the string is the requirements-file
    /// line.
    PipRequireHashes {
        /// The `<package>==<version> --hash=sha256:<hex>` requirement line.
        requirement: String,
    },
}

/// A fully resolved, blessed install action for one server.
///
/// Built by [`InstallPlan::resolve`] from a [`BlessedRecipe`] and the
/// [`ManagedHome`]; executed by [`execute`]. It carries exactly the pinned data
/// — package, version, ecosystem, verification tier, the managed-home
/// destination, and the per-ecosystem step — so the consent overlay can state
/// honestly what will run, how it is verified, and where it lands.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    /// The ecosystem the artifact installs from.
    ecosystem: Ecosystem,
    /// How the artifact is verified.
    tier: VerificationTier,
    /// The package/module identifier.
    package: String,
    /// The exact pinned version.
    version: String,
    /// The managed-home version dir the install lands in
    /// (`<data_dir>/catenary/servers/<name>/<version>/`).
    destination: PathBuf,
    /// The per-ecosystem step.
    kind: PlanKind,
}

impl InstallPlan {
    /// Resolve the install plan for a blessed recipe, destined for `home`.
    ///
    /// The destination is derived here — `home.version_dir(server, version)` —
    /// and nowhere else: the recipe is declarative and carries no destination.
    ///
    /// # Errors
    ///
    /// Returns an error when the ecosystem requires a content hash the recipe
    /// does not carry: an **npm** recipe with no tarball sha512 — for the server
    /// or for any `co_install` companion (the pinned set installs as one unit,
    /// so one unverifiable member refuses the whole plan) — or a **pip** recipe
    /// with no `--require-hashes` digest (refused politely, echoing the
    /// recipe's note). Also errs when the server name or version is not a plain
    /// path segment (defensive; blessed data never is).
    pub fn resolve(blessed: &BlessedRecipe, home: &ManagedHome) -> Result<Self> {
        let r = blessed.recipe();
        let destination = home.version_dir(blessed.server(), &r.version)?;
        let kind = match r.ecosystem {
            Ecosystem::Npm => {
                let sri = r.hash.clone().ok_or_else(|| {
                    anyhow!(
                        "npm recipe for `{}` is version-pinned but carries no tarball sha512; \
                         Catenary will not install an unverified npm artifact",
                        blessed.server()
                    )
                })?;
                let mut artifacts = vec![NpmArtifact {
                    label: format!("{}@{}", r.package, r.version),
                    url: npm_tarball_url(&r.package, &r.version),
                    sri,
                }];
                for co in &r.co_install {
                    let sri = co.hash.clone().ok_or_else(|| {
                        anyhow!(
                            "npm co-install `{}` for `{}` is version-pinned but carries no \
                             tarball sha512; the pinned set installs as one unit and Catenary \
                             will not install an unverified npm artifact",
                            co.package,
                            blessed.server()
                        )
                    })?;
                    artifacts.push(NpmArtifact {
                        label: format!("{}@{}", co.package, co.version),
                        url: npm_tarball_url(&co.package, &co.version),
                        sri,
                    });
                }
                PlanKind::NpmTarball { artifacts }
            }
            Ecosystem::Cargo => PlanKind::Direct {
                command: cargo_install_command(&r.package, &r.version, &destination),
            },
            Ecosystem::Go => PlanKind::Direct {
                command: go_install_command(&r.package, &r.version, &destination),
            },
            Ecosystem::Pip => {
                let hash = r.hash.clone().ok_or_else(|| {
                    let why = r
                        .note
                        .as_deref()
                        .map_or_else(String::new, |n| format!(" ({n})"));
                    anyhow!(
                        "pip recipe for `{}` has no --require-hashes digest yet{why}",
                        blessed.server()
                    )
                })?;
                PlanKind::PipRequireHashes {
                    requirement: pip_requirement_line(&r.package, &r.version, &hash),
                }
            }
        };
        Ok(Self {
            ecosystem: r.ecosystem,
            tier: r.tier,
            package: r.package.clone(),
            version: r.version.clone(),
            destination,
            kind,
        })
    }

    /// The ecosystem the artifact installs from.
    #[must_use]
    pub const fn ecosystem(&self) -> Ecosystem {
        self.ecosystem
    }

    /// The verification tier.
    #[must_use]
    pub const fn tier(&self) -> VerificationTier {
        self.tier
    }

    /// The package/module identifier.
    #[must_use]
    pub fn package(&self) -> &str {
        &self.package
    }

    /// The exact pinned version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The managed-home version dir this install lands in —
    /// `<data_dir>/catenary/servers/<name>/<version>/`, self-contained, with
    /// executables at `bin/` (the ruled invariant, ls-manager 01).
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// The tarball URL fetched-and-verified before install (npm only; the
    /// server's own artifact — co-install companions follow the same path).
    #[must_use]
    pub fn fetch_url(&self) -> Option<&str> {
        match &self.kind {
            PlanKind::NpmTarball { artifacts } => artifacts.first().map(|a| a.url.as_str()),
            PlanKind::Direct { .. } | PlanKind::PipRequireHashes { .. } => None,
        }
    }

    /// A one-line, honest description of how the artifact is verified.
    #[must_use]
    pub const fn verify_summary(&self) -> &'static str {
        match self.tier {
            VerificationTier::NpmTarballSha512 => {
                "fetch tarball, verify sha512, install verified artifact (--ignore-scripts)"
            }
            VerificationTier::CargoLocked => {
                "exact version + --locked (cargo verifies index checksums)"
            }
            VerificationTier::PipHashes => "--require-hashes (pip verifies the pinned sha256)",
            VerificationTier::GoChecksumdb => "version pin (Go checksum DB verifies transparently)",
        }
    }

    /// The command that runs, rendered for the consent overlay — it names the
    /// managed-home destination (`--prefix`/`--root`/`GOBIN`/the venv dir). For
    /// npm the verified-tarball paths are shown as labelled placeholders (they
    /// are staged at execution time); the string is display-only and never
    /// executed.
    #[must_use]
    pub fn display_command(&self) -> String {
        match &self.kind {
            PlanKind::NpmTarball { artifacts } => {
                let placeholders: Vec<String> = artifacts
                    .iter()
                    .map(|a| format!("<verified {}.tgz>", a.label))
                    .collect();
                let refs: Vec<&str> = placeholders.iter().map(String::as_str).collect();
                npm_install_command(&self.destination, &refs).display()
            }
            PlanKind::Direct { command } => command.display(),
            PlanKind::PipRequireHashes { requirement } => {
                format!(
                    "{} && {}   (requirements: {requirement})",
                    venv_create_command(&self.destination).display(),
                    pip_install_command(&self.destination, "<requirements>").display()
                )
            }
        }
    }
}

// ── verification ─────────────────────────────────────────────────────

/// Verify that `bytes` hash to the expected SRI sha512 (`sha512-<base64>`).
///
/// # Errors
///
/// Returns an error when `sri` is not an `sha512-` SRI string, or when the
/// computed digest does not match — in which case the caller must NOT install.
fn verify_sha512(bytes: &[u8], sri: &str) -> Result<()> {
    let expected = sri
        .strip_prefix("sha512-")
        .ok_or_else(|| anyhow!("recipe hash is not an SRI sha512 (`sha512-…`): {sri}"))?;
    let actual = base64_standard(&sha512(bytes));
    if actual != expected {
        bail!("sha512 mismatch: recipe pins {expected}, artifact hashed to {actual}");
    }
    Ok(())
}

// ── execution seams ──────────────────────────────────────────────────

/// The result of running one [`InstallCommand`].
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// Whether the process exited successfully.
    pub success: bool,
    /// The process exit code, when it exited via one (not a signal).
    pub code: Option<i32>,
    /// The combined captured stdout + stderr, surfaced in the UI.
    pub output: String,
}

/// Runs an [`InstallCommand`] to completion (argv, never a shell). Injectable so
/// the engine's command construction and ordering are tested without spawning
/// anything.
pub trait CommandRunner {
    /// Run `command`, capturing its combined stdout + stderr.
    ///
    /// # Errors
    ///
    /// Returns an error when the process cannot be spawned or its output cannot
    /// be collected. A non-zero exit is reported through
    /// [`CommandOutcome::success`], not as an error.
    fn run(&self, command: &InstallCommand) -> Result<CommandOutcome>;
}

/// Fetches the bytes of an npm tarball. Injectable so the verification logic is
/// tested against fixture bytes and never hits the network.
pub trait TarballFetcher {
    /// Fetch the bytes at `url`.
    ///
    /// # Errors
    ///
    /// Returns an error when the request fails or the body cannot be read.
    fn fetch(&self, url: &str) -> Result<Vec<u8>>;
}

/// The production [`CommandRunner`]: spawns via [`std::process::Command`] with the
/// literal argv and captures combined output.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, command: &InstallCommand) -> Result<CommandOutcome> {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            .envs(command.envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(std::process::Stdio::null())
            .output()
            .with_context(|| format!("spawning `{}`", command.program))?;
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(CommandOutcome {
            success: output.status.success(),
            code: output.status.code(),
            output: combined,
        })
    }
}

/// The production [`TarballFetcher`]: a bounded HTTP GET via `ureq`.
#[derive(Debug, Default, Clone, Copy)]
pub struct UreqFetcher;

impl TarballFetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let resp = ureq::get(url)
            .set("User-Agent", "catenary-install")
            .call()
            .with_context(|| format!("GET {url}"))?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(MAX_TARBALL_BYTES)
            .read_to_end(&mut bytes)
            .context("reading tarball body")?;
        Ok(bytes)
    }
}

// ── execution ────────────────────────────────────────────────────────

/// The result of executing an [`InstallPlan`].
///
/// Failures are ordinary data (never a panic, never a partial hidden state): a
/// fetch error, a hash mismatch, or a non-zero install exit all yield
/// `success = false` with the reason appended to the log. The npm path *never*
/// runs the install command unless the fetched bytes verified.
#[derive(Debug, Clone)]
pub struct InstallOutcome {
    /// Whether the install completed successfully.
    pub success: bool,
    /// The step-by-step log, surfaced verbatim in the consent overlay.
    pub log: Vec<String>,
}

impl InstallOutcome {
    /// A failed outcome carrying `log` plus a final `✗ <message>` line.
    fn failed(mut log: Vec<String>, message: &str) -> Self {
        log.push(format!("✗ {message}"));
        Self {
            success: false,
            log,
        }
    }
}

/// Execute a resolved install plan through the injected seams.
///
/// Every leg lands in the plan's managed-home destination
/// (`<data_dir>/catenary/servers/<name>/<version>/`) and nowhere else. npm
/// fetches and verifies **every** pinned artifact (server + co-install
/// companions; aborting before any install on the first mismatch — an
/// unverified artifact is never installed, and the set never lands
/// half-installed), stages the verified bytes to temp files, then installs them
/// in one `--prefix`ed invocation with `--ignore-scripts`. cargo/go run their
/// single self-verifying command, contained via `--root`/`GOBIN`. pip creates
/// the venv at the version dir, stages the hashed requirements file, and runs
/// the venv's own pip with `--require-hashes`. All output is captured and
/// returned in the outcome log.
#[must_use]
pub fn execute(
    plan: &InstallPlan,
    runner: &dyn CommandRunner,
    fetcher: &dyn TarballFetcher,
) -> InstallOutcome {
    let log = vec![format!("managed home: {}", plan.destination.display())];
    match &plan.kind {
        PlanKind::NpmTarball { artifacts } => {
            execute_npm(artifacts, &plan.destination, runner, fetcher, log)
        }
        PlanKind::Direct { command } => {
            // cargo/go land binaries in `<version-dir>/bin` themselves
            // (`--root` / `GOBIN`); pre-create it so the contained invocation
            // never depends on the tool's own directory creation.
            if let Err(e) = std::fs::create_dir_all(plan.destination.join("bin")) {
                return InstallOutcome::failed(
                    log,
                    &format!("could not create {}: {e}", plan.destination.display()),
                );
            }
            run_command(runner, command, log)
        }
        PlanKind::PipRequireHashes { requirement } => {
            execute_pip(requirement, &plan.destination, runner, log)
        }
    }
}

/// npm: fetch → verify (every artifact, before anything installs) → stage →
/// one `npm install -g --prefix <version-dir> … --ignore-scripts`.
fn execute_npm(
    artifacts: &[NpmArtifact],
    version_dir: &Path,
    runner: &dyn CommandRunner,
    fetcher: &dyn TarballFetcher,
    mut log: Vec<String>,
) -> InstallOutcome {
    // Verify the WHOLE pinned set first: a mismatch on any member (server or
    // companion) aborts before a single command runs, so the unit either lands
    // complete or not at all.
    let mut staged = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        log.push(format!("fetching {}", artifact.url));
        let bytes = match fetcher.fetch(&artifact.url) {
            Ok(b) => b,
            Err(e) => return InstallOutcome::failed(log, &format!("fetch failed: {e:#}")),
        };
        log.push(format!(
            "verifying sha512 over {} bytes ({})",
            bytes.len(),
            artifact.label
        ));
        if let Err(e) = verify_sha512(&bytes, &artifact.sri) {
            // Never install unverified bytes: stop here, no command runs.
            return InstallOutcome::failed(log, &e.to_string());
        }
        log.push(format!("sha512 verified ({})", artifact.label));
        match stage_temp(&bytes, ".tgz") {
            Ok(f) => staged.push(f),
            Err(e) => {
                return InstallOutcome::failed(log, &format!("could not stage artifact: {e:#}"));
            }
        }
    }
    if let Err(e) = std::fs::create_dir_all(version_dir) {
        return InstallOutcome::failed(
            log,
            &format!("could not create {}: {e}", version_dir.display()),
        );
    }
    let staged_paths: Vec<String> = staged
        .iter()
        .map(|f| f.path().to_string_lossy().into_owned())
        .collect();
    let refs: Vec<&str> = staged_paths.iter().map(String::as_str).collect();
    let command = npm_install_command(version_dir, &refs);
    run_command(runner, &command, log)
    // `staged` drops here (after the install has run), deleting the temp files.
}

/// pip: create the venv at the version dir → stage the hashed requirements
/// file → the venv's own `pip install --require-hashes`.
fn execute_pip(
    requirement: &str,
    version_dir: &Path,
    runner: &dyn CommandRunner,
    mut log: Vec<String>,
) -> InstallOutcome {
    if let Err(e) = std::fs::create_dir_all(version_dir) {
        return InstallOutcome::failed(
            log,
            &format!("could not create {}: {e}", version_dir.display()),
        );
    }
    log.push(format!("creating venv at {}", version_dir.display()));
    let venv = run_command(runner, &venv_create_command(version_dir), log);
    if !venv.success {
        return venv;
    }
    let mut log = venv.log;
    log.push("staging --require-hashes requirements".to_owned());
    let staged = match stage_temp(requirement.as_bytes(), ".txt") {
        Ok(f) => f,
        Err(e) => {
            return InstallOutcome::failed(log, &format!("could not stage requirements: {e:#}"));
        }
    };
    log.push(format!("requirements: {requirement}"));
    let command = pip_install_command(version_dir, &staged.path().to_string_lossy());
    run_command(runner, &command, log)
}

/// Run a command through the runner, appending its result to `log`.
fn run_command(
    runner: &dyn CommandRunner,
    command: &InstallCommand,
    mut log: Vec<String>,
) -> InstallOutcome {
    log.push(format!("running: {}", command.display()));
    match runner.run(command) {
        Ok(outcome) => {
            for line in outcome.output.lines() {
                log.push(line.to_owned());
            }
            if outcome.success {
                log.push("done".to_owned());
                InstallOutcome { success: true, log }
            } else {
                let status = outcome
                    .code
                    .map_or_else(|| "a signal".to_owned(), |c| format!("status {c}"));
                InstallOutcome::failed(log, &format!("`{}` exited with {status}", command.program))
            }
        }
        Err(e) => {
            InstallOutcome::failed(log, &format!("could not run `{}`: {e:#}", command.program))
        }
    }
}

/// Write `bytes` to a temp file with the given suffix, returning the handle. The
/// caller keeps the handle alive across the install so the file is not deleted
/// until the command has run.
fn stage_temp(bytes: &[u8], suffix: &str) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("catenary-install-")
        .suffix(suffix)
        .tempfile()
        .context("creating temp file")?;
    file.write_all(bytes).context("writing temp file")?;
    file.flush().context("flushing temp file")?;
    Ok(file)
}

// ── SHA-512 (FIPS 180-4) ─────────────────────────────────────────────

/// SHA-512 initial hash values (FIPS 180-4 §5.3.5).
const SHA512_H0: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// SHA-512 round constants (FIPS 180-4 §4.2.3).
const SHA512_K: [u64; 80] = [
    0x428a_2f98_d728_ae22,
    0x7137_4491_23ef_65cd,
    0xb5c0_fbcf_ec4d_3b2f,
    0xe9b5_dba5_8189_dbbc,
    0x3956_c25b_f348_b538,
    0x59f1_11f1_b605_d019,
    0x923f_82a4_af19_4f9b,
    0xab1c_5ed5_da6d_8118,
    0xd807_aa98_a303_0242,
    0x1283_5b01_4570_6fbe,
    0x2431_85be_4ee4_b28c,
    0x550c_7dc3_d5ff_b4e2,
    0x72be_5d74_f27b_896f,
    0x80de_b1fe_3b16_96b1,
    0x9bdc_06a7_25c7_1235,
    0xc19b_f174_cf69_2694,
    0xe49b_69c1_9ef1_4ad2,
    0xefbe_4786_384f_25e3,
    0x0fc1_9dc6_8b8c_d5b5,
    0x240c_a1cc_77ac_9c65,
    0x2de9_2c6f_592b_0275,
    0x4a74_84aa_6ea6_e483,
    0x5cb0_a9dc_bd41_fbd4,
    0x76f9_88da_8311_53b5,
    0x983e_5152_ee66_dfab,
    0xa831_c66d_2db4_3210,
    0xb003_27c8_98fb_213f,
    0xbf59_7fc7_beef_0ee4,
    0xc6e0_0bf3_3da8_8fc2,
    0xd5a7_9147_930a_a725,
    0x06ca_6351_e003_826f,
    0x1429_2967_0a0e_6e70,
    0x27b7_0a85_46d2_2ffc,
    0x2e1b_2138_5c26_c926,
    0x4d2c_6dfc_5ac4_2aed,
    0x5338_0d13_9d95_b3df,
    0x650a_7354_8baf_63de,
    0x766a_0abb_3c77_b2a8,
    0x81c2_c92e_47ed_aee6,
    0x9272_2c85_1482_353b,
    0xa2bf_e8a1_4cf1_0364,
    0xa81a_664b_bc42_3001,
    0xc24b_8b70_d0f8_9791,
    0xc76c_51a3_0654_be30,
    0xd192_e819_d6ef_5218,
    0xd699_0624_5565_a910,
    0xf40e_3585_5771_202a,
    0x106a_a070_32bb_d1b8,
    0x19a4_c116_b8d2_d0c8,
    0x1e37_6c08_5141_ab53,
    0x2748_774c_df8e_eb99,
    0x34b0_bcb5_e19b_48a8,
    0x391c_0cb3_c5c9_5a63,
    0x4ed8_aa4a_e341_8acb,
    0x5b9c_ca4f_7763_e373,
    0x682e_6ff3_d6b2_b8a3,
    0x748f_82ee_5def_b2fc,
    0x78a5_636f_4317_2f60,
    0x84c8_7814_a1f0_ab72,
    0x8cc7_0208_1a64_39ec,
    0x90be_fffa_2363_1e28,
    0xa450_6ceb_de82_bde9,
    0xbef9_a3f7_b2c6_7915,
    0xc671_78f2_e372_532b,
    0xca27_3ece_ea26_619c,
    0xd186_b8c7_21c0_c207,
    0xeada_7dd6_cde0_eb1e,
    0xf57d_4f7f_ee6e_d178,
    0x06f0_67aa_7217_6fba,
    0x0a63_7dc5_a2c8_98a6,
    0x113f_9804_bef9_0dae,
    0x1b71_0b35_131c_471b,
    0x28db_77f5_2304_7d84,
    0x32ca_ab7b_40c7_2493,
    0x3c9e_be0a_15c9_bebc,
    0x431d_67c4_9c10_0d4c,
    0x4cc5_d4be_cb3e_42b6,
    0x597f_299c_fc65_7e2a,
    0x5fcb_6fab_3ad6_faec,
    0x6c44_198c_4a47_5817,
];

/// Compute the SHA-512 digest of `data` (FIPS 180-4).
fn sha512(data: &[u8]) -> [u8; 64] {
    let mut h = SHA512_H0;

    // Pad: append 0x80, then zeros to 112 mod 128, then the 128-bit big-endian
    // bit length.
    let bit_len = (data.len() as u128).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 128 != 112 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(128) {
        let mut w = [0u64; 80];
        for (i, chunk) in block.chunks_exact(8).enumerate().take(16) {
            w[i] = u64::from_be_bytes(chunk.try_into().unwrap_or([0; 8]));
        }
        for t in 16..80 {
            let s0 = w[t - 15].rotate_right(1) ^ w[t - 15].rotate_right(8) ^ (w[t - 15] >> 7);
            let s1 = w[t - 2].rotate_right(19) ^ w[t - 2].rotate_right(61) ^ (w[t - 2] >> 6);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }

        // Working variables a..h held as v[0..8] (avoids eight single-char names).
        let mut v = h;
        for t in 0..80 {
            let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
            let ch = (v[4] & v[5]) ^ ((!v[4]) & v[6]);
            let temp1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA512_K[t])
                .wrapping_add(w[t]);
            let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let temp2 = s0.wrapping_add(maj);
            v[7] = v[6];
            v[6] = v[5];
            v[5] = v[4];
            v[4] = v[3].wrapping_add(temp1);
            v[3] = v[2];
            v[2] = v[1];
            v[1] = v[0];
            v[0] = temp1.wrapping_add(temp2);
        }
        for (hi, vi) in h.iter_mut().zip(v.iter()) {
            *hi = hi.wrapping_add(*vi);
        }
    }

    let mut out = [0u8; 64];
    for (word, slot) in h.iter().zip(out.chunks_exact_mut(8)) {
        slot.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Encode `bytes` as standard base64 (RFC 4648, `+`/`/`, `=` padding) — the SRI
/// hash form the recipe stores.
fn base64_standard(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(ALPHABET[((triple >> 18) & 0x3f) as usize]));
        out.push(char::from(ALPHABET[((triple >> 12) & 0x3f) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[((triple >> 6) & 0x3f) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[(triple & 0x3f) as usize])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests use expect/unwrap/panic for readable assertions"
)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::recipes::CoInstall;

    /// A fixed managed home for plan-shape assertions (never executed against,
    /// so nothing is ever created under it).
    fn fixed_home() -> ManagedHome {
        ManagedHome::at(PathBuf::from("/mh"))
    }

    /// Whether an argv token names a staged tarball.
    fn is_tgz(arg: &str) -> bool {
        Path::new(arg)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("tgz"))
    }

    /// A managed home rooted in a tempdir — the data dir pointed at a tempdir
    /// subdir, in the `isolate_env` mislocation-detector spirit — for tests
    /// that execute a plan (execution creates real directories).
    fn temp_home() -> (tempfile::TempDir, ManagedHome) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let home = ManagedHome::at(tmp.path().join("servers"));
        (tmp, home)
    }

    /// A recording fake runner: captures every command it is asked to run and
    /// replies with a scripted success/failure.
    struct FakeRunner {
        calls: RefCell<Vec<InstallCommand>>,
        succeed: bool,
    }

    impl FakeRunner {
        fn ok() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                succeed: true,
            }
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, command: &InstallCommand) -> Result<CommandOutcome> {
            self.calls.borrow_mut().push(command.clone());
            Ok(CommandOutcome {
                success: self.succeed,
                code: Some(i32::from(!self.succeed)),
                output: "fake output".to_owned(),
            })
        }
    }

    /// A fetcher that always returns the same fixture bytes and records the URL.
    struct FakeFetcher {
        bytes: Vec<u8>,
        fetched: RefCell<Vec<String>>,
    }

    impl FakeFetcher {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                fetched: RefCell::new(Vec::new()),
            }
        }
    }

    impl TarballFetcher for FakeFetcher {
        fn fetch(&self, url: &str) -> Result<Vec<u8>> {
            self.fetched.borrow_mut().push(url.to_owned());
            Ok(self.bytes.clone())
        }
    }

    /// A fetcher serving distinct fixture bodies per URL (the multi-artifact
    /// npm path: server tarball + co-install companion tarballs), recording
    /// every URL fetched.
    struct MapFetcher {
        bodies: std::collections::BTreeMap<String, Vec<u8>>,
        fetched: RefCell<Vec<String>>,
    }

    impl MapFetcher {
        fn new(bodies: &[(&str, &[u8])]) -> Self {
            Self {
                bodies: bodies
                    .iter()
                    .map(|(url, bytes)| ((*url).to_owned(), bytes.to_vec()))
                    .collect(),
                fetched: RefCell::new(Vec::new()),
            }
        }
    }

    impl TarballFetcher for MapFetcher {
        fn fetch(&self, url: &str) -> Result<Vec<u8>> {
            self.fetched.borrow_mut().push(url.to_owned());
            self.bodies
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow!("no fixture body for {url}"))
        }
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    fn recipe(
        ecosystem: Ecosystem,
        package: &str,
        version: &str,
        tier: VerificationTier,
    ) -> InstallRecipe {
        InstallRecipe {
            ecosystem,
            package: package.to_owned(),
            version: version.to_owned(),
            tier,
            draft: true,
            hash: None,
            note: None,
            conformance: true,
            co_install: Vec::new(),
            runtime: None,
        }
    }

    fn manifest(server: &str, version: &str) -> BlessedManifest {
        let mut m = BlessedManifest::default();
        let mut per_platform = std::collections::BTreeMap::new();
        per_platform.insert(
            "linux-x86_64".to_owned(),
            BlessedEntry {
                version: version.to_owned(),
                platform: "linux-x86_64".to_owned(),
                date: "2026-07-07".to_owned(),
                tier: Some("verified-on-linux".to_owned()),
            },
        );
        m.blessed.insert(server.to_owned(), per_platform);
        m
    }

    #[test]
    fn sha512_matches_known_vectors() {
        assert_eq!(
            hex(&sha512(b"")),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            hex(&sha512(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn sha512_matches_fips_two_block_vector() {
        // FIPS 180-4 two-block message vector (896 bits) — exercises the
        // multi-block compression path and the padding boundary that the
        // single-block vectors above never reach. A real tarball is always
        // multi-block, so this is the vector that stands between a subtly
        // wrong hasher and the supply-chain gate (landing rider, tui 06).
        let msg = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn\
hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
        assert_eq!(
            hex(&sha512(msg)),
            "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018\
             501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
        );
    }

    #[test]
    fn sha512_matches_million_a_vector() {
        // FIPS 180-4 long-message vector: one million 'a' bytes — thousands
        // of compression blocks plus a length field past 2^20 bits.
        let msg = vec![b'a'; 1_000_000];
        assert_eq!(
            hex(&sha512(&msg)),
            "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973eb\
             de0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
        );
    }

    #[test]
    fn base64_length_and_padding() {
        // A 64-byte digest base64-encodes to 88 chars ending in `==`.
        let sri = base64_standard(&sha512(b"abc"));
        assert_eq!(sri.len(), 88);
        assert!(sri.ends_with("=="));
    }

    #[test]
    fn verify_accepts_matching_and_rejects_tampered() {
        let bytes = b"a pretend tarball";
        let sri = format!("sha512-{}", base64_standard(&sha512(bytes)));
        assert!(verify_sha512(bytes, &sri).is_ok());
        // A single flipped byte must fail — an unverified artifact is never installed.
        assert!(verify_sha512(b"a pretend tarbalX", &sri).is_err());
        // A non-SRI string is rejected too.
        assert!(verify_sha512(bytes, "deadbeef").is_err());
    }

    #[test]
    fn blessing_gate_requires_matching_manifest_entry() {
        let r = recipe(
            Ecosystem::Cargo,
            "taplo-cli",
            "0.10.0",
            VerificationTier::CargoLocked,
        );
        // No manifest entry → unofferable.
        assert!(BlessedRecipe::resolve("taplo", &r, &BlessedManifest::default()).is_none());
        // Entry at a different version → unofferable.
        assert!(BlessedRecipe::resolve("taplo", &r, &manifest("taplo", "0.9.0")).is_none());
        // Matching entry → offerable.
        assert!(BlessedRecipe::resolve("taplo", &r, &manifest("taplo", "0.10.0")).is_some());
    }

    #[test]
    fn blessed_manifest_entry_without_recipe_yields_no_offer() {
        // A CI-provisioned server (rust-analyzer, clangd, lattice, …) blesses but
        // is NOT a recipe — it lives only in ci-provision.toml (tui-rework 10).
        // Offerability requires a recipe to construct a BlessedRecipe, so a
        // manifest entry with no matching recipe is never offerable: the offer
        // loop iterates recipes and never visits a manifest-only server.
        let manifest = manifest("rust-analyzer", "1.85.0"); // blessed, no recipe
        let recipes: std::collections::BTreeMap<String, InstallRecipe> =
            std::collections::BTreeMap::new(); // rust-analyzer carries no recipe
        let any_offerable = recipes
            .iter()
            .any(|(name, r)| BlessedRecipe::resolve(name, r, &manifest).is_some());
        assert!(
            !any_offerable,
            "a blessed-manifest entry with no recipe must yield nothing offerable",
        );
    }

    #[test]
    fn cargo_plan_argv_is_pinned_and_locked() {
        let r = recipe(
            Ecosystem::Cargo,
            "taplo-cli",
            "0.10.0",
            VerificationTier::CargoLocked,
        );
        let blessed = BlessedRecipe::resolve("taplo", &r, &manifest("taplo", "0.10.0")).unwrap();
        let plan = InstallPlan::resolve(&blessed, &fixed_home()).expect("cargo resolves");
        let PlanKind::Direct { command } = &plan.kind else {
            panic!("cargo is a direct command")
        };
        assert_eq!(command.program(), "cargo");
        assert_eq!(
            command.args(),
            [
                "install",
                "taplo-cli",
                "--version",
                "=0.10.0",
                "--locked",
                "--root",
                "/mh/taplo/0.10.0"
            ],
            "the install is contained in the managed-home version dir via --root",
        );
        assert_eq!(
            plan.destination(),
            Path::new("/mh/taplo/0.10.0"),
            "the plan names its managed-home destination",
        );
    }

    #[test]
    fn go_plan_argv_normalizes_version_prefix() {
        // The recipe already carries the `v`.
        let r = recipe(
            Ecosystem::Go,
            "golang.org/x/tools/gopls",
            "v0.22.0",
            VerificationTier::GoChecksumdb,
        );
        let blessed = BlessedRecipe::resolve("gopls", &r, &manifest("gopls", "v0.22.0")).unwrap();
        let plan = InstallPlan::resolve(&blessed, &fixed_home()).expect("go resolves");
        let PlanKind::Direct { command } = &plan.kind else {
            panic!("go is a direct command")
        };
        assert_eq!(command.program(), "go");
        assert_eq!(
            command.args(),
            ["install", "golang.org/x/tools/gopls@v0.22.0"]
        );
        // The containment leg: GOBIN names the version dir's bin/.
        assert_eq!(
            command.envs(),
            [("GOBIN".to_owned(), "/mh/gopls/v0.22.0/bin".to_owned())],
            "GOBIN pins the binary into the managed-home version dir",
        );
        // A bare version gets the `v` prefix.
        assert_eq!(go_version("1.2.3"), "v1.2.3");
    }

    #[test]
    fn npm_tarball_url_scoped_and_unscoped() {
        assert_eq!(
            npm_tarball_url("bash-language-server", "5.6.0"),
            "https://registry.npmjs.org/bash-language-server/-/bash-language-server-5.6.0.tgz"
        );
        assert_eq!(
            npm_tarball_url("@elm-tooling/elm-language-server", "2.8.0"),
            "https://registry.npmjs.org/@elm-tooling/elm-language-server/-/elm-language-server-2.8.0.tgz"
        );
    }

    #[test]
    fn npm_plan_fetches_verifies_then_installs_ignore_scripts() {
        let bytes = b"pretend bash-language-server tarball".to_vec();
        let sri = format!("sha512-{}", base64_standard(&sha512(&bytes)));
        let mut r = recipe(
            Ecosystem::Npm,
            "bash-language-server",
            "5.6.0",
            VerificationTier::NpmTarballSha512,
        );
        r.hash = Some(sri);
        let blessed = BlessedRecipe::resolve(
            "bash-language-server",
            &r,
            &manifest("bash-language-server", "5.6.0"),
        )
        .unwrap();
        let (_tmp, home) = temp_home();
        let plan = InstallPlan::resolve(&blessed, &home).expect("npm resolves");
        let dest = home
            .version_dir("bash-language-server", "5.6.0")
            .expect("version dir");

        let runner = FakeRunner::ok();
        let fetcher = FakeFetcher::new(bytes);
        let outcome = execute(&plan, &runner, &fetcher);

        assert!(
            outcome.success,
            "verified install succeeds: {:?}",
            outcome.log
        );
        // Fetched the pinned tarball exactly once.
        assert_eq!(
            fetcher.fetched.borrow().as_slice(),
            ["https://registry.npmjs.org/bash-language-server/-/bash-language-server-5.6.0.tgz"]
        );
        // Ran `npm install -g --prefix <version-dir> <verified tarball>
        // --ignore-scripts` — argv, install scripts disabled, artifact a staged
        // local path, contained in the managed-home version dir.
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        let cmd = &calls[0];
        assert_eq!(cmd.program(), "npm");
        assert_eq!(cmd.args()[0], "install");
        assert_eq!(cmd.args()[1], "-g");
        assert_eq!(cmd.args()[2], "--prefix");
        assert_eq!(
            cmd.args()[3],
            dest.to_string_lossy().as_ref(),
            "the prefix is the managed-home version dir",
        );
        assert!(
            std::path::Path::new(&cmd.args()[4])
                .extension()
                .is_some_and(|e| e == "tgz"),
            "installs the staged tarball path"
        );
        assert_eq!(cmd.args()[5], "--ignore-scripts");
        assert!(dest.is_dir(), "the version dir is created for the install");
    }

    #[test]
    fn npm_hash_mismatch_never_runs_install() {
        let mut r = recipe(
            Ecosystem::Npm,
            "bash-language-server",
            "5.6.0",
            VerificationTier::NpmTarballSha512,
        );
        // Pin the hash of DIFFERENT bytes than the fetcher will return.
        r.hash = Some(format!(
            "sha512-{}",
            base64_standard(&sha512(b"the real bytes"))
        ));
        let blessed = BlessedRecipe::resolve(
            "bash-language-server",
            &r,
            &manifest("bash-language-server", "5.6.0"),
        )
        .unwrap();
        let (_tmp, home) = temp_home();
        let plan = InstallPlan::resolve(&blessed, &home).unwrap();

        let runner = FakeRunner::ok();
        let fetcher = FakeFetcher::new(b"TAMPERED bytes".to_vec());
        let outcome = execute(&plan, &runner, &fetcher);

        assert!(!outcome.success, "a hash mismatch fails");
        assert!(
            runner.calls.borrow().is_empty(),
            "the install command NEVER runs on a mismatch",
        );
        assert!(
            outcome.log.iter().any(|l| l.contains("mismatch")),
            "the failure names the mismatch: {:?}",
            outcome.log,
        );
        assert!(
            !plan.destination().exists(),
            "a refused install leaves no version dir behind",
        );
    }

    #[test]
    fn npm_refuses_without_hash() {
        let r = recipe(
            Ecosystem::Npm,
            "bash-language-server",
            "5.6.0",
            VerificationTier::NpmTarballSha512,
        );
        let blessed = BlessedRecipe::resolve(
            "bash-language-server",
            &r,
            &manifest("bash-language-server", "5.6.0"),
        )
        .unwrap();
        let err =
            InstallPlan::resolve(&blessed, &fixed_home()).expect_err("npm without a hash refuses");
        assert!(
            err.to_string().contains("unverified"),
            "the refusal is explicit: {err}",
        );
    }

    #[test]
    fn pip_require_hashes_form_and_argv() {
        let mut r = recipe(
            Ecosystem::Pip,
            "cmake-language-server",
            "0.1.11",
            VerificationTier::PipHashes,
        );
        r.hash = Some(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
        );
        let blessed = BlessedRecipe::resolve(
            "cmake-language-server",
            &r,
            &manifest("cmake-language-server", "0.1.11"),
        )
        .unwrap();
        let (_tmp, home) = temp_home();
        let plan = InstallPlan::resolve(&blessed, &home).expect("pip with a hash resolves");
        let dest = home
            .version_dir("cmake-language-server", "0.1.11")
            .expect("version dir");
        let PlanKind::PipRequireHashes { requirement } = &plan.kind else {
            panic!("pip is a require-hashes install")
        };
        assert_eq!(
            requirement,
            "cmake-language-server==0.1.11 --hash=sha256:1111111111111111111111111111111111111111111111111111111111111111"
        );
        // The install is a venv at the version dir, then the venv's OWN pip
        // running `install --require-hashes --no-deps -r <file>` — everything
        // lands inside the version dir, entry points in its bin/.
        let runner = FakeRunner::ok();
        let fetcher = FakeFetcher::new(Vec::new());
        let outcome = execute(&plan, &runner, &fetcher);
        assert!(outcome.success, "{:?}", outcome.log);
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 2, "venv creation, then the hashed install");
        assert_eq!(calls[0].program(), "python3");
        assert_eq!(
            calls[0].args(),
            ["-m", "venv", dest.to_string_lossy().as_ref()],
            "the venv is created AT the managed-home version dir",
        );
        assert_eq!(
            calls[1].program(),
            dest.join("bin").join("pip").to_string_lossy().as_ref(),
            "the hashed install runs through the venv's own pip",
        );
        assert_eq!(calls[1].args()[0], "install");
        assert_eq!(calls[1].args()[1], "--require-hashes");
        assert_eq!(calls[1].args()[2], "--no-deps");
        assert_eq!(calls[1].args()[3], "-r");
    }

    #[test]
    fn pip_refuses_without_hash_and_echoes_note() {
        let mut r = recipe(
            Ecosystem::Pip,
            "cmake-language-server",
            "0.1.11",
            VerificationTier::PipHashes,
        );
        r.note = Some("closure hashes pending".to_owned());
        let blessed = BlessedRecipe::resolve(
            "cmake-language-server",
            &r,
            &manifest("cmake-language-server", "0.1.11"),
        )
        .unwrap();
        let err =
            InstallPlan::resolve(&blessed, &fixed_home()).expect_err("pip without a hash refuses");
        assert!(
            err.to_string().contains("closure hashes pending"),
            "echoes the note: {err}"
        );
    }

    #[test]
    fn direct_command_failure_is_a_plain_failed_outcome() {
        let r = recipe(
            Ecosystem::Cargo,
            "taplo-cli",
            "0.10.0",
            VerificationTier::CargoLocked,
        );
        let blessed = BlessedRecipe::resolve("taplo", &r, &manifest("taplo", "0.10.0")).unwrap();
        let (_tmp, home) = temp_home();
        let plan = InstallPlan::resolve(&blessed, &home).unwrap();
        let runner = FakeRunner {
            calls: RefCell::new(Vec::new()),
            succeed: false,
        };
        let fetcher = FakeFetcher::new(Vec::new());
        let outcome = execute(&plan, &runner, &fetcher);
        assert!(!outcome.success);
        assert!(
            outcome.log.iter().any(|l| l.contains("exited with")),
            "a non-zero exit is a plain failure: {:?}",
            outcome.log,
        );
    }

    #[test]
    fn pinned_fix_it_is_pinning_honest_per_ecosystem() {
        let cases = [
            (
                recipe(
                    Ecosystem::Cargo,
                    "taplo-cli",
                    "0.10.0",
                    VerificationTier::CargoLocked,
                ),
                "cargo install taplo-cli --version =0.10.0 --locked",
            ),
            (
                recipe(
                    Ecosystem::Go,
                    "golang.org/x/tools/gopls",
                    "v0.22.0",
                    VerificationTier::GoChecksumdb,
                ),
                "go install golang.org/x/tools/gopls@v0.22.0",
            ),
            (
                recipe(
                    Ecosystem::Pip,
                    "cmake-language-server",
                    "0.1.11",
                    VerificationTier::PipHashes,
                ),
                "pip install cmake-language-server==0.1.11",
            ),
            (
                recipe(
                    Ecosystem::Npm,
                    "bash-language-server",
                    "5.6.0",
                    VerificationTier::NpmTarballSha512,
                ),
                "npm install -g bash-language-server@5.6.0",
            ),
        ];
        for (r, expected_command) in cases {
            let blessed = BlessedRecipe::resolve("srv", &r, &manifest("srv", &r.version)).unwrap();
            let text = blessed.pinned_fix_it();
            assert!(
                text.contains(expected_command),
                "shows the pinned form: {text}"
            );
            // Never an unpinned form: an exact version or `@` pin is always present.
            assert!(
                text.contains('=') || text.contains('@'),
                "the form is pinned: {text}",
            );
        }
    }

    // ── the managed server home (ls-manager 01) ──────────────────────

    #[test]
    fn npm_co_install_companion_installs_into_the_same_version_dir() {
        let server_bytes = b"pretend typescript-language-server tarball".to_vec();
        let companion_bytes = b"pretend typescript tarball".to_vec();
        let mut r = recipe(
            Ecosystem::Npm,
            "typescript-language-server",
            "4.4.0",
            VerificationTier::NpmTarballSha512,
        );
        r.hash = Some(format!(
            "sha512-{}",
            base64_standard(&sha512(&server_bytes))
        ));
        r.co_install = vec![CoInstall {
            package: "typescript".to_owned(),
            version: "5.5.4".to_owned(),
            hash: Some(format!(
                "sha512-{}",
                base64_standard(&sha512(&companion_bytes))
            )),
        }];
        let blessed = BlessedRecipe::resolve(
            "typescript-language-server",
            &r,
            &manifest("typescript-language-server", "4.4.0"),
        )
        .unwrap();
        let (_tmp, home) = temp_home();
        let plan = InstallPlan::resolve(&blessed, &home).expect("npm with companion resolves");
        let dest = home
            .version_dir("typescript-language-server", "4.4.0")
            .expect("version dir");

        let server_url = "https://registry.npmjs.org/typescript-language-server/-/typescript-language-server-4.4.0.tgz";
        let companion_url = "https://registry.npmjs.org/typescript/-/typescript-5.5.4.tgz";
        let runner = FakeRunner::ok();
        let fetcher = MapFetcher::new(&[
            (server_url, server_bytes.as_slice()),
            (companion_url, companion_bytes.as_slice()),
        ]);
        let outcome = execute(&plan, &runner, &fetcher);
        assert!(outcome.success, "{:?}", outcome.log);

        // Both pinned tarballs were fetched-and-verified.
        assert_eq!(
            fetcher.fetched.borrow().as_slice(),
            [server_url, companion_url]
        );
        // ONE npm invocation carries the whole pinned set into ONE prefix — the
        // structural require-walk pin: with a shared `--prefix`, npm lands both
        // packages side by side in `<version-dir>/lib/node_modules/`, where
        // Node's require walk resolves the companion from inside the same
        // version dir. The pair travels as one unit.
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1, "the pinned set installs in one invocation");
        let cmd = &calls[0];
        assert_eq!(cmd.program(), "npm");
        assert_eq!(cmd.args()[2], "--prefix");
        assert_eq!(cmd.args()[3], dest.to_string_lossy().as_ref());
        let tarballs = cmd.args().iter().filter(|a| is_tgz(a)).count();
        assert_eq!(
            tarballs,
            2,
            "server and companion tarballs install together: {:?}",
            cmd.args(),
        );
        assert_eq!(
            cmd.args().last().map(String::as_str),
            Some("--ignore-scripts")
        );
    }

    #[test]
    fn npm_companion_hash_mismatch_installs_nothing() {
        let server_bytes = b"the real server tarball".to_vec();
        let mut r = recipe(
            Ecosystem::Npm,
            "typescript-language-server",
            "4.4.0",
            VerificationTier::NpmTarballSha512,
        );
        r.hash = Some(format!(
            "sha512-{}",
            base64_standard(&sha512(&server_bytes))
        ));
        r.co_install = vec![CoInstall {
            package: "typescript".to_owned(),
            version: "5.5.4".to_owned(),
            // Pins the hash of DIFFERENT bytes than the fetcher will return.
            hash: Some(format!(
                "sha512-{}",
                base64_standard(&sha512(b"the real companion bytes"))
            )),
        }];
        let blessed = BlessedRecipe::resolve(
            "typescript-language-server",
            &r,
            &manifest("typescript-language-server", "4.4.0"),
        )
        .unwrap();
        let (_tmp, home) = temp_home();
        let plan = InstallPlan::resolve(&blessed, &home).unwrap();

        let runner = FakeRunner::ok();
        let fetcher = MapFetcher::new(&[
            (
                "https://registry.npmjs.org/typescript-language-server/-/typescript-language-server-4.4.0.tgz",
                server_bytes.as_slice(),
            ),
            (
                "https://registry.npmjs.org/typescript/-/typescript-5.5.4.tgz",
                b"TAMPERED companion bytes".as_slice(),
            ),
        ]);
        let outcome = execute(&plan, &runner, &fetcher);

        assert!(!outcome.success, "a companion mismatch fails the set");
        assert!(
            runner.calls.borrow().is_empty(),
            "NOTHING installs when any member of the pinned set fails to verify \
             — the unit never lands half-installed",
        );
        assert!(
            !plan.destination().exists(),
            "a refused install leaves no version dir behind",
        );
    }

    #[test]
    fn npm_co_install_without_hash_refuses() {
        let mut r = recipe(
            Ecosystem::Npm,
            "typescript-language-server",
            "4.4.0",
            VerificationTier::NpmTarballSha512,
        );
        r.hash = Some(format!("sha512-{}", base64_standard(&sha512(b"srv"))));
        r.co_install = vec![CoInstall {
            package: "typescript".to_owned(),
            version: "5.5.4".to_owned(),
            hash: None,
        }];
        let blessed = BlessedRecipe::resolve(
            "typescript-language-server",
            &r,
            &manifest("typescript-language-server", "4.4.0"),
        )
        .unwrap();
        let err = InstallPlan::resolve(&blessed, &fixed_home())
            .expect_err("a hashless companion refuses the whole plan");
        assert!(
            err.to_string().contains("co-install `typescript`"),
            "the refusal names the companion: {err}",
        );
    }

    #[test]
    fn display_command_names_the_managed_home_destination() {
        let mut npm = recipe(
            Ecosystem::Npm,
            "bash-language-server",
            "5.6.0",
            VerificationTier::NpmTarballSha512,
        );
        npm.hash = Some(format!("sha512-{}", base64_standard(&sha512(b"x"))));
        let mut pip = recipe(
            Ecosystem::Pip,
            "cmake-language-server",
            "0.1.11",
            VerificationTier::PipHashes,
        );
        pip.hash = Some("sha256:1111".to_owned());
        let cases = [
            (
                recipe(
                    Ecosystem::Cargo,
                    "taplo-cli",
                    "0.10.0",
                    VerificationTier::CargoLocked,
                ),
                "--root /mh/srv/0.10.0",
            ),
            (
                recipe(
                    Ecosystem::Go,
                    "golang.org/x/tools/gopls",
                    "v0.22.0",
                    VerificationTier::GoChecksumdb,
                ),
                "GOBIN=/mh/srv/v0.22.0/bin",
            ),
            (npm, "--prefix /mh/srv/5.6.0"),
            (pip, "venv /mh/srv/0.1.11"),
        ];
        for (r, expected) in cases {
            let blessed = BlessedRecipe::resolve("srv", &r, &manifest("srv", &r.version)).unwrap();
            let plan = InstallPlan::resolve(&blessed, &fixed_home()).expect("plan resolves");
            let shown = plan.display_command();
            assert!(
                shown.contains(expected),
                "the consent overlay names the managed home: {shown}",
            );
        }
    }

    /// An installer emulator: performs each ecosystem's *filesystem effect*
    /// using ONLY the argv/env the containment code built — npm honors
    /// `--prefix`, cargo honors `--root`, go honors `GOBIN`, venv/pip write
    /// where the command names. If a built invocation named anywhere outside
    /// the version dir, files would land there and the no-strays walk in the
    /// containment test would fail.
    struct EmulatingRunner;

    impl EmulatingRunner {
        fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a String> {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
        }
    }

    impl CommandRunner for EmulatingRunner {
        fn run(&self, command: &InstallCommand) -> Result<CommandOutcome> {
            let args = command.args();
            match command.program() {
                "npm" => {
                    let prefix = PathBuf::from(
                        Self::arg_after(args, "--prefix").expect("npm carries --prefix"),
                    );
                    let node_modules = prefix.join("lib").join("node_modules");
                    let bin = prefix.join("bin");
                    for (i, _tgz) in args.iter().filter(|a| is_tgz(a)).enumerate() {
                        let pkg = node_modules.join(format!("pkg{i}"));
                        std::fs::create_dir_all(&pkg)?;
                        std::fs::write(pkg.join("package.json"), b"{}")?;
                        std::fs::create_dir_all(&bin)?;
                        std::fs::write(bin.join(format!("pkg{i}")), b"#!node")?;
                    }
                }
                "cargo" => {
                    let root = PathBuf::from(
                        Self::arg_after(args, "--root").expect("cargo carries --root"),
                    );
                    let bin = root.join("bin");
                    std::fs::create_dir_all(&bin)?;
                    std::fs::write(bin.join(&args[1]), b"elf")?;
                }
                "go" => {
                    let gobin = command
                        .envs()
                        .iter()
                        .find(|(k, _)| k == "GOBIN")
                        .map(|(_, v)| PathBuf::from(v))
                        .expect("go carries GOBIN");
                    std::fs::create_dir_all(&gobin)?;
                    std::fs::write(gobin.join("gopls"), b"elf")?;
                }
                "python3" => {
                    // `python3 -m venv <dir>`.
                    let dir = PathBuf::from(args.get(2).expect("venv names its dir"));
                    let bin = dir.join("bin");
                    std::fs::create_dir_all(&bin)?;
                    std::fs::write(bin.join("pip"), b"#!py")?;
                }
                pip if pip.ends_with("/pip") => {
                    // The venv's own pip: entry points land beside it.
                    let bin = Path::new(pip).parent().expect("pip lives in bin/");
                    std::fs::write(bin.join("cmake-language-server"), b"#!py")?;
                }
                other => panic!("unexpected program `{other}`"),
            }
            Ok(CommandOutcome {
                success: true,
                code: Some(0),
                output: String::new(),
            })
        }
    }

    /// Every file under `dir`, recursively.
    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(files_under(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn every_ecosystem_lands_contained_with_a_bin_dir_and_no_strays() {
        // The acceptance pin: point the data dir at a tempdir subdir, install
        // per ecosystem through an emulator that honors only the argv/env the
        // containment code built, then assert the ruled invariant (executables
        // at `<name>/<version>/bin/`) and that NOTHING landed outside the
        // version dir.
        let npm_bytes = b"npm server tarball".to_vec();
        let companion_bytes = b"npm companion tarball".to_vec();
        let mut npm = recipe(
            Ecosystem::Npm,
            "typescript-language-server",
            "4.4.0",
            VerificationTier::NpmTarballSha512,
        );
        npm.hash = Some(format!("sha512-{}", base64_standard(&sha512(&npm_bytes))));
        npm.co_install = vec![CoInstall {
            package: "typescript".to_owned(),
            version: "5.5.4".to_owned(),
            hash: Some(format!(
                "sha512-{}",
                base64_standard(&sha512(&companion_bytes))
            )),
        }];
        let mut pip = recipe(
            Ecosystem::Pip,
            "cmake-language-server",
            "0.1.11",
            VerificationTier::PipHashes,
        );
        pip.hash = Some(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
        );
        let cases = [
            ("typescript-language-server", npm),
            (
                "taplo",
                recipe(
                    Ecosystem::Cargo,
                    "taplo-cli",
                    "0.10.0",
                    VerificationTier::CargoLocked,
                ),
            ),
            (
                "gopls",
                recipe(
                    Ecosystem::Go,
                    "golang.org/x/tools/gopls",
                    "v0.22.0",
                    VerificationTier::GoChecksumdb,
                ),
            ),
            ("cmake-language-server", pip),
        ];
        let fetcher = MapFetcher::new(&[
            (
                "https://registry.npmjs.org/typescript-language-server/-/typescript-language-server-4.4.0.tgz",
                npm_bytes.as_slice(),
            ),
            (
                "https://registry.npmjs.org/typescript/-/typescript-5.5.4.tgz",
                companion_bytes.as_slice(),
            ),
        ]);
        for (server, r) in cases {
            let (tmp, home) = temp_home();
            let blessed =
                BlessedRecipe::resolve(server, &r, &manifest(server, &r.version)).unwrap();
            let plan = InstallPlan::resolve(&blessed, &home).expect("plan resolves");
            let outcome = execute(&plan, &EmulatingRunner, &fetcher);
            assert!(outcome.success, "{server}: {:?}", outcome.log);

            let dest = home.version_dir(server, &r.version).expect("version dir");
            // The ruled invariant: a runnable executable at `<version-dir>/bin/`.
            let bin_entries = files_under(&dest.join("bin"));
            assert!(
                !bin_entries.is_empty(),
                "{server}: the version dir exposes executables at bin/",
            );
            // No strays: everything under the data-dir stand-in lives inside
            // the version dir (the mislocation-detector spirit).
            for file in files_under(tmp.path()) {
                assert!(
                    file.starts_with(&dest),
                    "{server}: `{}` landed outside the version dir {}",
                    file.display(),
                    dest.display(),
                );
            }
            // The npm require-walk structural pin: the companion is a SIBLING
            // of the server package in the same `lib/node_modules/`.
            if server == "typescript-language-server" {
                let node_modules = dest.join("lib").join("node_modules");
                assert!(
                    node_modules.join("pkg0").is_dir() && node_modules.join("pkg1").is_dir(),
                    "server and companion sit side by side in the version dir's \
                     lib/node_modules, where Node's require walk finds the companion",
                );
            }
        }
    }
}
