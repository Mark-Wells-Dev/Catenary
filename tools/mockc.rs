// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! A mock compiler subprocess for testing load-aware wait detection.
//!
//! Burns CPU for a precise number of ticks (centiseconds, 100 Hz — the same
//! unit [`catenary_proc::ProcessMonitor`] measures), optionally writes output
//! to stdout, and exits. Used with mockls `--flycheck-command` to simulate
//! the real scheduling pattern where the LSP process sleeps while a compiler
//! subprocess does the work.

#![allow(clippy::print_stdout, reason = "CLI tool outputs to stdout")]

use clap::Parser;

/// Mock compiler subprocess for integration testing.
#[derive(Parser, Debug)]
#[command(name = "mockc")]
struct Args {
    /// Burn CPU for N ticks (centiseconds, 100 Hz). Default: 10.
    #[arg(long, default_value_t = 10)]
    ticks: u64,

    /// Exit with this code. Default: 0.
    #[arg(long, default_value_t = 0)]
    exit_code: i32,

    /// Write this text to stdout before exiting.
    #[arg(long)]
    output: Option<String>,

    /// Never exit (simulate a stuck compiler).
    #[arg(long)]
    hang: bool,
}

fn main() {
    let args = Args::parse();

    if args.hang {
        // Park the thread forever — simulates a stuck compiler.
        loop {
            std::thread::park();
        }
    }

    // Spin until we've burned the target number of ticks.
    // Read our own CPU time from /proc/self/stat (Linux) or platform
    // equivalent via catenary-proc.
    let start = self_cpu_ticks();
    let target = start + args.ticks;

    while self_cpu_ticks() < target {
        // Tight spin — burn CPU. hint::spin_loop() yields to
        // hyperthreading but keeps us on-core.
        std::hint::spin_loop();
    }

    if let Some(ref text) = args.output {
        print!("{text}");
    }

    std::process::exit(args.exit_code);
}

/// Read our own total CPU ticks (user + system) in centiseconds.
///
/// Uses the same source as `catenary-proc` — `/proc/self/stat` on Linux.
/// Falls back to a wall-clock spin on unsupported platforms.
fn self_cpu_ticks() -> u64 {
    #[cfg(target_os = "linux")]
    {
        linux_self_ticks().unwrap_or(0)
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Fallback: catenary-proc's stateless sampler on our own PID.
        catenary_proc::sample(std::process::id()).map_or(0, |s| s.cpu_ticks)
    }
}

/// Fast path for Linux — read `/proc/self/stat` directly (no PID lookup).
#[cfg(target_os = "linux")]
fn linux_self_ticks() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/self/stat").ok()?;
    parse_stat_cpu_ticks(&contents)
}

/// Parse utime + stime from a `/proc/[pid]/stat` line.
///
/// The format is: `pid (comm) state field3 field4 ... field13(utime) field14(stime) ...`
/// Fields are numbered from 0 after the comm-closing `)`. `rfind(')')` handles
/// comm names containing spaces or parentheses.
#[cfg(target_os = "linux")]
fn parse_stat_cpu_ticks(stat_line: &str) -> Option<u64> {
    let after_comm = stat_line.rfind(')')? + 1;
    let fields: Vec<&str> = stat_line[after_comm..].split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn self_ticks_advance_with_work() {
        let before = self_cpu_ticks();

        // Burn enough CPU to cross at least one tick boundary (10ms).
        // 50M iterations is ~100ms of CPU on most machines.
        let mut sum: u64 = 0;
        for i in 0..50_000_000 {
            sum = sum.wrapping_add(i);
        }
        std::hint::black_box(sum);

        let after = self_cpu_ticks();
        assert!(
            after > before,
            "Ticks should advance after CPU work: before={before}, after={after}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_self_ticks_returns_some() {
        let result = linux_self_ticks();
        assert!(
            result.is_some(),
            "/proc/self/stat should be readable on Linux"
        );
    }

    // Real /proc/self/stat format (field positions from proc(5)):
    //   pid (comm) state ppid pgrp session tty tpgid flags
    //   minflt cminflt majflt cmajflt utime stime cutime cstime ...
    //   Fields after ')': 0=state .. 11=utime 12=stime

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_extracts_utime_plus_stime() {
        let stat = "12345 (mockc) S 1 12345 12345 0 -1 4194304 \
                    100 0 0 0 42 7 0 0 20 0 1 0 1000 1000000 100 \
                    18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(parse_stat_cpu_ticks(stat), Some(49)); // 42 + 7
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_comm_with_spaces() {
        let stat = "12345 (my app) S 1 12345 12345 0 -1 4194304 \
                    100 0 0 0 10 5 0 0 20 0 1 0 1000 1000000 100 \
                    18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(parse_stat_cpu_ticks(stat), Some(15)); // 10 + 5
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_comm_with_parens() {
        // rfind(')') must find the *last* closing paren
        let stat = "12345 (app(1)) S 1 12345 12345 0 -1 4194304 \
                    100 0 0 0 20 3 0 0 20 0 1 0 1000 1000000 100 \
                    18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0";
        assert_eq!(parse_stat_cpu_ticks(stat), Some(23)); // 20 + 3
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_no_closing_paren() {
        assert_eq!(parse_stat_cpu_ticks("12345 (mockc S 1"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_stat_too_few_fields() {
        // Only state through minflt — missing utime/stime
        let stat = "12345 (mockc) S 1 12345 12345 0 -1 4194304 100 0 0 0";
        assert_eq!(parse_stat_cpu_ticks(stat), None);
    }
}
