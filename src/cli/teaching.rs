// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The teaching payload — Catenary's single-source prevention content.
//!
//! One module renders the full prevention payload that `catenary primer`
//! prints and that the `SessionStart` / `SubagentStart` hooks inline into the
//! agent's context. Because all three surfaces emit through [`emitted_payload`]
//! (the [`payload_body`] content plus the daemon-staleness note), the wording
//! can never drift between them.
//!
//! The payload has three tiers (workstream 36, ticket 01):
//! 1. **The live commands surface** — the allow / pipeline / deny surface
//!    computed from the *resolved config at emission time* (never a baked
//!    list), rendered by the same [`render_command_lines`] machinery
//!    `catenary commands` uses, and closing with the write-model line.
//! 2. **The invariants** — the prevention content a cold agent must know
//!    before its first edit: the edit→diagnostics debt loop, bare-only vs
//!    pipe-friendly command classes, and the glob quoting / pattern-path form.
//! 3. **Flag synopses** — compact long-form flag lists for `grep` and `glob`,
//!    each closing with a `full: catenary <cmd> --help` breadcrumb.
//!
//! The payload carries **no** instruction to run `catenary primer` or
//! `catenary commands` — inlining the content is the whole point. The `--help`
//! breadcrumbs are point-of-use depth, not a teaching pointer.

use crate::cli::commands::{render_command_lines, resolve_commands_for_cwd};

/// Prefix-identifiable header line.
///
/// Opens every rendering (primer, `SessionStart`, `SubagentStart`) so the
/// block is recognizable when a host concatenates several hooks' context under
/// one shared label.
const HEADER: &str = "\
Catenary — code intelligence you drive from the shell: you search, browse, and
diagnose through `catenary` subcommands (shell commands run via your shell
tool, not MCP tools), and a hook enforces the rules below before every tool
call.";

/// Label introducing the live commands-surface tier.
const SHELL_SURFACE_LABEL: &str = "Shell surface (this session's live config)";

/// Tier 2 — the invariants, stated for a cold agent against the current
/// contract. Stable prose; the per-command reference below regenerates from
/// the derive-generated CLI, but these invariants change rarely.
const INVARIANTS: &str = "\
The edit→diagnostics loop
  Editing tracks itself — your first edit starts it, no start step. Every
  edited file a language server covers joins a debt gate. Pay it by running
  `catenary diagnostics` (bare, its own step): it DIAGNOSES the whole edited
  set and prints a per-file receipt — each file named, clean ones marked
  `[clean]`, dirty ones listing their errors and warnings. Paying is
  diagnosing, not fixing: a file leaves the gate once you look at it, clean or
  dirty. Exit `0` means the run completed and its receipt is trustworthy,
  clean or dirty; it never exits `1`, so a dirty result is not a failed call.
  Scope it — `catenary diagnostics path…` — to diagnose and pay off just those
  files; the gate stays armed for any edited file left unpaid. Diagnostics are
  pulled: you see them only when you run the command.

Bare-only vs pipe-friendly
  `catenary diagnostics` and `catenary roots …` are bare-only: run each as the
  sole command — no pipe, no `&&`/`;`, no redirect — then read its output.
  `catenary grep` and `catenary glob` are pipe-friendly: they compose freely
  (`| head`, `| grep`) and their output is always complete, so a pipe never
  drops results (use `--count` for a bare tally).

Navigate through Catenary
  Search contents with `catenary grep`, find files with `catenary glob`. Quote
  glob patterns so Catenary expands them gitignore-aware, not the shell
  (`catenary grep 'fn main' 'src/**/*.rs'`, `catenary glob 'src/**/*.rs'`); the
  pattern is itself the path, so there is no separate directory argument
  (`catenary glob '/abs/dir/**/*.md'`). Where a code intelligence source
  covers a hit its enrichment rides along; where none does, `catenary grep`
  still returns the match. With no path argument, `… | catenary grep PAT` is a
  plain pass over the stream — complete matches, no enrichment — so matches
  come back with no source coverage.";

/// Tier 3 — compact flag synopses. Long forms only, natural clusters
/// brace-collapsed; only the two flags that need disambiguation (`--glob` vs
/// the PATH positional, `--type` as a ripgrep file-type) carry a gloss. Each
/// command closes with its `--help` breadcrumb for point-of-use depth.
const FLAG_SYNOPSES: &str = "\
Flag synopses (long forms)
  catenary grep PATTERN [PATH…]
    --ignore-case --case-sensitive --word-regexp --fixed-strings
    --invert-match --files-with-matches --count
    --{after,before,}-context N --exclude-pattern GLOB
    --include-{gitignored,hidden}
    --glob GLOB — scope which files are searched (vs the PATH positional)
    --type TYPE — restrict to a ripgrep file type (e.g. rust, md)
    full: catenary grep --help
  catenary glob [PATH…]
    --exclude-pattern GLOB — drop paths matching GLOB
    --count — report the path tally instead of the paths
    --include-gitignored — include files .gitignore hides
    --include-hidden — include hidden files and directories
    full: catenary glob --help";

/// The per-agent debt line appended for the `SubagentStart` variant.
///
/// Keys truthfully on the per-agent debt model without any agent-type
/// branching (the subagent namespace is open, carrying no capability signal).
const SUBAGENT_DEBT: &str = "\
Subagent note: your diagnostic debt is tracked per-agent — the main agent's
`catenary diagnostics` does not pay yours. Diagnose the files you edit before
you finish.";

/// One restrained line prepended to the payload when the serving daemon runs a
/// different build than this CLI (detected via
/// [`crate::cli::version::daemon_is_stale`], teaching-surface ticket 05).
///
/// It qualifies the agent's evidence — observations may reflect a daemon whose
/// behavior predates the current CLI and docs — without naming internals or
/// instructing a fix. The daemon lifecycle is host-only, so the line informs
/// evidence quality; it does not tell the agent to restart anything.
const DAEMON_STALENESS_NOTE: &str = "note: the serving daemon runs an older build than \
    this CLI — its behavior may predate the current docs, so treat observations as \
    potentially stale";

/// Assemble the payload body from an already-resolved commands surface.
///
/// Pure: the config IO lives in [`payload_body`]. Split out so the tiers can
/// be pinned against a fixture surface without touching `Config::load()`.
/// `resolved` and `build_tools` are the live surface (or `None` / empty when
/// no `[commands]` section applies).
#[must_use]
fn render(resolved: Option<&crate::config::ResolvedCommands>, build_tools: &[String]) -> String {
    let mut s = String::with_capacity(3072);
    s.push_str(HEADER);
    s.push_str("\n\n");

    // Tier 1 — the live commands surface, rendered by the same machinery
    // `catenary commands` prints (closing with the write-model line when the
    // surface is active).
    s.push_str(SHELL_SURFACE_LABEL);
    s.push('\n');
    for line in render_command_lines(resolved, build_tools) {
        s.push_str(&line);
        s.push('\n');
    }
    s.push('\n');

    // Tier 2 — the invariants.
    s.push_str(INVARIANTS);
    s.push_str("\n\n");

    // Tier 3 — the flag synopses.
    s.push_str(FLAG_SYNOPSES);
    s
}

/// Prepend the daemon-staleness note to `body` when `stale`.
///
/// The note opens the payload on its own line, set off from the header by a
/// blank line; otherwise `body` is returned untouched, so the common
/// (non-stale) case adds nothing — no line, zero cost. Pure so the shape and
/// byte-equal placement are testable without a live daemon.
#[must_use]
fn with_staleness_note(stale: bool, body: String) -> String {
    if stale {
        format!("{DAEMON_STALENESS_NOTE}\n\n{body}")
    } else {
        body
    }
}

/// Render the shared teaching payload body.
///
/// The single source printed by `catenary primer` and inlined verbatim into
/// the `SessionStart` `additionalContext`. The commands-surface tier is
/// resolved live via [`resolve_commands_for_cwd`]; a config-load failure is
/// degraded to an empty surface rather than propagated, so a hook never breaks
/// the host's flow.
///
/// Pure prevention content — the daemon-staleness note is *not* part of this
/// body; it is prepended at the host-emission boundary by [`emitted_payload`],
/// so this stays deterministic for structural tests and independent of the
/// running daemon.
#[must_use]
pub fn payload_body() -> String {
    let (resolved, build_tools) = resolve_commands_for_cwd().unwrap_or_default();
    render(resolved.as_ref(), &build_tools)
}

/// Render the `SubagentStart` variant of the teaching payload.
///
/// The same [`payload_body`] with the per-agent debt line appended, so the
/// body is a clean prefix of the subagent payload. Like [`payload_body`], this
/// carries no staleness note — [`emitted_subagent_payload`] adds it at the
/// emission boundary.
#[must_use]
pub fn subagent_payload() -> String {
    let mut s = payload_body();
    s.push_str("\n\n");
    s.push_str(SUBAGENT_DEBT);
    s
}

/// The teaching payload as emitted to a host — [`payload_body`] with the
/// daemon-staleness note prepended when the serving daemon runs a different
/// build than this CLI (teaching-surface ticket 05).
///
/// This is what every session-start surface emits: `catenary primer`, the Claude
/// `SessionStart` `additionalContext`, and the raw OpenCode payload all render
/// from this one function, so the note is byte-equal across them — part of the
/// payload, not a per-host fork. The staleness signal reuses the `catenary
/// version` `tool/version` probe ([`crate::cli::version::daemon_is_stale`]); a
/// current daemon adds no line (zero cost), and an unreachable or unresponsive
/// daemon is left to the existing degraded-payload path with no second warning.
#[must_use]
pub fn emitted_payload() -> String {
    with_staleness_note(crate::cli::version::daemon_is_stale(), payload_body())
}

/// The `SubagentStart` variant as emitted to a host — [`emitted_payload`] with
/// the per-agent debt line appended.
///
/// The emitted body is a clean prefix, so when the daemon is stale the note
/// stays the opening line for the subagent surface too, matching every other
/// host's emission.
#[must_use]
pub fn emitted_subagent_payload() -> String {
    with_staleness_note(crate::cli::version::daemon_is_stale(), subagent_payload())
}

/// Render the SSOT teaching payload with **runtime data structurally excluded**
/// — the header, the invariants, and the flag synopses, but *not* the live
/// commands-surface tier.
///
/// The allow surface, build tool, and roots are runtime data no generated file
/// may carry (workstream 36 runtime-data rule), so this rendering drops Tier 1
/// entirely — structurally, not just resolved-to-empty. It is the content of
/// the shipped Gemini context file (repo-root `gemini-context.md`): a
/// compaction-proof bootstrap/fallback for Gemini, whose live teaching rides the
/// `SessionStart` `additionalContext`. The `shipped_gemini_context_is_fresh`
/// freshness gate pins the shipped file to this rendering, so the two cannot
/// drift. (OpenCode dropped its static fallback in teaching-surface 08 — its
/// teaching is runtime-only, regenerated by the plugin's `config` hook.)
#[must_use]
pub fn fallback_body() -> String {
    let mut s = String::with_capacity(2048);
    s.push_str(HEADER);
    s.push_str("\n\n");
    s.push_str(INVARIANTS);
    s.push_str("\n\n");
    s.push_str(FLAG_SYNOPSES);
    s
}

// ── Runtime context-file delivery (teaching-surface ticket 12) ───────────────

/// Opening marker of the generation-stamp line every runtime-regenerated
/// context/rules file carries.
///
/// Both context-file hosts re-read their file per prompt/turn, so `catenary hook`
/// rewrites the *installed* file to the live workspace-invariant surface — which,
/// by design, diverges from the shipped cold-bootstrap content ([`fallback_body`]).
/// This HTML-comment marker lets `catenary doctor` recognize such a runtime-updated
/// file as valid rather than "stale", while staying invisible in the markdown both
/// hosts render. The stamp carries the generating CLI's build version (never a
/// timestamp) so identical config yields identical bytes and the hook's hash gate
/// stays a true no-op.
const CONTEXT_STAMP_MARKER: &str = "<!-- catenary:generated";

/// The YAML frontmatter block that opens the Antigravity rules file, pinning
/// `trigger: always_on` so the host loads it unconditionally every turn
/// (teaching-surface ticket 10). The runtime rewrite preserves it and places the
/// generation stamp immediately after.
const ANTIGRAVITY_RULES_FRONTMATTER: &str = "---\ntrigger: always_on\n---\n\n";

/// The generation-stamp line for the current build.
///
/// Version-pinned via `CATENARY_VERSION` — never a timestamp — so two renders from
/// one binary are byte-identical and the hook's hash gate is a true no-op.
fn context_stamp_line() -> String {
    format!("{CONTEXT_STAMP_MARKER} {} -->", env!("CATENARY_VERSION"))
}

/// Whether `content` is a valid runtime-regenerated context/rules file — i.e. its
/// body opens with the generation stamp.
///
/// `catenary doctor` accepts such a file as up to date: the runtime rewrite is by
/// design, so it will not byte-match the shipped bootstrap. The Antigravity
/// frontmatter block is stripped first so the stamp — which sits immediately after
/// it — is recognized for both hosts (Gemini's file opens with the stamp directly).
#[must_use]
pub fn is_runtime_stamped(content: &str) -> bool {
    content
        .strip_prefix(ANTIGRAVITY_RULES_FRONTMATTER)
        .unwrap_or(content)
        .starts_with(CONTEXT_STAMP_MARKER)
}

/// The workspace-invariant live teaching surface written into the runtime context
/// files.
///
/// The user-global surface only: the header, the live commands surface resolved
/// from user config (allow / pipeline / deny, forms, script hosts, write-model
/// line — today's live Tier 1 **minus** the per-session cwd build tool and roots),
/// the invariants, and the flag synopses. One context file serves every concurrent
/// session of a host, so it must carry no per-session data; unlike [`payload_body`]
/// it therefore resolves the *user-global* commands
/// ([`crate::config::Config::load`], cwd-invariant) and renders with an empty
/// build-tool set. The daemon-staleness note is prepended on a known version skew
/// under the same condition as [`emitted_payload`] — a version comparison, itself
/// cwd-invariant. A config-load failure degrades to an empty surface rather than
/// propagating, so a hook never breaks the host's flow.
#[must_use]
pub fn context_file_body() -> String {
    let resolved = crate::config::Config::load()
        .ok()
        .and_then(|c| c.resolved_commands);
    with_staleness_note(
        crate::cli::version::daemon_is_stale(),
        render(resolved.as_ref(), &[]),
    )
}

/// The full Gemini context-file content written at hook time.
///
/// The generation stamp, a blank line, then [`context_file_body`], closed with the
/// trailing newline files carry. Gemini re-reads the file every prompt, so this
/// rides every request once written.
#[must_use]
pub fn gemini_context_file() -> String {
    format!("{}\n\n{}\n", context_stamp_line(), context_file_body())
}

/// The full Antigravity rules-file content written at hook time.
///
/// The `trigger: always_on` frontmatter (so the host loads it every turn), then
/// the generation stamp, then [`context_file_body`]. Same version-stamped,
/// cwd-invariant construction as [`gemini_context_file`]; the rules file re-injects
/// per conversation turn.
#[must_use]
pub fn antigravity_rules_file() -> String {
    format!(
        "{ANTIGRAVITY_RULES_FRONTMATTER}{}\n\n{}\n",
        context_stamp_line(),
        context_file_body(),
    )
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// Build an active `ResolvedCommands` fixture carrying a distinctive
    /// command name, so a test can prove the allow surface is rendered from the
    /// live config rather than a baked list.
    fn fixture_surface() -> crate::config::ResolvedCommands {
        crate::config::ResolvedCommands {
            allow: std::collections::HashSet::from(["git".into(), "distinctivecmd".into()]),
            pipeline: std::collections::HashSet::from(["rg".into()]),
            ..crate::config::ResolvedCommands::default()
        }
    }

    #[test]
    fn payload_carries_the_invariants() {
        let body = render(Some(&fixture_surface()), &[]);
        for needle in [
            "The edit→diagnostics loop",
            "per-file receipt",
            "[clean]",
            "never exits `1`",
            "catenary diagnostics path…",
            "Bare-only vs pipe-friendly",
            "pipe-friendly",
            "catenary glob 'src/**/*.rs'",
            "no separate directory argument",
        ] {
            assert!(
                body.contains(needle),
                "invariants missing {needle:?}: {body}"
            );
        }
    }

    #[test]
    fn payload_carries_the_write_model_line() {
        // Tier 1 closes with the write-model line when the surface is active.
        let body = render(Some(&fixture_surface()), &[]);
        assert!(
            body.contains("Writes resolve-or-deny"),
            "write-model line absent: {body}"
        );
    }

    #[test]
    fn primer_is_capability_voiced_under_both_configs() {
        // teaching-surface 13 verification: rendered under a config that denies
        // the native scanners AND under a deny-nothing config, the primer payload
        // carries no assertion-voiced navigation-policy sentence and does carry
        // the capability sentences (grep/glob are the navigation tools, plus the
        // stdin pass). The navigation surface is config-free, so both renders read
        // the same. (The former teach-07 clause named which scanners "stay
        // denied"; the maintainer ruled that policy voice out of the primer — a
        // user may enable native `grep`/`find`.)
        use crate::config::{GuidanceEntry, ResolvedCommands};
        let guidance = std::collections::HashMap::from([
            (
                "grep".to_string(),
                GuidanceEntry::Redirect {
                    command: "grep".into(),
                },
            ),
            (
                "find".to_string(),
                GuidanceEntry::Redirect {
                    command: "glob".into(),
                },
            ),
        ]);
        let denies = ResolvedCommands {
            allow: std::collections::HashSet::from(["git".into()]),
            pipeline: std::collections::HashSet::from(["grep".into()]),
            deny: std::collections::HashMap::from([(
                "git".to_string(),
                std::collections::HashSet::from(["ls-files".into()]),
            )]),
            guidance,
            ..ResolvedCommands::default()
        };
        let denies_body = render(Some(&denies), &["make".to_string()]);
        // A deny-nothing surface (the fixture: no guidance, no git deny).
        let neutral_body = render(Some(&fixture_surface()), &["make".to_string()]);
        for body in [&denies_body, &neutral_body] {
            // Capability voice present.
            assert!(
                body.contains("`catenary grep` and `catenary glob` are the navigation tools."),
                "write-model line names the navigation tools in capability voice: {body}"
            );
            assert!(
                body.contains("… | catenary grep PAT"),
                "primer describes the stdin pass: {body}"
            );
            assert!(
                body.contains("Writes resolve-or-deny"),
                "resolve-or-deny half stays: {body}"
            );
            // No assertion-voiced navigation policy in either config.
            for policy in ["stays denied", "bypass", "Navigation that bypasses"] {
                assert!(
                    !body.contains(policy),
                    "primer asserts navigation policy ({policy:?}) under a config: {body}"
                );
            }
        }
    }

    #[test]
    fn invariants_navigation_is_capability_voiced() {
        // teaching-surface 13: the navigate invariant states capabilities, not
        // policy — it names the navigation tools, describes the enrichment and
        // the stdin pass, and must never assert a denial or a "bypass" framing (a
        // user's config may allow the native scanners). This block rides both the
        // live payload and the runtime-free fallbacks, so it must carry no claim a
        // user's config may not make.
        for policy in ["are denied", "stays denied", "bypass"] {
            assert!(
                !INVARIANTS.contains(policy),
                "invariants assert policy ({policy:?}): {INVARIANTS}"
            );
        }
        assert!(
            INVARIANTS
                .contains("Search contents with `catenary grep`, find files with `catenary glob`."),
            "invariants name the navigation tools: {INVARIANTS}"
        );
        assert!(
            INVARIANTS.contains("… | catenary grep PAT"),
            "invariants describe the stdin pass: {INVARIANTS}"
        );
        assert!(
            INVARIANTS.contains("no source coverage"),
            "invariants state matches return without server coverage: {INVARIANTS}"
        );
    }

    #[test]
    fn allow_surface_reflects_the_live_config() {
        // The configured command name appears — the surface is projected from
        // the resolved config, not a hardcoded list.
        let body = render(Some(&fixture_surface()), &["make".to_string()]);
        assert!(
            body.contains("distinctivecmd"),
            "allow surface not sourced from config: {body}"
        );
        assert!(
            body.contains("Build tool: make"),
            "build tool absent: {body}"
        );
    }

    #[test]
    fn payload_has_no_run_pointers() {
        // Inlining is the point: the body must not tell the agent to run
        // `catenary primer` or `catenary commands`. (The commands surface
        // content itself is inlined, so we assert the absence of the
        // instruction-to-run subcommands, not the bare word "commands".)
        let body = render(Some(&fixture_surface()), &[]);
        assert!(
            !body.contains("catenary primer"),
            "payload points at `catenary primer`: {body}"
        );
        assert!(
            !body.contains("catenary commands"),
            "payload points at `catenary commands`: {body}"
        );
        assert!(
            !body.contains("catenary sed"),
            "payload names the retired `catenary sed`: {body}"
        );
    }

    #[test]
    fn payload_carries_help_breadcrumbs() {
        let body = render(Some(&fixture_surface()), &[]);
        assert!(
            body.contains("full: catenary grep --help"),
            "grep --help breadcrumb absent: {body}"
        );
        assert!(
            body.contains("full: catenary glob --help"),
            "glob --help breadcrumb absent: {body}"
        );
    }

    #[test]
    fn no_retired_output_language() {
        // Decision 025 retired budgets / valves / spill files; the payload must
        // not resurrect that vocabulary (output is complete).
        let body = render(Some(&fixture_surface()), &[]);
        for retired in ["spill", "line budget", "valve", "paged"] {
            assert!(
                !body.to_lowercase().contains(retired),
                "payload uses retired output vocabulary {retired:?}: {body}"
            );
        }
    }

    #[test]
    fn subagent_payload_extends_the_body_with_the_per_agent_line() {
        let body = render(Some(&fixture_surface()), &[]);
        // The subagent variant is the body plus the per-agent debt line; the
        // body is a clean prefix (payload_body resolves live config, so compare
        // structurally rather than byte-for-byte against the fixture render).
        assert!(
            SUBAGENT_DEBT.contains("tracked per-agent"),
            "per-agent debt line missing its keying phrase"
        );
        let sub = {
            let mut s = body.clone();
            s.push_str("\n\n");
            s.push_str(SUBAGENT_DEBT);
            s
        };
        assert!(
            sub.starts_with(&body),
            "subagent payload must extend the body"
        );
        assert!(
            sub.contains("your diagnostic debt is tracked per-agent"),
            "subagent payload missing the per-agent debt line: {sub}"
        );
    }

    #[test]
    fn subagent_payload_body_is_a_prefix() {
        // Live variant: `subagent_payload` == `payload_body` + the debt line.
        let body = payload_body();
        let sub = subagent_payload();
        assert!(
            sub.starts_with(&body),
            "body is not a prefix of the subagent payload"
        );
        assert!(
            sub.ends_with(SUBAGENT_DEBT),
            "subagent payload must end with the debt line"
        );
    }

    #[test]
    fn fallback_body_excludes_runtime_data() {
        // The runtime-free fallback must carry no runtime data (workstream 36
        // runtime-data rule): no live commands surface, no build tool, no allow
        // list. Those only ever appear in the runtime-sourced projection
        // (`payload_body`). The Tier-1 label and its distinctive markers must be
        // structurally absent — not merely resolved to empty.
        let fb = fallback_body();
        for forbidden in [
            SHELL_SURFACE_LABEL,
            "Build tool:",
            "Allowed:",
            "Allowed in pipelines",
            "resolve-or-deny",
        ] {
            assert!(
                !fb.contains(forbidden),
                "fallback carries runtime data {forbidden:?}: {fb}"
            );
        }
    }

    #[test]
    fn fallback_body_carries_the_static_tiers() {
        // Everything build-knowable survives: the header, the invariants, and
        // the flag synopses — parity with the SSOT's static tiers.
        let fb = fallback_body();
        assert!(fb.starts_with(HEADER), "fallback missing the header: {fb}");
        assert!(
            fb.contains("The edit→diagnostics loop"),
            "fallback missing the invariants: {fb}"
        );
        assert!(
            fb.contains("full: catenary glob --help"),
            "fallback missing the flag synopses: {fb}"
        );
    }

    #[test]
    fn fallback_body_is_the_ssot_static_tiers() {
        // Parity with the SSOT: the fallback draws its static tiers from the
        // same consts the live payload uses, so the two cannot drift. The live
        // payload interleaves the runtime surface between the header and the
        // invariants; the fallback is exactly those static tiers with the
        // runtime tier removed.
        let fb = fallback_body();
        let expected = format!("{HEADER}\n\n{INVARIANTS}\n\n{FLAG_SYNOPSES}");
        assert_eq!(fb, expected);
        // The live payload shares the same invariants / synopses source.
        let live = render(Some(&fixture_surface()), &["make".to_string()]);
        assert!(
            live.contains(INVARIANTS),
            "live payload lost the invariants"
        );
        assert!(
            live.contains(FLAG_SYNOPSES),
            "live payload lost the flag synopses"
        );
    }

    #[test]
    fn staleness_note_prepends_one_line_and_preserves_the_body() {
        // A stale daemon prepends the note as the payload's opening line, set off
        // from the header by a blank line, with the full original body preserved
        // verbatim after it — the line is part of the payload, not a fork.
        let body = render(Some(&fixture_surface()), &["make".to_string()]);
        let noted = with_staleness_note(true, body.clone());
        assert!(
            noted.starts_with(DAEMON_STALENESS_NOTE),
            "payload must open with the staleness note: {noted}"
        );
        assert_eq!(
            noted,
            format!("{DAEMON_STALENESS_NOTE}\n\n{body}"),
            "note is one prepended line, then the unchanged body"
        );
        // The note is a single line (no embedded newline).
        assert!(
            !DAEMON_STALENESS_NOTE.contains('\n'),
            "the note is one line: {DAEMON_STALENESS_NOTE}"
        );
    }

    #[test]
    fn staleness_note_conveys_skew_without_a_restart_instruction() {
        // Convey version skew and evidence quality; never instruct the agent to
        // restart the daemon (host-only by ruling), and never leak internals.
        assert!(
            DAEMON_STALENESS_NOTE.contains("older build"),
            "note conveys version skew: {DAEMON_STALENESS_NOTE}"
        );
        let lowered = DAEMON_STALENESS_NOTE.to_lowercase();
        assert!(
            !lowered.contains("restart") && !lowered.contains("catenary stop"),
            "note must not instruct a daemon restart: {DAEMON_STALENESS_NOTE}"
        );
        for internal in ["socket", "ipc", "tool/version", "git describe"] {
            assert!(
                !lowered.contains(internal),
                "note leaks internals ({internal:?}): {DAEMON_STALENESS_NOTE}"
            );
        }
    }

    #[test]
    fn emitted_subagent_payload_extends_the_emitted_body() {
        // The emitted subagent payload is the emitted body plus the per-agent
        // debt line; the emitted body is a clean prefix, so when the daemon is
        // stale the note stays the opening line for the subagent surface too.
        // Deterministic: both derive from the same daemon observation.
        let body = emitted_payload();
        let sub = emitted_subagent_payload();
        assert!(
            sub.starts_with(&body),
            "emitted body must prefix the emitted subagent payload"
        );
        assert!(
            sub.ends_with(SUBAGENT_DEBT),
            "emitted subagent payload must end with the debt line"
        );
        // The static invariants survive the emission wrapping.
        assert!(
            body.contains("The edit→diagnostics loop"),
            "emitted body carries the invariants"
        );
    }

    #[test]
    fn no_staleness_note_leaves_the_body_untouched() {
        // The common case: a current (or unreachable) daemon adds nothing — no
        // line, zero cost.
        let body = render(Some(&fixture_surface()), &["make".to_string()]);
        assert_eq!(
            with_staleness_note(false, body.clone()),
            body,
            "no staleness → the body is returned unchanged"
        );
        assert!(
            !with_staleness_note(false, body).starts_with(DAEMON_STALENESS_NOTE),
            "no note prefix when the daemon is current"
        );
    }

    #[test]
    fn shipped_gemini_context_is_fresh() {
        // Freshness gate (ws36 ticket 06): the shipped Gemini context file
        // (repo-root `gemini-context.md`, embedded as `GEMINI_CONTEXT_EXPECTED`
        // and doctor freshness-checked) is the `fallback_body` rendering verbatim
        // (files carry a trailing newline). Gemini's runtime teaching rides the
        // SessionStart `additionalContext`; this file is the compaction-proof
        // bootstrap/fallback. It shares the OpenCode fallback's SSOT renderer, so
        // the two fallbacks cannot drift from each other or from the SSOT tiers —
        // regenerate the file from `fallback_body`.
        const SHIPPED: &str = include_str!("../../gemini-context.md");
        assert_eq!(
            SHIPPED,
            format!("{}\n", fallback_body()),
            "gemini-context.md is stale — regenerate it from \
             `catenary::cli::teaching::fallback_body()`"
        );
    }

    #[test]
    fn shipped_antigravity_rules_are_fresh() {
        // Freshness gate (teaching-surface ticket 10): the shipped Antigravity
        // rules file (`plugins/catenary-antigravity/rules/catenary.md`, embedded
        // as `ANTIGRAVITY_RULES_EXPECTED` and doctor freshness-checked) is the
        // `fallback_body` rendering verbatim — the compaction-proof teaching leg.
        // Rules files re-inject per conversation turn, so this file carries the
        // static tiers every turn; teach-03's persisted `PreInvocation`
        // `userMessage` carries the live surface once (and dies at compaction).
        //
        // Unlike gemini-context.md, this file opens with a YAML frontmatter block
        // pinning `trigger: always_on` so the host loads it unconditionally every
        // turn (host contract: agy SKILL.md, "Progressive Disclosure" — "Only
        // `always_on` rules are loaded unconditionally"). The frontmatter breaks
        // byte-equality with `fallback_body`, so the gate compares the body
        // *after* the frontmatter block; the block itself is asserted separately.
        const SHIPPED: &str = include_str!("../../plugins/catenary-antigravity/rules/catenary.md");
        const FRONTMATTER: &str = "---\ntrigger: always_on\n---\n\n";
        let body = SHIPPED.strip_prefix(FRONTMATTER).expect(
            "antigravity rules file must open with the `trigger: always_on` frontmatter block",
        );
        assert_eq!(
            body,
            format!("{}\n", fallback_body()),
            "rules/catenary.md body is stale — regenerate it from \
             `catenary::cli::teaching::fallback_body()` (preserve the frontmatter block)"
        );
    }

    // ── Runtime context-file delivery (teaching-surface ticket 12) ──────

    #[test]
    fn context_body_is_workspace_invariant() {
        // One context file serves every concurrent session, so it must exclude
        // per-session (cwd-derived) data. The live payload interleaves the cwd
        // build tool into Tier 1 — two sessions whose cwds resolve different
        // build tools get different payloads — but the context render passes an
        // empty build set, so it is byte-identical regardless of cwd.
        let surface = fixture_surface();
        let session_a = render(Some(&surface), &["make".to_string()]);
        let session_b = render(Some(&surface), &["bazel".to_string()]);
        assert_ne!(
            session_a, session_b,
            "live payloads differ by the cwd build tool"
        );

        // The context render (empty build set) is the workspace-invariant body:
        // no cwd input, so identical for every session.
        let ctx = render(Some(&surface), &[]);
        assert_eq!(ctx, render(Some(&surface), &[]));
        assert!(
            !ctx.contains("Build tool:"),
            "context body must omit the per-session cwd build tool: {ctx}"
        );
        // It still carries the user-global surface and the static tiers.
        assert!(
            ctx.contains("distinctivecmd"),
            "context body carries the user-global allow surface: {ctx}"
        );
        assert!(
            ctx.contains(SHELL_SURFACE_LABEL),
            "context body carries the Tier-1 label: {ctx}"
        );
        assert!(
            ctx.contains("The edit→diagnostics loop"),
            "context body carries the invariants: {ctx}"
        );
    }

    #[test]
    fn gemini_context_file_is_stamped_and_doctor_accepted() {
        let file = gemini_context_file();
        assert!(
            file.starts_with(CONTEXT_STAMP_MARKER),
            "gemini file opens with the generation stamp: {file}"
        );
        assert!(
            is_runtime_stamped(&file),
            "doctor must accept the runtime-stamped gemini file"
        );
        assert!(
            file.ends_with('\n'),
            "file carries a trailing newline: {file}"
        );
        assert!(
            file.contains(HEADER),
            "gemini file carries the header: {file}"
        );
        assert!(
            file.contains("The edit→diagnostics loop"),
            "gemini file carries the invariants: {file}"
        );
    }

    #[test]
    fn antigravity_rules_file_keeps_frontmatter_then_stamp() {
        let file = antigravity_rules_file();
        assert!(
            file.starts_with(ANTIGRAVITY_RULES_FRONTMATTER),
            "rules file opens with the always_on frontmatter: {file}"
        );
        let body = file
            .strip_prefix(ANTIGRAVITY_RULES_FRONTMATTER)
            .expect("frontmatter present");
        assert!(
            body.starts_with(CONTEXT_STAMP_MARKER),
            "the stamp follows the frontmatter: {body}"
        );
        assert!(
            file.starts_with("---\ntrigger: always_on"),
            "frontmatter stays the very first bytes for the host parser: {file}"
        );
        assert!(
            is_runtime_stamped(&file),
            "doctor must accept the runtime-stamped rules file"
        );
        assert!(
            file.ends_with('\n'),
            "file carries a trailing newline: {file}"
        );
    }

    #[test]
    fn is_runtime_stamped_rejects_the_shipped_bootstrap() {
        // The shipped cold-bootstrap content bears no stamp — doctor must not
        // mistake it for a runtime-updated file (it is the fallback, not the
        // live surface).
        assert!(
            !is_runtime_stamped(&format!("{}\n", fallback_body())),
            "shipped gemini bootstrap is not runtime-stamped"
        );
        let shipped_agy = format!("{ANTIGRAVITY_RULES_FRONTMATTER}{}\n", fallback_body());
        assert!(
            !is_runtime_stamped(&shipped_agy),
            "shipped antigravity bootstrap is not runtime-stamped"
        );
    }

    #[test]
    fn runtime_stamp_is_version_pinned_not_time() {
        // The hash gate depends on identical config → identical bytes; a
        // timestamp would defeat it. Two renders must match, and the stamp
        // carries the build version.
        assert_eq!(
            context_stamp_line(),
            context_stamp_line(),
            "the stamp is deterministic (no timestamp)"
        );
        assert!(
            context_stamp_line().contains(env!("CATENARY_VERSION")),
            "the stamp names the build version: {}",
            context_stamp_line()
        );
    }

    #[test]
    fn payload_stays_in_the_token_band() {
        // Size guard: the ticket targets ~600–800 tokens. Using a ~4 chars/token
        // proxy that band is ~2400–3200 chars; we allow a slightly wider
        // 2400..3300 window so ordinary wording tweaks and the small config-driven
        // Tier 1 don't trip it, while a tier being dropped or doubled still does.
        // Rendered against a minimal active fixture surface — a large real
        // allowlist grows Tier 1 further, which is expected and unbounded here.
        let chars = render(Some(&fixture_surface()), &["make".to_string()])
            .chars()
            .count();
        assert!(
            (2400..=3300).contains(&chars),
            "teaching payload is {chars} chars (~{} tokens); expected the ~600–800 token band",
            chars / 4
        );
    }
}
