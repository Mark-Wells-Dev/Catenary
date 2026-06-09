// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Firehose reaping — one bound per level of the JSONL storage tree.
//!
//! The `SQLite` `messages` firehose had **no retention** (only cascade-delete on
//! a 7-day-dead session deletion, which never fired on a long-running daemon); it
//! grew to 2.3 GB / ~5M rows and wedged the daemon. The 2.0 bar is absolute: a
//! build must not be able to wedge a user. So every level of the JSONL tree
//! ([`crate::logging::jsonl_sink`]) has exactly one reaper:
//!
//! | Level | Bound | When | Owner |
//! |---|---|---|---|
//! | instance dir | keep last `N_inst` dead (+ age `X`) | daemon startup | [`reap_instances`] |
//! | tool dir (`grep`/`glob`) | byte budget, evict oldest by ts-prefix | on write | `jsonl_sink` |
//! | per-scope files (`sessions/`, dead-root `servers/`) | dead + stale (days) | periodic | [`sweep_stale`] |
//! | streams (`servers/`, `mcp`, `trace`) | segment rotation (size, keep `K`) | on write | `jsonl_sink` |
//!
//! **rotation** bounds growth *within* a daemon lifetime (never-idle streams);
//! the **instance cap** bounds it *across* lifetimes (start/stop churn); the
//! **per-tool byte budget** bounds call-file width; the **staleness sweep**
//! removes dead sessions and dead-root server files. Session files need no size
//! bound — append is O(1) and every reader tails or seeks, so length is
//! irrelevant.
//!
//! This module owns the two periodic/startup reapers ([`reap_instances`],
//! [`sweep_stale`]); the two on-write reapers (rotation, per-tool budget) live on
//! the write path in [`crate::logging::jsonl_sink`]. [`ReapPolicy`] carries the
//! tunable knobs for all four and is shared between this module and the sink.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::db::encode_cwd;
use crate::source::Source;

/// Seconds in a day, for the day-granularity age/staleness knobs.
const SECS_PER_DAY: u64 = 86_400;

/// How often the daemon runs the periodic [`sweep_stale`] timer.
///
/// Hourly: the wedge risk is multi-day uptime, so a coarse cadence bounds
/// dead-file accumulation while keeping the sweep off the hot path. The startup
/// [`reap_instances`] pass and on-write rotation/budget cover everything between
/// ticks.
pub const STALENESS_SWEEP_INTERVAL: Duration = Duration::from_hours(1);

/// Tunable bounds for firehose reaping, shared by the on-write reapers
/// ([`crate::logging::jsonl_sink`]) and the startup/periodic reapers here.
///
/// Surfaced as the `[observability]` config section with these defaults; no user
/// action is required. Bumping any knob is a one-line change with no format
/// impact. Staleness reuses the existing `log_retention_days` config key (passed
/// separately to [`sweep_stale`]); it is not duplicated here.
///
/// At the defaults, the worst-case footprint at `L` live projects, `S` servers
/// each, is ≈ `L × S × segments_kept × segment_bytes` for server streams plus
/// `tool_dir_budget` per tool dir — bounded and, in realistic idle/typical use,
/// in the low tens of MB.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct ReapPolicy {
    /// Stream segment size: a stream (`mcp`/`trace`/`servers`) rolls to a new
    /// segment once the live file reaches this many bytes.
    pub segment_bytes: u64,
    /// Stream segments retained per stream, **including the live file** (`K`).
    /// `K = 3` with `segment_bytes = 2 MiB` ⇒ a 6 MiB per-stream ceiling. The
    /// oldest segment is dropped on each roll.
    pub segments_kept: usize,
    /// Per-tool-dir (`grep`/`glob`) byte budget. On write, the oldest call files
    /// (by lexical ts-prefix) are evicted until the dir is under budget.
    pub tool_dir_budget: u64,
    /// Dead instance dirs retained at startup (`N_inst`). One daemon per host ⇒
    /// every non-self instance dir is dead; the newest `N_inst` within
    /// [`Self::instance_max_age_days`] are kept, the rest `rm -rf`'d.
    pub instance_keep: usize,
    /// Maximum age (days) of a retained dead instance dir (`X`). A dead dir older
    /// than this is reaped even if within [`Self::instance_keep`].
    pub instance_max_age_days: u64,
}

impl Default for ReapPolicy {
    fn default() -> Self {
        Self {
            segment_bytes: 2 * 1024 * 1024,
            segments_kept: 3,
            tool_dir_budget: 8 * 1024 * 1024,
            instance_keep: 3,
            instance_max_age_days: 7,
        }
    }
}

/// Startup instance cap: bound firehose growth *across* daemon lifetimes.
///
/// `catenary_root` is `cache_dir()/catenary` — the parent of the per-instance
/// dirs. One daemon runs per host, so every dir whose name is not
/// `self_instance` belongs to a dead daemon. The newest [`ReapPolicy::instance_keep`]
/// dead dirs that are also within [`ReapPolicy::instance_max_age_days`] are kept;
/// the rest are `rm -rf`'d. The self dir is never touched.
///
/// `now` is injected for testability. Best-effort: filesystem errors leave the
/// dir in place (reaped on a later startup).
pub fn reap_instances(
    catenary_root: &Path,
    self_instance: &str,
    policy: ReapPolicy,
    now: SystemTime,
) {
    let Ok(entries) = std::fs::read_dir(catenary_root) else {
        return;
    };

    // Every non-self instance dir is dead (one daemon per host).
    let mut dead: Vec<(std::path::PathBuf, SystemTime)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == self_instance {
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let mtime = entry.metadata().and_then(|m| m.modified()).unwrap_or(now);
        dead.push((path, mtime));
    }

    // Newest-first, so rank < instance_keep are the survivors.
    dead.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));
    let cutoff = now
        .checked_sub(Duration::from_secs(
            policy.instance_max_age_days.saturating_mul(SECS_PER_DAY),
        ))
        .unwrap_or(now);

    let mut reaped = 0usize;
    for (rank, (path, mtime)) in dead.iter().enumerate() {
        let within_cap = rank < policy.instance_keep;
        let fresh = *mtime >= cutoff;
        if within_cap && fresh {
            continue;
        }
        if std::fs::remove_dir_all(path).is_ok() {
            reaped += 1;
        }
    }
    if reaped > 0 {
        tracing::debug!(
            source = Source::LoggingFirehose.as_str(),
            reaped,
            "reaped {reaped} dead firehose instance dir(s) at startup",
        );
    }
}

/// Periodic staleness sweep: reap dead-and-stale per-scope files.
///
/// Path-agnostic (the storage tree is flat — no project subtree to sweep as a
/// unit): a file is reaped only when it is **both** absent from `state.json`
/// liveness **and** older than `retention_days`. Both gates are required — an
/// idle-but-alive worktree (no live server, files untouched overnight) must not
/// be reaped on liveness alone; staleness protects it, and a later query/edit
/// respawns the server and re-creates the file.
///
/// - **sessions:** `sessions/<id>.jsonl` for an `id` absent from the snapshot's
///   live session set.
/// - **dead-root server files:** `servers/<server>@<enc-root>.jsonl` (and its
///   rotation segments) whose `server@root` identity no longer resolves to a
///   *live* (non-`died_at`) server in the snapshot.
///
/// `firehose_root` is the current instance's dir
/// (`cache_dir()/catenary/<instance_id>`); `state_json` is
/// `runtime_dir()/catenary/state.json`. `retention_days` is the existing
/// `log_retention_days` config key: `< 0` retains forever (sweep is a no-op);
/// `0` reaps any dead file immediately; `> 0` reaps files older than N days.
/// `now` is injected for testability. Best-effort throughout.
pub fn sweep_stale(firehose_root: &Path, state_json: &Path, retention_days: i64, now: SystemTime) {
    let Some(cutoff) = stale_cutoff(now, retention_days) else {
        return; // retain forever
    };

    let (live_sessions, live_servers) = read_liveness(state_json);

    reap_dir(
        &firehose_root.join("sessions"),
        cutoff,
        false,
        &live_sessions,
    );
    reap_dir(&firehose_root.join("servers"), cutoff, true, &live_servers);
}

/// The mtime threshold below which a file is "stale", or `None` to retain
/// forever (`retention_days < 0`).
fn stale_cutoff(now: SystemTime, retention_days: i64) -> Option<SystemTime> {
    if retention_days < 0 {
        return None;
    }
    let days = u64::try_from(retention_days).unwrap_or(0);
    Some(
        now.checked_sub(Duration::from_secs(days.saturating_mul(SECS_PER_DAY)))
            .unwrap_or(now),
    )
}

/// Reap `*.jsonl` files in `dir` whose stem is **not** in `live` and whose mtime
/// is older than `cutoff`. `strip_segments` strips a trailing rotation index
/// (`.1`, `.2`, …) before matching, so a dead-root server's rotated segments
/// share the base stem's liveness verdict. Both gates required; best-effort.
fn reap_dir(dir: &Path, cutoff: SystemTime, strip_segments: bool, live: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(bare) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let stem = if strip_segments {
            strip_segment_index(bare)
        } else {
            bare
        };
        if live.contains(stem) {
            continue; // live → keep regardless of age
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|mtime| mtime < cutoff);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Strip a trailing `.<digits>` rotation index from a server file's base name.
///
/// `rust-analyzer@-home-mark-Projects-Catenary.2` → `rust-analyzer@-home-mark-Projects-Catenary`.
/// Server names and encoded roots contain no `.` (the cwd encoding maps `.` → `-`),
/// so the only dotted suffix is a rotation index.
fn strip_segment_index(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, idx)) if !idx.is_empty() && idx.bytes().all(|b| b.is_ascii_digit()) => stem,
        _ => name,
    }
}

/// Read the live session ids and live `server@root` file-stems from `state.json`.
///
/// A missing or unparseable snapshot yields empty sets — the staleness gate then
/// stands alone, which only reaps files already older than retention (never a
/// fresh one). Dead servers (those carrying `died_at`) do **not** protect their
/// file: only a live server's root resolves.
fn read_liveness(state_json: &Path) -> (HashSet<String>, HashSet<String>) {
    let mut sessions = HashSet::new();
    let mut servers = HashSet::new();

    let Ok(text) = std::fs::read_to_string(state_json) else {
        return (sessions, servers);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (sessions, servers);
    };

    if let Some(arr) = json.get("sessions").and_then(serde_json::Value::as_array) {
        for entry in arr {
            if let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) {
                sessions.insert(id.to_string());
            }
        }
    }

    if let Some(arr) = json.get("servers").and_then(serde_json::Value::as_array) {
        for entry in arr {
            // A dead server's root no longer "resolves to a live server".
            let dead = entry.get("died_at").is_some_and(|v| !v.is_null());
            if dead {
                continue;
            }
            let Some(server) = entry.get("server").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let root = entry
                .get("scope_root")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let stem = if root.is_empty() {
                server.to_string()
            } else {
                format!("{server}@{}", encode_cwd(Path::new(root)))
            };
            servers.insert(stem);
        }
    }

    (sessions, servers)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::fs;

    /// A fixed, comfortably-past base time so `checked_sub` never underflows.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000)
    }

    fn days_ago(base: SystemTime, days: u64) -> SystemTime {
        base - Duration::from_secs(days * SECS_PER_DAY)
    }

    /// Stamp a file or directory's mtime via std (no `filetime` dep). The test
    /// owns the path, so `futimens` succeeds even on a read-only handle.
    fn set_mtime(path: &Path, t: SystemTime) {
        let handle = fs::OpenOptions::new()
            .read(true)
            .open(path)
            .expect("open for mtime");
        handle.set_modified(t).expect("set mtime");
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, contents).expect("write");
    }

    // ── Instance cap ─────────────────────────────────────────────────

    #[test]
    fn instance_cap_keeps_newest_n_removes_rest_and_never_self() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let base = now();

        // Self dir (always kept) plus N_inst+2 dead dirs at increasing age.
        let self_id = "daemon:self";
        fs::create_dir_all(root.join(self_id)).expect("mkdir self");
        set_mtime(&root.join(self_id), days_ago(base, 100)); // old, but self → kept

        for i in 0..5u64 {
            let d = root.join(format!("daemon:dead-{i}"));
            fs::create_dir_all(&d).expect("mkdir dead");
            // dead-0 newest … dead-4 oldest, all within age X.
            set_mtime(&d, days_ago(base, i));
        }

        let policy = ReapPolicy {
            instance_keep: 3,
            instance_max_age_days: 7,
            ..ReapPolicy::default()
        };
        reap_instances(root, self_id, policy, base);

        // Self always survives.
        assert!(root.join(self_id).exists(), "self dir must never be reaped");
        // Newest 3 dead survive; oldest 2 reaped.
        for i in 0..3u64 {
            assert!(
                root.join(format!("daemon:dead-{i}")).exists(),
                "newest {i} should be kept"
            );
        }
        for i in 3..5u64 {
            assert!(
                !root.join(format!("daemon:dead-{i}")).exists(),
                "older {i} should be reaped"
            );
        }
    }

    #[test]
    fn instance_cap_reaps_aged_out_even_within_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let base = now();

        // Two dead dirs, both within the count cap, but one older than X days.
        let fresh = root.join("daemon:fresh");
        let aged = root.join("daemon:aged");
        fs::create_dir_all(&fresh).expect("mkdir");
        fs::create_dir_all(&aged).expect("mkdir");
        set_mtime(&fresh, days_ago(base, 1));
        set_mtime(&aged, days_ago(base, 30));

        let policy = ReapPolicy {
            instance_keep: 5, // both within count
            instance_max_age_days: 7,
            ..ReapPolicy::default()
        };
        reap_instances(root, "daemon:self", policy, base);

        assert!(fresh.exists(), "fresh within-age dir kept");
        assert!(!aged.exists(), "aged-out dir reaped despite count headroom");
    }

    #[test]
    fn instance_cap_tolerates_missing_root() {
        // Robustness: a non-existent cache root is a no-op, not a panic.
        reap_instances(
            Path::new("/nonexistent/catenary/cache"),
            "daemon:self",
            ReapPolicy::default(),
            now(),
        );
    }

    // ── Staleness sweep ──────────────────────────────────────────────

    /// Writes a fixture `state.json` listing the given live sessions and live
    /// (root, server) server entries, plus one dead server (to prove dead does
    /// not protect).
    fn write_state_json(
        path: &Path,
        live_sessions: &[&str],
        live_servers: &[(&str, &str)],
        dead_server: Option<(&str, &str)>,
    ) {
        let sessions: Vec<_> = live_sessions
            .iter()
            .map(|id| serde_json::json!({ "id": id }))
            .collect();
        let mut servers: Vec<_> = live_servers
            .iter()
            .map(|(server, root)| serde_json::json!({ "server": server, "scope_root": root }))
            .collect();
        if let Some((server, root)) = dead_server {
            servers.push(serde_json::json!({
                "server": server,
                "scope_root": root,
                "died_at": "2026-06-08T00:00:00Z",
            }));
        }
        let doc = serde_json::json!({ "sessions": sessions, "servers": servers });
        write_file(path, &doc.to_string());
    }

    #[test]
    fn session_staleness_reaps_dead_and_stale_keeps_live() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("daemon:inst");
        let state = dir.path().join("state.json");
        let base = now();

        // "live" is present in state.json; "deadold" + "deadnew" are not.
        write_state_json(&state, &["live"], &[], None);

        let mk = |id: &str, age: u64| {
            let p = root.join("sessions").join(format!("{id}.jsonl"));
            write_file(&p, "{}\n");
            set_mtime(&p, days_ago(base, age));
            p
        };
        let live = mk("live", 30); // old, but live → kept
        let dead_old = mk("deadold", 30); // dead + stale → reaped
        let dead_new = mk("deadnew", 1); // dead but fresh → kept

        sweep_stale(&root, &state, 7, base);

        assert!(live.exists(), "live session kept even when old");
        assert!(!dead_old.exists(), "dead + stale session reaped");
        assert!(
            dead_new.exists(),
            "dead but fresh session kept (staleness gate)"
        );
    }

    #[test]
    fn dead_root_server_file_reaped_live_root_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("daemon:inst");
        let state = dir.path().join("state.json");
        let base = now();

        let live_root = "/home/mark/Projects/Live";
        let dead_root = "/home/mark/Projects/Dead";
        // Only the live root resolves to a live server in the snapshot. The
        // dead-root server is present but carries died_at (must not protect).
        write_state_json(
            &state,
            &[],
            &[("rust-analyzer", live_root)],
            Some(("rust-analyzer", dead_root)),
        );

        let live_file = root.join("servers").join(format!(
            "rust-analyzer@{}.jsonl",
            encode_cwd(Path::new(live_root))
        ));
        let dead_file = root.join("servers").join(format!(
            "rust-analyzer@{}.jsonl",
            encode_cwd(Path::new(dead_root))
        ));
        let dead_seg = root.join("servers").join(format!(
            "rust-analyzer@{}.1.jsonl",
            encode_cwd(Path::new(dead_root))
        ));
        write_file(&live_file, "{}\n");
        write_file(&dead_file, "{}\n");
        write_file(&dead_seg, "{}\n");
        for p in [&live_file, &dead_file, &dead_seg] {
            set_mtime(p, days_ago(base, 30));
        }

        sweep_stale(&root, &state, 7, base);

        assert!(live_file.exists(), "live-root server file kept");
        assert!(!dead_file.exists(), "dead-root server file reaped");
        assert!(
            !dead_seg.exists(),
            "dead-root server rotation segment reaped via base-stem match"
        );
    }

    #[test]
    fn retention_negative_retains_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("daemon:inst");
        let state = dir.path().join("state.json");
        let base = now();
        write_state_json(&state, &[], &[], None);

        let p = root.join("sessions").join("ancient.jsonl");
        write_file(&p, "{}\n");
        set_mtime(&p, days_ago(base, 9_999));

        sweep_stale(&root, &state, -1, base);
        assert!(p.exists(), "retention_days = -1 retains forever");
    }

    #[test]
    fn missing_state_json_only_reaps_by_staleness() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("daemon:inst");
        let base = now();
        let state = dir.path().join("absent.json"); // never written

        let aged = root.join("sessions").join("aged.jsonl");
        let recent = root.join("sessions").join("recent.jsonl");
        write_file(&aged, "{}\n");
        write_file(&recent, "{}\n");
        set_mtime(&aged, days_ago(base, 30));
        set_mtime(&recent, days_ago(base, 1));

        sweep_stale(&root, &state, 7, base);
        assert!(!aged.exists(), "stale file reaped with no snapshot");
        assert!(recent.exists(), "fresh file kept with no snapshot");
    }

    #[test]
    fn sweep_tolerates_empty_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No sessions/ or servers/ dirs exist — must be a clean no-op.
        sweep_stale(
            &dir.path().join("daemon:inst"),
            &dir.path().join("state.json"),
            7,
            now(),
        );
    }

    #[test]
    fn strip_segment_index_only_strips_numeric_suffix() {
        assert_eq!(strip_segment_index("ra@-p-Catenary"), "ra@-p-Catenary");
        assert_eq!(strip_segment_index("ra@-p-Catenary.2"), "ra@-p-Catenary");
        // A non-numeric dotted suffix is not a rotation index.
        assert_eq!(strip_segment_index("ra@-p-Catenary.x"), "ra@-p-Catenary.x");
    }

    #[test]
    fn read_liveness_excludes_dead_servers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state.json");
        write_state_json(
            &state,
            &["s1"],
            &[("ra", "/p/Live")],
            Some(("ra", "/p/Dead")),
        );
        let (sessions, servers) = read_liveness(&state);
        assert!(sessions.contains("s1"));
        assert!(servers.contains(&format!("ra@{}", encode_cwd(Path::new("/p/Live")))));
        assert!(
            !servers.contains(&format!("ra@{}", encode_cwd(Path::new("/p/Dead")))),
            "dead server excluded from live set"
        );
    }

    #[test]
    fn read_liveness_rootless_server_stem_is_bare_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state.json");
        // A rootless (workspace/single-file) server serializes scope_root = "".
        write_file(
            &state,
            &serde_json::json!({
                "servers": [{ "server": "taplo", "scope_root": "" }]
            })
            .to_string(),
        );
        let (_sessions, servers) = read_liveness(&state);
        assert!(
            servers.contains("taplo"),
            "rootless stem is the bare server name"
        );
    }
}
