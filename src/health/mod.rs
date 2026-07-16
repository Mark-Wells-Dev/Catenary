// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Health model: the single source of truth for "is it working?".
//!
//! Every check `catenary doctor` performs — config migration walk, validation,
//! unknown keys, unreferenced servers, duplicate extensions, project-config
//! warnings, server probes, the routing table, and hooks/instructions/filter
//! staleness — resolves here into **typed findings** ([`Finding`]): a
//! [`Severity`], a stable [`FindingCode`], a one-line message, and optional
//! fix-it text carried as data rather than baked into a render.
//!
//! `doctor` (see [`crate::cli::doctor`]) is a one-shot renderer over this
//! model. The TUI (a later phase) renders the same findings live. The two never
//! diverge in *what they can know* — the only permitted difference is the data
//! **feed**, expressed through the [`servers::HealthFeed`] seam:
//!
//! - the **probe feed** ([`servers::ProbeFeed`]) — doctor's own one-shot
//!   `initialize` probes, which work with the daemon down;
//! - the **snapshot feed** — `state.json`, consumed by the TUI in a later
//!   phase (this module defines the trait; the consumer is out of scope here).
//!
//! Routed-vs-dormant derivation lives in the model
//! ([`servers::is_intent_routed`]): a server is *routed* iff a configured
//! language binding targets it and that language is **activity-live** — a
//! tracked session touched a file of it (tui-rework 09) — or the server is
//! explicitly configured. Everything else configured is *dormant inventory*,
//! never a warning; presence in a fixture directory no one opened lights
//! nothing.

pub mod config_checks;
pub mod install_checks;
pub mod servers;
pub mod skew;

/// Severity of a health finding — the `:checkhealth` ladder.
///
/// Ratified 2026-07-07 into five tiers so a renderer can distinguish an
/// intent-broken failure from a broken fact, an unchosen gap from silent
/// inventory. The ladder, most severe first:
///
/// - [`Severity::Fatal`] — *intent-broken*: a routed server the user
///   configured or installed that isn't functioning (a missing binary counts
///   when the server is explicitly configured — a PATH break eating a chosen
///   server ranks with a crash).
/// - [`Severity::Error`] — *broken facts*, intent-independent: config
///   validation failures, unresolvable server refs, unreadable hook
///   registrations.
/// - [`Severity::Warning`] — *impaired or stale*: 027 coverage degradation,
///   respawn-looping-but-up, stale hooks, version skew.
/// - [`Severity::Suggestion`] — *unchosen gaps*: a live workspace language
///   with a shipped default binding but no binary and no explicit config.
///   Never a problem, never in the verdict count — an honest green may still
///   carry suggestions.
/// - [`Severity::Ok`] — a confirmed-healthy state; never shouts.
/// - [`Severity::Info`] — non-actionable inventory or a note (dormant server);
///   never shouts.
///
/// [`Severity::Fatal`], [`Severity::Error`], and [`Severity::Warning`] are the
/// *problems* ([`Finding::is_problem`]) a health verdict counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// An intent-broken failure — a configured/installed routed server down.
    Fatal,
    /// A user-actionable failure of fact (config error, unresolvable ref).
    Error,
    /// A user-actionable warning (unknown key, stale hooks, version skew).
    Warning,
    /// An unchosen gap — a live language wanting a not-yet-installed server.
    Suggestion,
    /// A confirmed-healthy state (server ready, hooks match, up to date).
    Ok,
    /// Non-actionable inventory or a note (dormant server, disable toggle).
    Info,
}

impl Severity {
    /// Whether this severity denotes a user-actionable *problem*
    /// ([`Severity::Fatal`], [`Severity::Error`], or [`Severity::Warning`]) —
    /// the tiers the verdict counts. [`Severity::Suggestion`] is deliberately
    /// excluded: a suggestion never displaces a problem or dents the verdict.
    #[must_use]
    pub const fn is_problem(self) -> bool {
        matches!(self, Self::Fatal | Self::Error | Self::Warning)
    }

    /// A stable sort rank (0 = most severe), so a renderer can order the
    /// problems pane Fatal → Error → Warning → Suggestion without matching on
    /// each variant. Ok/Info share the tail (they are never in the pane).
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Fatal => 0,
            Self::Error => 1,
            Self::Warning => 2,
            Self::Suggestion => 3,
            Self::Ok => 4,
            Self::Info => 5,
        }
    }

    /// The severity's label word (`Fatal` / `Error` / `Warning` / `Suggestion`
    /// / `Ok` / `Info`) — the problems-pane and doctor prefix.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fatal => "Fatal",
            Self::Error => "Error",
            Self::Warning => "Warning",
            Self::Suggestion => "Suggestion",
            Self::Ok => "Ok",
            Self::Info => "Info",
        }
    }
}

/// A stable, machine-readable identifier for a class of [`Finding`].
///
/// The code is the contract tests pin: a renderer may reflow the prose, but the
/// set of codes a given config/daemon state produces may not silently change.
/// Reach for [`FindingCode::as_str`] for the stable kebab-case string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingCode {
    /// A pre-namespacing top-level `[server.*]`/`[language.*]`/`[linter.*]`
    /// table that moved under `[lsp.*]`/`[linter.rule.*]` (linters ticket 04).
    ConfigLegacyNamespace,
    /// A `[lsp.language.*]` entry using the removed `inherit` field.
    ConfigLanguageInherit,
    /// A `[lsp.language.*]` entry inlining `[lsp.server.*]` definition fields.
    ConfigLanguageInlinesServer,
    /// A `[commands]` entry using a removed denylist-era field.
    ConfigLegacyCommandsField,
    /// A config section that failed validation and was quarantined — defaulted
    /// out so the load succeeds on the valid remainder (bug 110). Its consumers
    /// degrade; the fix restores them.
    ConfigSectionQuarantined,
    /// A config key no Catenary schema defines (misc 131).
    ConfigUnknownKey,
    /// A `[roots] pinned` entry whose path is missing on disk — kept in the
    /// config (never pruned) but not restored at boot (misc 175).
    ConfigPinnedRootMissing,
    /// A `Config::validate` error.
    ConfigValidationError,
    /// A user-defined server no `[lsp.language.*]` entry routes to.
    ConfigUnreferencedServer,
    /// An extension claimed by two `[lsp.language.*]` entries.
    ConfigDuplicateExtension,
    /// A `[lsp.server.<key>]` whose `args` contain the server key itself — the
    /// leftover-launcher shape from a pre-162 `command`+`args` pair (bug 94).
    ConfigLeftoverLauncherArgs,
    /// A project `.catenary.toml` using a removed bare `lsp`/`enabled` toggle.
    ProjectRemovedToggle,
    /// A project `.catenary.toml` that fails to load.
    ProjectLoadError,
    /// A project `.catenary.toml` summary (language/server counts).
    ProjectSummary,
    /// A project per-root feeder/surface `disable` toggle in effect.
    ProjectDisableToggle,
    /// A project `[lsp.server.*]` with a `command` nothing references.
    ProjectOrphanServer,
    /// A project `[lsp.language.*]` referencing an undefined server.
    ProjectUnresolvedServerRef,
    /// Project `[commands]` enforcement keys ignored at project scope.
    ProjectIgnoredEnforcement,
    /// A routed server that failed its probe (missing binary, init failure)
    /// with intent evidence — configured or installed. A [`Severity::Fatal`].
    ServerRoutedBroken,
    /// A live workspace language with a shipped default binding but no binary
    /// and no explicit config — an unchosen gap ([`Severity::Suggestion`]).
    ServerInstallSuggestion,
    /// A configured server nothing routes to — dormant inventory.
    ServerDormant,
    /// A routed server that is **unverified** (a custom `[lsp.server.*]` def
    /// absent from the blessed manifest) — enrichment-only, never a diagnostics
    /// source (diagnostics-debt 04b). A [`Severity::Warning`] disclosure.
    ServerEnrichmentOnly,
    /// A server that probed ready.
    ServerReady,
    /// A ready blessed server whose reported `serverInfo.version` matches none
    /// of its blessed manifest pins (lsm 03) — the running binary is not the
    /// vetted one (PATH residue, e.g. a `pacman -Syu` swap). Advisory
    /// [`Severity::Info`]: running your own version is a choice, not a fault,
    /// and nothing gates on it.
    ServerVersionDrift,
    /// A background auto-install record on the running daemon (lsm 05) — an
    /// `installing`/`installed` standing, rendered as non-problem
    /// [`Severity::Info`] inventory (the doctor-visible half of the
    /// announcement floor).
    ServerAutoInstall,
    /// A background auto-install that failed this daemon lifetime (lsm 05) —
    /// skip-with-finding: a [`Severity::Warning`] naming the reason; the next
    /// session start's detection retries naturally.
    ServerAutoInstallFailed,
    /// The running daemon serves a different build than this binary.
    VersionSkew,
    /// A connected bridge links a different `catenary-mcp` protocol version than
    /// the daemon (ws41-02) — the bridge or the daemon is older. Persists until
    /// the versions agree; the cure names the older side (`/mcp` or a binary
    /// bounce).
    BridgeVersionMismatch,
    /// Installed host hooks diverge from the shipped set.
    HooksStale,
    /// Installed host hooks are missing from the plugin cache.
    HooksMissing,
    /// A host plugin registration could not be read or parsed.
    HooksUnreadable,
    /// Installed host hooks match the shipped set.
    HooksOk,
    /// A host integration is not installed.
    NotInstalled,
    /// Installed agent instructions diverge from the shipped set.
    InstructionsStale,
    /// Installed agent instructions are up to date.
    InstructionsOk,
    /// The running binary differs from what `$PATH` resolves.
    PathMismatch,
    /// The running binary matches `$PATH`.
    PathOk,
    /// A legacy `constrained_bash.py` hook is still configured.
    LegacyScript,
    /// The resolved command-filter status.
    CommandFilterStatus,
    /// The external signed registry could not be refreshed (fetch failed or the
    /// cached copy is past its freshness horizon); a cached/seed registry is in
    /// use (tui-rework 08).
    RegistryStale,
    /// A fetched (or cached) registry artifact failed ed25519 signature
    /// verification against the in-binary trust root — a loud finding; the loader
    /// fell back to the cached/seed registry (tui-rework 08).
    RegistryBadSignature,
}

impl FindingCode {
    /// The stable kebab-case string form of this code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConfigLegacyNamespace => "config-legacy-namespace",
            Self::ConfigLanguageInherit => "config-language-inherit",
            Self::ConfigLanguageInlinesServer => "config-language-inlines-server",
            Self::ConfigLegacyCommandsField => "config-legacy-commands-field",
            Self::ConfigSectionQuarantined => "config-section-quarantined",
            Self::ConfigUnknownKey => "config-unknown-key",
            Self::ConfigPinnedRootMissing => "config-pinned-root-missing",
            Self::ConfigValidationError => "config-validation-error",
            Self::ConfigUnreferencedServer => "config-unreferenced-server",
            Self::ConfigDuplicateExtension => "config-duplicate-extension",
            Self::ConfigLeftoverLauncherArgs => "config-leftover-launcher-args",
            Self::ProjectRemovedToggle => "project-removed-toggle",
            Self::ProjectLoadError => "project-load-error",
            Self::ProjectSummary => "project-summary",
            Self::ProjectDisableToggle => "project-disable-toggle",
            Self::ProjectOrphanServer => "project-orphan-server",
            Self::ProjectUnresolvedServerRef => "project-unresolved-server-ref",
            Self::ProjectIgnoredEnforcement => "project-ignored-enforcement",
            Self::ServerRoutedBroken => "server-routed-broken",
            Self::ServerInstallSuggestion => "server-install-suggestion",
            Self::ServerDormant => "server-dormant",
            Self::ServerEnrichmentOnly => "server-enrichment-only",
            Self::ServerReady => "server-ready",
            Self::ServerVersionDrift => "server-version-drift",
            Self::ServerAutoInstall => "server-auto-install",
            Self::ServerAutoInstallFailed => "server-auto-install-failed",
            Self::VersionSkew => "version-skew",
            Self::BridgeVersionMismatch => "bridge-version-mismatch",
            Self::HooksStale => "hooks-stale",
            Self::HooksMissing => "hooks-missing",
            Self::HooksUnreadable => "hooks-unreadable",
            Self::HooksOk => "hooks-ok",
            Self::NotInstalled => "not-installed",
            Self::InstructionsStale => "instructions-stale",
            Self::InstructionsOk => "instructions-ok",
            Self::PathMismatch => "path-mismatch",
            Self::PathOk => "path-ok",
            Self::LegacyScript => "legacy-script",
            Self::CommandFilterStatus => "command-filter-status",
            Self::RegistryStale => "registry-stale",
            Self::RegistryBadSignature => "registry-bad-signature",
        }
    }
}

/// Stale installed-vs-expected content, carried on a [`Finding`] so a renderer
/// can show a unified diff on demand (`catenary doctor --diff`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleDiff {
    /// The content currently installed on disk.
    pub installed: String,
    /// The content Catenary ships and expects.
    pub expected: String,
}

/// A single typed health finding: severity, a stable code, a one-line message,
/// and optional fix-it guidance (and, for staleness findings, a diff payload).
///
/// The message is the rendered one-liner; `fix_it` is the misc-120 guidance as
/// data — a renderer shows it indented under the message, or a future guided
/// mutation turns it into an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The stable class identifier.
    pub code: FindingCode,
    /// How loud this finding is.
    pub severity: Severity,
    /// The one-line human message (no leading glyph or color).
    pub message: String,
    /// Optional fix-it guidance, rendered under the message.
    pub fix_it: Option<String>,
    /// Optional routing provenance — which root and file(s) made this finding's
    /// language activity-live — rendered under the fix-it line (tui-rework 09,
    /// item 4). Carried as its own field, never folded into [`Self::message`].
    pub provenance: Option<String>,
    /// Optional stale-content diff, rendered only under `--diff`.
    pub diff: Option<StaleDiff>,
}

impl Finding {
    /// Build a finding with a code, severity, and message.
    #[must_use]
    pub fn new(code: FindingCode, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code,
            severity,
            message: message.into(),
            fix_it: None,
            provenance: None,
            diff: None,
        }
    }

    /// Attach fix-it guidance (chainable).
    #[must_use]
    pub fn with_fix_it(mut self, fix_it: impl Into<String>) -> Self {
        self.fix_it = Some(fix_it.into());
        self
    }

    /// Attach routing provenance, rendered under the fix-it line (chainable).
    #[must_use]
    pub fn with_provenance(mut self, provenance: impl Into<String>) -> Self {
        self.provenance = Some(provenance.into());
        self
    }

    /// Attach a stale-content diff payload (chainable).
    #[must_use]
    pub fn with_diff(mut self, diff: StaleDiff) -> Self {
        self.diff = Some(diff);
        self
    }

    /// Whether this finding is a user-actionable problem
    /// ([`Severity::is_problem`]).
    #[must_use]
    pub const fn is_problem(&self) -> bool {
        self.severity.is_problem()
    }
}
