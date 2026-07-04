// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Filesystem path resolvers for Catenary's base directories.
//!
//! Catenary keeps its data across three XDG base directories, each chosen for
//! its durability semantics:
//!
//! - [`state_dir`] — durable, per-host state (the Unix socket).
//! - [`runtime_dir`] — ephemeral, tmpfs-backed runtime files (the `state.json`
//!   snapshot and the per-session diagnostics receipt stores).
//! - [`cache_dir`] — regenerable, high-volume telemetry (the JSONL firehose).
//!
//! [`encode_cwd`] flattens an absolute path into a single filesystem-safe
//! directory-name component, used as the per-root shard key in the firehose tree.
//! [`diagnostics_receipt_dir`] / [`diagnostics_receipt_file`] locate the
//! per-session `catenary diagnostics` receipt store under [`runtime_dir`];
//! [`discontinuity_mark_dir`] / [`discontinuity_mark_file`] locate the
//! per-session Gemini `PreCompress` discontinuity mark under [`runtime_dir`].

use std::path::{Path, PathBuf};

/// Resolve the Catenary state directory.
///
/// Resolution order:
/// 1. `CATENARY_STATE_DIR` environment variable (cross-platform override).
/// 2. `dirs::state_dir()` (`XDG_STATE_HOME` on Linux).
/// 3. `dirs::data_local_dir()` (macOS / Windows fallback).
/// 4. `/tmp` as a last resort.
#[must_use]
pub fn state_dir() -> PathBuf {
    std::env::var_os("CATENARY_STATE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Resolve the Catenary runtime directory.
///
/// Home for ephemeral, regenerable runtime files (the daemon-owned `state.json`
/// snapshot) — tmpfs-backed and OS-cleared on logout on Linux, which is the
/// semantically-correct place for them. Unlike the socket (which lives under
/// [`state_dir`]), these files do not need to survive a logout.
///
/// Resolution order:
/// 1. `CATENARY_RUNTIME_DIR` environment variable (cross-platform override).
/// 2. `dirs::runtime_dir()` (`XDG_RUNTIME_DIR` on Linux).
/// 3. [`state_dir`] as a fallback when no runtime dir is configured (macOS /
///    Windows, or `XDG_RUNTIME_DIR` unset).
#[must_use]
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("CATENARY_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(dirs::runtime_dir)
        .unwrap_or_else(state_dir)
}

/// Resolve the Catenary cache directory.
///
/// Home for the regenerable JSONL telemetry firehose — safe to delete, never
/// holds durable state. Unlike [`state_dir`] (socket) and [`runtime_dir`] (small
/// ephemeral runtime reports), the cache dir holds high-volume, append-mostly
/// logs that can be discarded at any time without affecting correctness.
///
/// Resolution order:
/// 1. `CATENARY_CACHE_DIR` environment variable (cross-platform override).
/// 2. `dirs::cache_dir()` (`XDG_CACHE_HOME` on Linux).
/// 3. [`state_dir`] as a fallback when no cache dir is configured.
#[must_use]
pub fn cache_dir() -> PathBuf {
    std::env::var_os("CATENARY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(dirs::cache_dir)
        .unwrap_or_else(state_dir)
}

/// Flatten a string into one filesystem-safe path component.
///
/// Every character that is not ASCII alphanumeric (path separators, `.`, `_`,
/// spaces, …) becomes `-`. Shared by [`encode_cwd`] (the firehose shard key)
/// and [`diagnostics_receipt_file`] (the per-session receipt-store name). The
/// mapping is stable but intentionally lossy — distinct inputs can collide
/// (e.g. `a/b` and `a.b`) — which is acceptable for the regenerable ephemera
/// both callers key.
fn flatten_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Flatten an absolute path into one filesystem-safe directory-name component.
///
/// Matches the encoding Claude Code uses for `~/.claude/projects/`: every
/// character that is not ASCII alphanumeric (path separators, `.`, `_`,
/// spaces, …) becomes `-`.
///
/// `/home/mark/Projects/Catenary` → `-home-mark-Projects-Catenary`.
///
/// Used as the per-root shard key in the JSONL firehose tree. The encoding is
/// stable but intentionally lossy — it is a shard key, not a reversible
/// encoding, so distinct paths can collide (e.g. `a/b` and `a.b`), which is
/// acceptable for a regenerable cache.
#[must_use]
pub fn encode_cwd(path: &Path) -> String {
    flatten_component(&path.to_string_lossy())
}

/// Directory holding the per-session `catenary diagnostics` receipt stores.
///
/// Co-located with the `state.json` snapshot under `runtime_dir()/catenary/`, so
/// it shares that ephemeral, tmpfs-backed, OS-cleared-on-logout lifecycle. The
/// daemon writes the full rendered receipt here at compute time (misc 139 / bug
/// 60) so a `catenary diagnostics` CLI client killed after dispatch cannot lose
/// it; the next bare run points the agent back at the store. The receipt is
/// regenerable ephemera — a lost store just means the next run recomputes.
#[must_use]
pub fn diagnostics_receipt_dir() -> PathBuf {
    runtime_dir().join("catenary").join("receipts")
}

/// The per-session receipt-store filename: the host `session_id` flattened to a
/// single filesystem-safe component (same rule as [`encode_cwd`]) plus `.txt`.
///
/// The flattening is lossy — matching the firehose shard-key tradeoff — so two
/// session ids differing only in punctuation could share a store; realistic host
/// session ids (UUIDs) never collide, and the store is regenerable ephemera.
#[must_use]
pub fn diagnostics_receipt_file(session_id: &str) -> String {
    format!("{}.txt", flatten_component(session_id))
}

/// Directory holding the per-session Gemini `PreCompress` discontinuity marks.
///
/// Co-located with the `state.json` snapshot and the diagnostics receipts under
/// `runtime_dir()/catenary/`, so it shares that ephemeral, tmpfs-backed,
/// OS-cleared-on-logout lifecycle. The Gemini `pre-compress` hook writes a marker
/// here on a real (manual) compaction; the next `before-agent` hook consumes it
/// to re-inject the teaching payload once (teaching-surface ticket 14). The mark
/// is regenerable ephemera — a lost mark just means one skipped re-injection, and
/// no daemon is involved.
#[must_use]
pub fn discontinuity_mark_dir() -> PathBuf {
    runtime_dir().join("catenary").join("marks")
}

/// The per-session discontinuity-mark filename: the host `session_id` flattened
/// to a single filesystem-safe component (same rule as [`encode_cwd`]) plus
/// `.mark`.
///
/// The flattening is lossy — matching the firehose shard-key tradeoff — so two
/// session ids differing only in punctuation could share a mark; realistic host
/// session ids (UUIDs) never collide, and the mark is regenerable ephemera.
#[must_use]
pub fn discontinuity_mark_file(session_id: &str) -> String {
    format!("{}.mark", flatten_component(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_matches_claude_code_form() {
        assert_eq!(
            encode_cwd(Path::new("/home/mark/Projects/Catenary")),
            "-home-mark-Projects-Catenary"
        );
    }

    #[test]
    fn encode_cwd_replaces_dots_underscores_and_preserves_dashes() {
        // `/` `.` `_` and spaces all map to `-`; existing `-` and alphanumerics
        // survive (mirrors Claude Code's `[^a-zA-Z0-9] -> -` rule).
        assert_eq!(
            encode_cwd(Path::new("/home/mark/.local/share/dot_local")),
            "-home-mark--local-share-dot-local"
        );
        assert_eq!(encode_cwd(Path::new("/p/Catenary-00")), "-p-Catenary-00");
    }

    #[test]
    fn encode_cwd_is_stable() {
        let p = Path::new("/a/b/c");
        assert_eq!(encode_cwd(p), encode_cwd(p));
    }

    #[test]
    fn diagnostics_receipt_file_flattens_session_id() {
        // A UUID-shaped session id survives verbatim (hex + dashes are all
        // preserved), so the store name is legible.
        assert_eq!(
            diagnostics_receipt_file("7da239b1-d3c7-42b7-a7a4-38b4205f576a"),
            "7da239b1-d3c7-42b7-a7a4-38b4205f576a.txt"
        );
        // Punctuation that is unsafe in a filename flattens to `-`.
        assert_eq!(diagnostics_receipt_file("sess/1.2"), "sess-1-2.txt");
    }

    #[test]
    fn diagnostics_receipt_dir_sits_beside_state_json() {
        // The receipt store shares the `runtime_dir()/catenary/` parent with the
        // `state.json` snapshot (same ephemeral lifecycle).
        let dir = diagnostics_receipt_dir();
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("receipts"));
        assert_eq!(
            dir.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("catenary")
        );
    }

    #[test]
    fn discontinuity_mark_file_flattens_session_id() {
        // A UUID-shaped session id survives verbatim; unsafe punctuation flattens
        // to `-` (same rule as the diagnostics receipt store).
        assert_eq!(
            discontinuity_mark_file("7da239b1-d3c7-42b7-a7a4-38b4205f576a"),
            "7da239b1-d3c7-42b7-a7a4-38b4205f576a.mark"
        );
        assert_eq!(discontinuity_mark_file("sess/1.2"), "sess-1-2.mark");
    }

    #[test]
    fn discontinuity_mark_dir_sits_beside_state_json() {
        // The mark store shares the `runtime_dir()/catenary/` parent with the
        // `state.json` snapshot and the diagnostics receipts (same ephemeral
        // lifecycle).
        let dir = discontinuity_mark_dir();
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("marks"));
        assert_eq!(
            dir.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some("catenary")
        );
    }
}
