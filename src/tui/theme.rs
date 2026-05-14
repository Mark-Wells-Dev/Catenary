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
        assert!(!theme.border_focused.add_modifier.contains(Modifier::DIM));
        assert!(theme.border_unfocused.add_modifier.contains(Modifier::DIM));
    }

    // ── rgb_to_hsl tests ────────────────────────────────────────────────

    /// Assert HSL values within a tight tolerance (0.001).
    fn assert_hsl(actual: (f64, f64, f64), expected: (f64, f64, f64), label: &str) {
        let eps = 0.001;
        assert!(
            (actual.0 - expected.0).abs() < eps,
            "{label}: hue {:.4} != {:.4}",
            actual.0,
            expected.0,
        );
        assert!(
            (actual.1 - expected.1).abs() < eps,
            "{label}: sat {:.4} != {:.4}",
            actual.1,
            expected.1,
        );
        assert!(
            (actual.2 - expected.2).abs() < eps,
            "{label}: light {:.4} != {:.4}",
            actual.2,
            expected.2,
        );
    }

    #[test]
    fn test_rgb_to_hsl_achromatic() {
        assert_hsl(rgb_to_hsl(0, 0, 0), (0.0, 0.0, 0.0), "black");
        assert_hsl(rgb_to_hsl(255, 255, 255), (0.0, 0.0, 1.0), "white");
        assert_hsl(
            rgb_to_hsl(128, 128, 128),
            (0.0, 0.0, 128.0 / 255.0),
            "mid-gray",
        );
    }

    #[test]
    fn test_rgb_to_hsl_primaries() {
        assert_hsl(rgb_to_hsl(255, 0, 0), (0.0, 1.0, 0.5), "red");
        assert_hsl(rgb_to_hsl(0, 255, 0), (120.0, 1.0, 0.5), "green");
        assert_hsl(rgb_to_hsl(0, 0, 255), (240.0, 1.0, 0.5), "blue");
    }

    #[test]
    fn test_rgb_to_hsl_secondaries() {
        assert_hsl(rgb_to_hsl(255, 255, 0), (60.0, 1.0, 0.5), "yellow");
        assert_hsl(rgb_to_hsl(0, 255, 255), (180.0, 1.0, 0.5), "cyan");
        // Magenta exercises negative hue correction: sector = -1, hue = -60 → 300.
        assert_hsl(rgb_to_hsl(255, 0, 255), (300.0, 1.0, 0.5), "magenta");
    }

    #[test]
    fn test_rgb_to_hsl_hue_sectors_non_unit_delta() {
        // Colors where delta != 1.0 to exercise division arithmetic in
        // each hue sector. With delta=1 the `/delta` is identity and
        // mutations like `/ with *` or `/ with %` go undetected.
        // Red-max: (200, 100, 50) → hue ≈ 20
        let (h, s, l) = rgb_to_hsl(200, 100, 50);
        assert!((h - 20.0).abs() < 0.5, "red-max hue: {h}");
        assert!(s > 0.5, "red-max saturation: {s}");
        assert!((l - 0.490).abs() < 0.01, "red-max lightness: {l}");

        // Green-max: (50, 200, 100) → hue ≈ 140
        let (h, s, l) = rgb_to_hsl(50, 200, 100);
        assert!((h - 140.0).abs() < 0.5, "green-max hue: {h}");
        assert!(s > 0.5, "green-max saturation: {s}");
        assert!((l - 0.490).abs() < 0.01, "green-max lightness: {l}");

        // Blue-max: (100, 50, 200) → hue ≈ 260
        let (h, s, l) = rgb_to_hsl(100, 50, 200);
        assert!((h - 260.0).abs() < 0.5, "blue-max hue: {h}");
        assert!(s > 0.5, "blue-max saturation: {s}");
        assert!((l - 0.490).abs() < 0.01, "blue-max lightness: {l}");
    }

    #[test]
    fn test_rgb_to_hsl_light_gt_half() {
        // Exercises the else saturation branch: delta / (2.0 - max - min).
        let (h, s, l) = rgb_to_hsl(200, 100, 100);
        assert!((h - 0.0).abs() < 0.5, "hue should be ~0 (reddish): {h}");
        assert!((s - 0.476).abs() < 0.01, "saturation: {s}");
        assert!((l - 0.588).abs() < 0.01, "lightness: {l}");
    }

    // ── hsl_to_rgb tests ────────────────────────────────────────────────

    #[test]
    fn test_hsl_to_rgb_achromatic() {
        assert_eq!(hsl_to_rgb(0.0, 0.0, 0.0), (0, 0, 0), "black");
        assert_eq!(hsl_to_rgb(0.0, 0.0, 1.0), (255, 255, 255), "white");
        let (r, g, b) = hsl_to_rgb(0.0, 0.0, 0.5);
        assert_eq!(r, g, "mid-gray r==g");
        assert_eq!(g, b, "mid-gray g==b");
        assert_eq!(r, 128, "mid-gray value");
    }

    #[test]
    fn test_hsl_to_rgb_primaries() {
        assert_eq!(hsl_to_rgb(0.0, 1.0, 0.5), (255, 0, 0), "red");
        assert_eq!(hsl_to_rgb(120.0, 1.0, 0.5), (0, 255, 0), "green");
        assert_eq!(hsl_to_rgb(240.0, 1.0, 0.5), (0, 0, 255), "blue");
    }

    #[test]
    fn test_hsl_to_rgb_secondaries() {
        assert_eq!(hsl_to_rgb(60.0, 1.0, 0.5), (255, 255, 0), "yellow");
        assert_eq!(hsl_to_rgb(180.0, 1.0, 0.5), (0, 255, 255), "cyan");
        assert_eq!(hsl_to_rgb(300.0, 1.0, 0.5), (255, 0, 255), "magenta");
    }

    // ── Roundtrip tests ─────────────────────────────────────────────────

    /// Assert RGB roundtrip through HSL preserves values within ±1.
    #[allow(
        clippy::many_single_char_names,
        reason = "r/g/b are standard color notation"
    )]
    fn assert_roundtrip(r: u8, g: u8, b: u8, label: &str) {
        let (h, s, l) = rgb_to_hsl(r, g, b);
        let (r2, g2, b2) = hsl_to_rgb(h, s, l);
        assert!(
            (i16::from(r) - i16::from(r2)).abs() <= 1
                && (i16::from(g) - i16::from(g2)).abs() <= 1
                && (i16::from(b) - i16::from(b2)).abs() <= 1,
            "{label}: ({r},{g},{b}) → hsl({h:.2},{s:.4},{l:.4}) → ({r2},{g2},{b2})",
        );
    }

    #[test]
    fn test_hsl_roundtrip_achromatic() {
        assert_roundtrip(30, 30, 30, "dark gray");
        assert_roundtrip(192, 192, 192, "light gray");
    }

    #[test]
    fn test_hsl_roundtrip_light_lt_half() {
        // Teal: light ≈ 0.45, exercises falling branch for G channel.
        assert_roundtrip(50, 130, 180, "teal");
        // Warm brown: light ≈ 0.49, exercises rising branch for G with
        // non-zero p_val (tc ≈ 0.056 in [0, 1/6)).
        assert_roundtrip(153, 85, 51, "warm brown");
    }

    #[test]
    fn test_hsl_roundtrip_light_gt_half() {
        // Exercises the else q_val branch: light.mul_add(-sat, light + sat).
        assert_roundtrip(200, 100, 100, "light red");
        assert_roundtrip(100, 200, 150, "light green");
        assert_roundtrip(100, 100, 200, "light blue");
    }

    #[test]
    fn test_hsl_roundtrip_non_unit_delta() {
        // All three hue sectors with delta != 1, non-zero p_val.
        assert_roundtrip(200, 100, 50, "orange (red-max)");
        assert_roundtrip(50, 200, 100, "green-teal (green-max)");
        assert_roundtrip(100, 50, 200, "violet (blue-max)");
    }

    #[test]
    fn test_hsl_roundtrip_negative_hue() {
        // Magenta needs the hue < 0 → hue + 360 correction.
        assert_roundtrip(255, 0, 255, "magenta");
        assert_roundtrip(200, 50, 180, "pink-magenta");
    }

    // ── selection_bg tests ──────────────────────────────────────────────

    #[test]
    fn test_selection_bg_dark_background_lightens() {
        // Achromatic dark: rgb(26,26,26) → L≈0.102, shifted to L≈0.302.
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(26, 26, 26) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        assert_eq!((r, g, b), (77, 77, 77), "dark achromatic shift");
    }

    #[test]
    fn test_selection_bg_light_background_darkens() {
        // Achromatic light: rgb(240,240,240) → L≈0.941, shifted to L≈0.741.
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(240, 240, 240) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        assert_eq!((r, g, b), (189, 189, 189), "light achromatic shift");
    }

    #[test]
    fn test_selection_bg_preserves_hue() {
        // Blueish dark: rgb(20,20,40) → hue=240, dark → lightens.
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(20, 20, 40) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        assert!(b > r, "blue should dominate: r={r} g={g} b={b}");
        assert!(b > g, "blue should dominate: r={r} g={g} b={b}");
        assert_eq!((r, g, b), (54, 54, 108), "blueish dark shift");
    }

    #[test]
    fn test_selection_bg_clamps_lightness() {
        // Near-black: L≈0, shift +0.2 stays in [0,1].
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(0, 0, 0) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        assert_eq!((r, g, b), (51, 51, 51), "black shifted to L=0.2");

        // Near-white: L≈1, shift −0.2 stays in [0,1].
        let Color::Rgb(r, g, b) = selection_bg_from_terminal(255, 255, 255) else {
            unreachable!("selection_bg_from_terminal always returns Color::Rgb");
        };
        assert_eq!((r, g, b), (204, 204, 204), "white shifted to L=0.8");
    }

    // ── OSC parsing tests ────────────────────────────────────────────────

    #[test]
    fn test_parse_osc11_response_4digit() {
        let response = b"\x1b]11;rgb:1a1a/1a1a/1a1a\x07";
        assert_eq!(parse_osc11_response(response), Some((0x1a, 0x1a, 0x1a)));
    }

    #[test]
    fn test_parse_osc11_response_2digit() {
        let response = b"\x1b]11;rgb:1a/1a/1a\x07";
        assert_eq!(parse_osc11_response(response), Some((0x1a, 0x1a, 0x1a)));
    }

    #[test]
    fn test_parse_osc11_response_st_terminator() {
        let response = b"\x1b]11;rgb:ffff/0000/8080\x1b\\";
        assert_eq!(parse_osc11_response(response), Some((0xff, 0x00, 0x80)));
    }

    #[test]
    fn test_parse_osc11_garbage() {
        assert!(parse_osc11_response(b"not a valid response").is_none());
    }

    #[test]
    fn test_parse_osc_channel_all_lengths() {
        // 1-digit: scale ×17
        assert_eq!(parse_osc_channel("0"), Some(0x00));
        assert_eq!(parse_osc_channel("f"), Some(0xff));
        assert_eq!(parse_osc_channel("8"), Some(0x88));
        // 2-digit: use directly
        assert_eq!(parse_osc_channel("ff"), Some(0xff));
        assert_eq!(parse_osc_channel("00"), Some(0x00));
        assert_eq!(parse_osc_channel("80"), Some(0x80));
        // 3-digit: shift >>4 (high 8 bits of 12-bit value)
        assert_eq!(parse_osc_channel("1a1"), Some(0x1a));
        assert_eq!(parse_osc_channel("fff"), Some(0xff));
        assert_eq!(parse_osc_channel("000"), Some(0x00));
        // 4-digit: shift >>8 (high byte of 16-bit value)
        assert_eq!(parse_osc_channel("ffff"), Some(0xff));
        assert_eq!(parse_osc_channel("0000"), Some(0x00));
        assert_eq!(parse_osc_channel("8080"), Some(0x80));
        // 0 and 5+ digits: None
        assert_eq!(parse_osc_channel(""), None);
        assert_eq!(parse_osc_channel("12345"), None);
    }

    #[test]
    fn test_theme_detect_fallback() {
        // In CI, detect_terminal_bg() returns None → falls back to Theme::new().
        let theme = Theme::detect();
        let _ = theme.selection;
    }
}
