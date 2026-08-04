// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The answer desk (misc 201).
//!
//! Catenary answers Claude Code permission prompts for read-class operations so
//! an agent never parks on one.
//!
//! The ruled policy (decision 031 — "Reads nod through, writes wait"):
//!
//! - **Reads: every prompt answered, none waits.**
//!   1. **Deny only sensitive files** — the lean tier-0 [`SensitiveDenylist`]
//!      (SSH keys, PEM/key material, `.env`-class, cloud credentials). The deny
//!      carries [`SENSITIVE_DENY_MESSAGE`], a teaching, not a park.
//!   2. **Quiet allow** inside the declared [`ReadScope`] — the decision carries
//!      the resolved realpath.
//!   3. **Loud allow** outside declared scope — the read is allowed AND recorded
//!      (a firehose event plus a TUI health finding via `warn!`), never denied.
//!   4. **Writes: the desk answers nothing** — the human decides.
//!
//! This module is the pure policy core: it takes a resolved path plus the
//! canonicalized scope and denylist and returns a [`Decision`]; the emission of
//! the wire field names lives in [`Decision::to_pretooluse_json`] (the
//! `PreToolUse` envelope — the sole delivery seat; the `PermissionRequest`
//! seat retired 2026-07-19), so a field rename is a one-place fix. The
//! transport (`catenary hook pre-tool`) and the daemon method that resolves
//! the scope live elsewhere (`src/cli/hooks.rs`, `src/router.rs`).
//!
//! ## Path-spelling discipline
//!
//! Every path — the prompt's target, the scope roots, and the denylist match —
//! is canonicalized at its ingestion seam ([`canonicalize_lenient`]), so a
//! symlinked-prefix alias gets the SAME verdict as the canonical spelling.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Built-in tier-0 sensitive-path denylist, embedded as data (misc 201).
///
/// Shipped with the binary and consulted by the answer-desk deny leg; the query
/// auto-mount gate (`ensure_ephemeral_mounts` in `src/router.rs`, ws43-05)
/// consumes the same compiled [`SensitiveDenylist`] — one source of truth.
/// User-extended via `[permissions] deny_paths`.
pub const DEFAULT_SENSITIVE_PATHS: &str = include_str!("../defaults/sensitive-paths.toml");

/// The maintainer-dictated teaching a sensitive-file deny carries.
///
/// A hook deny does not park the agent — the model gets this reason and
/// continues. The wording is fixed by ruling (misc 201): name the sensitivity,
/// keep the bytes out of context, offer the file-level escape hatch, note the
/// redaction backstop, and forbid obfuscation that would bypass the sniffer.
pub const SENSITIVE_DENY_MESSAGE: &str = "This file is sensitive — keep its contents out of your context. \
If the bytes need to move, use a file-level operation (`cp`, `install -m 600`) so they move without entering your context. \
Any secrets that do reach a tool's output are automatically redacted by Catenary's PostToolUse hook. \
Do not obfuscate a secret into a form that bypasses the secrets sniffer.";

/// The redaction marker prefix used by the `PostToolUse` backstop.
///
/// The suffix names WHAT was redacted (e.g. `[REDACTED: private key]`), so the
/// agent sees a shape, not the bytes, and knows a real value stood there.
const REDACT_PREFIX: &str = "[REDACTED: ";

/// The `permissionDecisionReason` a quiet (in-scope) allow carries at the
/// `PreToolUse` seat (bug 123).
pub const QUIET_ALLOW_REASON: &str = "Catenary read policy: allowed within declared read scope";

/// The `permissionDecisionReason` a loud (out-of-scope, recorded) allow carries
/// at the `PreToolUse` seat (bug 123).
pub const LOUD_ALLOW_REASON: &str =
    "Catenary read policy: allowed outside declared scope (recorded)";

// ── The sensitive-path denylist ──────────────────────────────────────────────

/// TOML shape of `defaults/sensitive-paths.toml`.
#[derive(Debug, Deserialize)]
struct SensitivePathsFile {
    deny: SensitivePathsDeny,
}

/// The `[deny]` table: the glob patterns plus the `.pub`-style allow-overrides.
#[derive(Debug, Deserialize)]
struct SensitivePathsDeny {
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    allow_overrides: Vec<String>,
}

/// A compiled sensitive-path denylist — the built-in defaults unioned with the
/// user's `[permissions] deny_paths` extensions.
///
/// Each pattern is matched against both the file's basename and the
/// `~`-collapsed path, so `~/.ssh/id_ed25519`, `/home/me/.ssh/id_ed25519`, and a
/// bare `id_ed25519` all match one `**/.ssh/id_*` entry. An `allow_overrides`
/// match (e.g. `*.pub`) wins over a deny match, so a public key is never denied.
#[derive(Debug, Clone)]
pub struct SensitiveDenylist {
    deny: GlobSet,
    allow: GlobSet,
}

impl SensitiveDenylist {
    /// Builds the denylist from the embedded defaults plus user-config
    /// `deny_paths` extensions.
    ///
    /// A malformed built-in or user glob is skipped (never a hard failure — the
    /// desk must not break the host's flow over a bad pattern); a broken default
    /// would surface in the crate's own test, and a broken user glob simply
    /// contributes nothing.
    #[must_use]
    pub fn load(user_deny_paths: &[String]) -> Self {
        let file: SensitivePathsFile =
            toml::from_str(DEFAULT_SENSITIVE_PATHS).unwrap_or(SensitivePathsFile {
                deny: SensitivePathsDeny {
                    patterns: Vec::new(),
                    allow_overrides: Vec::new(),
                },
            });

        let mut deny = GlobSetBuilder::new();
        for pat in file.deny.patterns.iter().chain(user_deny_paths.iter()) {
            if let Ok(glob) = Glob::new(pat) {
                deny.add(glob);
            }
        }
        let mut allow = GlobSetBuilder::new();
        for pat in &file.deny.allow_overrides {
            if let Ok(glob) = Glob::new(pat) {
                allow.add(glob);
            }
        }

        Self {
            deny: deny.build().unwrap_or_else(|_| GlobSet::empty()),
            allow: allow.build().unwrap_or_else(|_| GlobSet::empty()),
        }
    }

    /// Whether `path` (already canonicalized by the caller) is sensitive.
    ///
    /// Tries the `~`-collapsed path and the bare basename against the deny set;
    /// an `allow_overrides` hit on either spelling vetoes the deny.
    #[must_use]
    pub fn is_sensitive(&self, path: &Path) -> bool {
        let collapsed = collapse_home(path);
        let basename = path
            .file_name()
            .map_or_else(|| collapsed.clone(), |n| PathBuf::from(n.to_owned()));

        let allowed = self.allow.is_match(&collapsed) || self.allow.is_match(&basename);
        if allowed {
            return false;
        }
        self.deny.is_match(&collapsed) || self.deny.is_match(&basename)
    }
}

// ── The declared read scope ──────────────────────────────────────────────────

/// The declared readable scope (decision 031).
///
/// The union of the session's workspace roots, the agents-class worktree base,
/// configured companion repos, and the user-config `always_read` path list — the
/// caller folds all of these into one prefix list.
///
/// Every prefix is canonicalized at construction ([`canonicalize_lenient`]) so a
/// symlinked-prefix alias gets the same verdict as the canonical spelling. Pins
/// and ephemeral mounts confer nothing — they are never fed in here (agent-
/// reachable is self-grantable; mount state never converts into a desk answer).
#[derive(Debug, Clone, Default)]
pub struct ReadScope {
    /// Declared prefixes: workspace roots, the agents-worktree base, companion
    /// repos, and the `always_read` prefixes. A read under one of these is a
    /// QUIET allow.
    prefixes: Vec<PathBuf>,
}

impl ReadScope {
    /// Builds a scope, canonicalizing every prefix at ingestion.
    #[must_use]
    pub fn new(prefixes: &[PathBuf]) -> Self {
        Self {
            prefixes: prefixes.iter().map(|p| canonicalize_lenient(p)).collect(),
        }
    }

    /// Classifies a canonical `realpath` against the declared scope.
    #[must_use]
    pub fn classify(&self, realpath: &Path) -> ScopeVerdict {
        if self
            .prefixes
            .iter()
            .any(|prefix| path_is_within(realpath, prefix))
        {
            return ScopeVerdict::InScope;
        }
        ScopeVerdict::OutOfScope
    }
}

/// Where a read's resolved realpath falls relative to the declared scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeVerdict {
    /// Inside a declared prefix (workspace root / companion / agents base /
    /// `always_read`) — a quiet allow.
    InScope,
    /// Outside every declared prefix — a LOUD allow (allow + record).
    OutOfScope,
}

// ── The decision ─────────────────────────────────────────────────────────────

/// The answer-desk verdict for one permission prompt.
///
/// [`Decision::to_pretooluse_json`] (the `PreToolUse` envelope — the sole
/// delivery seat since the `PermissionRequest` seat retired, 2026-07-19) is the
/// emission site for the wire field names (`hookSpecificOutput`,
/// `hookEventName`, `permissionDecision`, `permissionDecisionReason`), so a
/// field rename is a one-place fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Deny the read (sensitive file). Carries the teaching message.
    Deny {
        /// The maintainer-dictated teaching ([`SENSITIVE_DENY_MESSAGE`]).
        message: String,
    },
    /// Quiet allow inside declared scope.
    QuietAllow {
        /// The resolved realpath the verdict is about (the canonical spelling
        /// the scope predicate judged).
        realpath: PathBuf,
    },
    /// Loud allow outside declared scope — the RECORDING (a firehose event + a
    /// TUI health finding) is the caller's responsibility.
    LoudAllow {
        /// The resolved realpath the recording names.
        realpath: PathBuf,
    },
    /// The desk answers nothing — the human's prompt must stand. Emitted for
    /// write-class tools and for every fail-PASS path (unresolvable target,
    /// non-read tool).
    NoDecision,
}

impl Decision {
    /// Render the decision as the `PreToolUse` permission envelope (bug 123 —
    /// the sole delivery seat; the `PermissionRequest` seat retired 2026-07-19),
    /// or `None` for `NoDecision` (absence means the host's normal permission
    /// flow proceeds — silence is the pass-through; never an "ask"-equivalent).
    ///
    /// The shape is the documented `PreToolUse` decision envelope:
    /// `{"hookSpecificOutput": {"hookEventName": "PreToolUse",
    /// "permissionDecision": "allow"|"deny", "permissionDecisionReason": "…"}}`.
    /// It carries NO `updatedInput`.
    #[must_use]
    pub fn to_pretooluse_json(&self) -> Option<serde_json::Value> {
        let (decision, reason) = match self {
            Self::NoDecision => return None,
            Self::Deny { message } => ("deny", message.as_str()),
            Self::QuietAllow { .. } => ("allow", QUIET_ALLOW_REASON),
            Self::LoudAllow { .. } => ("allow", LOUD_ALLOW_REASON),
        };
        Some(serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
                "permissionDecisionReason": reason,
            }
        }))
    }
}

/// Tool classification for the answer desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// A read-class tool (`Read`, and the host `Grep`/`Glob` prompts). Routed
    /// through the read policy.
    Read,
    /// Everything else (write-class and unknown). The desk answers nothing.
    Other,
}

/// Classify a Claude Code tool name for the desk.
///
/// Read-class = `Read` plus the host `Grep`/`Glob` tools (their prompts park an
/// agent exactly like a `Read` prompt). Every other tool — `Write`, `Edit`,
/// `Bash`, `NotebookEdit`, an MCP tool — is [`ToolClass::Other`]: the desk emits
/// no decision, so write-class prompts pass through untouched and the human
/// decides.
#[must_use]
pub fn classify_tool(tool_name: &str) -> ToolClass {
    match tool_name {
        "Read" | "Grep" | "Glob" => ToolClass::Read,
        _ => ToolClass::Other,
    }
}

/// The read-policy decision for a resolved read prompt (decision 031).
///
/// `realpath` is the CANONICAL target (the caller canonicalizes at ingestion).
///
/// Order is the ruled order: sensitive-deny first (even inside scope — a `.env`
/// in the workspace is still sensitive), then scope classification.
#[must_use]
pub fn decide_read(realpath: &Path, scope: &ReadScope, denylist: &SensitiveDenylist) -> Decision {
    if denylist.is_sensitive(realpath) {
        return Decision::Deny {
            message: SENSITIVE_DENY_MESSAGE.to_string(),
        };
    }
    match scope.classify(realpath) {
        ScopeVerdict::InScope => Decision::QuietAllow {
            realpath: realpath.to_path_buf(),
        },
        ScopeVerdict::OutOfScope => Decision::LoudAllow {
            realpath: realpath.to_path_buf(),
        },
    }
}

// ── Path helpers (the spelling rule) ─────────────────────────────────────────

/// Canonicalize `path` to its realpath — the read-policy ingestion seam.
///
/// A thin alias for [`crate::paths::canonicalize_lenient`], which is the single
/// implementation of the lenient-canonicalize idiom (misc 230 follow-up). It
/// resolves the nearest existing ancestor and keeps the not-yet-existing tail,
/// so a symlinked-prefix alias gives one answer per inode-path **whether or not
/// the leaf exists yet** — a read prompt naming a file that has not been created
/// no longer classifies against a different spelling than the same file gets a
/// moment later.
#[must_use]
pub fn canonicalize_lenient(path: &Path) -> PathBuf {
    crate::paths::canonicalize_lenient(path)
}

/// Whether `path` is `prefix` itself or lives beneath it.
///
/// Both sides are expected canonical (the caller's ingestion seam), so this is a
/// pure component-wise prefix test — no further fs touch.
/// [`Path::starts_with`] is component-wise (not a raw string prefix), so it is
/// both equality-inclusive and free of the `/a/bc` vs `/a/b` false match.
#[must_use]
fn path_is_within(path: &Path, prefix: &Path) -> bool {
    path.starts_with(prefix)
}

/// Collapse a leading `$HOME` in `path` to a `~`-rooted path for glob matching.
///
/// `~/.ssh/id_ed25519` in a pattern must match the absolute `/home/me/.ssh/…`
/// realpath, so both meet at the `~`-collapsed spelling. A path outside the home
/// directory (or a homeless host) is returned unchanged.
///
/// Home resolves through [`crate::paths::home_dir`] (misc 229) — the same
/// resolver every other home-rooted path uses, `%USERPROFILE%` fallback
/// included, so a `CATENARY_HOME_DIR` test moves the denylist's home with it.
#[must_use]
fn collapse_home(path: &Path) -> PathBuf {
    let Some(home) = crate::paths::home_dir() else {
        return path.to_path_buf();
    };
    path.strip_prefix(&home)
        .map_or_else(|_| path.to_path_buf(), |rest| PathBuf::from("~").join(rest))
}

// ── The PostToolUse secret-redaction backstop ────────────────────────────────

/// A high-confidence secret shape, shipped as data (misc 201, component 3).
///
/// Each entry pairs a hand-written matcher with the label the redaction marker
/// names. The corpus is deliberately LEAN — a false positive (redacting a
/// non-secret) is worse than a miss here, so every shape is unambiguous. No
/// dependency: matching is hand-rolled byte scanning, not regex.
struct RedactionPattern {
    /// What the marker names, e.g. `private key` → `[REDACTED: private key]`.
    label: &'static str,
    /// The matcher: given the full output, returns the byte spans to redact.
    find: fn(&str) -> Vec<(usize, usize)>,
}

/// The redaction pattern table (misc 201). Ordered, but spans are merged before
/// replacement so overlap between two patterns collapses to one marker.
const REDACTION_PATTERNS: &[RedactionPattern] = &[
    RedactionPattern {
        label: "private key",
        find: find_pem_private_key_blocks,
    },
    RedactionPattern {
        label: "AWS access key ID",
        find: find_aws_access_key_ids,
    },
    RedactionPattern {
        label: "GitHub token",
        find: find_github_tokens,
    },
    RedactionPattern {
        label: "Slack token",
        find: find_slack_tokens,
    },
];

/// Scan `output` for high-confidence secret shapes and return the redacted text.
///
/// Returns `None` when the output is clean (no hit), so the caller emits NO
/// `updatedToolOutput` at all and the original bytes pass through byte-identical.
///
/// On a hit, every matched span is replaced by a marker naming WHAT was redacted
/// (`[REDACTED: private key]`) — the surrounding output is preserved byte-for-
/// byte, the rest is never blocked or discarded.
#[must_use]
pub fn redact_secrets(output: &str) -> Option<String> {
    // Collect (start, end, label) spans across every pattern.
    let mut hits: Vec<(usize, usize, &'static str)> = Vec::new();
    for pattern in REDACTION_PATTERNS {
        for (start, end) in (pattern.find)(output) {
            if end > start && end <= output.len() {
                hits.push((start, end, pattern.label));
            }
        }
    }
    if hits.is_empty() {
        return None;
    }

    // Sort by start, then drop any span that overlaps an earlier kept one (the
    // earlier, wider span wins — e.g. a token inside a PEM block redacts once).
    hits.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
    let mut kept: Vec<(usize, usize, &'static str)> = Vec::new();
    let mut cursor = 0usize;
    for (start, end, label) in hits {
        if start >= cursor {
            kept.push((start, end, label));
            cursor = end;
        }
    }

    // Rebuild the output, replacing each kept span with its marker.
    let mut redacted = String::with_capacity(output.len());
    let mut pos = 0usize;
    for (start, end, label) in kept {
        redacted.push_str(&output[pos..start]);
        redacted.push_str(REDACT_PREFIX);
        redacted.push_str(label);
        redacted.push(']');
        pos = end;
    }
    redacted.push_str(&output[pos..]);
    Some(redacted)
}

/// Find PEM / OpenSSH private-key armor blocks: `-----BEGIN … PRIVATE KEY-----`
/// through the matching `-----END … PRIVATE KEY-----`.
///
/// Covers `RSA`/`EC`/`OPENSSH`/`DSA`/`PGP` private keys and the generic
/// `-----BEGIN PRIVATE KEY-----` form in one sweep — the whole armored block
/// (header, body, footer) is one span, so the key material never survives.
fn find_pem_private_key_blocks(text: &str) -> Vec<(usize, usize)> {
    const BEGIN: &str = "-----BEGIN ";
    const END_MARK: &str = "PRIVATE KEY-----";
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(BEGIN) {
        let begin = search + rel;
        // The header must be a PRIVATE KEY header on its own armor line.
        let header_end = text[begin..]
            .find("-----\n")
            .or_else(|| text[begin..].find("-----"));
        let Some(header_end_rel) = header_end else {
            break;
        };
        let header_line_end = begin + header_end_rel + "-----".len();
        let header = &text[begin..header_line_end];
        if !header.contains("PRIVATE KEY") {
            search = header_line_end;
            continue;
        }
        // Find the matching END … PRIVATE KEY----- footer after the header.
        if let Some(end_rel) = text[header_line_end..].find(END_MARK) {
            let end = header_line_end + end_rel + END_MARK.len();
            spans.push((begin, end));
            search = end;
        } else {
            // Unterminated block: redact to end of output rather than leak the body.
            spans.push((begin, bytes.len()));
            break;
        }
    }
    spans
}

/// Find AWS access key IDs: `AKIA` followed by 16 uppercase-alphanumeric chars
/// (the classic 20-char access-key-ID shape), also matching the `ASIA` temporary
/// form.
fn find_aws_access_key_ids(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for prefix in ["AKIA", "ASIA"] {
        let mut search = 0usize;
        while let Some(rel) = text[search..].find(prefix) {
            let start = search + rel;
            let after = start + prefix.len();
            // Exactly 16 [A-Z0-9] following the prefix, not preceded by an
            // alnum (so it stands alone, not a substring of a longer token).
            let preceded_ok = start == 0
                || !text[..start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_ascii_alphanumeric());
            let tail: String = text[after..]
                .chars()
                .take(16)
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                .collect();
            let follows_ok = text[after + tail.len()..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            if preceded_ok && tail.len() == 16 && follows_ok {
                spans.push((start, after + 16));
                search = after + 16;
            } else {
                search = after;
            }
        }
    }
    spans
}

/// Find GitHub tokens: the `ghp_`/`gho_`/`ghs_`/`ghu_`/`ghr_` prefixes followed
/// by a run of token characters (≥ 20, the personal/OAuth/server/user/refresh
/// token shapes).
fn find_github_tokens(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for prefix in ["ghp_", "gho_", "ghs_", "ghu_", "ghr_"] {
        let mut search = 0usize;
        while let Some(rel) = text[search..].find(prefix) {
            let start = search + rel;
            let after = start + prefix.len();
            let run: String = text[after..]
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect();
            if run.len() >= 20 {
                let end = after + run.len();
                spans.push((start, end));
                search = end;
            } else {
                search = after;
            }
        }
    }
    spans
}

/// Find Slack tokens: `xox` followed by a type char and `-`, then the token body
/// (`xoxb-`/`xoxp-`/`xoxa-`/`xoxr-`/`xoxs-`/`xoxo-…`).
fn find_slack_tokens(text: &str) -> Vec<(usize, usize)> {
    const PREFIX: &str = "xox";
    let mut spans = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = text[search..].find(PREFIX) {
        let start = search + rel;
        let after = start + PREFIX.len();
        // Next: a single type char then a `-`, then ≥ 10 token-body chars.
        let mut chars = text[after..].char_indices();
        let type_ok = chars.next().is_some_and(|(_, c)| c.is_ascii_alphanumeric());
        let dash_ok = chars.next().is_some_and(|(_, c)| c == '-');
        if type_ok && dash_ok {
            // after + 2 bytes consumed (type + dash) — both ASCII.
            let body_start = after + 2;
            let body: String = text[body_start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect();
            if body.len() >= 10 {
                let end = body_start + body.len();
                spans.push((start, end));
                search = end;
                continue;
            }
        }
        search = after;
    }
    spans
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Denylist ─────────────────────────────────────────────────────────

    #[test]
    fn default_denylist_flags_ssh_keys_and_env_and_creds() {
        let deny = SensitiveDenylist::load(&[]);
        assert!(deny.is_sensitive(Path::new("/home/me/.ssh/id_ed25519")));
        assert!(deny.is_sensitive(Path::new("/home/me/.ssh/id_rsa")));
        assert!(deny.is_sensitive(Path::new("/srv/app/.env")));
        assert!(deny.is_sensitive(Path::new("/srv/app/.env.production")));
        assert!(deny.is_sensitive(Path::new("/home/me/.aws/credentials")));
        assert!(deny.is_sensitive(Path::new("/tmp/server.pem")));
        assert!(deny.is_sensitive(Path::new("/tmp/tls.key")));
    }

    #[test]
    fn default_denylist_allows_public_key_and_ordinary_files() {
        let deny = SensitiveDenylist::load(&[]);
        // A `.pub` is public — the allow-override vetoes the deny.
        assert!(!deny.is_sensitive(Path::new("/home/me/.ssh/id_ed25519.pub")));
        assert!(!deny.is_sensitive(Path::new("/src/main.rs")));
        assert!(!deny.is_sensitive(Path::new("/src/lib.rs")));
    }

    #[test]
    fn user_deny_paths_extend_the_denylist() {
        let deny = SensitiveDenylist::load(&["**/secret-plans/**".to_string()]);
        assert!(deny.is_sensitive(Path::new("/work/secret-plans/q3.md")));
        assert!(!deny.is_sensitive(Path::new("/work/public/q3.md")));
    }

    // ── Scope predicate ──────────────────────────────────────────────────

    #[test]
    fn scope_classifies_in_scope_and_out_of_scope() {
        let scope = ReadScope::new(&[PathBuf::from("/work/repo"), PathBuf::from("/docs")]);
        assert_eq!(
            scope.classify(Path::new("/work/repo/src/main.rs")),
            ScopeVerdict::InScope
        );
        assert_eq!(
            scope.classify(Path::new("/docs/guide.md")),
            ScopeVerdict::InScope
        );
        assert_eq!(
            scope.classify(Path::new("/etc/hosts")),
            ScopeVerdict::OutOfScope
        );
    }

    // ── Symlink-alias pins (the spelling rule) ───────────────────────────

    #[test]
    fn scope_predicate_matches_through_a_symlinked_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real-root");
        std::fs::create_dir_all(real.join("src")).expect("mkdir real");
        std::fs::write(real.join("src/main.rs"), "fn main() {}").expect("write");
        let link = tmp.path().join("alias-root");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        // Scope declared with the CANONICAL real root; a read spelled through
        // the symlinked alias must land in-scope after canonicalization.
        let scope = ReadScope::new(std::slice::from_ref(&real));
        let aliased_read = canonicalize_lenient(&link.join("src/main.rs"));
        assert_eq!(scope.classify(&aliased_read), ScopeVerdict::InScope);
    }

    #[test]
    fn denylist_matches_through_a_symlinked_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sshdir = tmp.path().join(".ssh");
        std::fs::create_dir_all(&sshdir).expect("mkdir .ssh");
        let key = sshdir.join("id_ed25519");
        std::fs::write(&key, "PRIVATE").expect("write key");
        let link = tmp.path().join("ssh-alias");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&sshdir, &link).expect("symlink");

        let deny = SensitiveDenylist::load(&[]);
        // The alias canonicalizes to the real `.ssh/id_ed25519`, still sensitive.
        let aliased = canonicalize_lenient(&link.join("id_ed25519"));
        assert!(deny.is_sensitive(&aliased));
    }

    // ── decide_read ──────────────────────────────────────────────────────

    #[test]
    fn decide_read_denies_sensitive_even_inside_scope() {
        let scope = ReadScope::new(&[PathBuf::from("/work/repo")]);
        let deny = SensitiveDenylist::load(&[]);
        // A `.env` inside the workspace is still sensitive.
        let d = decide_read(Path::new("/work/repo/.env"), &scope, &deny);
        assert_eq!(
            d,
            Decision::Deny {
                message: SENSITIVE_DENY_MESSAGE.to_string()
            }
        );
    }

    #[test]
    fn decide_read_quiet_allow_in_scope_pins_realpath() {
        let scope = ReadScope::new(&[PathBuf::from("/work/repo")]);
        let deny = SensitiveDenylist::load(&[]);
        let d = decide_read(Path::new("/work/repo/src/main.rs"), &scope, &deny);
        assert_eq!(
            d,
            Decision::QuietAllow {
                realpath: PathBuf::from("/work/repo/src/main.rs")
            }
        );
    }

    #[test]
    fn decide_read_loud_allow_out_of_scope() {
        let scope = ReadScope::new(&[PathBuf::from("/work/repo")]);
        let deny = SensitiveDenylist::load(&[]);
        let d = decide_read(Path::new("/etc/hosts"), &scope, &deny);
        assert_eq!(
            d,
            Decision::LoudAllow {
                realpath: PathBuf::from("/etc/hosts")
            }
        );
    }

    // ── The documented PreToolUse envelope (bug 123 — the delivery seat) ──

    #[test]
    fn pretooluse_envelope_allow_pins_documented_shape_verbatim() {
        let d = Decision::QuietAllow {
            realpath: PathBuf::from("/work/repo/src/main.rs"),
        };
        let json = d.to_pretooluse_json().expect("allow emits json");
        assert_eq!(
            json["hookSpecificOutput"],
            serde_json::json!({
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": QUIET_ALLOW_REASON,
            })
        );
    }

    #[test]
    fn pretooluse_envelope_deny_pins_documented_shape() {
        let d = Decision::Deny {
            message: SENSITIVE_DENY_MESSAGE.to_string(),
        };
        let json = d.to_pretooluse_json().expect("deny emits json");
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            json["hookSpecificOutput"]["permissionDecisionReason"],
            SENSITIVE_DENY_MESSAGE
        );
    }

    #[test]
    fn pretooluse_loud_allow_pins_recorded_reason() {
        let d = Decision::LoudAllow {
            realpath: PathBuf::from("/etc/hosts"),
        };
        let json = d.to_pretooluse_json().expect("loud allow emits json");
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            json["hookSpecificOutput"]["permissionDecisionReason"],
            LOUD_ALLOW_REASON
        );
    }

    #[test]
    fn pretooluse_no_decision_emits_nothing() {
        assert!(Decision::NoDecision.to_pretooluse_json().is_none());
    }

    // ── Tool classification ──────────────────────────────────────────────

    #[test]
    fn classify_tool_read_class_vs_other() {
        assert_eq!(classify_tool("Read"), ToolClass::Read);
        assert_eq!(classify_tool("Grep"), ToolClass::Read);
        assert_eq!(classify_tool("Glob"), ToolClass::Read);
        assert_eq!(classify_tool("Write"), ToolClass::Other);
        assert_eq!(classify_tool("Edit"), ToolClass::Other);
        assert_eq!(classify_tool("Bash"), ToolClass::Other);
        assert_eq!(classify_tool("NotebookEdit"), ToolClass::Other);
    }

    // ── Redaction ────────────────────────────────────────────────────────

    #[test]
    fn clean_output_passes_through_as_none() {
        assert!(redact_secrets("just ordinary text, nothing secret here").is_none());
        assert!(redact_secrets("").is_none());
        // A public key prefix `AKIA` substring that isn't a real ID stays clean.
        assert!(redact_secrets("PACKAGE_AKIALIKE_lowercase").is_none());
    }

    #[test]
    fn redacts_pem_private_key_block_and_preserves_surroundings() {
        let output = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIabc\nDEFghi\n-----END RSA PRIVATE KEY-----\nafter";
        let redacted = redact_secrets(output).expect("pem redacted");
        assert!(redacted.starts_with("before\n"));
        assert!(redacted.ends_with("\nafter"));
        assert!(redacted.contains("[REDACTED: private key]"));
        assert!(!redacted.contains("MIIabc"));
    }

    #[test]
    fn redacts_generic_private_key_block() {
        let output = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----";
        let redacted = redact_secrets(output).expect("generic pem redacted");
        assert_eq!(redacted, "[REDACTED: private key]");
    }

    #[test]
    fn redacts_openssh_private_key_block() {
        let output =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNza\n-----END OPENSSH PRIVATE KEY-----";
        let redacted = redact_secrets(output).expect("openssh pem redacted");
        assert_eq!(redacted, "[REDACTED: private key]");
    }

    #[test]
    fn redacts_aws_access_key_id() {
        let output = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE done";
        let redacted = redact_secrets(output).expect("aws redacted");
        assert_eq!(
            redacted,
            "AWS_ACCESS_KEY_ID=[REDACTED: AWS access key ID] done"
        );
    }

    #[test]
    fn redacts_github_token() {
        let output = "token: ghp_1234567890abcdefghijABCDEFGHIJ0987 end";
        let redacted = redact_secrets(output).expect("gh redacted");
        assert!(redacted.contains("[REDACTED: GitHub token]"));
        assert!(!redacted.contains("ghp_1234567890"));
        assert!(redacted.starts_with("token: "));
        assert!(redacted.ends_with(" end"));
    }

    #[test]
    fn redacts_slack_token() {
        // The fixture token is assembled at runtime so no literal token shape
        // lives in the source: GitHub push protection scans blob content and
        // rejects a commit carrying a realistic Slack token, even in a test —
        // the redactor and the upstream scanner match the same shapes by design.
        let token = format!("xox{}-2401234567-abcDEF12ghiJKL", 'b');
        let output = format!("SLACK={token} end");
        let redacted = redact_secrets(&output).expect("slack redacted");
        assert!(redacted.contains("[REDACTED: Slack token]"));
        assert!(!redacted.contains("2401234567"));
    }

    #[test]
    fn redaction_preserves_surrounding_output_byte_for_byte() {
        // The non-secret bytes are preserved exactly; only the span is swapped.
        let output = "line1\nkey=AKIAIOSFODNN7EXAMPLE\nline3";
        let redacted = redact_secrets(output).expect("redacted");
        assert_eq!(redacted, "line1\nkey=[REDACTED: AWS access key ID]\nline3");
    }

    #[test]
    fn multiple_secrets_each_redact() {
        let output = "AKIAIOSFODNN7EXAMPLE and ghp_1234567890abcdefghijABCDEFGHIJ0987";
        let redacted = redact_secrets(output).expect("redacted");
        assert!(redacted.contains("[REDACTED: AWS access key ID]"));
        assert!(redacted.contains("[REDACTED: GitHub token]"));
    }
}
