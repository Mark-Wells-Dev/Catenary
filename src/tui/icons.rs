// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Icon set resolution and icon-adjacent helpers for the TUI.
//!
//! Resolves an [`IconConfig`] into a fully populated [`IconSet`] by applying
//! per-icon overrides on top of the chosen preset defaults.

use ratatui::style::Style;

use super::theme::Theme;
use crate::config::{IconConfig, IconPreset};

// ── Icon set ─────────────────────────────────────────────────────────────

/// Resolved icon set with all values as owned strings.
///
/// Built from [`IconConfig`] by applying per-icon overrides on top of the
/// chosen preset defaults.
pub struct IconSet {
    /// Diagnostic error icon.
    pub diag_error: String,
    /// Diagnostic warning icon.
    pub diag_warn: String,
    /// Diagnostic info icon.
    pub diag_info: String,
    /// Diagnostic ok (clean) icon.
    pub diag_ok: String,
    /// Search tool icon.
    pub tool_search: String,
    /// Glob tool icon.
    pub tool_glob: String,
    /// Default (fallback) tool icon.
    pub tool_default: String,
    /// Workspace expanded icon.
    pub workspace_open: String,
    /// Workspace collapsed icon.
    pub workspace_closed: String,
    /// Pinned panel icon.
    pub pinned: String,
    /// Progress spinner icon (static fallback; animation is per-preset).
    pub progress: String,
    /// Session started event icon.
    pub session_started: String,
    /// Session shutdown event icon.
    pub session_shutdown: String,
    /// Server state change event icon.
    pub server_state: String,
    /// Sed tool icon.
    pub tool_sed: String,
    /// Language server active status icon.
    pub ls_active: String,
    /// Language server inactive status icon.
    pub ls_inactive: String,
    /// Protocol success icon (request completed successfully).
    pub proto_ok: String,
    /// Protocol error icon (JSON-RPC error or tool isError).
    pub proto_error: String,
    /// Request cancelled icon.
    pub cancelled: String,
    /// Server log info icon (collapsed `window/logMessage` runs at info level).
    pub log_info: String,
    /// Spinner grow phase frames (plays once at start).
    pub spinner_grow: Vec<String>,
    /// Spinner cycle phase frames (loops during progress).
    pub spinner_cycle: Vec<String>,
    /// Spinner done frame (shown on progress end).
    pub spinner_done: String,
}

/// Static preset defaults for a single icon preset.
struct PresetDefaults {
    diag_error: &'static str,
    diag_warn: &'static str,
    diag_info: &'static str,
    diag_ok: &'static str,
    tool_search: &'static str,
    tool_glob: &'static str,
    tool_default: &'static str,
    workspace_open: &'static str,
    workspace_closed: &'static str,
    pinned: &'static str,
    progress: &'static str,
    session_started: &'static str,
    session_shutdown: &'static str,
    server_state: &'static str,
    tool_sed: &'static str,
    ls_active: &'static str,
    ls_inactive: &'static str,
    proto_ok: &'static str,
    proto_error: &'static str,
    cancelled: &'static str,
    log_info: &'static str,
    spinner_grow: &'static [&'static str],
    spinner_cycle: &'static [&'static str],
    spinner_done: &'static str,
}

const PRESET_UNICODE: PresetDefaults = PresetDefaults {
    diag_error: "\u{00D7} ",                                          // ×
    diag_warn: "! ",                                                  // !
    diag_info: "\u{2139} ",                                           // ℹ
    diag_ok: "\u{25CF} ",                                             // ●
    tool_search: "\u{2B9E} ",                                         // ⮞
    tool_glob: "\u{2B9E} ",                                           // ⮞
    tool_default: "\u{2B9E} ",                                        // ⮞
    workspace_open: "\u{25BC} ",                                      // ▼
    workspace_closed: "\u{25B6} ",                                    // ▶
    pinned: "\u{2020}",                                               // †
    progress: "\u{2726} ",                                            // ✦
    session_started: "\u{25CF} ",                                     // ●
    session_shutdown: "\u{25CB} ",                                    // ○
    server_state: "\u{25C6} ",                                        // ◆
    tool_sed: "\u{2B9E} ",                                            // ⮞
    ls_active: "\u{25CF} ",                                           // ●
    ls_inactive: "\u{25CB} ",                                         // ○
    proto_ok: "\u{2714} ",                                            // ✔
    proto_error: "\u{2718} ",                                         // ✘
    cancelled: "\u{2501} ",                                           // ━
    log_info: "\u{25A2} ",                                            // ▢
    spinner_grow: &["\u{2596}", "\u{258C}", "\u{259B}"],              // ▖ ▌ ▛
    spinner_cycle: &["\u{259C}", "\u{259F}", "\u{2599}", "\u{259B}"], // ▜ ▟ ▙ ▛
    spinner_done: "\u{2588}",                                         // █
};

const PRESET_NERD: PresetDefaults = PresetDefaults {
    diag_error: " ",               // nf-cod-error
    diag_warn: " ",                // nf-cod-warning
    diag_info: " ",                // nf-cod-info
    diag_ok: " ",                  // nf-cod-check
    tool_search: " ",              // nf-cod-search
    tool_glob: " ",                // nf-cod-file_directory
    tool_default: "\u{2192} ",     // →
    workspace_open: " ",           // nf-cod-chevron_down
    workspace_closed: " ",         // nf-cod-chevron_right
    pinned: " ",                   // nf-cod-pinned
    progress: " ",                 // nf-cod-loading
    session_started: "\u{EB2C} ",  // nf-cod-play
    session_shutdown: "\u{EB67} ", // nf-cod-debug_stop
    server_state: "\u{EB99} ",     // nf-cod-server
    tool_sed: "\u{EA73} ",         // nf-cod-edit
    ls_active: "\u{EAB0} ",        // nf-cod-circle_filled
    ls_inactive: "\u{EAB1} ",      // nf-cod-circle_outline
    proto_ok: "\u{F0C1} ",         // nf-fa-chain
    proto_error: "\u{F127} ",      // nf-fa-chain_broken
    cancelled: "\u{F0374} ",       // nf-md-minus_thick
    log_info: "\u{F0B79} ",        // nf-md-chat
    spinner_grow: &[],
    spinner_cycle: &[
        "\u{F144B}",
        "\u{F144C}",
        "\u{F144D}",
        "\u{F144E}",
        "\u{F144F}",
        "\u{F1450}",
        "\u{F1451}",
        "\u{F1452}",
        "\u{F1453}",
        "\u{F1454}",
        "\u{F1455}",
        "\u{F1456}",
    ],
    spinner_done: "\u{F1456}", // nf-md-clock_time_twelve
};

const PRESET_EMOJI: PresetDefaults = PresetDefaults {
    diag_error: "\u{274C}\u{FE0F}",   // ❌️
    diag_warn: "\u{26A0}\u{FE0F} ",   // ⚠️
    diag_info: "\u{2139}\u{FE0F} ",   // ℹ️
    diag_ok: "\u{2705}\u{FE0F}",      // ✅️
    tool_search: "\u{1F50D}",         // 🔍
    tool_glob: "\u{1F5C2}\u{FE0F}",   // 🗂️
    tool_default: "\u{27A1}\u{FE0F}", // ➡️
    workspace_open: "\u{1F4C2}",      // 📂
    workspace_closed: "\u{1F4C1}",    // 📁
    pinned: "\u{1F4CC}",              // 📌
    progress: "\u{2726} ",            // ✦ (static fallback; snake spinner is animated)
    session_started: "\u{1F7E2}",     // 🟢
    session_shutdown: "\u{1F534}",    // 🔴
    server_state: "\u{1F537}",        // 🔷
    tool_sed: "\u{270F}\u{FE0F}",     // ✏️
    ls_active: "\u{1F7E2}",           // 🟢
    ls_inactive: "\u{26AA}",          // ⚪
    proto_ok: "\u{2705}",             // ✅
    proto_error: "\u{274C}",          // ❌
    cancelled: "\u{1F6AB}",           // 🚫
    log_info: "\u{1F5E8}\u{FE0F}",    // 🗨️
    spinner_grow: &[],
    spinner_cycle: &[
        "\u{1F550}",
        "\u{1F551}",
        "\u{1F552}",
        "\u{1F553}",
        "\u{1F554}",
        "\u{1F555}",
        "\u{1F556}",
        "\u{1F557}",
        "\u{1F558}",
        "\u{1F559}",
        "\u{1F55A}",
        "\u{1F55B}",
        "\u{1F55C}",
        "\u{1F55D}",
        "\u{1F55E}",
        "\u{1F55F}",
        "\u{1F560}",
        "\u{1F561}",
        "\u{1F562}",
        "\u{1F563}",
        "\u{1F564}",
        "\u{1F565}",
        "\u{1F566}",
        "\u{1F567}",
    ],
    spinner_done: "\u{1F55B}", // 🕛
};

impl IconSet {
    /// Resolve an [`IconConfig`] into a fully populated [`IconSet`].
    #[must_use]
    pub fn from_config(config: IconConfig) -> Self {
        let base = match config.preset {
            IconPreset::Unicode => &PRESET_UNICODE,
            IconPreset::Nerd => &PRESET_NERD,
            IconPreset::Emoji => &PRESET_EMOJI,
        };
        Self {
            diag_error: config
                .diag_error
                .unwrap_or_else(|| base.diag_error.to_string()),
            diag_warn: config
                .diag_warn
                .unwrap_or_else(|| base.diag_warn.to_string()),
            diag_info: config
                .diag_info
                .unwrap_or_else(|| base.diag_info.to_string()),
            diag_ok: config.diag_ok.unwrap_or_else(|| base.diag_ok.to_string()),
            tool_search: config
                .tool_search
                .unwrap_or_else(|| base.tool_search.to_string()),
            tool_glob: config
                .tool_glob
                .unwrap_or_else(|| base.tool_glob.to_string()),
            tool_default: config
                .tool_default
                .unwrap_or_else(|| base.tool_default.to_string()),
            workspace_open: config
                .workspace_open
                .unwrap_or_else(|| base.workspace_open.to_string()),
            workspace_closed: config
                .workspace_closed
                .unwrap_or_else(|| base.workspace_closed.to_string()),
            pinned: config.pinned.unwrap_or_else(|| base.pinned.to_string()),
            progress: config.progress.unwrap_or_else(|| base.progress.to_string()),
            session_started: config
                .session_started
                .unwrap_or_else(|| base.session_started.to_string()),
            session_shutdown: config
                .session_shutdown
                .unwrap_or_else(|| base.session_shutdown.to_string()),
            server_state: config
                .server_state
                .unwrap_or_else(|| base.server_state.to_string()),
            tool_sed: config.tool_sed.unwrap_or_else(|| base.tool_sed.to_string()),
            ls_active: config
                .ls_active
                .unwrap_or_else(|| base.ls_active.to_string()),
            ls_inactive: config
                .ls_inactive
                .unwrap_or_else(|| base.ls_inactive.to_string()),
            proto_ok: config.proto_ok.unwrap_or_else(|| base.proto_ok.to_string()),
            proto_error: config
                .proto_error
                .unwrap_or_else(|| base.proto_error.to_string()),
            cancelled: config
                .cancelled
                .unwrap_or_else(|| base.cancelled.to_string()),
            log_info: config.log_info.unwrap_or_else(|| base.log_info.to_string()),
            spinner_grow: config
                .spinner_grow
                .unwrap_or_else(|| base.spinner_grow.iter().map(|s| (*s).to_string()).collect()),
            spinner_cycle: config.spinner_cycle.unwrap_or_else(|| {
                base.spinner_cycle
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            }),
            spinner_done: config
                .spinner_done
                .unwrap_or_else(|| base.spinner_done.to_string()),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Choose an icon for a tool call based on the tool name.
#[must_use]
pub fn tool_icon<'a>(name: &str, icons: &'a IconSet) -> &'a str {
    match name {
        "grep" => &icons.tool_search,
        "glob" => &icons.tool_glob,
        "sed" => &icons.tool_sed,
        _ => &icons.tool_default,
    }
}

/// Extract the basename from a file path.
#[must_use]
pub fn basename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

/// Determine the diagnostic icon and style from the count and preview text.
#[must_use]
pub fn diag_style<'a>(
    count: usize,
    preview: &str,
    icons: &'a IconSet,
    theme: &Theme,
) -> (&'a str, Style) {
    if count == 0 {
        return (&icons.diag_ok, theme.success);
    }
    let lower = preview.to_lowercase();
    if lower.contains("[error]") {
        (&icons.diag_error, theme.error)
    } else if lower.contains("[warning]") {
        (&icons.diag_warn, theme.warning)
    } else if lower.contains("[info]") || lower.contains("[hint]") {
        (&icons.diag_info, theme.info)
    } else {
        (&icons.diag_warn, theme.warning)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    use crate::config::IconConfig;

    #[test]
    fn test_icon_set_unicode_preset() {
        let icons = IconSet::from_config(IconConfig::default());
        assert_eq!(icons.diag_error, "\u{00D7} ");
    }

    #[test]
    fn test_icon_set_nerd_preset() {
        let config = IconConfig {
            preset: IconPreset::Nerd,
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.diag_error, " ");
    }

    #[test]
    fn test_icon_set_new_unicode_fields() {
        let icons = IconSet::from_config(IconConfig::default());
        assert_eq!(icons.session_started, "\u{25CF} ");
        assert_eq!(icons.session_shutdown, "\u{25CB} ");
        assert_eq!(icons.server_state, "\u{25C6} ");
        assert_eq!(icons.tool_sed, "\u{2B9E} ");
        assert_eq!(icons.ls_active, "\u{25CF} ");
        assert_eq!(icons.ls_inactive, "\u{25CB} ");
        assert_eq!(icons.spinner_grow.len(), 3);
        assert_eq!(icons.spinner_cycle.len(), 4);
        assert_eq!(icons.spinner_done, "\u{2588}");
    }

    #[test]
    fn test_tool_icon_sed() {
        let icons = IconSet::from_config(IconConfig::default());
        assert_eq!(tool_icon("sed", &icons), "\u{2B9E} ");
    }

    #[test]
    fn test_emoji_preset_tool_glob() {
        let config = IconConfig {
            preset: IconPreset::Emoji,
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.tool_glob, "\u{1F5C2}\u{FE0F}");
    }

    #[test]
    fn test_icon_set_overrides() {
        let config = IconConfig {
            diag_error: Some("ERR ".into()),
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.diag_error, "ERR ");
    }

    #[test]
    fn test_icon_set_proto_ok_unicode() {
        let icons = IconSet::from_config(IconConfig::default());
        assert_eq!(icons.proto_ok, "\u{2714} ");
    }

    #[test]
    fn test_icon_set_proto_error_unicode() {
        let icons = IconSet::from_config(IconConfig::default());
        assert_eq!(icons.proto_error, "\u{2718} ");
    }

    #[test]
    fn test_icon_set_cancelled_unicode() {
        let icons = IconSet::from_config(IconConfig::default());
        assert_eq!(icons.cancelled, "\u{2501} ");
    }

    #[test]
    fn test_icon_set_proto_ok_nerd() {
        let config = IconConfig {
            preset: IconPreset::Nerd,
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.proto_ok, "\u{F0C1} ");
    }

    #[test]
    fn test_icon_set_proto_ok_emoji() {
        let config = IconConfig {
            preset: IconPreset::Emoji,
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.proto_ok, "\u{2705}");
    }

    #[test]
    fn test_icon_set_proto_ok_override() {
        let config = IconConfig {
            proto_ok: Some("OK ".into()),
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(icons.proto_ok, "OK ");
    }

    #[test]
    fn test_tool_icon_grep_uses_search_icon() {
        let config = IconConfig {
            preset: IconPreset::Nerd,
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(
            tool_icon("grep", &icons),
            icons.tool_search,
            "grep should map to tool_search icon"
        );
        assert_ne!(
            icons.tool_search, icons.tool_default,
            "Nerd preset search and default icons must differ for this test"
        );
    }

    #[test]
    fn test_tool_icon_glob_uses_glob_icon() {
        let config = IconConfig {
            preset: IconPreset::Nerd,
            ..IconConfig::default()
        };
        let icons = IconSet::from_config(config);
        assert_eq!(
            tool_icon("glob", &icons),
            icons.tool_glob,
            "glob should map to tool_glob icon"
        );
        assert_ne!(
            icons.tool_glob, icons.tool_default,
            "Nerd preset glob and default icons must differ for this test"
        );
    }

    #[test]
    fn test_glyph_families_distinct() {
        for preset in [IconPreset::Unicode, IconPreset::Nerd, IconPreset::Emoji] {
            let config = IconConfig {
                preset,
                ..IconConfig::default()
            };
            let icons = IconSet::from_config(config);
            assert_ne!(
                icons.proto_ok, icons.diag_ok,
                "proto_ok and diag_ok must be distinct"
            );
            assert_ne!(
                icons.proto_error, icons.diag_error,
                "proto_error and diag_error must be distinct"
            );
        }
    }
}
