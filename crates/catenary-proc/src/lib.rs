// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Single-process sampling for tick-budget diagnostics waits and work
//! intensity profiling.
//!
//! Provides both a stateful [`ProcessMonitor`] for persistent monitoring
//! (persistent file handles, amortized syscall costs, built-in delta tracking)
//! and a stateless [`sample`] function for one-off snapshots.
//!
//! Each sample captures three counters — user CPU time (`utime`), system
//! CPU time (`stime`), and page fault count (`pfc`) — plus the parent PID
//! and scheduling state. CPU times are normalized to centiseconds (100 Hz)
//! across platforms; 1 tick = 10ms. Page faults are raw counts.

use std::collections::{HashMap, HashSet};

/// A snapshot of a single process's CPU consumption, page faults, and state.
#[derive(Debug, Clone, Copy)]
pub struct ProcessSample {
    /// User CPU time consumed (centiseconds, 100 Hz). Monotonically increasing.
    pub utime: u64,
    /// System CPU time consumed (centiseconds, 100 Hz). Monotonically increasing.
    pub stime: u64,
    /// Total page faults (minor + major). Monotonically increasing.
    pub pfc: u64,
    /// Parent process ID.
    pub ppid: u32,
    /// Current scheduling/execution state.
    pub state: ProcessState,
}

/// What the process is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// Running on a core or in the run queue.
    Running,
    /// Voluntarily sleeping — waiting for I/O, socket, futex, etc.
    Sleeping,
    /// Uninterruptible sleep — kernel I/O (disk, NFS).
    Blocked,
    /// Stopped (SIGSTOP, debugger), zombie, dead, or unreadable.
    Dead,
}

/// Whether [`ProcessState`]'s `Running`/`Blocked` distinction reflects the
/// kernel's real scheduler state on this platform.
///
/// `true` on Linux (parsed from the `/proc/<pid>/stat` state char) and macOS
/// (derived from `proc_pidinfo` running-thread counts): a `Running` sample
/// there means the process is genuinely on a core or waiting in the run queue,
/// and a `Blocked` sample means uninterruptible kernel I/O — both are
/// pending-work signals independent of the sampled CPU deltas.
///
/// `false` on Windows and unsupported platforms. The Win32 API exposes no
/// scheduler state, so [`sample`] reports every live process as `Running` as a
/// liveness placeholder (see the Windows `sample_handle`); that value carries
/// no run-queue information. Consumers must treat scheduler state as
/// meaningless here and fall back to CPU deltas alone.
pub const SCHEDULER_STATE_OBSERVABLE: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// CPU and page fault deltas since the last sample.
///
/// Returned by [`ProcessMonitor::sample`]. All deltas are 0 on the first
/// call. CPU times are in centiseconds (100 Hz); page faults are raw counts.
#[derive(Debug, Clone, Copy)]
pub struct ProcessDelta {
    /// User CPU time delta since last sample (centiseconds).
    pub delta_utime: u64,
    /// System CPU time delta since last sample (centiseconds).
    pub delta_stime: u64,
    /// Page fault count delta since last sample.
    pub delta_pfc: u64,
    /// Parent process ID.
    pub ppid: u32,
    /// Current scheduling/execution state.
    pub state: ProcessState,
}

/// Stateful process monitor with persistent handles.
///
/// Created once at server spawn, lives on `LspClient` for the server's
/// lifetime. Amortizes handle/fd open costs and encapsulates delta tracking
/// for three counters: user CPU time, system CPU time, and page faults.
///
/// # Platform behavior
///
/// - **Linux:** Holds a persistent `File` for `/proc/<pid>/stat` and a
///   reusable read buffer. Each sample is seek(0) + read — 2 syscalls
///   instead of 3 (open + read + close).
/// - **macOS:** Holds only the PID — `proc_pidinfo` is a stateless syscall.
/// - **Windows:** Holds a persistent `HANDLE` from `OpenProcess`, closed
///   in `Drop`. Each sample is 3 syscalls instead of 5+.
pub struct ProcessMonitor {
    prev_utime: Option<u64>,
    prev_stime: Option<u64>,
    prev_pfc: Option<u64>,
    prev_csw: Option<u64>,
    inner: platform::MonitorInner,
}

impl ProcessMonitor {
    /// Creates a new monitor for the given PID.
    ///
    /// Opens persistent handles/fds for the server's lifetime.
    /// Returns `None` if the process doesn't exist or can't be monitored.
    #[must_use]
    pub fn new(pid: u32) -> Option<Self> {
        Some(Self {
            prev_utime: None,
            prev_stime: None,
            prev_pfc: None,
            prev_csw: None,
            inner: platform::MonitorInner::new(pid)?,
        })
    }

    /// Sample the process and return deltas since the last sample.
    ///
    /// All deltas are 0 on the first call.
    /// Returns `None` if the process can no longer be sampled.
    #[allow(
        clippy::similar_names,
        reason = "delta_utime/delta_stime are standard counter names"
    )]
    pub fn sample(&mut self) -> Option<ProcessDelta> {
        let (utime, stime, pfc, ppid, state) = self.inner.sample()?;
        let csw = self.inner.context_switches();
        let delta_utime = self.prev_utime.map_or(0, |prev| utime.saturating_sub(prev));
        let delta_stime = self.prev_stime.map_or(0, |prev| stime.saturating_sub(prev));
        let delta_pfc = self.prev_pfc.map_or(0, |prev| pfc.saturating_sub(prev));
        self.prev_utime = Some(utime);
        self.prev_stime = Some(stime);
        self.prev_pfc = Some(pfc);
        self.prev_csw = Some(csw);
        Some(ProcessDelta {
            delta_utime,
            delta_stime,
            delta_pfc,
            ppid,
            state,
        })
    }

    /// Returns the sum of cumulative counters from the most recent sample.
    ///
    /// Returns 0 before the first [`Self::sample`] call. After sampling,
    /// this is `utime + stime + pfc + csw` — a monotonically increasing
    /// value that advances whenever the process is scheduled. The context
    /// switch count (`csw`) ensures detection of sub-centisecond processing
    /// that would otherwise round to zero in the time-based counters.
    #[must_use]
    pub fn cumulative_ticks(&self) -> u64 {
        self.prev_utime.unwrap_or(0)
            + self.prev_stime.unwrap_or(0)
            + self.prev_pfc.unwrap_or(0)
            + self.prev_csw.unwrap_or(0)
    }
}

/// Per-process sample from a tree walk.
#[derive(Debug, Clone, Copy)]
pub struct TreeSample {
    /// Process ID.
    pub pid: u32,
    /// Parent process ID.
    pub ppid: u32,
    /// User CPU time delta since last sample (centiseconds).
    pub delta_utime: u64,
    /// System CPU time delta since last sample (centiseconds).
    pub delta_stime: u64,
    /// Page fault count delta since last sample.
    pub delta_pfc: u64,
    /// Current scheduling/execution state.
    pub state: ProcessState,
}

/// Result of sampling the entire process tree.
#[derive(Debug)]
pub struct TreeSnapshot {
    /// Per-process samples (root + all descendants).
    pub samples: Vec<TreeSample>,
    /// Total processes discovered (root + children).
    pub process_count: usize,
    /// Sum of cumulative `utime + stime + pfc + csw` across all sampled processes.
    ///
    /// Monotonically increasing. Useful for pre/post-stimulus comparison:
    /// if `cumulative_ticks` advanced between two snapshots, at least one
    /// process in the tree was scheduled.
    pub cumulative_ticks: u64,
}

/// Monitors a root process and all its descendants.
///
/// On each [`sample`](TreeMonitor::sample) call, discovers live descendants
/// via platform-specific tree walking, inserts fresh [`ProcessMonitor`]s for
/// new children, drops monitors for exited children, and returns per-process
/// deltas for the entire tree.
pub struct TreeMonitor {
    root_pid: u32,
    monitors: HashMap<u32, ProcessMonitor>,
}

impl TreeMonitor {
    /// Creates a new tree monitor for the given root PID.
    ///
    /// Returns `None` if the root process doesn't exist or can't be monitored.
    #[must_use]
    pub fn new(root_pid: u32) -> Option<Self> {
        let root_monitor = ProcessMonitor::new(root_pid)?;
        let mut monitors = HashMap::new();
        monitors.insert(root_pid, root_monitor);
        Some(Self { root_pid, monitors })
    }

    /// Sample the root and all live descendants.
    ///
    /// Discovers children via tree walk on every call. New children get a
    /// fresh `ProcessMonitor` (first delta = 0). Children that have exited
    /// are dropped. Returns one [`TreeSample`] per process.
    #[allow(
        clippy::similar_names,
        reason = "delta_utime/delta_stime are standard counter names"
    )]
    pub fn sample(&mut self) -> TreeSnapshot {
        // Discover all live descendants.
        let mut live_pids = HashSet::new();
        live_pids.insert(self.root_pid);
        for child_pid in platform::discover_children(self.root_pid) {
            live_pids.insert(child_pid);
        }

        // Insert monitors for newly discovered PIDs.
        for &pid in &live_pids {
            if let std::collections::hash_map::Entry::Vacant(e) = self.monitors.entry(pid)
                && let Some(monitor) = ProcessMonitor::new(pid)
            {
                e.insert(monitor);
            }
        }

        // Remove monitors for exited PIDs.
        self.monitors.retain(|pid, _| live_pids.contains(pid));

        let process_count = live_pids.len();

        // If the root is gone, return empty.
        if !self.monitors.contains_key(&self.root_pid) {
            return TreeSnapshot {
                samples: Vec::new(),
                process_count: 0,
                cumulative_ticks: 0,
            };
        }

        // Sample every monitor.
        let mut samples = Vec::with_capacity(self.monitors.len());
        let mut cumulative_ticks: u64 = 0;
        for (&pid, monitor) in &mut self.monitors {
            if let Some(delta) = monitor.sample() {
                samples.push(TreeSample {
                    pid,
                    ppid: delta.ppid,
                    delta_utime: delta.delta_utime,
                    delta_stime: delta.delta_stime,
                    delta_pfc: delta.delta_pfc,
                    state: delta.state,
                });
            }
            cumulative_ticks += monitor.cumulative_ticks();
        }

        TreeSnapshot {
            samples,
            process_count,
            cumulative_ticks,
        }
    }

    /// Returns the number of currently tracked monitors (for testing).
    #[cfg(test)]
    fn monitor_count(&self) -> usize {
        self.monitors.len()
    }
}

/// Compute work intensity: log-scaled page faults per CPU tick.
///
/// Returns `0.0` when idle (0 or 1 page faults per tick). Returns `None`
/// when `delta_utime` is 0 (server wasn't scheduled — no data).
#[must_use]
pub fn intensity(delta_pfc: u64, delta_utime: u64) -> Option<f64> {
    if delta_utime == 0 {
        return None;
    }
    #[allow(
        clippy::cast_precision_loss,
        reason = "counter values fit comfortably in f64 mantissa"
    )]
    Some((1.0_f64.max(delta_pfc as f64 / delta_utime as f64)).ln())
}

/// Sample a single process by PID (stateless).
///
/// Returns absolute CPU times (centiseconds), page fault count, parent PID,
/// and scheduling state. Returns `None` if the process doesn't exist or
/// can't be read.
#[must_use]
pub fn sample(pid: u32) -> Option<ProcessSample> {
    platform::sample(pid)
}

/// Configure a command so its child process receives `SIGTERM` when the
/// parent process dies.
///
/// On Linux, uses `prctl(PR_SET_PDEATHSIG, SIGTERM)` via [`pre_exec`].
/// This ensures child processes are cleaned up even if the parent is
/// `SIGKILL`'d (where no signal handler or `Drop` runs).
///
/// No-op on non-Linux platforms.
///
/// [`pre_exec`]: std::os::unix::process::CommandExt::pre_exec
#[cfg(target_os = "linux")]
pub fn set_parent_death_signal(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `prctl(PR_SET_PDEATHSIG, SIGTERM)` is async-signal-safe and
    // only affects the new child process between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Configure a command so its child process receives `SIGTERM` when the
/// parent process dies.
///
/// No-op on non-Linux platforms. See the Linux variant for details.
#[cfg(not(target_os = "linux"))]
pub fn set_parent_death_signal(_cmd: &mut std::process::Command) {}

/// Returns `true` if the calling process is in the foreground process
/// group of its controlling terminal.
///
/// A process in a background group that reads from `/dev/tty` receives
/// `SIGTTIN`, which stops the entire process group. Call this before
/// terminal I/O to avoid stopping the process under cargo-mutants or
/// other tools that run tests in background process groups.
///
/// Returns `false` if there is no controlling terminal or the query fails.
#[cfg(unix)]
#[must_use]
pub fn is_foreground_process_group() -> bool {
    // SAFETY: tcgetpgrp and getpgrp are async-signal-safe POSIX functions
    // with no preconditions. tcgetpgrp returns -1 on error (no tty).
    unsafe {
        let fg_pgid = libc::tcgetpgrp(libc::STDIN_FILENO);
        if fg_pgid == -1 {
            return false;
        }
        fg_pgid == libc::getpgrp()
    }
}

/// Returns `true` if the calling process is in the foreground process
/// group of its controlling terminal.
///
/// Always returns `false` on non-Unix platforms.
#[cfg(not(unix))]
#[must_use]
pub const fn is_foreground_process_group() -> bool {
    false
}

/// Assign a child process to a kill-on-close Job Object so it is
/// terminated when the parent exits.
///
/// On Windows, creates a process-wide Job Object (once) with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assigns the child to it.
/// When the Catenary process exits — even via crash or `TerminateProcess`
/// — the Job Object handle is closed and Windows kills all assigned
/// children.
///
/// No-op on non-Windows platforms (Linux uses [`set_parent_death_signal`]
/// instead).
#[cfg(target_os = "windows")]
pub fn register_child_process(pid: u32) {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// A wrapper so the raw `HANDLE` can be stored in a `OnceLock`.
    struct JobHandle(windows_sys::Win32::Foundation::HANDLE);

    // SAFETY: Windows `HANDLE`s are kernel objects usable from any thread.
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB: OnceLock<Option<JobHandle>> = OnceLock::new();

    let job = JOB.get_or_init(|| {
        // SAFETY: `CreateJobObjectW` with null name creates an anonymous job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }

        // SAFETY: zeroed struct is valid for `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`;
        // we set only `LimitFlags` which is a bitmask field.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let size =
            u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0);

        // SAFETY: `handle` is valid, `info` is correctly sized.
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                size,
            )
        };
        if ok == 0 {
            unsafe { CloseHandle(handle) };
            return None;
        }

        Some(JobHandle(handle))
    });

    let Some(job) = job else { return };

    // SAFETY: `OpenProcess` with valid access flags; handle closed after use.
    let child_handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
    if child_handle.is_null() {
        return;
    }
    // SAFETY: both handles are valid.
    unsafe { AssignProcessToJobObject(job.0, child_handle) };
    unsafe { CloseHandle(child_handle) };
}

/// Assign a child process to a kill-on-close Job Object so it is
/// terminated when the parent exits.
///
/// No-op on non-Windows platforms. See the Windows variant for details.
#[cfg(not(target_os = "windows"))]
pub const fn register_child_process(_pid: u32) {}

/// Send `SIGKILL` to a process by PID.
///
/// Best-effort: returns silently if the process doesn't exist or the
/// signal cannot be delivered (e.g., permission denied). Used as a
/// synchronous fallback in `Drop` impls where async child-reap tasks
/// may not run (runtime shutdown).
#[cfg(unix)]
pub fn kill_process(pid: u32) {
    // SAFETY: `kill` is a POSIX function. Sending SIGKILL to a valid
    // PID is well-defined; sending to a nonexistent PID returns ESRCH
    // which we ignore.
    #[allow(unsafe_code, reason = "libc::kill is the POSIX signal API")]
    #[allow(clippy::cast_possible_wrap, reason = "PID fits in i32")]
    unsafe {
        libc::kill(pid.cast_signed(), libc::SIGKILL);
    }
}

/// Send `SIGKILL` to a process by PID.
///
/// No-op on non-Unix, non-Windows platforms.
#[cfg(not(any(unix, windows)))]
pub const fn kill_process(_pid: u32) {}

// ─── Linux ──────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod platform {
    use super::{ProcessSample, ProcessState};
    use std::io::{Read, Seek, SeekFrom};

    /// Persistent handles for Linux `/proc/<pid>/stat` and `/proc/<pid>/status`.
    pub struct MonitorInner {
        file: std::fs::File,
        status_file: Option<std::fs::File>,
        buf: String,
        status_buf: String,
    }

    impl MonitorInner {
        /// Opens `/proc/<pid>/stat` and `/proc/<pid>/status` and returns a
        /// persistent monitor.
        pub fn new(pid: u32) -> Option<Self> {
            let path = format!("/proc/{pid}/stat");
            let file = std::fs::File::open(path).ok()?;
            let status_path = format!("/proc/{pid}/status");
            let status_file = std::fs::File::open(status_path).ok();
            Some(Self {
                file,
                status_file,
                buf: String::with_capacity(256),
                status_buf: String::with_capacity(1024),
            })
        }

        /// Reads counters and state via the persistent fd.
        ///
        /// Returns `(utime, stime, pfc, ppid, state)`.
        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            self.buf.clear();
            self.file.seek(SeekFrom::Start(0)).ok()?;
            self.file.read_to_string(&mut self.buf).ok()?;
            parse_stat(&self.buf)
        }

        /// Returns cumulative voluntary context switches from `/proc/<pid>/status`.
        ///
        /// Returns 0 if the status file is unavailable or the field cannot be parsed.
        pub fn context_switches(&mut self) -> u64 {
            let Some(ref mut f) = self.status_file else {
                return 0;
            };
            self.status_buf.clear();
            if f.seek(SeekFrom::Start(0)).is_err() {
                return 0;
            }
            if f.read_to_string(&mut self.status_buf).is_err() {
                return 0;
            }
            parse_voluntary_ctxt_switches(&self.status_buf)
        }
    }

    /// Parse `voluntary_ctxt_switches` from `/proc/<pid>/status` contents.
    fn parse_voluntary_ctxt_switches(contents: &str) -> u64 {
        for line in contents.lines() {
            if let Some(val) = line.strip_prefix("voluntary_ctxt_switches:\t") {
                return val.trim().parse().unwrap_or(0);
            }
        }
        0
    }

    /// Parse `/proc/<pid>/stat` contents.
    ///
    /// Fields after the comm field (past last `)`):
    /// - index 0: state char (`R`, `S`, `D`, `T`, `Z`, `X`, etc.)
    /// - index 1: ppid
    /// - index 7: minflt
    /// - index 9: majflt
    /// - index 11: utime (`USER_HZ` ticks, always 100 Hz)
    /// - index 12: stime (`USER_HZ` ticks)
    fn parse_stat(contents: &str) -> Option<(u64, u64, u64, u32, ProcessState)> {
        // The comm field (index 1) may contain spaces and parentheses,
        // so find the last ')' to skip past it.
        let after_comm = contents.rfind(')')? + 1;
        let fields: Vec<&str> = contents[after_comm..].split_whitespace().collect();

        let state_char = fields.first()?.chars().next()?;
        let ppid: u32 = fields.get(1)?.parse().ok()?;
        let minflt: u64 = fields.get(7)?.parse().ok()?;
        let majflt: u64 = fields.get(9)?.parse().ok()?;
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;

        let state = match state_char {
            'R' => ProcessState::Running,
            'S' | 'I' => ProcessState::Sleeping,
            'D' => ProcessState::Blocked,
            // T (stopped), Z (zombie), X (dead), etc.
            _ => ProcessState::Dead,
        };

        Some((utime, stime, minflt + majflt, ppid, state))
    }

    /// Discover all descendant PIDs by walking `/proc/{pid}/task/{tid}/children`.
    pub fn discover_children(root_pid: u32) -> Vec<u32> {
        let mut result = Vec::new();
        let mut stack = vec![root_pid];
        while let Some(pid) = stack.pop() {
            let task_dir = format!("/proc/{pid}/task");
            let Ok(entries) = std::fs::read_dir(&task_dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let children_path = entry.path().join("children");
                let Ok(contents) = std::fs::read_to_string(&children_path) else {
                    continue;
                };
                for token in contents.split_whitespace() {
                    if let Ok(child_pid) = token.parse::<u32>() {
                        result.push(child_pid);
                        stack.push(child_pid);
                    }
                }
            }
        }
        result
    }

    /// Stateless sample — opens, reads, closes.
    #[allow(clippy::similar_names, reason = "ppid/pid are distinct concepts")]
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (utime, stime, pfc, ppid, state) = parse_stat(&contents)?;
        Some(ProcessSample {
            utime,
            stime,
            pfc,
            ppid,
            state,
        })
    }
}

// ─── macOS ──────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod platform {
    use super::{ProcessSample, ProcessState};

    /// macOS monitor — no persistent handle needed (`proc_pidinfo` is stateless).
    /// Caches ppid at construction to avoid the `PROC_PIDTBSDINFO` syscall
    /// on every sample.
    pub struct MonitorInner {
        pid: u32,
        ppid: u32,
        last_csw: u64,
    }

    impl MonitorInner {
        /// Verifies the process exists and returns a monitor.
        pub fn new(pid: u32) -> Option<Self> {
            let (_, _, _, ppid, _) = sample_raw(pid)?;
            Some(Self {
                pid,
                ppid,
                last_csw: 0,
            })
        }

        /// Samples via `proc_pidinfo` (task info only — ppid is cached).
        ///
        /// Returns `(utime, stime, pfc, ppid, state)`.
        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            let (utime, stime, pfc, csw, state) = sample_task(self.pid)?;
            self.last_csw = csw;
            Some((utime, stime, pfc, self.ppid, state))
        }

        /// Returns cumulative context switches from the most recent sample.
        pub fn context_switches(&mut self) -> u64 {
            self.last_csw
        }
    }

    /// Task-info-only sampling (no ppid). Used by [`MonitorInner::sample`]
    /// which caches ppid at construction.
    fn sample_task(pid: u32) -> Option<(u64, u64, u64, u64, ProcessState)> {
        // Safety: calling POSIX and Mach APIs with correctly sized buffers.
        unsafe { sample_task_inner(pid) }
    }

    /// # Safety
    ///
    /// Calls `libc::proc_pidinfo` (`PROC_PIDTASKINFO`) and
    /// `mach2::mach_time::mach_timebase_info` with correctly sized and zeroed
    /// buffers.
    unsafe fn sample_task_inner(pid: u32) -> Option<(u64, u64, u64, u64, ProcessState)> {
        let (info, state) = unsafe { read_task_info(pid) }?;

        let mut timebase = mach2::mach_time::mach_timebase_info_data_t { numer: 0, denom: 0 };
        unsafe { mach2::mach_time::mach_timebase_info(&mut timebase) };

        let numer = u64::from(timebase.numer);
        let denom = u64::from(timebase.denom);
        let utime = info.pti_total_user * numer / denom / 10_000_000;
        let stime = info.pti_total_system * numer / denom / 10_000_000;

        #[allow(
            clippy::cast_sign_loss,
            reason = "page fault and context switch counts are always non-negative"
        )]
        let pfc = info.pti_faults as u64;

        #[allow(
            clippy::cast_sign_loss,
            reason = "page fault and context switch counts are always non-negative"
        )]
        let csw = info.pti_csw as u64;

        Some((utime, stime, pfc, csw, state))
    }

    /// Full sampling including ppid. Used by the stateless [`sample`] function.
    fn sample_raw(pid: u32) -> Option<(u64, u64, u64, u32, ProcessState)> {
        // Safety: calling POSIX and Mach APIs with correctly sized buffers.
        unsafe { sample_raw_inner(pid) }
    }

    /// # Safety
    ///
    /// Calls `libc::proc_pidinfo` (twice: `PROC_PIDTASKINFO` and
    /// `PROC_PIDTBSDINFO`) and `libc::mach_timebase_info` with correctly
    /// sized and zeroed buffers.
    unsafe fn sample_raw_inner(pid: u32) -> Option<(u64, u64, u64, u32, ProcessState)> {
        let (utime, stime, pfc, _csw, state) = unsafe { sample_task_inner(pid) }?;

        // BSD info for parent PID
        let mut bsd_info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let bsd_size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;

        let bsd_ret = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(bsd_info).cast(),
                bsd_size,
            )
        };
        let ppid = if bsd_ret > 0 { bsd_info.pbi_ppid } else { 0 };

        Some((utime, stime, pfc, ppid, state))
    }

    /// Read `proc_taskinfo` and derive scheduling state.
    ///
    /// # Safety
    ///
    /// Calls `libc::proc_pidinfo` with `PROC_PIDTASKINFO` and a correctly
    /// sized and zeroed buffer.
    unsafe fn read_task_info(pid: u32) -> Option<(libc::proc_taskinfo, ProcessState)> {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(std::mem::size_of::<libc::proc_taskinfo>()).ok()?;

        let ret = unsafe {
            libc::proc_pidinfo(
                i32::try_from(pid).ok()?,
                libc::PROC_PIDTASKINFO,
                0,
                std::ptr::addr_of_mut!(info).cast(),
                size,
            )
        };

        if ret <= 0 {
            return None;
        }

        let state = if info.pti_numrunning > 0 {
            ProcessState::Running
        } else {
            ProcessState::Sleeping
        };

        Some((info, state))
    }

    /// Discover all descendant PIDs via `proc_listchildpids`.
    pub fn discover_children(root_pid: u32) -> Vec<u32> {
        let mut result = Vec::new();
        let mut stack = vec![root_pid];
        while let Some(pid) = stack.pop() {
            // Safety: `proc_listchildpids` is a standard macOS API called with
            // a correctly sized buffer.
            let children = unsafe { list_child_pids(pid) };
            for child_pid in children {
                result.push(child_pid);
                stack.push(child_pid);
            }
        }
        result
    }

    /// List direct child PIDs of a process.
    ///
    /// # Safety
    ///
    /// Calls `libc::proc_listchildpids` with a correctly sized buffer.
    unsafe fn list_child_pids(pid: u32) -> Vec<u32> {
        let pid_i32 = match i32::try_from(pid) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        // First call with size 0 to get the count.
        let count = unsafe { libc::proc_listchildpids(pid_i32, std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }

        #[allow(clippy::cast_sign_loss, reason = "count is checked > 0 above")]
        let num = count as usize;
        let mut buf = vec![0i32; num];
        let buf_size = match i32::try_from(num * std::mem::size_of::<i32>()) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let ret = unsafe { libc::proc_listchildpids(pid_i32, buf.as_mut_ptr().cast(), buf_size) };
        if ret <= 0 {
            return Vec::new();
        }

        #[allow(clippy::cast_sign_loss, reason = "ret is checked > 0 above")]
        let actual = ret as usize / std::mem::size_of::<i32>();
        buf.truncate(actual);

        #[allow(clippy::cast_sign_loss, reason = "PIDs are always non-negative")]
        buf.into_iter()
            .filter(|&p| p > 0)
            .map(|p| p as u32)
            .collect()
    }

    /// Stateless sample for the `sample(pid)` function.
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        let (utime, stime, pfc, ppid, state) = sample_raw(pid)?;
        Some(ProcessSample {
            utime,
            stime,
            pfc,
            ppid,
            state,
        })
    }
}

// ─── Windows ────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod platform {
    use super::{ProcessSample, ProcessState};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessIoCounters, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    /// Windows monitor — persistent `HANDLE`, closed in `Drop`.
    pub struct MonitorInner {
        handle: HANDLE,
        ppid: u32,
    }

    impl MonitorInner {
        /// Opens a process handle with query and VM read access.
        pub fn new(pid: u32) -> Option<Self> {
            // Safety: `OpenProcess` is called with valid access flags.
            // Returns 0 on failure (invalid PID).
            let handle =
                unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
            if handle.is_null() {
                return None;
            }
            let ppid = unsafe { get_ppid(pid) };
            Some(Self { handle, ppid })
        }

        /// Samples via persistent handle.
        ///
        /// Returns `(utime, stime, pfc, ppid, state)`.
        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            // Safety: `self.handle` is valid (opened in `new`, closed in `Drop`).
            let (utime, stime, pfc, state) = unsafe { sample_handle(self.handle) }?;
            Some((utime, stime, pfc, self.ppid, state))
        }

        /// Returns cumulative I/O operation count as a proxy for context switches.
        ///
        /// Uses `GetProcessIoCounters` to read `ReadOperationCount` — a
        /// count-based signal that increments on every `ReadFile()` call.
        /// The server must read from stdin to receive notifications, so
        /// this is guaranteed to advance on any processed stimulus.
        pub fn context_switches(&mut self) -> u64 {
            // Safety: `self.handle` is valid (opened in `new`, closed in `Drop`).
            unsafe { io_operation_count(self.handle) }
        }
    }

    impl Drop for MonitorInner {
        fn drop(&mut self) {
            // Safety: `self.handle` is valid and will not be used after drop.
            unsafe { CloseHandle(self.handle) };
        }
    }

    // Safety: Windows HANDLEs are kernel objects that can be used from any thread.
    unsafe impl Send for MonitorInner {}

    /// Sample CPU times, page faults, and state via an existing handle.
    ///
    /// Returns `(utime, stime, pfc, state)`.
    ///
    /// # Safety
    ///
    /// Caller must ensure `handle` is a valid process handle with
    /// `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ` access.
    unsafe fn sample_handle(handle: HANDLE) -> Option<(u64, u64, u64, ProcessState)> {
        let mut creation = unsafe { std::mem::zeroed() };
        let mut exit = unsafe { std::mem::zeroed() };
        let mut kernel = unsafe { std::mem::zeroed() };
        let mut user = unsafe { std::mem::zeroed() };

        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }

        // FILETIME: 100-nanosecond intervals -> centiseconds (/ 100_000)
        let user_100ns = (u64::from(user.dwHighDateTime) << 32) | u64::from(user.dwLowDateTime);
        let kernel_100ns =
            (u64::from(kernel.dwHighDateTime) << 32) | u64::from(kernel.dwLowDateTime);
        let utime = user_100ns / 100_000;
        let stime = kernel_100ns / 100_000;

        // Page fault count via GetProcessMemoryInfo
        let mut mem_counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let cb = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>()).unwrap_or(0);
        mem_counters.cb = cb;
        let pfc = if unsafe { K32GetProcessMemoryInfo(handle, &mut mem_counters, cb) } != 0 {
            u64::from(mem_counters.PageFaultCount)
        } else {
            0
        };

        let mut exit_code: u32 = 0;
        unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        #[allow(
            clippy::cast_sign_loss,
            reason = "STILL_ACTIVE is a well-known Windows constant"
        )]
        let state = if exit_code == STILL_ACTIVE as u32 {
            // Can't distinguish Running/Sleeping on Windows — report Running
            // so the failure detector uses tick delta for discrimination.
            ProcessState::Running
        } else {
            ProcessState::Dead
        };

        Some((utime, stime, pfc, state))
    }

    /// Read cumulative I/O read operation count via `GetProcessIoCounters`.
    ///
    /// Returns 0 on failure.
    ///
    /// # Safety
    ///
    /// Caller must ensure `handle` is a valid process handle with
    /// `PROCESS_QUERY_INFORMATION` access.
    unsafe fn io_operation_count(handle: HANDLE) -> u64 {
        let mut counters: windows_sys::Win32::System::Threading::IO_COUNTERS =
            unsafe { std::mem::zeroed() };
        if unsafe { GetProcessIoCounters(handle, &mut counters) } != 0 {
            counters.ReadOperationCount
        } else {
            0
        }
    }

    /// Get parent PID via a toolhelp snapshot.
    ///
    /// Returns 0 if the PID cannot be found.
    ///
    /// # Safety
    ///
    /// Calls Win32 ToolHelp APIs with correctly sized buffers.
    unsafe fn get_ppid(pid: u32) -> u32 {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return 0;
        }

        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0);

        let mut found = 0u32;
        if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    found = entry.th32ParentProcessID;
                    break;
                }
                if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }

        unsafe { CloseHandle(snapshot) };
        found
    }

    /// Discover all descendant PIDs via a toolhelp snapshot.
    pub fn discover_children(root_pid: u32) -> Vec<u32> {
        // Safety: calling Win32 ToolHelp APIs with correctly sized buffers.
        unsafe { discover_children_inner(root_pid) }
    }

    /// # Safety
    ///
    /// Calls Win32 `CreateToolhelp32Snapshot`, `Process32FirstW`, and
    /// `Process32NextW` with correctly sized buffers.
    unsafe fn discover_children_inner(root_pid: u32) -> Vec<u32> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Vec::new();
        }

        // Build parent → children map from the full snapshot.
        let mut children_map: std::collections::HashMap<u32, Vec<u32>> =
            std::collections::HashMap::new();
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = u32::try_from(std::mem::size_of::<PROCESSENTRY32W>()).unwrap_or(0);

        if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
            loop {
                children_map
                    .entry(entry.th32ParentProcessID)
                    .or_default()
                    .push(entry.th32ProcessID);
                if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }

        unsafe { CloseHandle(snapshot) };

        // Walk from root, collecting all descendants.
        let mut result = Vec::new();
        let mut stack = vec![root_pid];
        while let Some(pid) = stack.pop() {
            if let Some(kids) = children_map.get(&pid) {
                for &child_pid in kids {
                    result.push(child_pid);
                    stack.push(child_pid);
                }
            }
        }
        result
    }

    /// Stateless sample — opens handle, reads, closes.
    pub fn sample(pid: u32) -> Option<ProcessSample> {
        // Safety: calling Win32 APIs with correctly sized buffers and
        // properly closing the handle on all paths.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid);
            if handle.is_null() {
                return None;
            }
            let result = sample_handle(handle);
            CloseHandle(handle);
            let (utime, stime, pfc, state) = result?;
            let ppid = get_ppid(pid);
            Some(ProcessSample {
                utime,
                stime,
                pfc,
                ppid,
                state,
            })
        }
    }
}

// ─── Unsupported ────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{ProcessSample, ProcessState};

    /// Stub monitor for unsupported platforms.
    pub(super) struct MonitorInner;

    impl MonitorInner {
        pub fn new(_pid: u32) -> Option<Self> {
            None
        }

        pub fn sample(&mut self) -> Option<(u64, u64, u64, u32, ProcessState)> {
            None
        }

        pub fn context_switches(&mut self) -> u64 {
            0
        }
    }

    pub fn discover_children(_root_pid: u32) -> Vec<u32> {
        Vec::new()
    }

    pub fn sample(_pid: u32) -> Option<ProcessSample> {
        None
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ─── Stateless sample() tests ───────────────────────────────────────

    #[test]
    fn sample_self_succeeds() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        // Under nextest each test runs in its own short-lived process,
        // so utime may be 0. Just verify the sample succeeds.
        let _ = s.utime;
    }

    #[test]
    fn sample_nonexistent_returns_none() {
        let result = sample(u32::MAX);
        assert!(result.is_none(), "Nonexistent PID should return None");
    }

    #[test]
    fn sample_self_state_is_running_or_sleeping() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        // Under nextest each test is a separate process that may be
        // Sleeping (between syscalls) rather than Running.
        assert!(
            matches!(s.state, ProcessState::Running | ProcessState::Sleeping),
            "Own process should be Running or Sleeping, got {:?}",
            s.state
        );
    }

    #[test]
    fn sample_self_has_ppid() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        assert!(s.ppid > 0, "Own process should have a parent PID");
    }

    #[test]
    fn sample_self_has_page_faults() {
        let pid = std::process::id();
        let s = sample(pid).expect("should be able to sample own process");
        assert!(s.pfc > 0, "Own process should have had page faults");
    }

    // ─── ProcessMonitor tests ───────────────────────────────────────────

    #[test]
    fn monitor_self_succeeds() {
        let pid = std::process::id();
        let monitor = ProcessMonitor::new(pid);
        assert!(monitor.is_some(), "Should be able to monitor own process");
    }

    #[test]
    fn monitor_nonexistent_returns_none() {
        let result = ProcessMonitor::new(u32::MAX);
        assert!(result.is_none(), "Nonexistent PID should return None");
    }

    #[test]
    fn monitor_first_sample_delta_is_zero() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");
        let d = monitor.sample().expect("Should sample self");
        assert_eq!(d.delta_utime, 0, "First sample should have delta_utime 0");
        assert_eq!(d.delta_stime, 0, "First sample should have delta_stime 0");
        assert_eq!(d.delta_pfc, 0, "First sample should have delta_pfc 0");
    }

    #[test]
    fn monitor_persistent_handle_reuse() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");

        // Multiple samples through the same persistent handle
        for i in 0..5 {
            let result = monitor.sample();
            assert!(result.is_some(), "Sample {i} should succeed");
        }
    }

    #[test]
    fn monitor_delta_advances_with_cpu_work() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");

        // First sample: delta is 0
        let d = monitor.sample().expect("First sample");
        assert_eq!(d.delta_utime, 0);

        // Burn some CPU so ticks advance
        let mut sum: u64 = 0;
        for i in 0..10_000_000 {
            sum = sum.wrapping_add(i);
        }
        // Prevent optimizing away the loop
        std::hint::black_box(sum);

        // Second sample: delta may be > 0 (depends on granularity)
        let d = monitor.sample().expect("Second sample");
        // We can't guarantee delta_utime > 0 due to 10ms tick granularity,
        // but the sample should succeed. Under nextest the process may be
        // Sleeping between syscalls.
        assert!(
            matches!(d.state, ProcessState::Running | ProcessState::Sleeping),
            "Own process should be Running or Sleeping, got {:?}",
            d.state
        );
        let _ = d.delta_utime;
    }

    #[test]
    fn monitor_pfc_delta_advances() {
        let pid = std::process::id();
        let mut monitor = ProcessMonitor::new(pid).expect("Should monitor self");

        // First sample: establishes baseline
        let d = monitor.sample().expect("First sample");
        assert_eq!(d.delta_pfc, 0, "First sample should have delta_pfc 0");

        // Force page faults: non-zero fill requires touching every page
        let buf = vec![1u8; 4 * 1024 * 1024];
        std::hint::black_box(&buf);

        // Second sample: page faults should have advanced
        let d = monitor.sample().expect("Second sample");
        assert!(
            d.delta_pfc > 0,
            "Page fault delta should advance after large allocation, got {}",
            d.delta_pfc,
        );
    }

    // ─── TreeMonitor tests ─────────────────────────────────────────────

    #[test]
    fn tree_monitor_self_succeeds() {
        let pid = std::process::id();
        let mut tm = TreeMonitor::new(pid).expect("Should monitor own process tree");
        let snap = tm.sample();
        assert!(
            !snap.samples.is_empty(),
            "Tree sample should contain at least the root"
        );
        assert!(
            snap.process_count >= 1,
            "Process count should be at least 1"
        );
    }

    #[test]
    fn tree_monitor_root_sample_has_correct_pid() {
        let pid = std::process::id();
        let mut tm = TreeMonitor::new(pid).expect("Should monitor own process tree");
        let snap = tm.sample();
        let root = snap.samples.iter().find(|s| s.pid == pid);
        assert!(root.is_some(), "Root PID should appear in samples");
    }

    #[test]
    fn tree_monitor_nonexistent_returns_none() {
        let result = TreeMonitor::new(u32::MAX);
        assert!(result.is_none(), "Nonexistent PID should return None");
    }

    #[test]
    fn tree_monitor_discovers_child() {
        use std::process::Command;

        let mut child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("Failed to spawn sleep");

        let pid = std::process::id();
        let mut tm = TreeMonitor::new(pid).expect("Should monitor own process tree");
        let snap = tm.sample();
        assert!(
            snap.samples.len() >= 2,
            "Should find root + child, got {} samples",
            snap.samples.len()
        );

        child.kill().expect("Failed to kill child");
        child.wait().expect("Failed to wait for child");

        // Give the OS a moment to clean up.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let snap = tm.sample();
        assert_eq!(
            snap.samples.len(),
            1,
            "After child exit, should have only root"
        );
    }

    #[test]
    fn tree_monitor_drops_exited_child() {
        use std::process::Command;

        let mut child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("Failed to spawn sleep");

        let pid = std::process::id();
        let mut tm = TreeMonitor::new(pid).expect("Should monitor own process tree");
        let _ = tm.sample();
        assert!(
            tm.monitor_count() >= 2,
            "Should track root + child, got {}",
            tm.monitor_count()
        );

        child.kill().expect("Failed to kill child");
        child.wait().expect("Failed to wait for child");

        std::thread::sleep(std::time::Duration::from_millis(50));

        let _ = tm.sample();
        assert_eq!(
            tm.monitor_count(),
            1,
            "After child exit, monitor map should shrink to 1"
        );
    }

    // ─── set_parent_death_signal tests ──────────────────────────────────

    #[test]
    fn set_parent_death_signal_spawns_successfully() {
        use std::process::Command;

        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        set_parent_death_signal(&mut cmd);
        let mut child = cmd.spawn().expect("Failed to spawn with death signal");

        // Child should be alive
        assert!(child.try_wait().expect("try_wait failed").is_none());

        child.kill().expect("Failed to kill child");
        child.wait().expect("Failed to wait for child");
    }

    // ─── intensity() tests ─────────────────────────────────────────────

    #[test]
    fn intensity_zero_pfc_returns_zero() {
        let v = intensity(0, 100);
        assert_eq!(v, Some(0.0));
    }

    #[test]
    fn intensity_one_pfc_returns_zero() {
        // max(1, 1/100) = 1, ln(1) = 0
        let v = intensity(1, 100);
        assert_eq!(v, Some(0.0));
    }

    #[test]
    fn intensity_high_pfc_returns_positive() {
        let v = intensity(10000, 100).expect("Should return Some");
        assert!(
            v > 0.0,
            "High pfc/utime ratio should give positive intensity, got {v}"
        );
    }

    #[test]
    fn intensity_zero_utime_returns_none() {
        let v = intensity(100, 0);
        assert_eq!(v, None);
    }

    // ─── Platform-specific tree walk tests ─────────────────────────────

    #[cfg(target_os = "linux")]
    #[test]
    fn tree_walk_discovers_children_linux() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        // Spawn a shell that itself spawns a child (two levels deep).
        // Use process_group(0) so the shell and its children share a
        // process group — killing the group reaps the backgrounded sleep.
        let mut parent = unsafe {
            Command::new("sh")
                .arg("-c")
                .arg("sleep 60 & wait")
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()
                .expect("Failed to spawn sh")
        };

        // Give the shell time to spawn its child.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let pid = std::process::id();
        let mut tm = TreeMonitor::new(pid).expect("Should monitor own process tree");
        let snap = tm.sample();

        // We should see: our process, the sh, and the sleep (3+).
        assert!(
            snap.samples.len() >= 3,
            "Should find root + sh + sleep, got {} samples",
            snap.samples.len()
        );

        // Kill the entire process group (negative PID = group).
        let pgid = parent.id();
        unsafe { libc::kill(-(i32::try_from(pgid).expect("PID fits i32")), libc::SIGKILL) };
        parent.wait().expect("Failed to wait for parent");
    }
}
