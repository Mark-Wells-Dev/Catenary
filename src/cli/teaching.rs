// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The teaching payload — Catenary's single-source prevention content.
//!
//! One module renders the full prevention payload that `catenary primer`
//! prints and that the `SessionStart` / `SubagentStart` hooks inline into the
//! agent's context. Because all three surfaces call [`payload_body`], the
//! wording can never drift between them.
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
  Find files with `catenary glob`, search contents with `catenary grep`; native
  `grep`/`find`/`ls` are denied so results stay LSP-enriched. Quote glob
  patterns so Catenary expands them gitignore-aware, not the shell
  (`catenary grep 'fn main' 'src/**/*.rs'`, `catenary glob 'src/**/*.rs'`). A
  glob pattern is itself the path — absolute or cwd-relative, with the anchor
  written in (`catenary glob '/abs/dir/**/*.md'`); there is no separate
  directory argument. Where a server covers a hit its enrichment rides along;
  where none does, `catenary grep` only flags the location — open the file and
  read it.";

/// Tier 3 — compact flag synopses. Long forms only, natural clusters
/// brace-collapsed; only the two flags that need disambiguation (`--glob` vs
/// the PATH positional, `--type` as a ripgrep file-type) carry a gloss. Each
/// command closes with its `--help` breadcrumb for point-of-use depth.
const FLAG_SYNOPSES: &str = "\
Flag synopses (long forms; each has a short `-x` alias too)
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

/// Render the shared teaching payload body.
///
/// The single source printed by `catenary primer` and inlined verbatim into
/// the `SessionStart` `additionalContext`. The commands-surface tier is
/// resolved live via [`resolve_commands_for_cwd`]; a config-load failure is
/// degraded to an empty surface rather than propagated, so a hook never breaks
/// the host's flow.
#[must_use]
pub fn payload_body() -> String {
    let (resolved, build_tools) = resolve_commands_for_cwd().unwrap_or_default();
    render(resolved.as_ref(), &build_tools)
}

/// Render the `SubagentStart` variant of the teaching payload.
///
/// The same [`payload_body`] with the per-agent debt line appended, so the
/// body is a clean prefix of the subagent payload.
#[must_use]
pub fn subagent_payload() -> String {
    let mut s = payload_body();
    s.push_str("\n\n");
    s.push_str(SUBAGENT_DEBT);
    s
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
            "no separate\n  directory argument",
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
