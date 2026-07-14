// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Per-server behavior profiles (misc 157; projected from the manifest,
//! diagnostics-debt 04).
//!
//! One engine-internal lookup, [`ServerProfile::for_server`], resolves a server's
//! **conformance settings** — the engine-required invariants Catenary needs to
//! function correctly against that server. These are distinct from the *default
//! configurables* (the shipped [`crate::config::server::ServerDef`] layer in
//! `defaults/servers.toml`): configurables are user/project-overridable through
//! today's config merge; conformance settings resolve through this profile and are
//! applied **after** every user/project merge and are **never** overridable by any
//! config layer. This is the enforcement-keys boundary inverted — enforcement keys
//! are user-scope-only; conformance settings are shipped/engine-scope-only.
//!
//! The knowledge lives in exactly one place — the blessed **manifest**
//! (`defaults/blessed-manifest.toml`, `[blessed.*]` + `[discipline.<server>]`
//! tables). This module is the *projection* of that data onto the shape the seams
//! consume: the former hand-coded profile table became the manifest's build-time
//! projection (diagnostics-debt 04 — generated from the manifest, not a rival
//! home). The projection reads the process-wide **active** manifest
//! ([`crate::recipes::active_manifest`]) — seeded with the embedded seed and
//! upgraded in place by the daemon's registry refresh (diagnostics-debt 04b) — so
//! a re-pin ships updated discipline/casing/classification without a binary
//! release; the seed remains the offline floor and the directional-safety default.
//! The consuming seams (client-capability construction, initialization-option
//! assembly, the diagnostics pull gate) each make a single profile call and are
//! themselves server-name-blind: no seam special-cases a server by name. The CI
//! conformance matrix re-verifies every profile invariant on every re-pin, which
//! is what makes carrying these settings safe (maintainer direction, bug 82:
//! discipline knowledge is "not something I want to be 'configurable' by the user
//! but set on a case by case basis").
//!
//! The profile also carries the **blessed/unverified classification**
//! (diagnostics-debt 04b / DESIGN §"The blessed set"): a server absent from the
//! manifest's blessed set is an unverified custom def and is **enrichment-only**
//! ([`ServerProfile::is_enrichment_only`]) — no diagnostics capability advertised,
//! publishes ignored, no batch sync lifecycle — while grep/glob queries and
//! watched-files delivery are untouched.
//!
//! Three conformance invariants are cased today:
//!
//! - **rust-analyzer** — [`ServerProfile::suppresses_pull_diagnostics`]. Catenary
//!   is push-first for the Rust family: RA suppressing native pushes when the
//!   client advertises pull would drop warnings/hints the push channel carries (RA
//!   #18709), and the flycheck family (clippy/cargo) is push-only upstream so pull
//!   was never a complete answer. The profile withholds the
//!   `textDocument.diagnostic` client capability *and* gates the client-side pull
//!   path, so RA's native pushes are the sole diagnostic channel — airtight even
//!   if RA spontaneously advertises `diagnosticProvider`.
//! - **gopls** — [`ServerProfile::forbidden_initialization_options`] +
//!   `discipline = "pull"`. Pull is **re-enabled** (diagnostics-debt 05): bug 87
//!   had forced `pullDiagnostics: false` (conformance run 8) because in pull mode
//!   gopls stopped pushing real diagnostics and published empty placeholders the
//!   then-unconditional heard-empty rule (misc 153) read as authoritative — the
//!   pull that would fetch the real results was suppressed and dirty files read
//!   `[clean]`. Ledger 03's version-echo settlement retired that defeat
//!   **structurally**: an unversioned empty placeholder echoes no version, so it
//!   settles nothing; the debt stays open and the pull (`textDocument/diagnostic`)
//!   is what settles it, pull-first. So the `pullDiagnostics: false` override is
//!   lifted and gopls rides its native pull mode, conformance-gated against the
//!   pin (`conformance_gopls_pull_mode`). `diagnosticsDelay` stays **enforced
//!   absent** (maintainer ruling after conformance run 9): `"0s"` decouples
//!   publishing from analysis — every publish fired ~1 ms after its document
//!   event, empty, on the not-yet-checked snapshot, and the completed type-check
//!   never got a publish of its own. The debounce is not a blind window to zero
//!   out; it is the coupling between analysis completion and the publish. A user
//!   reasoning "zero the delay to minimize latency" would reintroduce exactly
//!   that, so the key is stripped from whatever the config layers produce and
//!   gopls's own default is the only value that can ever reach the server. This
//!   is a conformance setting, not a `defaults/servers.toml` entry, because a user
//!   `[lsp.server.gopls]` replaces the shipped default wholesale (no field merge —
//!   see `test_builtin_no_merge`), which would silently drop it; and because it
//!   must win over a user who sets it otherwise.
//! - **lattice** — [`ServerProfile::declares_push`] (misc 187). Its publish
//!   contract is pinned cross-repo (misc 153 / Lattice ticket 16 / its decision
//!   022): a publish on **every** `didOpen`, including unchanged files, with an
//!   explicit `[]` for clean. The retrieval evidence bar arms on this
//!   declaration even before the connection's first publish, closing the
//!   first-run false-`[clean]` window that per-connection demonstration
//!   (`has_ever_published`) reopens on every respawn and daemon bounce.

use serde_json::Value;

use crate::config::merge::deep_merge;

/// The resolved conformance profile for one LSP server.
///
/// Built by [`Self::for_server`] as the projection of the server's manifest
/// [`crate::recipes::DisciplineRecord`] onto the shape the seams consume. All
/// seams consult a `ServerProfile` rather than testing a server name, so the
/// per-server knowledge stays in the manifest and the seams stay
/// server-name-blind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerProfile {
    /// When set, the server is **unverified** — a custom `[lsp.server.*]` def
    /// absent from the blessed manifest — so it is **enrichment-only**
    /// (diagnostics-debt 04b / DESIGN §"The blessed set"): it advertises no
    /// diagnostics capability, its publishes are ignored if sent anyway, and the
    /// held-open batch sync lifecycle never engages it. grep/glob queries keep
    /// their own open→query→close cycle (it is the diagnostics *listening* that is
    /// withheld); watched-files are still delivered. A blessed server clears this.
    enrichment_only: bool,
    /// When set, the server must never receive the `textDocument.diagnostic`
    /// client capability, and must never be sent `textDocument/diagnostic`
    /// (advertised pull *or* best-effort probe) — its native pushes are the sole
    /// diagnostic channel.
    suppress_pull_diagnostics: bool,
    /// Conformance `initializationOptions` overlaid onto (and winning over) the
    /// user-supplied options at initialize time. `None` when the server has no
    /// forced options. Projected from the manifest's `forced_init_options` (a
    /// `toml::Value` converted to JSON — the shape the LSP wire uses).
    forced_initialization_options: Option<Value>,
    /// Top-level `initializationOptions` keys **enforced absent**: stripped after
    /// the user/forced merge, so no config layer can deliver them and the
    /// server's own built-in default is the only value that ever applies.
    /// Projected from the manifest's `forbidden_init_options`.
    forbidden_initialization_options: Vec<String>,
    /// When set, the server **contractually publishes** diagnostics for every
    /// opened document — a publish on every `didOpen` (including unchanged
    /// files), an explicit `[]` for clean. The retrieval evidence bar arms on
    /// this declaration alone, before the connection has demonstrated a single
    /// publish (misc 187).
    declares_push: bool,
    /// The declared debounce window in milliseconds for a
    /// [`crate::recipes::Discipline::Debounce`] server (ts-ls's declared
    /// 300–800 ms + 50 ms), projected from the manifest's `debounce_ms`
    /// (diagnostics-debt 05). `Some` only for a debounce-discipline row carrying
    /// the constant: the retrieval evidence bar awaits the version echo bounded
    /// by this declared constant — data riding the pin, re-verified at every
    /// re-pin, never a measured guess. `None` for every non-debounce discipline.
    debounce_ms: Option<u64>,
    /// The server's publisher discipline, projected verbatim from the manifest's
    /// [`crate::recipes::DisciplineRecord::discipline`] (misc 196). `None` for a
    /// server with no discipline row (an unverified custom def, or a casing-only
    /// row). The static [`Self::owes_answer`] contract (declared-push / debounce)
    /// is captured by the dedicated fields above; this field carries the
    /// **round-conditional** disciplines whose owed-answer depends on what the
    /// round delivered — [`crate::recipes::Discipline::Scan`] (owes its
    /// whole-workspace answer when the workspace pull is stimulated) and
    /// [`crate::recipes::Discipline::Diff`] (owes a publish on a round that
    /// delivered its save trigger). The floor's scan/diff arms read this via
    /// [`Self::is_scan`] / [`Self::is_diff`] together with the round context the
    /// static predicate cannot see (DESIGN §"Publisher-discipline metadata").
    discipline: Option<crate::recipes::Discipline>,
}

impl ServerProfile {
    /// Resolves the conformance profile for `server_name` — the single lookup
    /// every seam calls. Reads the process-wide **active** manifest
    /// ([`crate::recipes::active_manifest`]), so a re-pin's classification and
    /// casing reach the seams without a binary release (diagnostics-debt 04b). A
    /// server absent from the manifest's blessed set is **enrichment-only**; a
    /// blessed server with no discipline row (or one that needs no casing)
    /// resolves to the casing-free (but blessed) profile.
    #[must_use]
    pub fn for_server(server_name: &str) -> Self {
        let manifest = crate::recipes::active_manifest();
        // Classification consults the operator opt-in too
        // ([`crate::recipes::is_server_blessed`]); the discipline record is
        // manifest-only (an opt-in server carries no casing, which is the safe
        // default — no forced options, no pull suppression).
        Self::from_record(&manifest.discipline_for(server_name))
            .with_enrichment_only(!crate::recipes::is_server_blessed(server_name))
    }

    /// Projects a manifest [`crate::recipes::DisciplineRecord`] onto a
    /// `ServerProfile` — the single place the manifest's casing data becomes the
    /// shape the seams consume (diagnostics-debt 04).
    ///
    /// Leaves the blessed/unverified classification at its default (blessed —
    /// `enrichment_only == false`); [`Self::for_server`] sets it from the
    /// manifest's blessed set. A bare `from_record` is the casing projection only,
    /// used where the discipline record is already in hand.
    #[must_use]
    pub fn from_record(record: &crate::recipes::DisciplineRecord) -> Self {
        Self {
            enrichment_only: false,
            suppress_pull_diagnostics: record.suppress_pull,
            forced_initialization_options: record.forced_init_options.as_ref().map(toml_to_json),
            forbidden_initialization_options: record.forbidden_init_options.clone(),
            declares_push: record.declares_push,
            // The declared debounce bound rides the pin only for a debounce
            // discipline row (diagnostics-debt 05): the evidence bar awaits the
            // version echo bounded by this constant, never interpreting silence.
            // A `debounce_ms` present on a non-debounce row is ignored — the
            // constant governs only the discipline that reads it.
            debounce_ms: matches!(
                record.discipline,
                Some(crate::recipes::Discipline::Debounce)
            )
            .then_some(record.debounce_ms)
            .flatten(),
            discipline: record.discipline,
        }
    }

    /// Sets the enrichment-only (unverified) classification, consuming and
    /// returning `self` — the builder leg [`Self::for_server`] uses after the
    /// casing projection.
    #[must_use]
    const fn with_enrichment_only(mut self, enrichment_only: bool) -> Self {
        self.enrichment_only = enrichment_only;
        self
    }

    /// Whether this server is **enrichment-only** — unverified, so it is never a
    /// diagnostics source (diagnostics-debt 04b / DESIGN §"The blessed set").
    ///
    /// An enrichment-only server advertises no diagnostics capability
    /// ([`Self::shape_client_capabilities`] strips both the pull capability and
    /// `publishDiagnostics`), its publishes are ignored, and the batch sync
    /// lifecycle never engages it ([`super::LspServer::supports_diagnostics`] is
    /// `false`). grep/glob queries keep their own open→query→close cycle and
    /// watched-files are still delivered — it is the diagnostics *listening* that
    /// is withheld, exactly the delivery behaviour that is unverified.
    #[must_use]
    pub const fn is_enrichment_only(&self) -> bool {
        self.enrichment_only
    }

    /// Whether this server's client-side pull path is gated off.
    ///
    /// A `true` profile withholds the `textDocument.diagnostic` client capability
    /// ([`Self::shape_client_capabilities`]) and suppresses every pull the daemon
    /// would otherwise issue for the server.
    #[must_use]
    pub const fn suppresses_pull_diagnostics(&self) -> bool {
        self.suppress_pull_diagnostics
    }

    /// Whether this server contractually publishes diagnostics for every opened
    /// document (misc 187).
    ///
    /// A `true` profile arms the retrieval evidence bar from turn zero of a
    /// fresh connection — declaration-OR-demonstration — so a declared push
    /// server's never-heard files render the honest `[unverified]` instead of a
    /// probe-backed `[clean]` while its first publish is still in flight.
    #[must_use]
    pub const fn declares_push(&self) -> bool {
        self.declares_push
    }

    /// The declared debounce window for a debounce-discipline server, or `None`
    /// (diagnostics-debt 05).
    ///
    /// `Some(ms)` only when the manifest classifies the server
    /// [`crate::recipes::Discipline::Debounce`] and carries the declared
    /// `debounce_ms` constant. The retrieval evidence bar awaits the version echo
    /// bounded by this constant rather than by the generic dead-air budget — an
    /// arrival-based gate on the declared bound, never silence-interpretation.
    #[must_use]
    pub const fn debounce_ms(&self) -> Option<u64> {
        self.debounce_ms
    }

    /// Whether the server's **verified discipline STATICALLY owes an answer** for
    /// a round that stimulated it (diagnostics-debt 05).
    ///
    /// True for a server whose blessed adapter contracts a per-round response
    /// *unconditionally on stimulus*: a [`Self::declares_push`] server (a publish
    /// on every `didOpen`, explicit `[]` for clean — misc 187) or a
    /// debounce-discipline server ([`Self::debounce_ms`] — the version echo is
    /// owed within the declared bound). When the retrieval evidence bar arms and
    /// expires for such a server, the discipline said an answer was owed and none
    /// came — a **verified-contract violation**, the fault floor's third arm
    /// (DESIGN §"The floor is fault attribution"). A merely *demonstrated*-push
    /// server (has published before but declares nothing) owes no verified
    /// contract, so its expiry stays the softer silent wording, not a fault.
    ///
    /// This predicate is deliberately **round-context-free**, so the
    /// [`crate::recipes::Discipline::Scan`] and [`crate::recipes::Discipline::Diff`]
    /// disciplines are NOT covered here: their owed-answer is conditional on what
    /// the round delivered (a stimulated workspace pull for scan, a delivered save
    /// trigger for diff — DESIGN's table). Those arms live at their retrieval seams
    /// ([`super::super::bridge::diagnostics_server`]) and read [`Self::is_scan`] /
    /// [`Self::is_diff`] together with that round context; folding them in here
    /// would fire on the wrong rounds (a diff server that received no trigger owes
    /// nothing). Keeping this predicate static preserves ledger 05's
    /// declared-push/debounce arms and the unverified-never-fires boundary exactly.
    #[must_use]
    pub const fn owes_answer(&self) -> bool {
        self.declares_push || self.debounce_ms.is_some()
    }

    /// Whether the server is a [`crate::recipes::Discipline::Scan`] server
    /// (marksman-class scan-once; DESIGN §"Publisher-discipline metadata").
    ///
    /// A scan server owes its **whole-workspace answer**: when the round's
    /// `workspace/diagnostic` pull goes unanswered or refused by an alive scan
    /// server, that is a verified-contract violation (the floor's scan arm, misc
    /// 196). Round-conditional, so it is read at the workspace-pull seam rather
    /// than folded into the static [`Self::owes_answer`].
    #[must_use]
    pub fn is_scan(&self) -> bool {
        self.discipline == Some(crate::recipes::Discipline::Scan)
    }

    /// Whether the server is a [`crate::recipes::Discipline::Diff`] server
    /// (marksman diff-only; DESIGN §"Publisher-discipline metadata").
    ///
    /// A diff server owes a publish on any round that **delivered its trigger**
    /// (our lifecycle sends `didSave` for changed files): an alive diff server
    /// silent after its delivered save trigger violates its contract (the floor's
    /// diff arm, misc 196). A diff server with NO delivered trigger this round owes
    /// nothing. Round-conditional, so it is read at the per-file batch seam
    /// together with the "a save was delivered this round" signal rather than
    /// folded into the static [`Self::owes_answer`].
    #[must_use]
    pub fn is_diff(&self) -> bool {
        self.discipline == Some(crate::recipes::Discipline::Diff)
    }

    /// Applies the profile's client-capability shaping to a built `capabilities`
    /// object in place — the capability-construction seam.
    ///
    /// Two shapings, both by key removal so the capability shape stays
    /// byte-for-byte identical for every un-profiled (blessed, uncased) server:
    ///
    /// - a **pull-suppressed** server (rust-analyzer) loses `textDocument.diagnostic`
    ///   — it is never asked to serve pull diagnostics;
    /// - an **enrichment-only** server (unverified, diagnostics-debt 04b) loses
    ///   **both** `textDocument.diagnostic` (pull) *and*
    ///   `textDocument.publishDiagnostics` (push): Catenary advertises no
    ///   diagnostics capability at all, so the server has no signal to publish
    ///   into and its diagnostics listening is withheld. Every other advertised
    ///   capability (definition, references, symbols, …) survives, so grep/glob
    ///   enrichment and watched-files continue unchanged.
    pub fn shape_client_capabilities(&self, capabilities: &mut Value) {
        if !(self.suppress_pull_diagnostics || self.enrichment_only) {
            return;
        }
        let Some(text_document) = capabilities
            .get_mut("textDocument")
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        // A pull-suppressed OR enrichment-only server loses the pull capability.
        text_document.remove("diagnostic");
        // An enrichment-only server additionally loses the push capability — no
        // diagnostics advertisement whatsoever.
        if self.enrichment_only {
            text_document.remove("publishDiagnostics");
        }
    }

    /// Resolves the effective `initializationOptions` for the server's initialize
    /// — the init-options-assembly seam.
    ///
    /// The profile's forced conformance options are overlaid onto the
    /// `user`-supplied options and **win on conflict**: they are applied after the
    /// user/project merge and are not overridable (existing [`deep_merge`]
    /// semantics — the forced options are the overlay). A user's *unrelated* keys
    /// survive; a user value for a forced key is replaced. Forbidden keys are then
    /// stripped — enforced absent, whatever any layer supplied — so the server's
    /// own default is the only value that can apply. With no forced and no
    /// forbidden options this is the identity on `user`.
    #[must_use]
    pub fn effective_initialization_options(&self, user: Option<&Value>) -> Option<Value> {
        let mut merged = match (self.forced_initialization_options.as_ref(), user) {
            (Some(forced), Some(user)) => Some(deep_merge(user, forced)),
            (Some(forced), None) => Some(forced.clone()),
            (None, user) => user.cloned(),
        };
        if let Some(object) = merged.as_mut().and_then(Value::as_object_mut) {
            for key in &self.forbidden_initialization_options {
                object.remove(key.as_str());
            }
        }
        merged
    }
}

/// Convert a `toml::Value` (the manifest's forced-init-options shape) into a
/// `serde_json::Value` (the LSP wire shape).
///
/// Tables become objects, arrays become arrays, and scalars map across the two
/// value spaces. TOML datetimes stringify (LSP has no datetime scalar); a numeric
/// integer that overflows `i64` cannot occur in a TOML source, so the mapping is
/// total for any parsed manifest value.
fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::ServerProfile;
    use serde_json::json;

    #[test]
    fn rust_analyzer_suppresses_pull() {
        assert!(ServerProfile::for_server("rust-analyzer").suppresses_pull_diagnostics());
    }

    #[test]
    fn other_servers_do_not_suppress_pull() {
        for name in [
            "gopls",
            "lattice",
            "clangd",
            "typescript-language-server",
            "yX4Za",
        ] {
            assert!(
                !ServerProfile::for_server(name).suppresses_pull_diagnostics(),
                "{name} must not suppress pull",
            );
        }
    }

    #[test]
    fn lattice_declares_push() {
        // The misc-153 / Lattice-16 publish contract, carried as a conformance
        // invariant (misc 187): the evidence bar arms on it from turn zero.
        assert!(ServerProfile::for_server("lattice").declares_push());
    }

    #[test]
    fn other_servers_do_not_declare_push() {
        for name in [
            "rust-analyzer",
            "gopls",
            "marksman",
            // clangd is a VERSIONED-event server (misc 196 verified live): its
            // versioned publish settles by echo, so it does not carry the
            // UNVERSIONED `declares_push` contract (that is the lattice/taplo shape).
            "clangd",
            "yX4Za",
        ] {
            assert!(
                !ServerProfile::for_server(name).declares_push(),
                "{name} must not declare push without conformance evidence",
            );
        }
    }

    #[test]
    fn taplo_declares_push_verified_live() {
        // misc 196 (subsuming misc 194's verify-then-declare candidate): taplo was
        // verified live against the real 0.10.0 binary — it publishes UNVERSIONED
        // on didOpen, an explicit empty `[]` for a clean file and the diagnostic for
        // a broken one, ~1.3 s after open (once the client answers taplo's
        // `workspace/configuration` request). That publish-per-didOpen-including-
        // clean IS the `declares_push` contract, so the evidence bar arms from turn
        // zero and misc 194's double-miss `[clean]`-when-dirty window closes
        // structurally.
        assert!(ServerProfile::for_server("taplo").declares_push());
    }

    #[test]
    fn scan_and_diff_disciplines_project_from_the_personas() {
        // misc 196: the round-conditional disciplines surface through
        // `is_scan`/`is_diff` (read at the retrieval seams with round context), and
        // are DISTINCT from the static `owes_answer` contract — a scan/diff row owes
        // nothing unconditionally, so `owes_answer` stays false for them (ledger
        // 05's boundary, unregressed).
        let scan = ServerProfile::for_server("mockls-scan");
        assert!(
            scan.is_scan(),
            "the scan persona projects the scan discipline"
        );
        assert!(!scan.is_diff(), "a scan server is not a diff server");
        assert!(
            !scan.owes_answer(),
            "scan owes nothing to the STATIC contract — its arm is round-conditional",
        );

        let diff = ServerProfile::for_server("mockls-diff");
        assert!(
            diff.is_diff(),
            "the diff persona projects the diff discipline"
        );
        assert!(!diff.is_scan(), "a diff server is not a scan server");
        assert!(
            !diff.owes_answer(),
            "diff owes nothing to the STATIC contract — its arm is round-conditional",
        );

        // Every other discipline (and an unverified name) is neither scan nor diff.
        for name in [
            "rust-analyzer",
            "gopls",
            "lattice",
            "taplo",
            "mockls-event",
            "mockls-pull",
            "mockls-debounce",
            "mockls-declared",
            "yX4Za",
        ] {
            let p = ServerProfile::for_server(name);
            assert!(!p.is_scan(), "{name} must not be scan");
            assert!(!p.is_diff(), "{name} must not be diff");
        }
    }

    // ── declared-constant gate: debounce_ms + owes_answer (diagnostics-debt 05) ──

    #[test]
    fn debounce_persona_projects_the_declared_constant() {
        // The `mockls-debounce` manifest row declares `discipline = "debounce"`
        // with `debounce_ms = 300` (defaults/mockls-personas.toml, present under
        // the `mockls` feature the test build carries). The projection surfaces
        // the declared constant so the evidence bar can bound its await by it —
        // data riding the pin, never a measured guess.
        let profile = ServerProfile::for_server("mockls-debounce");
        assert_eq!(
            profile.debounce_ms(),
            Some(300),
            "the debounce persona must project its declared window"
        );
        // A debounce server owes an answer per round — the fault floor arms on it.
        assert!(
            profile.owes_answer(),
            "a debounce server's discipline owes a per-round response"
        );
    }

    #[test]
    fn non_debounce_servers_carry_no_debounce_bound() {
        // Only a debounce-discipline row projects a bound; every other discipline
        // (event, pull, scan, diff, or an unverified name) carries `None`, so the
        // generic dead-air budget governs them.
        for name in [
            "rust-analyzer",
            "gopls",
            "lattice",
            "mockls-event",
            "mockls-pull",
            "mockls-scan",
            "mockls-diff",
            "yX4Za",
        ] {
            assert_eq!(
                ServerProfile::for_server(name).debounce_ms(),
                None,
                "{name} must carry no debounce bound",
            );
        }
    }

    #[test]
    fn owes_answer_arms_on_declaration_or_debounce_only() {
        // A verified contract that owes a per-round answer: declared-push (misc
        // 187) OR debounce discipline (the version echo owed within the bound).
        for name in [
            "lattice",
            "mockls-declared",
            "mockls-debounce",
            "mockls-violator",
        ] {
            assert!(
                ServerProfile::for_server(name).owes_answer(),
                "{name} owes a per-round answer (declared-push or debounce)",
            );
        }
        // A merely event / pull / scan / diff server owes no verified per-round
        // contract — its silence is the misc-153 residual, never a fault. An
        // unverified name (enrichment-only) never owes anything.
        for name in [
            "rust-analyzer",
            "gopls",
            "mockls-event",
            "mockls-pull",
            "mockls-scan",
            "mockls-diff",
            "yX4Za",
        ] {
            assert!(
                !ServerProfile::for_server(name).owes_answer(),
                "{name} owes no verified per-round contract",
            );
        }
    }

    #[test]
    fn unverified_server_never_owes_an_answer_so_the_floor_never_fires() {
        // The fault floor is a blessed-set privilege (diagnostics-debt 04b/05 /
        // DESIGN §"The floor is fault attribution"): an UNVERIFIED custom def is
        // enrichment-only, so it is never a diagnostics source and its profile
        // owes no per-round answer — the contract-violation arm (which arms on
        // `owes_answer`) can never fire for it. Both facts pinned together.
        for name in ["some-custom-server", "yX4Za"] {
            let profile = ServerProfile::for_server(name);
            assert!(
                profile.is_enrichment_only(),
                "{name} must be enrichment-only",
            );
            assert!(
                !profile.owes_answer(),
                "{name} is unverified — it can never trigger the fault floor",
            );
            assert_eq!(profile.debounce_ms(), None, "{name} carries no bound");
        }
    }

    #[test]
    fn debounce_ms_on_a_non_debounce_row_is_ignored() {
        // The constant governs only the discipline that reads it: a `debounce_ms`
        // present on a non-debounce record projects to `None` (belt-and-braces —
        // the manifest never ships such a row, but the projection must not honour
        // a stray constant on the wrong discipline).
        use crate::recipes::{Discipline, DisciplineRecord};
        let record = DisciplineRecord {
            discipline: Some(Discipline::Event),
            debounce_ms: Some(500),
            ..DisciplineRecord::default()
        };
        assert_eq!(
            ServerProfile::from_record(&record).debounce_ms(),
            None,
            "a debounce_ms on a non-debounce row must not project a bound",
        );
    }

    #[test]
    fn shape_removes_diagnostic_for_suppressed_server() {
        let mut caps = json!({
            "textDocument": { "diagnostic": { "dynamicRegistration": false }, "definition": {} }
        });
        ServerProfile::for_server("rust-analyzer").shape_client_capabilities(&mut caps);
        assert!(caps["textDocument"].get("diagnostic").is_none());
        // Only `diagnostic` is dropped; siblings survive. A blessed pull-suppressed
        // server still advertises push (`publishDiagnostics`) — it is not
        // enrichment-only.
        assert!(caps["textDocument"].get("definition").is_some());
    }

    #[test]
    fn shape_is_identity_for_uncased_server() {
        let original = json!({
            "textDocument": { "diagnostic": { "dynamicRegistration": false } }
        });
        let mut caps = original.clone();
        ServerProfile::for_server("clangd").shape_client_capabilities(&mut caps);
        assert_eq!(caps, original);
    }

    // ── blessed / unverified classification (diagnostics-debt 04b) ───────

    #[test]
    fn blessed_servers_are_not_enrichment_only() {
        // Every blessed server (whether pull-suppressed, declared-push, or uncased)
        // is a diagnostics source — never enrichment-only.
        for name in ["rust-analyzer", "gopls", "lattice", "clangd", "taplo"] {
            assert!(
                !ServerProfile::for_server(name).is_enrichment_only(),
                "{name} is blessed and must not be enrichment-only",
            );
        }
    }

    #[test]
    fn unverified_custom_def_is_enrichment_only() {
        // A custom `[lsp.server.*]` def absent from the blessed manifest is
        // unverified ⇒ enrichment-only.
        assert!(
            ServerProfile::for_server("some-custom-server").is_enrichment_only(),
            "an unverified custom def must be enrichment-only",
        );
        assert!(ServerProfile::for_server("yX4Za").is_enrichment_only());
    }

    #[test]
    fn shape_withholds_all_diagnostics_for_enrichment_only() {
        // An enrichment-only (unverified) server advertises NO diagnostics
        // capability: both the pull `diagnostic` block and the push
        // `publishDiagnostics` block are stripped, while every other capability
        // survives so grep/glob enrichment and watched-files are untouched.
        let mut caps = json!({
            "textDocument": {
                "diagnostic": { "dynamicRegistration": false },
                "publishDiagnostics": { "versionSupport": true },
                "definition": { "linkSupport": true },
                "documentSymbol": {}
            }
        });
        let profile = ServerProfile::for_server("some-custom-server");
        assert!(profile.is_enrichment_only());
        profile.shape_client_capabilities(&mut caps);
        assert!(
            caps["textDocument"].get("diagnostic").is_none(),
            "the pull capability is withheld",
        );
        assert!(
            caps["textDocument"].get("publishDiagnostics").is_none(),
            "the push capability is withheld — no diagnostics advertisement at all",
        );
        // Non-diagnostics capabilities survive: grep/glob enrichment continues.
        assert!(caps["textDocument"].get("definition").is_some());
        assert!(caps["textDocument"].get("documentSymbol").is_some());
    }

    #[test]
    fn shape_keeps_publish_for_blessed_suppressed_server() {
        // A blessed pull-suppressed server (rust-analyzer) keeps its push
        // capability — only the pull block is dropped. This is the exact boundary
        // between "pull-suppressed" and "enrichment-only".
        let mut caps = json!({
            "textDocument": {
                "diagnostic": { "dynamicRegistration": false },
                "publishDiagnostics": { "versionSupport": true }
            }
        });
        ServerProfile::for_server("rust-analyzer").shape_client_capabilities(&mut caps);
        assert!(caps["textDocument"].get("diagnostic").is_none());
        assert!(
            caps["textDocument"].get("publishDiagnostics").is_some(),
            "a blessed suppressed server still advertises push",
        );
    }

    #[test]
    fn gopls_no_longer_forces_pull_off_but_forbids_the_delay() {
        // diagnostics-debt 05 re-enabled gopls's pull (bug 87's suppression is
        // lifted — ledger 03's version-echo settlement retired the placeholder
        // defeat structurally). So `pullDiagnostics` is NO LONGER forced: with no
        // user options there are no forced options at all, and gopls rides its
        // native pull mode. The `diagnosticsDelay` enforcement (run 9) stands: it
        // is a forbidden key, so an absent user options set stays `None`.
        assert_eq!(
            ServerProfile::for_server("gopls").effective_initialization_options(None),
            None,
            "gopls no longer forces any init option when the user supplies none",
        );
    }

    #[test]
    fn gopls_pull_mode_leaves_pull_diagnostics_to_the_user_but_forbids_the_delay() {
        // Pull is re-enabled (05), so a user MAY set `pullDiagnostics` — Catenary
        // no longer overrides it (the bug-87 override is retired structurally by
        // ledger 03). The run-9 footgun ("zero the delay to minimize latency")
        // stays enforced absent: `diagnosticsDelay` is stripped whatever the user
        // set, so gopls's own default is the only value that can ever apply.
        let user = json!({
            "diagnosticsDelay": "0s",
            "pullDiagnostics": true,
            "buildFlags": ["-tags=integration"],
        });
        let opts = ServerProfile::for_server("gopls")
            .effective_initialization_options(Some(&user))
            .expect("merged options");
        assert_eq!(
            opts["pullDiagnostics"],
            json!(true),
            "with pull re-enabled, the user's pullDiagnostics is no longer overridden",
        );
        assert!(
            opts.get("diagnosticsDelay").is_none(),
            "a user cannot deliver diagnosticsDelay at all (run 9)",
        );
        // The user's unrelated key survives.
        assert_eq!(opts["buildFlags"], json!(["-tags=integration"]));
    }

    #[test]
    fn gopls_is_pull_discipline_carrying_no_debounce_bound() {
        // The re-enable makes gopls a pull-discipline server (DESIGN's table row):
        // a pull settles the debt directly. It owes no per-round push contract and
        // carries no debounce bound — the fault floor never arms on it (a pull
        // error leaves the debt unsettled, bug 84's honesty, not the floor).
        let profile = ServerProfile::for_server("gopls");
        assert!(
            !profile.owes_answer(),
            "a pull server owes no per-round push answer"
        );
        assert_eq!(
            profile.debounce_ms(),
            None,
            "gopls carries no debounce bound"
        );
        assert!(
            !profile.declares_push(),
            "gopls is pull-discipline, not declared-push",
        );
    }

    #[test]
    fn uncased_server_passes_user_options_through() {
        let user = json!({ "some": "option" });
        assert_eq!(
            ServerProfile::for_server("clangd").effective_initialization_options(Some(&user)),
            Some(user.clone()),
        );
        assert_eq!(
            ServerProfile::for_server("clangd").effective_initialization_options(None),
            None,
        );
    }
}
