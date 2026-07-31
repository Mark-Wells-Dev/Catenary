// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! User context files (misc 224): role-scoped markdown beside the config,
//! injected on each host's session-start seam.
//!
//! Two opaque files live next to `config.toml` in the user config dir:
//!
//! - `AGENTS.md` — served to **lead** (top-level) agents.
//! - `SUBAGENTS.md` — served to dispatched **workers**.
//!
//! The filename IS the role scoping — there are no config keys. Client-specific
//! content rides a suffixed sibling keyed by the token the hooks already speak
//! (the `--format` value, [`HostFormat::as_str`]): `AGENTS.claude.md`,
//! `SUBAGENTS.claude.md`, `AGENTS.antigravity.md`, … The shared file and the
//! client addendum are **concatenated, never overridden** — shared core first,
//! addendum after (specific refines general) — because override semantics force
//! duplicating the invariant core per client, and duplicated policy drifts.
//!
//! Content is **opaque**: Catenary neither parses, validates, nor transforms it.
//! No markdown processing, no frontmatter handling, no templating. Each emitted
//! block is preceded by a one-line provenance header naming the file it came
//! from, so any line a reader sees is traceable to the file that taught it.
//!
//! Absent files are silent no-ops: this is an opt-in surface the user populates
//! by hand, so "no file" means "no block", never a warning.

use std::path::{Path, PathBuf};

use crate::cli::HostFormat;

/// Which role's context files to compose.
///
/// The audience selects the filename stem — the whole of the role scoping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    /// Top-level (lead) agents — `AGENTS.md` and its client addendum.
    Lead,
    /// Dispatched subagent workers — `SUBAGENTS.md` and its client addendum.
    Subagent,
}

impl Audience {
    /// The filename stem this audience reads.
    #[must_use]
    const fn stem(self) -> &'static str {
        match self {
            Self::Lead => "AGENTS",
            Self::Subagent => "SUBAGENTS",
        }
    }
}

/// Opening words of every provenance header.
///
/// A visible line (not an HTML comment): the reader is the agent, and the point
/// of provenance is that a misbehaving agent can be traced back to the file that
/// taught it — which only works if the header is in the text the agent reads.
const PROVENANCE_LABEL: &str = "Catenary user context —";

/// The provenance header naming `path` as the source of the block that follows.
///
/// The path is rendered `~`-compressed ([`crate::bridge::compress_home`]) — the
/// same spelling the rest of Catenary's user-facing surfaces use, and the one
/// the user can paste straight back into an editor.
#[must_use]
fn provenance_header(path: &Path) -> String {
    format!("{PROVENANCE_LABEL} {}:", crate::bridge::compress_home(path))
}

/// The two files that compose this audience's payload, in emission order:
/// the shared core first, the client addendum second.
///
/// `config_base` is the config *base* directory (the one holding `catenary/`),
/// matching [`crate::paths::config_dir`]'s contract.
#[must_use]
fn candidates(config_base: &Path, audience: Audience, client: HostFormat) -> [PathBuf; 2] {
    let dir = config_base.join("catenary");
    let stem = audience.stem();
    [
        dir.join(format!("{stem}.md")),
        dir.join(format!("{stem}.{}.md", client.as_str())),
    ]
}

/// One emitted block: the provenance header for `path` and the file's content
/// verbatim, or `None` when the file is absent, unreadable, or blank.
///
/// The content is passed through untouched apart from trailing newlines, which
/// are trimmed so blocks join with exactly one blank line between them — a seam
/// decision, not a transform of what the file says. A file that exists but holds
/// only whitespace yields no block: a bare header naming an empty file teaches
/// nothing.
///
/// A read failure other than "not found" (a permission bit, a directory in the
/// file's place) is recorded at `debug!` and otherwise treated exactly like an
/// absent file — a hook never breaks the host's flow, and this surface is
/// silent by contract.
#[must_use]
fn block(path: &Path) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(
                    source = crate::source::Source::HookDispatch.as_str(),
                    "user context file {} is unreadable, skipping: {e}",
                    path.display(),
                );
            }
            return None;
        }
    };
    let body = content.trim_end_matches('\n');
    if body.trim().is_empty() {
        return None;
    }
    Some(format!("{}\n{body}", provenance_header(path)))
}

/// Compose this audience's user-context payload from `config_base`, or `None`
/// when neither file exists.
///
/// Shared core then client addendum, joined by a blank line, each under its own
/// provenance header. Split from [`compose`] so the composition is testable
/// against a fixture directory — the process-wide `CATENARY_CONFIG_DIR` cannot
/// be set in-process (Rust 2024 makes `std::env::set_var` `unsafe`, which this
/// crate forbids).
#[must_use]
pub fn compose_in(config_base: &Path, audience: Audience, client: HostFormat) -> Option<String> {
    let blocks: Vec<String> = candidates(config_base, audience, client)
        .iter()
        .filter_map(|path| block(path))
        .collect();
    if blocks.is_empty() {
        None
    } else {
        Some(blocks.join("\n\n"))
    }
}

/// Compose this audience's user-context payload from the live user config dir,
/// or `None` when the user has populated neither file.
///
/// Resolves through [`crate::paths::config_dir`] (honoring the
/// `CATENARY_CONFIG_DIR` override), so an isolated subprocess reads its own
/// files and never the operator's.
#[must_use]
pub fn compose(audience: Audience, client: HostFormat) -> Option<String> {
    compose_in(&crate::paths::config_dir(), audience, client)
}

/// `base` with this audience's user-context payload appended as a trailing
/// block, or `base` unchanged when there is nothing to append.
///
/// The append is the emission shape on both Claude surfaces: the user context
/// closes the `additionalContext` payload, after the teaching body and the
/// situational notices.
#[must_use]
pub fn appended(base: String, audience: Audience, client: HostFormat) -> String {
    match compose(audience, client) {
        Some(block) => format!("{base}\n\n{block}"),
        None => base,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// Write `content` to `<config_base>/catenary/<name>`.
    fn write_context_file(config_base: &Path, name: &str, content: &str) {
        let dir = config_base.join("catenary");
        std::fs::create_dir_all(&dir).expect("create catenary config dir");
        std::fs::write(dir.join(name), content).expect("write context file");
    }

    #[test]
    fn candidates_are_shared_then_client_addendum() {
        let base = Path::new("/cfg");
        let [shared, addendum] = candidates(base, Audience::Lead, HostFormat::Claude);
        assert_eq!(shared, Path::new("/cfg/catenary/AGENTS.md"));
        assert_eq!(addendum, Path::new("/cfg/catenary/AGENTS.claude.md"));

        // The audience is the whole of the role scoping; the client token is the
        // `--format` value the hooks already speak.
        let [shared, addendum] = candidates(base, Audience::Subagent, HostFormat::Antigravity);
        assert_eq!(shared, Path::new("/cfg/catenary/SUBAGENTS.md"));
        assert_eq!(
            addendum,
            Path::new("/cfg/catenary/SUBAGENTS.antigravity.md")
        );

        // The Antigravity LEAD pair — the only user-context surface that host
        // has (its `PreInvocation` turn-0 injection). There is no agy
        // subagent-start seam, so the SUBAGENTS pair above injects nowhere
        // there: a recorded gap (misc 224), deliberately not approximated via
        // `PreToolUse`.
        let [shared, addendum] = candidates(base, Audience::Lead, HostFormat::Antigravity);
        assert_eq!(shared, Path::new("/cfg/catenary/AGENTS.md"));
        assert_eq!(addendum, Path::new("/cfg/catenary/AGENTS.antigravity.md"));
    }

    #[test]
    fn absent_everything_is_silent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Not even the `catenary/` dir exists — no block, no error, no warning.
        assert!(compose_in(tmp.path(), Audience::Lead, HostFormat::Claude).is_none());
        assert!(compose_in(tmp.path(), Audience::Subagent, HostFormat::Claude).is_none());
    }

    #[test]
    fn shared_only_carries_the_shared_file_under_its_provenance_header() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_context_file(tmp.path(), "AGENTS.md", "Discuss before dispatching.\n");

        let payload = compose_in(tmp.path(), Audience::Lead, HostFormat::Claude)
            .expect("the shared file must compose a payload");
        assert!(
            payload.contains("Discuss before dispatching."),
            "content rides verbatim: {payload}"
        );
        let shared = tmp.path().join("catenary").join("AGENTS.md");
        assert!(
            payload.contains(&crate::bridge::compress_home(&shared)),
            "the provenance header must name {}: {payload}",
            shared.display(),
        );
        assert!(
            payload.starts_with(PROVENANCE_LABEL),
            "the payload opens with its provenance header: {payload}"
        );
    }

    #[test]
    fn shared_then_addendum_in_that_order_each_with_provenance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_context_file(tmp.path(), "AGENTS.md", "SHARED-CORE\n");
        write_context_file(tmp.path(), "AGENTS.claude.md", "CLAUDE-ADDENDUM\n");

        let payload = compose_in(tmp.path(), Audience::Lead, HostFormat::Claude)
            .expect("both files must compose a payload");
        let shared_at = payload.find("SHARED-CORE").expect("shared content present");
        let addendum_at = payload
            .find("CLAUDE-ADDENDUM")
            .expect("addendum content present");
        assert!(
            shared_at < addendum_at,
            "shared core comes first, the addendum refines it: {payload}"
        );
        // The addendum NEVER replaces the shared file — both are present.
        let dir = tmp.path().join("catenary");
        assert!(
            payload.contains(&crate::bridge::compress_home(&dir.join("AGENTS.md"))),
            "shared provenance header: {payload}"
        );
        assert!(
            payload.contains(&crate::bridge::compress_home(&dir.join("AGENTS.claude.md"))),
            "addendum provenance header: {payload}"
        );
        // One header per file, and the blocks are separated by a blank line.
        assert_eq!(
            payload.matches(PROVENANCE_LABEL).count(),
            2,
            "exactly one provenance header per concatenated file: {payload}"
        );
        assert!(
            payload.contains("SHARED-CORE\n\nCatenary user context —"),
            "blocks join with one blank line: {payload}"
        );
    }

    #[test]
    fn addendum_only_composes_on_its_own() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_context_file(tmp.path(), "AGENTS.claude.md", "CLAUDE-ONLY\n");

        let payload = compose_in(tmp.path(), Audience::Lead, HostFormat::Claude)
            .expect("an addendum with no shared file still composes");
        assert!(payload.contains("CLAUDE-ONLY"), "{payload}");
        assert_eq!(
            payload.matches(PROVENANCE_LABEL).count(),
            1,
            "only the addendum's header: {payload}"
        );
    }

    #[test]
    fn client_token_keys_the_addendum() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_context_file(tmp.path(), "AGENTS.md", "SHARED-CORE\n");
        write_context_file(tmp.path(), "AGENTS.claude.md", "CLAUDE-ADDENDUM\n");

        // A `claude` dispatch pulls the `.claude.md` addendum …
        let claude = compose_in(tmp.path(), Audience::Lead, HostFormat::Claude)
            .expect("claude payload composes");
        assert!(claude.contains("CLAUDE-ADDENDUM"), "{claude}");

        // … and a different token does not: the shared core rides alone.
        let agy = compose_in(tmp.path(), Audience::Lead, HostFormat::Antigravity)
            .expect("antigravity payload composes from the shared file");
        assert!(agy.contains("SHARED-CORE"), "{agy}");
        assert!(
            !agy.contains("CLAUDE-ADDENDUM"),
            "another client's addendum must not leak: {agy}"
        );
    }

    #[test]
    fn audience_selects_the_file_pair() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_context_file(tmp.path(), "AGENTS.md", "LEAD-POLICY\n");
        write_context_file(tmp.path(), "SUBAGENTS.md", "WORKER-POLICY\n");

        let lead =
            compose_in(tmp.path(), Audience::Lead, HostFormat::Claude).expect("lead payload");
        assert!(lead.contains("LEAD-POLICY"), "{lead}");
        assert!(
            !lead.contains("WORKER-POLICY"),
            "a lead must not read SUBAGENTS.md: {lead}"
        );

        let worker = compose_in(tmp.path(), Audience::Subagent, HostFormat::Claude)
            .expect("subagent payload");
        assert!(worker.contains("WORKER-POLICY"), "{worker}");
        assert!(
            !worker.contains("LEAD-POLICY"),
            "a worker must not read AGENTS.md: {worker}"
        );
        // The stems are distinct files, not a prefix match.
        assert!(
            worker.contains(&crate::bridge::compress_home(
                &tmp.path().join("catenary").join("SUBAGENTS.md")
            )),
            "the worker's provenance header names SUBAGENTS.md: {worker}"
        );
    }

    #[test]
    fn content_is_opaque() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Frontmatter, headings, and `{EDIT}`-style tokens all ride verbatim —
        // Catenary parses, validates, and transforms none of it.
        let raw = "---\nkey: value\n---\n\n# Heading\n\nUse {EDIT} and `catenary`.\n";
        write_context_file(tmp.path(), "AGENTS.md", raw);

        let payload =
            compose_in(tmp.path(), Audience::Lead, HostFormat::Claude).expect("payload composes");
        assert!(
            payload.ends_with(raw.trim_end_matches('\n')),
            "the file's bytes ride verbatim beneath the header: {payload}"
        );
    }

    #[test]
    fn blank_file_yields_no_block() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_context_file(tmp.path(), "AGENTS.md", "\n   \n");
        assert!(
            compose_in(tmp.path(), Audience::Lead, HostFormat::Claude).is_none(),
            "a whitespace-only file teaches nothing, so it emits no header",
        );
    }

    #[test]
    fn appended_is_the_identity_without_files() {
        // `compose` reads the live config dir; under `make check` the operator's
        // real `~/.config/catenary/AGENTS.md` may or may not exist, so this
        // asserts only the invariant that holds either way: the base survives
        // as a prefix.
        let base = "TEACHING-BODY".to_string();
        let out = appended(base.clone(), Audience::Lead, HostFormat::Claude);
        assert!(
            out.starts_with(&base),
            "the base payload must open the result: {out}"
        );
    }
}
