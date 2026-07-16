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
//!
//! The rendering is additionally keyed by an optional **declared client**
//! ([`HostFormat`], misc 177): a client whose installed hook set registers the
//! `WorktreeCreate` hook (today Claude Code) gets the misc-146
//! isolated-subagents mention extended into the "Dispatching isolated work"
//! section, which teaches `isolation: "worktree"` subagent dispatch. The
//! identity is always declared — the `catenary primer <client>` positional or
//! the hook definition's `--format` — never sniffed from a host name at
//! runtime (maintainer ruling: hooks are hand-crafted per host and there is no
//! standardized hook protocol to auto-detect against).

use crate::cli::HostFormat;
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
  `catenary diagnostics`: it DIAGNOSES the whole edited set and prints a
  per-file receipt — each file named, clean ones marked `[clean]`, dirty ones
  listing their errors and warnings. Paying is diagnosing, not fixing: a file
  leaves the gate once you look at it, clean or dirty. Exit `0` means the run
  completed and its receipt is trustworthy, clean or dirty; it never exits `1`,
  so a dirty result is not a failed call. Scope it —
  `catenary diagnostics path…` — to diagnose just those files; the gate stays
  armed for any edited file left unpaid. Diagnostics are pulled: you see them
  only when you run it. Once you diagnose the edited set the debt is paid — a
  repeat bare run answers `[no edited files]`; re-check specific files with the
  scoped form.

Bare-only vs pipe-friendly
  `catenary diagnostics` is bare-only: run it as the sole command — no pipe, no
  `&&`/`;`, no redirect — then read its output. `catenary grep` and
  `catenary glob` are pipe-friendly: they compose freely (`| head`, `| grep`)
  and their output is always complete, so a pipe never drops results (use
  `--count` for a bare tally). `catenary claim` is bare-only too.

Work in isolated subagents
  Coverage is automatic — you never manage roots. Two agents in one workspace
  step on each other and, sharing its language server, break settle detection.

Navigate through Catenary
  Search contents with `catenary grep`, find files with `catenary glob`. Quote
  glob patterns so Catenary expands them gitignore-aware, not the shell
  (`catenary grep 'fn main' 'src/**/*.rs'`, `catenary glob 'src/**/*.rs'`).
  `catenary glob`'s positional IS a pattern — one pattern, quoted; list a
  directory with `catenary glob 'dir/*'`. Where a code intelligence source
  covers a hit its enrichment rides along; where none does, `catenary grep`
  still returns the match. With no path argument, `… | catenary grep PAT` is a
  plain pass over the stream — complete matches, no enrichment — so matches
  come back with no source coverage.";

/// The misc-146 isolated-subagents mention, byte-exact as it appears inside
/// [`INVARIANTS`] — the anchor block [`invariants_for`] swaps out for clients
/// whose installed hook set carries `WorktreeCreate`. A test pins that the
/// anchor occurs in [`INVARIANTS`] exactly once, so the swap can never
/// silently no-op or over-fire.
const ISOLATED_SUBAGENTS_MENTION: &str = "\
Work in isolated subagents
  Coverage is automatic — you never manage roots. Two agents in one workspace
  step on each other and, sharing its language server, break settle detection.";

/// The client-keyed "Dispatching isolated work" section (misc 177).
///
/// Replaces [`ISOLATED_SUBAGENTS_MENTION`] for clients whose installed hook
/// set registers `WorktreeCreate` — it absorbs the mention (its opening
/// capability facts survive verbatim, so isolation is never taught twice) and
/// adds the dispatch flow: the host's `isolation: "worktree"` fires the
/// `WorktreeCreate` hook, which runs the worktree add itself, relocates the
/// root out of the repo, and anchors the subagent — where a hand-run
/// `catenary worktree add` anchors nothing. Landing is git-native (wf-03: the
/// `worktree diff`/`land` verbs retired) — the section teaches the merge flow
/// and the merge bracket's automatic debt transfer (wf-01). Capability voice
/// throughout: it states what each flow does, never what is forbidden.
const DISPATCH_ISOLATED_WORK: &str = "\
Dispatching isolated work
  Coverage is automatic — you never manage roots. Two agents in one workspace
  step on each other and, sharing its language server, break settle detection.
  Dispatch isolated work with the Agent/Task tool's `isolation: \"worktree\"` —
  Catenary's WorktreeCreate hook creates the worktree itself, relocates it
  outside the repo (so language servers pick up no recursive roots), and
  anchors the subagent's workspace there. Hand-running `catenary worktree add`
  for an agent skips that anchoring — the agent stays pinned to the main tree
  and its file access prompts against the wrong workspace. Land finished work
  git-natively: commit in the branch, review with `git diff main...<branch>`,
  `git merge --squash <branch>` in the owning repo, commit, then
  `catenary worktree rm <path>` (WorktreeRemove never fires — a known
  upstream Claude Code bug, so `rm` is the cleanup path). The merge bracket
  transfers unpaid worker debt automatically; pay it with
  `catenary diagnostics`.";

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
    --exclude-pattern GLOB — drop paths matching GLOB (repeatable)
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

/// Whether the declared client's installed hook set registers the
/// `WorktreeCreate` hook — the capability the "Dispatching isolated work"
/// section teaches (misc 177).
///
/// The identity is declared, never sniffed (maintainer ruling): it travels
/// with the hook definition (`--format=claude`) or the `catenary primer
/// <client>` positional, so whatever host executes that hooks.json carries the
/// registration it is taught, by construction. Today only Claude Code's
/// shipped hook set (`plugins/catenary/hooks/hooks.json`) registers
/// `WorktreeCreate`; Antigravity's and OpenCode's do not.
///
/// Shared with the command filter (misc 177): the same predicate that keys the
/// primer's "Dispatching isolated work" section keys the agent-side
/// `catenary worktree add` dispatch denial, so the two surfaces can never skew.
pub(crate) const fn hook_set_has_worktree_create(client: Option<HostFormat>) -> bool {
    matches!(client, Some(HostFormat::Claude))
}

/// The invariants tier for the declared client.
///
/// A client whose hook set carries `WorktreeCreate` gets the misc-146
/// isolated-subagents mention replaced by [`DISPATCH_ISOLATED_WORK`] — the
/// section absorbs the mention, so the payload never teaches isolation twice.
/// Every other rendering (the bare primer, OpenCode, Antigravity, the
/// fallbacks) keeps [`INVARIANTS`] verbatim, byte-identical to the client-less
/// payload.
fn invariants_for(client: Option<HostFormat>) -> std::borrow::Cow<'static, str> {
    if hook_set_has_worktree_create(client) {
        std::borrow::Cow::Owned(
            INVARIANTS.replace(ISOLATED_SUBAGENTS_MENTION, DISPATCH_ISOLATED_WORK),
        )
    } else {
        std::borrow::Cow::Borrowed(INVARIANTS)
    }
}

/// Assemble the payload body from an already-resolved commands surface.
///
/// Pure: the config IO lives in [`payload_body`]. Split out so the tiers can
/// be pinned against a fixture surface without touching `Config::load()`.
/// `resolved` and `build_tools` are the live surface (or `None` / empty when
/// no `[commands]` section applies); `client` is the declared client identity
/// keying client-specific teaching ([`invariants_for`], misc 177) — `None`
/// renders the client-neutral payload.
#[must_use]
fn render(
    resolved: Option<&crate::config::ResolvedCommands>,
    build_tools: &[String],
    client: Option<HostFormat>,
) -> String {
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

    // Tier 2 — the invariants, keyed by the declared client.
    s.push_str(&invariants_for(client));
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

/// Render the shared teaching payload body for the declared client.
///
/// The single source printed by `catenary primer` (whose optional positional
/// declares the client) and inlined verbatim into the `SessionStart`
/// `additionalContext` (whose `--format` declares it) — one rendering, keyed
/// by the declared identity, so the section can never fork between surfaces.
/// The commands-surface tier is resolved live via [`resolve_commands_for_cwd`];
/// a config-load failure is degraded to an empty surface rather than
/// propagated, so a hook never breaks the host's flow.
///
/// Pure prevention content — the daemon-staleness note is *not* part of this
/// body; it is prepended at the host-emission boundary by [`emitted_payload`],
/// so this stays deterministic for structural tests and independent of the
/// running daemon.
#[must_use]
pub fn payload_body(client: Option<HostFormat>) -> String {
    let (resolved, build_tools) = resolve_commands_for_cwd().unwrap_or_default();
    render(resolved.as_ref(), &build_tools, client)
}

/// Render the `SubagentStart` variant of the teaching payload.
///
/// The client-neutral [`payload_body`] with the per-agent debt line appended,
/// so the body is a clean prefix of the subagent payload. Deliberately
/// client-less: the `SubagentStart` surface teaches the *worker*, which does
/// not dispatch isolated work itself, so the misc-177 dispatch section stays
/// out and the misc-146 mention rides as-is. Like [`payload_body`], this
/// carries no staleness note — [`emitted_subagent_payload`] adds it at the
/// emission boundary.
#[must_use]
pub fn subagent_payload() -> String {
    let mut s = payload_body(None);
    s.push_str("\n\n");
    s.push_str(SUBAGENT_DEBT);
    s
}

/// The teaching payload as emitted to a host for the declared client.
///
/// [`payload_body`] with the daemon-staleness note prepended when the serving
/// daemon runs a different build than this CLI (teaching-surface ticket 05).
///
/// This is what every session-start surface emits: `catenary primer`, the Claude
/// `SessionStart` `additionalContext`, and the raw OpenCode payload all render
/// from this one function (each passing its declared client), so the note is
/// byte-equal across them — part of the payload, not a per-host fork. The
/// staleness signal reuses the `catenary version` `tool/version` probe
/// ([`crate::cli::version::daemon_is_stale`]); a current daemon adds no line
/// (zero cost), and an unreachable or unresponsive daemon is left to the
/// existing degraded-payload path with no second warning.
#[must_use]
pub fn emitted_payload(client: Option<HostFormat>) -> String {
    with_staleness_note(crate::cli::version::daemon_is_stale(), payload_body(client))
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
/// the shipped Antigravity rules file
/// (`plugins/catenary-antigravity/rules/catenary.md`, under its `always_on`
/// frontmatter): a compaction-proof bootstrap that rides every conversation turn,
/// whose per-session delta rides the `PreInvocation` sliver. The
/// `shipped_antigravity_rules_are_fresh` freshness gate pins the shipped file to
/// this rendering, so the two cannot drift. (OpenCode dropped its static fallback
/// in teaching-surface 08 — its teaching is runtime-only, regenerated by the
/// plugin's `config` hook.)
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

// ── Runtime rules-file delivery (teaching-surface ticket 12; the Antigravity
//    rules file) ─────────────────────────────────────────────────────────────

/// Opening marker of the generation-stamp line the runtime-regenerated Antigravity
/// rules file carries.
///
/// Antigravity's `always_on` rules file is re-injected per conversation turn, so
/// `catenary hook pre-invocation` rewrites the *installed* file to the live
/// workspace-invariant surface — which, by design, diverges from the shipped
/// cold-bootstrap content ([`fallback_body`]). This HTML-comment marker lets
/// `catenary doctor` recognize such a runtime-updated file as valid rather than
/// "stale", while staying invisible in the markdown the host renders. The stamp
/// carries the generating CLI's build version (never a timestamp) so identical
/// config yields identical bytes and the hook's hash gate stays a true no-op.
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
/// it — is recognized.
#[must_use]
pub fn is_runtime_stamped(content: &str) -> bool {
    content
        .strip_prefix(ANTIGRAVITY_RULES_FRONTMATTER)
        .unwrap_or(content)
        .starts_with(CONTEXT_STAMP_MARKER)
}

/// The workspace-invariant live teaching surface written into the runtime
/// Antigravity rules file.
///
/// The user-global surface only: the header, the live commands surface resolved
/// from user config (allow / pipeline / deny, forms, script hosts, write-model
/// line — today's live Tier 1 **minus** the per-session cwd build tool and roots),
/// the invariants, and the flag synopses. One rules file serves every concurrent
/// session of the host, so it must carry no per-session data; unlike [`payload_body`]
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
        render(resolved.as_ref(), &[], Some(HostFormat::Antigravity)),
    )
}

/// The full Antigravity rules-file content written at hook time.
///
/// The `trigger: always_on` frontmatter (so the host loads it every turn), then
/// the generation stamp, then [`context_file_body`]. Version-stamped and
/// cwd-invariant; the rules file re-injects per conversation turn.
#[must_use]
pub fn antigravity_rules_file() -> String {
    format!(
        "{ANTIGRAVITY_RULES_FRONTMATTER}{}\n\n{}\n",
        context_stamp_line(),
        context_file_body(),
    )
}

// ── Per-session teaching sliver (Antigravity `PreInvocation`, ticket 14) ─────

/// Label opening the per-session teaching sliver, naming the block and pointing
/// at the rules file that carries the rest.
///
/// Distinct from every line of [`context_file_body`] so the sliver reads as a
/// coherent, self-labeled block and never duplicates a sentence the always-on
/// rules file already delivers.
const SLIVER_LABEL: &str = "Catenary — this session's workspace specifics (the \
    always-on rules file carries the shared command surface):";

/// The per-session teaching sliver: the session-specific delta the shared
/// `always_on` rules file structurally cannot carry.
///
/// The Antigravity rules file ([`context_file_body`]) renders the user-global
/// surface with an *empty* build-tool set, so it omits the cwd build tool — the
/// one thing [`payload_body`] renders that the rules file cannot. This carries
/// exactly that delta as a self-labeled block, so an Antigravity `PreInvocation`
/// first-sighting injects only what the rules file lacks (teaching-surface ticket
/// 14, replacing the full-payload injection of ticket 03). `None` when the cwd
/// resolves no build tool — nothing session-specific to add, so nothing is
/// injected.
#[must_use]
pub fn session_sliver() -> Option<String> {
    let (_, build_tools) = resolve_commands_for_cwd().unwrap_or_default();
    build_tool_sliver(&build_tools)
}

/// Render the sliver block from an already-resolved build-tool set.
///
/// Pure: the cwd resolution lives in [`session_sliver`]. Split out so the block
/// shape and the "no delta → no block" case are testable against a fixture. The
/// `Build tool:` line matches the one [`render_command_lines`] emits, so the
/// sliver reads identically to Tier 1's build-tool line.
#[must_use]
fn build_tool_sliver(build_tools: &[String]) -> Option<String> {
    if build_tools.is_empty() {
        return None;
    }
    Some(format!(
        "{SLIVER_LABEL}\nBuild tool: {}",
        build_tools.join(", ")
    ))
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
        let body = render(Some(&fixture_surface()), &[], None);
        for needle in [
            "The edit→diagnostics loop",
            "per-file receipt",
            "[clean]",
            "never exits `1`",
            "catenary diagnostics path…",
            "Bare-only vs pipe-friendly",
            "pipe-friendly",
            "catenary glob 'src/**/*.rs'",
            "positional IS a pattern",
            "catenary glob 'dir/*'",
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
        let body = render(Some(&fixture_surface()), &[], None);
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
        let denies_body = render(Some(&denies), &["make".to_string()], None);
        // A deny-nothing surface (the fixture: no guidance, no git deny).
        let neutral_body = render(Some(&fixture_surface()), &["make".to_string()], None);
        // The Claude render carries the misc-177 dispatch section — it must
        // survive the same capability-voice bar as the bare payload.
        let claude_body = render(
            Some(&fixture_surface()),
            &["make".to_string()],
            Some(HostFormat::Claude),
        );
        for body in [&denies_body, &neutral_body, &claude_body] {
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
    fn primer_omits_root_management_but_mentions_isolated_subagents() {
        // misc 146: coverage is automatic, so the primer must not teach agents
        // to manage roots — `pin`/`unpin`/`roots` live in `catenary -h` only.
        // In their place the primer carries one informative mention: work in
        // isolated subagents (the shared-language-server settle-detection gotcha),
        // stated as a capability fact, not an instruction to manage roots.
        let body = render(Some(&fixture_surface()), &["make".to_string()], None);
        for instruction in ["catenary pin", "catenary unpin", "catenary roots"] {
            assert!(
                !body.contains(instruction),
                "primer teaches root management ({instruction:?}): {body}"
            );
        }
        assert!(
            body.contains("isolated subagents"),
            "primer missing the isolated-subagents mention: {body}"
        );
        assert!(
            body.contains("settle detection"),
            "primer missing the settle-detection gotcha: {body}"
        );
        // Stated as capability, not an imperative to add/pin roots.
        assert!(
            body.contains("never manage roots"),
            "primer must frame coverage as automatic, not a root-management chore: {body}"
        );
    }

    // ── Client-keyed dispatch teaching (misc 177) ────────────────────────

    #[test]
    fn dispatch_anchor_is_pinned_in_the_invariants() {
        // `invariants_for` swaps the misc-146 mention for the dispatch section
        // by anchor replacement — the anchor must appear in INVARIANTS
        // byte-exact, exactly once, or the swap silently no-ops (or over-fires).
        assert_eq!(
            INVARIANTS.matches(ISOLATED_SUBAGENTS_MENTION).count(),
            1,
            "the isolated-subagents mention must anchor the invariants exactly once"
        );
    }

    #[test]
    fn bare_payload_keeps_the_misc_146_mention_and_no_dispatch_section() {
        // No declared client → the client-neutral payload, byte-identical to
        // the pre-misc-177 rendering: the misc-146 mention stays as-is and the
        // dispatch section is absent.
        let body = render(Some(&fixture_surface()), &["make".to_string()], None);
        assert!(
            body.contains(ISOLATED_SUBAGENTS_MENTION),
            "bare payload keeps the isolated-subagents mention verbatim: {body}"
        );
        assert!(
            !body.contains("Dispatching isolated work"),
            "bare payload must not carry the dispatch section: {body}"
        );
        assert!(
            !body.contains("isolation: \"worktree\""),
            "bare payload must not teach worktree dispatch: {body}"
        );
    }

    #[test]
    fn claude_payload_carries_the_dispatch_section() {
        // A declared client whose installed hook set registers WorktreeCreate
        // (Claude Code) gets the dispatch teaching: the isolation flag, the
        // hook's anchoring, the hand-run gap, the git-native landing flow
        // (wf-03), the WorktreeRemove-never-fires cleanup note, and the merge
        // bracket's automatic debt transfer (wf-01).
        let body = render(
            Some(&fixture_surface()),
            &["make".to_string()],
            Some(HostFormat::Claude),
        );
        for needle in [
            "Dispatching isolated work",
            "isolation: \"worktree\"",
            "WorktreeCreate hook creates the worktree itself",
            "catenary worktree add",
            "git diff main...<branch>",
            "git merge --squash <branch>",
            "catenary worktree rm <path>",
            "(WorktreeRemove never fires",
            "transfers unpaid worker debt automatically",
            "catenary diagnostics",
        ] {
            assert!(
                body.contains(needle),
                "dispatch section missing {needle:?}: {body}"
            );
        }
        // The section absorbs the misc-146 mention: its capability facts
        // survive under the new heading, and the old heading is gone, so the
        // payload never teaches isolation twice.
        assert!(
            !body.contains("Work in isolated subagents"),
            "the dispatch section must absorb the misc-146 mention, not duplicate it: {body}"
        );
        assert!(
            body.contains("settle detection"),
            "the misc-146 settle-detection gotcha survives the absorption: {body}"
        );
        assert!(
            body.contains("never manage roots"),
            "the misc-146 coverage-is-automatic fact survives the absorption: {body}"
        );
    }

    #[test]
    fn dispatch_section_is_capability_voiced() {
        // The section states what each flow does — dispatch anchors, a
        // hand-run skips the anchoring — never a prohibition (the primer's
        // capability-voice ruling, teach 13).
        let lowered = DISPATCH_ISOLATED_WORK.to_lowercase();
        for policy in [
            "denied",
            "forbidden",
            "never run",
            "never hand-run",
            "do not",
            "don't",
            "must not",
            "bypass",
        ] {
            assert!(
                !lowered.contains(policy),
                "dispatch section asserts policy ({policy:?}): {DISPATCH_ISOLATED_WORK}"
            );
        }
    }

    #[test]
    fn clients_without_worktree_create_keep_the_bare_payload() {
        // Only a hook set that registers WorktreeCreate keys the section —
        // OpenCode's and Antigravity's do not (yet), so their declared
        // identities render byte-identical to the client-neutral payload.
        let bare = render(Some(&fixture_surface()), &["make".to_string()], None);
        for client in [HostFormat::OpenCode, HostFormat::Antigravity] {
            assert_eq!(
                render(
                    Some(&fixture_surface()),
                    &["make".to_string()],
                    Some(client)
                ),
                bare,
                "{client:?} must render the bare payload until its hook set carries WorktreeCreate"
            );
        }
    }

    #[test]
    fn dispatch_section_stays_tight() {
        // Size guard for the client-keyed delta: the section replaces the
        // misc-146 mention and must stay one tight block (~100–200 tokens),
        // not a second primer.
        let bare = render(Some(&fixture_surface()), &["make".to_string()], None)
            .chars()
            .count();
        let claude = render(
            Some(&fixture_surface()),
            &["make".to_string()],
            Some(HostFormat::Claude),
        )
        .chars()
        .count();
        let delta = claude
            .checked_sub(bare)
            .expect("the claude payload extends the bare payload");
        assert!(
            (200..=900).contains(&delta),
            "dispatch delta is {delta} chars (~{} tokens); expected a tight section",
            delta / 4
        );
    }

    #[test]
    fn allow_surface_reflects_the_live_config() {
        // The configured command name appears — the surface is projected from
        // the resolved config, not a hardcoded list.
        let body = render(Some(&fixture_surface()), &["make".to_string()], None);
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
        let body = render(Some(&fixture_surface()), &[], None);
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
        let body = render(Some(&fixture_surface()), &[], None);
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
        let body = render(Some(&fixture_surface()), &[], None);
        for retired in ["spill", "line budget", "valve", "paged"] {
            assert!(
                !body.to_lowercase().contains(retired),
                "payload uses retired output vocabulary {retired:?}: {body}"
            );
        }
    }

    #[test]
    fn subagent_payload_extends_the_body_with_the_per_agent_line() {
        let body = render(Some(&fixture_surface()), &[], None);
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
        let body = payload_body(None);
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
        let live = render(Some(&fixture_surface()), &["make".to_string()], None);
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
        let body = render(Some(&fixture_surface()), &["make".to_string()], None);
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
        let body = emitted_payload(None);
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
        let body = render(Some(&fixture_surface()), &["make".to_string()], None);
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
    fn shipped_antigravity_rules_are_fresh() {
        // Freshness gate (teaching-surface ticket 10): the shipped Antigravity
        // rules file (`plugins/catenary-antigravity/rules/catenary.md`, embedded
        // as `ANTIGRAVITY_RULES_EXPECTED` and doctor freshness-checked) is the
        // `fallback_body` rendering verbatim — the compaction-proof teaching leg.
        // Rules files re-inject per conversation turn, so this file carries the
        // static tiers every turn; teach-03's persisted `PreInvocation`
        // `userMessage` carries the live surface once (and dies at compaction).
        //
        // Unlike a bare fallback body, this file opens with a YAML frontmatter
        // block pinning `trigger: always_on` so the host loads it unconditionally
        // every turn (host contract: agy SKILL.md, "Progressive Disclosure" — "Only
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
        let session_a = render(Some(&surface), &["make".to_string()], None);
        let session_b = render(Some(&surface), &["bazel".to_string()], None);
        assert_ne!(
            session_a, session_b,
            "live payloads differ by the cwd build tool"
        );

        // The context render (empty build set, the declared Antigravity client
        // as in `context_file_body`) is the workspace-invariant body: no cwd
        // input, so identical for every session.
        let ctx = render(Some(&surface), &[], Some(HostFormat::Antigravity));
        assert_eq!(
            ctx,
            render(Some(&surface), &[], Some(HostFormat::Antigravity))
        );
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

    // ── Per-session sliver (Antigravity PreInvocation, ticket 14) ───────

    #[test]
    fn sliver_is_none_without_a_build_tool() {
        // No cwd build tool → nothing session-specific the rules file lacks → no
        // sliver to inject.
        assert!(
            build_tool_sliver(&[]).is_none(),
            "an empty build set yields no sliver"
        );
    }

    #[test]
    fn sliver_carries_the_build_tool_as_a_labeled_block() {
        let sliver = build_tool_sliver(&["make".to_string()]).expect("build tool → sliver");
        // A coherent, self-labeled block.
        assert!(
            sliver.starts_with(SLIVER_LABEL),
            "sliver opens with its label: {sliver}"
        );
        // The build-tool line matches the Tier-1 render's `Build tool:` line.
        assert!(
            sliver.contains("Build tool: make"),
            "sliver carries the cwd build tool: {sliver}"
        );
    }

    #[test]
    fn sliver_and_context_body_do_not_overlap() {
        // The whole point of the split (ticket 14): the always-on rules file
        // ([`context_file_body`]) carries the shared surface, the `PreInvocation`
        // sliver carries only what the rules file structurally cannot — the cwd
        // build tool. No sentence appears in both, so the two never duplicate.
        let sliver = build_tool_sliver(&["make".to_string()]).expect("build tool → sliver");
        let ctx = context_file_body();
        let ctx_lines: std::collections::HashSet<&str> = ctx
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        for line in sliver.lines().map(str::trim).filter(|l| !l.is_empty()) {
            assert!(
                !ctx_lines.contains(line),
                "sliver line duplicated in the context body: {line:?}"
            );
        }
        // Concretely: the sliver's build-tool line is exactly what the context
        // body structurally omits.
        assert!(sliver.contains("Build tool: make"));
        assert!(
            !ctx.contains("Build tool:"),
            "context body omits the per-session build tool: {ctx}"
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
        // live surface). A plain fallback body (no frontmatter, no stamp) is not
        // runtime-stamped, and neither is the shipped Antigravity bootstrap (the
        // fallback body under the `always_on` frontmatter).
        assert!(
            !is_runtime_stamped(&format!("{}\n", fallback_body())),
            "an unstamped fallback body is not runtime-stamped"
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
        // proxy that band is ~2400–3200 chars; we allow a wider 2400..3600 window
        // so ordinary wording tweaks, the small config-driven Tier 1, the brief
        // isolated-subagents mention (misc 146), and the retired-bare-rerun
        // contract line (root-ownership stage 3) don't trip it, while a tier being
        // dropped or doubled (a ~1000-char swing) still does. Rendered against a
        // minimal active fixture surface — a large real allowlist grows Tier 1
        // further, which is expected and unbounded here.
        let chars = render(Some(&fixture_surface()), &["make".to_string()], None)
            .chars()
            .count();
        assert!(
            (2400..=3600).contains(&chars),
            "teaching payload is {chars} chars (~{} tokens); expected the ~600–800 token band",
            chars / 4
        );
    }
}
