// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The session handle: the host process a hook session is a descendant of, and
//! the daemon-side liveness watch that tears its roots down when it vanishes.
//!
//! MCP's connection lifecycle used to be the session handle — connect/disconnect
//! drove the census, roots teardown, and LSP release. Dropping MCP (workstream
//! 49) means replacing that handle. The replacement is the parked pulse design's
//! mechanism with the socket leg dropped (`pulse/README.md`: SO_PEERCRED +
//! ancestry): the hook CLI runs as a **descendant** of the host session process
//! (`claude` / `agy`), so at hook time it can walk its own ancestry to the host
//! pid and declare it — alongside the hook's `session_id` — to the daemon.
//!
//! The declaration is `(pid, start-time)`. The start-time is the pid-reuse
//! identity guard: a later liveness probe that finds the pid alive but with a
//! *different* start-time treats the host as vanished (the pid was recycled by an
//! unrelated process). The daemon keeps a session-keyed registry of these handles
//! and, on the existing reaper cadence, probes each for liveness. A vanished host
//! — crash, kill, OOM, a `/clear` that skipped SessionEnd — is detected and its
//! session torn down through the normal per-session release path.
//!
//! ## Platform liveness
//!
//! - **Linux**: `/proc/<pid>` presence is the liveness signal; the start-time is
//!   field 22 of `/proc/<pid>/stat` (clock ticks since boot). Both are pure
//!   filesystem reads — no `unsafe`, no `libc`.
//! - **macOS / other**: `/proc` does not exist, and a `kill(pid, 0)` probe plus a
//!   `sysctl(KERN_PROC)` start-time read both need `libc` FFI, which
//!   `forbid(unsafe_code)` disallows without a safe wrapper crate. This module
//!   therefore implements the **Linux subset** and, on non-Linux, returns
//!   [`Liveness::Unknown`] — the watch treats an unknowable handle as *present*
//!   (never a false teardown). The macOS leg is a flagged follow-up (see the
//!   module's `probe` on non-Linux and ticket 01's report).

#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// A declared host-process handle: the pid the hook walked its ancestry to, plus
/// that process's start-time as the pid-reuse identity guard.
///
/// Declared by the hook CLI (which alone runs as a descendant of the host), sent
/// to the daemon on the hook payload, and stored in the daemon's session-keyed
/// [`crate::router`] handle registry. Two handles are the *same host* iff both
/// fields match — a recycled pid carries a fresh start-time and so is a different
/// host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostHandle {
    /// The host session process id (`claude` / `agy`), reached by walking the
    /// hook process's ancestry.
    pub pid: u32,
    /// The host process's start-time, an opaque identity token compared for
    /// equality only. On Linux this is field 22 of `/proc/<pid>/stat` (clock
    /// ticks since boot); `0` when it could not be read (the probe then falls
    /// back to bare pid presence).
    pub start_time: u64,
}

impl HostHandle {
    /// Builds a handle for an explicit pid, reading its start-time on the current
    /// platform.
    ///
    /// A start-time of `0` means "unread" (a `/proc` race) — the handle is
    /// still valid for a bare pid-presence probe, just without the pid-reuse
    /// guard.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn for_pid(pid: u32) -> Self {
        Self {
            pid,
            start_time: read_start_time(pid),
        }
    }

    /// Builds a handle for an explicit pid.
    ///
    /// Non-Linux: the start-time is always `0` ("unread") — the sysctl read
    /// needs `libc` FFI this crate forbids, so the handle carries no pid-reuse
    /// guard (and [`probe`] answers [`Liveness::Unknown`] regardless).
    #[cfg(not(target_os = "linux"))]
    #[must_use]
    pub const fn for_pid(pid: u32) -> Self {
        Self { pid, start_time: 0 }
    }
}

/// The result of probing a declared handle for liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The host process is present and its start-time matches the declaration —
    /// the session is live.
    Alive,
    /// The host process is gone (pid absent), or present but with a *different*
    /// start-time (the pid was recycled) — the session has vanished.
    Vanished,
    /// Liveness could not be determined on this platform (no `/proc`, no safe
    /// probe). Treated as present by the watch — never a false teardown.
    Unknown,
}

/// Probes a declared [`HostHandle`] for liveness.
///
/// The pid is present iff `/proc/<pid>` exists; when the declaration carries a
/// non-zero `start_time`, the live process's start-time must match, or the pid
/// was recycled ([`Liveness::Vanished`]).
#[cfg(target_os = "linux")]
#[must_use]
pub fn probe(handle: HostHandle) -> Liveness {
    probe_in(handle, &proc_root())
}

/// Probes a declared [`HostHandle`] for liveness.
///
/// Non-Linux: always [`Liveness::Unknown`] — the macOS `kill(0)` + sysctl leg
/// needs `libc` FFI this crate forbids without a safe wrapper crate (the flagged
/// Darwin subset; ticket 01's follow-up). The watch treats Unknown as present,
/// never a false teardown.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub const fn probe(_handle: HostHandle) -> Liveness {
    Liveness::Unknown
}

/// The `/proc` mount the Linux probe reads. Split out so a unit test can point it
/// at a fixture tree (a tempdir with hand-written `<pid>/stat` files) and exercise
/// the presence / start-time-mismatch / recycled-pid legs without spawning real
/// processes.
#[cfg(target_os = "linux")]
fn proc_root() -> PathBuf {
    PathBuf::from("/proc")
}

/// The root-injectable core of [`probe`]: `proc_root` is `/proc` in production
/// and a fixture tree in tests.
#[cfg(target_os = "linux")]
#[must_use]
fn probe_in(handle: HostHandle, proc_root: &std::path::Path) -> Liveness {
    let proc_dir = proc_root.join(handle.pid.to_string());
    if !proc_dir.exists() {
        return Liveness::Vanished;
    }
    // The pid is present. With no declared start-time (unread at declaration
    // time) we can only assert presence — Alive. With one, the live
    // start-time must match, or the pid was recycled by an unrelated process
    // (Vanished).
    if handle.start_time == 0 {
        return Liveness::Alive;
    }
    match read_start_time_in(handle.pid, proc_root) {
        0 => Liveness::Alive, // could not re-read; presence stands
        live if live == handle.start_time => Liveness::Alive,
        _ => Liveness::Vanished,
    }
}

/// Reads a process's start-time (field 22 of `/proc/<pid>/stat`), or `0` when
/// unavailable.
#[cfg(target_os = "linux")]
#[must_use]
fn read_start_time(pid: u32) -> u64 {
    read_start_time_in(pid, &proc_root())
}

/// The root-injectable core of [`read_start_time`].
///
/// Parses field 22 (`starttime`) of `/proc/<pid>/stat`. The stat line's second
/// field is the comm name wrapped in parentheses and may itself contain spaces
/// and parentheses (e.g. `(my program)`), so fields are counted from **after the
/// last `)`** — the kernel guarantees the parenthesized comm is the only place a
/// `)` appears before the numeric fields. From that point, `starttime` is the
/// 20th whitespace-separated token (fields 3..=22).
#[cfg(target_os = "linux")]
#[must_use]
fn read_start_time_in(pid: u32, proc_root: &std::path::Path) -> u64 {
    let stat_path = proc_root.join(pid.to_string()).join("stat");
    let Ok(contents) = std::fs::read_to_string(&stat_path) else {
        return 0;
    };
    parse_starttime(&contents).unwrap_or(0)
}

/// Parses the `starttime` field (field 22) from a `/proc/<pid>/stat` line.
///
/// Skips past the comm field by cutting at the last `)`: everything after it is
/// the space-separated numeric run `state ppid ... starttime ...`, in which
/// `starttime` is the 20th token (stat fields 3 through 22 inclusive). Returns
/// `None` if the line is malformed.
#[cfg(target_os = "linux")]
fn parse_starttime(stat: &str) -> Option<u64> {
    let after_comm = stat.rsplit_once(')')?.1;
    // Fields after comm: index 0 == field 3 (state). starttime is field 22,
    // i.e. index 22 - 3 == 19.
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

/// The set of process names treated as a **host session process** — the ancestry
/// walk stops at the first ancestor whose comm matches one of these.
///
/// `claude` is Claude Code; `agy` is the Antigravity CLI. The walk must stop here
/// and NOT at an interposed `sh -c` wrapper in the hook chain (bug 96's lesson:
/// on Darwin ancestry is the coarse leg). A truncated Linux comm (16 bytes) still
/// matches these short names exactly.
#[cfg(target_os = "linux")]
const HOST_PROCESS_NAMES: &[&str] = &["claude", "agy"];

/// Whether a process comm names a known host session process.
#[cfg(target_os = "linux")]
#[must_use]
fn is_host_process(comm: &str) -> bool {
    HOST_PROCESS_NAMES.contains(&comm)
}

/// Walks the current process's ancestry to the host session process and returns
/// its handle, or `None` when the host cannot be reliably identified.
///
/// Called by the hook CLI (which runs as a descendant of the host). Starts at the
/// hook process's own parent and climbs the ppid chain, stopping at the first
/// ancestor whose comm is a [`HOST_PROCESS_NAMES`] entry — never at an interposed
/// `sh -c` wrapper. Returns `None` when the walk reaches pid 1 (or a broken link)
/// without finding a host: better to declare nothing than to declare a wrong pid
/// whose later reuse would tear a live session down.
#[cfg(target_os = "linux")]
#[must_use]
pub fn resolve_host_handle() -> Option<HostHandle> {
    resolve_host_handle_from(std::process::id(), &proc_root())
}

/// Walks the current process's ancestry to the host session process.
///
/// Non-Linux: always `None` — the walk needs a `libc`/sysctl ppid read this
/// crate forbids without a safe wrapper crate, so no handle is declared, the
/// daemon keeps no registry entry for the session, and the vanish watch never
/// fires for it (today's behavior for a hookless session). This is the flagged
/// Darwin subset: the mechanism is honest where it can walk, silent where it
/// cannot (ticket 01's follow-up).
#[cfg(not(target_os = "linux"))]
#[must_use]
pub const fn resolve_host_handle() -> Option<HostHandle> {
    None
}

/// The pid- and root-injectable core of [`resolve_host_handle`].
///
/// `start` is the pid whose ancestry to climb (the hook process in production);
/// `proc_root` is `/proc` in production and a fixture tree in tests. Bounded to
/// [`MAX_ANCESTRY_DEPTH`] hops so a pathological ppid cycle (a fixture, or a
/// kernel oddity) can never spin.
#[cfg(target_os = "linux")]
#[must_use]
fn resolve_host_handle_from(start: u32, proc_root: &std::path::Path) -> Option<HostHandle> {
    let mut pid = read_ppid_in(start, proc_root)?;
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if pid <= 1 {
            // Reached init without a host ancestor — do not guess.
            return None;
        }
        let comm = read_comm_in(pid, proc_root)?;
        if is_host_process(&comm) {
            return Some(HostHandle {
                pid,
                start_time: read_start_time_in(pid, proc_root),
            });
        }
        pid = read_ppid_in(pid, proc_root)?;
    }
    None
}

/// Maximum ancestry hops the walk climbs before giving up — a cycle guard.
#[cfg(target_os = "linux")]
const MAX_ANCESTRY_DEPTH: usize = 64;

/// Reads a process's parent pid (field 4 of `/proc/<pid>/stat`).
///
/// Same comm-skipping discipline as [`parse_starttime`]: cut at the last `)`,
/// then `ppid` is the 2nd token after (stat field 4, index `4 - 3 == 1`).
#[cfg(target_os = "linux")]
#[must_use]
fn read_ppid_in(pid: u32, proc_root: &std::path::Path) -> Option<u32> {
    let stat_path = proc_root.join(pid.to_string()).join("stat");
    let contents = std::fs::read_to_string(&stat_path).ok()?;
    let after_comm = contents.rsplit_once(')')?.1;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// Reads a process's comm (executable name) from `/proc/<pid>/comm`.
///
/// `/proc/<pid>/comm` is the bare name with a trailing newline and no
/// parentheses — cleaner than parsing it back out of `stat` — and is truncated to
/// 15 bytes by the kernel, which still matches the short host names exactly.
#[cfg(target_os = "linux")]
#[must_use]
fn read_comm_in(pid: u32, proc_root: &std::path::Path) -> Option<String> {
    let comm_path = proc_root.join(pid.to_string()).join("comm");
    let contents = std::fs::read_to_string(&comm_path).ok()?;
    Some(contents.trim_end().to_string())
}

#[cfg(all(test, not(target_os = "linux")))]
mod non_linux_tests {
    use super::{HostHandle, Liveness, probe, resolve_host_handle};

    #[test]
    fn probe_is_unknown_and_resolve_declares_nothing() {
        // The honest non-Linux subset: no walk, no probe — never a false
        // teardown (the watch treats Unknown as present).
        let handle = HostHandle {
            pid: 1,
            start_time: 0,
        };
        assert_eq!(probe(handle), Liveness::Unknown);
        assert!(resolve_host_handle().is_none());
        assert_eq!(HostHandle::for_pid(7).start_time, 0);
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// Writes a fake `/proc/<pid>/stat` and `/proc/<pid>/comm` under `root`.
    ///
    /// The stat line mimics the kernel layout: `pid (comm) state ppid ...` with
    /// `starttime` placed at field 22. Only the fields the parsers read need be
    /// accurate; the rest are filler zeros.
    fn write_fake_proc(root: &std::path::Path, pid: u32, comm: &str, parent: u32, starttime: u64) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).expect("mk proc dir");
        // After the ')' the tokens are, by index: 0 state, 1 ppid, ..., 19
        // starttime (stat field 22). Build `state ppid <pad> starttime` so
        // starttime lands at index 19.
        let mut fields = vec![String::from("R"), parent.to_string()];
        while fields.len() < 19 {
            fields.push("0".to_string());
        }
        fields.push(starttime.to_string());
        let stat = format!("{pid} ({comm}) {}", fields.join(" "));
        std::fs::write(dir.join("stat"), stat).expect("write stat");
        std::fs::write(dir.join("comm"), format!("{comm}\n")).expect("write comm");
    }

    #[test]
    fn parse_starttime_skips_comm_with_spaces_and_parens() {
        // A comm containing spaces and a ')' must not fool the field counter —
        // the kernel's last ')' is the split point.
        let stat = "1234 (weird ) name) R 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 987654";
        assert_eq!(parse_starttime(stat), Some(987_654));
    }

    #[test]
    fn parse_starttime_rejects_malformed() {
        assert_eq!(parse_starttime("no parens here"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_present_matching_start_time_is_alive() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_fake_proc(dir.path(), 4242, "claude", 1, 555);
        let handle = HostHandle {
            pid: 4242,
            start_time: 555,
        };
        assert_eq!(probe_in(handle, dir.path()), Liveness::Alive);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_absent_pid_is_vanished() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No proc entry written for this pid.
        let handle = HostHandle {
            pid: 9999,
            start_time: 555,
        };
        assert_eq!(probe_in(handle, dir.path()), Liveness::Vanished);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_recycled_pid_different_start_time_is_vanished() {
        // The pid is present, but the live process started at a different time —
        // the pid was recycled. The start-time guard must call this vanished.
        let dir = tempfile::tempdir().expect("tempdir");
        write_fake_proc(dir.path(), 4242, "claude", 1, 999);
        let handle = HostHandle {
            pid: 4242,
            start_time: 555, // declared start-time differs from the live 999
        };
        assert_eq!(probe_in(handle, dir.path()), Liveness::Vanished);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn probe_present_pid_unread_start_time_is_alive() {
        // A handle declared with start_time 0 (unread) probes on bare presence.
        let dir = tempfile::tempdir().expect("tempdir");
        write_fake_proc(dir.path(), 4242, "claude", 1, 777);
        let handle = HostHandle {
            pid: 4242,
            start_time: 0,
        };
        assert_eq!(probe_in(handle, dir.path()), Liveness::Alive);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestry_walk_stops_at_host_not_interposed_shell() {
        // Chain: hook(100) -> sh(50) -> claude(10) -> login(1). The walk from the
        // hook must stop at claude(10), NOT the interposed sh(50).
        let dir = tempfile::tempdir().expect("tempdir");
        write_fake_proc(dir.path(), 1, "login", 0, 1);
        write_fake_proc(dir.path(), 10, "claude", 1, 424_242);
        write_fake_proc(dir.path(), 50, "sh", 10, 1);
        write_fake_proc(dir.path(), 100, "catenary", 50, 1);

        let handle = resolve_host_handle_from(100, dir.path()).expect("host found");
        assert_eq!(handle.pid, 10, "walk stops at the host (claude), not sh");
        assert_eq!(handle.start_time, 424_242);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestry_walk_finds_antigravity_host() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_fake_proc(dir.path(), 1, "login", 0, 1);
        write_fake_proc(dir.path(), 10, "agy", 1, 11);
        write_fake_proc(dir.path(), 100, "catenary", 10, 1);

        let handle = resolve_host_handle_from(100, dir.path()).expect("agy host found");
        assert_eq!(handle.pid, 10);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestry_walk_gives_up_at_init_without_host() {
        // No host in the chain — reaching pid 1 must yield None, never a guess.
        let dir = tempfile::tempdir().expect("tempdir");
        write_fake_proc(dir.path(), 1, "init", 0, 1);
        write_fake_proc(dir.path(), 50, "bash", 1, 1);
        write_fake_proc(dir.path(), 100, "catenary", 50, 1);

        assert!(resolve_host_handle_from(100, dir.path()).is_none());
    }

    #[test]
    fn is_host_process_matches_known_hosts_only() {
        assert!(is_host_process("claude"));
        assert!(is_host_process("agy"));
        assert!(!is_host_process("sh"));
        assert!(!is_host_process("bash"));
        assert!(!is_host_process("catenary"));
    }

    #[test]
    fn for_pid_reads_or_zeros_start_time() {
        // On Linux this reads /proc/<pid>/stat; for a bogus pid it must degrade
        // to 0, not panic. On non-Linux start_time is always 0.
        let handle = HostHandle::for_pid(0);
        assert_eq!(handle.pid, 0);
        let _ = handle.start_time;
    }
}
