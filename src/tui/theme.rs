// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Terminal theme and color detection for the TUI.
//!
//! All colors use the terminal's ANSI palette so the TUI automatically
//! inherits whatever theme the user has configured.

use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier, Style};

// ── Theme ────────────────────────────────────────────────────────────────

/// Semantic color theme that defers to the terminal's ANSI palette.
///
/// Uses only base ANSI colors (`Color::Green`, `Color::Red`, etc.) and
/// modifiers (`DIM`, `BOLD`, `REVERSED`) so the TUI automatically inherits
/// whatever theme the user has configured in their terminal emulator.
pub struct Theme {
    /// Style for the focused pane border.
    pub border_focused: Style,
    /// Style for the unfocused pane border.
    pub border_unfocused: Style,
    /// Style for pane titles.
    pub title: Style,
    /// Style for hint keybinding labels.
    pub hint_key: Style,
    /// Style for hint description text.
    pub hint_label: Style,
    /// Style for the selection highlight.
    pub selection: Style,

    /// Style for active sessions.
    pub session_active: Style,
    /// Style for dead sessions.
    pub session_dead: Style,
    /// Style for session metadata (language list, etc.).
    pub session_meta: Style,

    /// Style for timestamps.
    pub timestamp: Style,
    /// Style for normal text.
    pub text: Style,
    /// Style for accented text (language names, etc.).
    pub accent: Style,
    /// Style for success indicators.
    pub success: Style,
    /// Style for error indicators.
    pub error: Style,
    /// Style for warning indicators.
    pub warning: Style,
    /// Style for informational indicators.
    pub info: Style,
    /// Style for muted/dimmed text.
    pub muted: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new()
    }
}

impl Theme {
    /// Build the default theme from the terminal's palette.
    ///
    /// Uses `REVERSED` for selection highlight. Prefer [`Theme::detect()`]
    /// at runtime to derive a subtler background-shifted highlight color.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            border_focused: Style::new(),
            border_unfocused: Style::new().add_modifier(Modifier::DIM),
            title: Style::new().add_modifier(Modifier::BOLD),
            hint_key: Style::new().add_modifier(Modifier::BOLD),
            hint_label: Style::new().add_modifier(Modifier::DIM),
            selection: Style::new().add_modifier(Modifier::REVERSED),

            session_active: Style::new().fg(Color::Green),
            session_dead: Style::new().add_modifier(Modifier::DIM),
            session_meta: Style::new().add_modifier(Modifier::DIM),

            timestamp: Style::new().add_modifier(Modifier::DIM),
            text: Style::new(),
            accent: Style::new().fg(Color::Cyan),
            success: Style::new().fg(Color::Green),
            error: Style::new().fg(Color::Red),
            warning: Style::new().fg(Color::Yellow),
            info: Style::new().fg(Color::Blue),
            muted: Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// Build a theme by querying the terminal's background color.
    ///
    /// Sends an OSC 11 query to detect the terminal background, then derives
    /// a subtle selection highlight by shifting the lightness in HSL space
    /// (+0.2 for dark backgrounds, −0.2 for light). Falls back to `REVERSED`
    /// if the terminal doesn't respond or the query fails.
    #[must_use]
    pub fn detect() -> Self {
        let mut theme = Self::new();
        if let Some((r, g, b)) = detect_terminal_bg() {
            theme.selection = Style::new().bg(selection_bg_from_terminal(r, g, b));
        }
        theme
    }
}

// ── Terminal background detection ────────────────────────────────────────

/// Lightness shift applied to the terminal background for the selection
/// highlight. Positive for dark backgrounds, negative for light.
const SELECTION_LIGHTNESS_SHIFT: f64 = 0.2;

/// Poll interval for failure detection sampling (matches `wait.rs`).
const DETECTION_POLL: Duration = Duration::from_millis(200);

/// CPU-tick threshold before giving up on the terminal response.
///
/// 10 ticks = 100ms of actual CPU time. Generous for a one-shot query
/// that completes in <10ms under normal conditions.
const DETECTION_TICK_THRESHOLD: i64 = 10;

/// Wall-clock safety cap for pathological cases (D-state, NFS hang).
const DETECTION_WALL_CAP: Duration = Duration::from_secs(2);

/// Query the terminal's background color via OSC 11.
///
/// Uses load-aware failure detection (same pattern as `load_aware_grace`
/// in `src/lsp/wait.rs`) instead of a fixed wall-clock timeout: keeps
/// waiting while the system is under load and our process is sleeping,
/// bails only when real CPU time has been burned without a response.
///
/// Returns `Some((r, g, b))` with 8-bit RGB values if the terminal
/// responds, or `None` if the query fails or the threshold is exhausted.
#[cfg(unix)]
fn detect_terminal_bg() -> Option<(u8, u8, u8)> {
    use std::io::{Read, Write};
    use std::sync::mpsc::RecvTimeoutError;

    use catenary_proc::ProcessMonitor;

    // Background process groups receive SIGTTIN when reading from the
    // terminal, which stops the entire process group. Bail out early
    // to avoid stopping the test binary under cargo-mutants.
    if !catenary_proc::is_foreground_process_group() {
        return None;
    }

    // Open /dev/tty directly to avoid contention with crossterm's stdin.
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;

    // We need raw mode for character-at-a-time reads.
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw {
        crossterm::terminal::enable_raw_mode().ok()?;
    }

    // Send OSC 11 query: "what is the background color?"
    tty.write_all(b"\x1b]11;?\x07").ok()?;
    tty.flush().ok()?;

    // Read the response in a background thread.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        loop {
            match tty.read(&mut byte) {
                Ok(1) => {
                    buf.push(byte[0]);
                    // Terminators: BEL (\x07) or ST (\x1b\\).
                    if byte[0] == 0x07 {
                        let _ = tx.send(buf);
                        return;
                    }
                    if buf.len() >= 2 && buf[buf.len() - 2] == 0x1b && buf[buf.len() - 1] == b'\\' {
                        let _ = tx.send(buf);
                        return;
                    }
                }
                _ => return,
            }
        }
    });

    // Load-aware wait: poll for the response, sampling our own process to
    // distinguish "system is loaded" (sleeping, ticks flat → keep waiting)
    // from "terminal won't respond" (CPU budget exhausted → give up).
    let mut monitor = ProcessMonitor::new(std::process::id())?;
    let deadline = Instant::now() + DETECTION_WALL_CAP;
    let mut remaining_threshold = DETECTION_TICK_THRESHOLD;

    let result = loop {
        match rx.recv_timeout(DETECTION_POLL) {
            Ok(response) => break Some(response),
            Err(RecvTimeoutError::Disconnected) => break None,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let d = monitor.sample()?;
        if d.state == catenary_proc::ProcessState::Dead {
            break None;
        }
        // Only drain threshold on unexplained CPU work: Running + advancing ticks.
        let delta = d.delta_utime + d.delta_stime;
        if d.state == catenary_proc::ProcessState::Running && delta > 0 {
            remaining_threshold -= i64::try_from(delta).unwrap_or(remaining_threshold);
        }

        if remaining_threshold <= 0 || Instant::now() >= deadline {
            break None;
        }
    };

    if !was_raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }

    result.and_then(|r| parse_osc11_response(&r))
}

/// Non-Unix fallback: always returns `None`.
#[cfg(not(unix))]
const fn detect_terminal_bg() -> Option<(u8, u8, u8)> {
    None
}

/// Parse an OSC 11 response into 8-bit RGB.
///
/// Expected format: `\x1b]11;rgb:RRRR/GGGG/BBBB<terminator>`
/// where each channel is 1–4 hex digits. We take the high byte of each
/// 16-bit value (i.e., for `1a1a` we return `0x1a`).
fn parse_osc11_response(response: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(response).ok()?;

    // Find "rgb:" and extract the color portion.
    let rgb_start = text.find("rgb:")?;
    let rgb_part = &text[rgb_start + 4..];

    // Strip terminator characters from the end.
    let rgb_clean = rgb_part.trim_end_matches(['\x07', '\\', '\x1b']);

    let mut channels = rgb_clean.splitn(3, '/');
    let r_hex = channels.next()?;
    let g_hex = channels.next()?;
    let b_hex = channels.next()?;

    Some((
        parse_osc_channel(r_hex)?,
        parse_osc_channel(g_hex)?,
        parse_osc_channel(b_hex)?,
    ))
}

/// Parse a single OSC color channel (1–4 hex digits) into an 8-bit value.
///
/// Terminals may report 4, 2, or even 1 hex digit(s) per channel.
/// For 4 digits (16-bit), we take the high byte. For 2, use directly.
/// For 1, scale up.
fn parse_osc_channel(hex: &str) -> Option<u8> {
    let val = u16::from_str_radix(hex, 16).ok()?;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "intentional 16-to-8-bit conversion"
    )]
    let byte = match hex.len() {
        4 => (val >> 8) as u8,
        3 => (val >> 4) as u8,
        2 => val as u8,
        1 => (val * 17) as u8, // 0x0 → 0x00, 0xf → 0xff
        _ => return None,
    };
    Some(byte)
}

// ── HSL color math ───────────────────────────────────────────────────────

/// Convert 8-bit RGB to HSL.
///
/// Returns `(hue, sat, light)` where `hue` is in `[0, 360)`, `sat` and
/// `light` in `[0, 1]`.
#[allow(
    clippy::many_single_char_names,
    reason = "r/g/b/h/s/l are standard color math notation"
)]
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let rf = f64::from(r) / 255.0;
    let gf = f64::from(g) / 255.0;
    let bf = f64::from(b) / 255.0;

    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;

    let light = f64::midpoint(max, min);

    if delta < f64::EPSILON {
        return (0.0, 0.0, light);
    }

    let sat = if light <= 0.5 {
        delta / (max + min)
    } else {
        delta / (2.0 - max - min)
    };

    let hue_sector = if (max - rf).abs() < f64::EPSILON {
        ((gf - bf) / delta) % 6.0
    } else if (max - gf).abs() < f64::EPSILON {
        (bf - rf) / delta + 2.0
    } else {
        (rf - gf) / delta + 4.0
    };

    let hue = hue_sector * 60.0;
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };

    (hue, sat, light)
}

/// Convert HSL to 8-bit RGB.
///
/// `hue` is in `[0, 360)`, `sat` and `light` in `[0, 1]`.
#[allow(
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "standard HSL math with clamped f64 to u8 conversion"
)]
fn hsl_to_rgb(hue: f64, sat: f64, light: f64) -> (u8, u8, u8) {
    if sat < f64::EPSILON {
        let val = (light * 255.0).round() as u8;
        return (val, val, val);
    }

    let q_val = if light < 0.5 {
        light * (1.0 + sat)
    } else {
        light.mul_add(-sat, light + sat)
    };
    let p_val = 2.0f64.mul_add(light, -q_val);
    let h_norm = hue / 360.0;

    let channel = |tc: f64| -> u8 {
        let tc = if tc < 0.0 {
            tc + 1.0
        } else if tc > 1.0 {
            tc - 1.0
        } else {
            tc
        };
        let out = if tc < 1.0 / 6.0 {
            ((q_val - p_val) * 6.0).mul_add(tc, p_val)
        } else if tc < 0.5 {
            q_val
        } else if tc < 2.0 / 3.0 {
            ((q_val - p_val) * (2.0 / 3.0 - tc)).mul_add(6.0, p_val)
        } else {
            p_val
        };
        (out * 255.0).round() as u8
    };

    (
        channel(h_norm + 1.0 / 3.0),
        channel(h_norm),
        channel(h_norm - 1.0 / 3.0),
    )
}

/// Derive a selection background color from the terminal's background.
///
/// Shifts lightness in HSL space: +0.2 for dark backgrounds, −0.2 for light.
#[allow(
    clippy::many_single_char_names,
    reason = "r/g/b are standard color notation"
)]
fn selection_bg_from_terminal(r: u8, g: u8, b: u8) -> Color {
    let (hue, sat, light) = rgb_to_hsl(r, g, b);
    let new_light = if light < 0.5 {
        (light + SELECTION_LIGHTNESS_SHIFT).min(1.0)
    } else {
        (light - SELECTION_LIGHTNESS_SHIFT).max(0.0)
    };
    let (nr, ng, nb) = hsl_to_rgb(hue, sat, new_light);
    Color::Rgb(nr, ng, nb)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_construction() {
        let theme = Theme::new();
        // border_focused has no DIM modifier
        assert!(!theme.border_focused.add_modifier.contains(Modifier::DIM));
        // border_unfocused has DIM modifier
        assert!(theme.border_unfocused.add_modifier.contains(Modifier::DIM));
    }

    // ── HSL color math tests ─────────────────────────────────────────────

    #[test]
    fn test_rgb_to_hsl_black() {
        let (h, s, l) = rgb_to_hsl(0, 0, 0);
        assert!((h - 0.0).abs() < 0.01);
        assert!((s - 0.0).abs() < 0.01);
        assert!((l - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_rgb_to_hsl_white() {
        let (h, s, l) = rgb_to_hsl(255, 255, 255);
        assert!((h - 0.0).abs() < 0.01);
        assert!((s - 0.0).abs() < 0.01);
        assert!((l - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_rgb_to_hsl_pure_red() {
        let (h, s, l) = rgb_to_hsl(255, 0, 0);
        assert!((h - 0.0).abs() < 0.01);
        assert!((s - 1.0).abs() < 0.01);
        assert!((l - 0.5).abs() < 0.01);
    }

    #[test]
    #[allow(
        clippy::many_single_char_names,
        reason = "r/g/b/h/s/l are standard color math notation"
    )]
    fn test_hsl_roundtrip_dark_gray() {
        let (r, g, b) = (30u8, 30, 30);
        let (h, s, l) = rgb_to_hsl(r, g, b);
        let (r2, g2, b2) = hsl_to_rgb(h, s, l);
        assert!((i16::from(r) - i16::from(r2)).abs() <= 1);
        assert!((i16::from(g) - i16::from(g2)).abs() <= 1);
        assert!((i16::from(b) - i16::from(b2)).abs() <= 1);
    }

    #[test]
    #[allow(
        clippy::many_single_char_names,
        reason = "r/g/b/h/s/l are standard color math notation"
    )]
    fn test_hsl_roundtrip_color() {
        // Teal-ish: rgb(50, 130, 180)
        let (r, g, b) = (50u8, 130, 180);
        let (h, s, l) = rgb_to_hsl(r, g, b);
        let (r2, g2, b2) = hsl_to_rgb(h, s, l);
        assert!((i16::from(r) - i16::from(r2)).abs() <= 1);
        assert!((i16::from(g) - i16::from(g2)).abs() <= 1);
        assert!((i16::from(b) - i16::from(b2)).abs() <= 1);
    }

    #[test]
    fn test_selection_bg_dark_background_lightens() {
        // Typical dark terminal: rgb(26, 26, 26), L ≈ 0.10
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(26, 26, 26) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        // Should be noticeably lighter.
        assert!(r > 26, "red channel should increase: {r}");
        assert!(g > 26, "green channel should increase: {g}");
        assert!(b > 26, "blue channel should increase: {b}");
    }

    #[test]
    fn test_selection_bg_light_background_darkens() {
        // Light terminal: rgb(240, 240, 240), L ≈ 0.94
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(240, 240, 240) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        // Should be noticeably darker.
        assert!(r < 240, "red channel should decrease: {r}");
        assert!(g < 240, "green channel should decrease: {g}");
        assert!(b < 240, "blue channel should decrease: {b}");
    }

    #[test]
    fn test_selection_bg_preserves_hue() {
        // Blueish dark bg: rgb(20, 20, 40)
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(20, 20, 40) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        // Blue channel should still be the dominant one.
        assert!(b >= r, "blue should still dominate: r={r} b={b}");
        assert!(b >= g, "blue should still dominate: g={g} b={b}");
    }

    // ── OSC parsing tests ────────────────────────────────────────────────

    #[test]
    fn test_parse_osc11_response_4digit() {
        // Typical response: ESC ] 11 ; rgb:1a1a/1a1a/1a1a BEL
        let response = b"\x1b]11;rgb:1a1a/1a1a/1a1a\x07";
        let result = parse_osc11_response(response);
        assert_eq!(result, Some((0x1a, 0x1a, 0x1a)));
    }

    #[test]
    fn test_parse_osc11_response_2digit() {
        let response = b"\x1b]11;rgb:1a/1a/1a\x07";
        let result = parse_osc11_response(response);
        assert_eq!(result, Some((0x1a, 0x1a, 0x1a)));
    }

    #[test]
    fn test_parse_osc11_response_st_terminator() {
        // ST terminator: ESC \
        let response = b"\x1b]11;rgb:ffff/0000/8080\x1b\\";
        let result = parse_osc11_response(response);
        assert_eq!(result, Some((0xff, 0x00, 0x80)));
    }

    #[test]
    fn test_parse_osc11_garbage() {
        let response = b"not a valid response";
        assert!(parse_osc11_response(response).is_none());
    }

    #[test]
    fn test_parse_osc_channel_variants() {
        assert_eq!(parse_osc_channel("ff"), Some(0xff));
        assert_eq!(parse_osc_channel("ffff"), Some(0xff));
        assert_eq!(parse_osc_channel("0000"), Some(0x00));
        assert_eq!(parse_osc_channel("8080"), Some(0x80));
        assert_eq!(parse_osc_channel("f"), Some(0xff));
        assert_eq!(parse_osc_channel("0"), Some(0x00));
    }

    #[test]
    fn test_theme_detect_fallback() {
        // Theme::detect() should not panic even if the terminal doesn't
        // support OSC 11 (e.g., in CI). It falls back to Theme::new().
        let theme = Theme::detect();
        // Selection style should be set (either Rgb bg or REVERSED fallback).
        let _ = theme.selection;
    }
}
