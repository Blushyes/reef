//! Color theme for reef's UI chrome and diff surfaces.
//!
//! `Theme` is a flat struct of `ratatui::Color` fields addressed by role
//! (`chrome_bg`, `selection_bg`, `added_bg`, …), not by UI component. Every
//! panel grabs `let th = app.theme;` once at the top of its render fn and
//! looks up colors by role. Two presets (`dark`, `light`) match our hardcoded
//! dark values byte-for-byte and a GitHub-Light-derived light palette.
//!
//! `resolve()` decides which preset to use on startup:
//!   1. `prefs.ui.theme` — `"dark" | "light" | "auto"` (default `"auto"`).
//!   2. Under `"auto"`, if stdin or stdout isn't a TTY (CI, piped, snapshot
//!      tests), fall back to `dark()` so builds stay deterministic.
//!   3. Otherwise probe the terminal via `terminal-colorsaurus` (OSC 11).
//!      Must run BEFORE `enable_raw_mode()` in `main.rs` — otherwise the
//!      terminal's reply fragments leak onto the TUI.

use ratatui::style::Color;
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub is_dark: bool,

    // Chrome — title bar, tab bar, status bar, help popup.
    pub chrome_bg: Color,
    pub chrome_fg: Color,
    pub chrome_muted_fg: Color,
    pub chrome_active_bg: Color,
    pub chrome_active_fg: Color,

    // Panel borders and separators.
    pub border: Color,

    // Selection / hover row backgrounds.
    pub selection_bg: Color,
    pub hover_bg: Color,

    // Search match backgrounds (all matches vs the current one).
    pub search_match: Color,
    pub search_current: Color,

    // Diff line highlights.
    pub added_bg: Color,
    pub removed_bg: Color,
    pub added_accent: Color,
    pub removed_accent: Color,

    // Conflict / error banner background (distinct from `removed_bg` so a
    // banner stays visible when stacked over a diff).
    pub error_bg: Color,

    // Warning banner background — paste-conflict status-bar prompt and
    // similar "needs attention but not destructive" badges. Yellow-
    // family across both presets so the meaning telegraphs without
    // theme inversion.
    pub warn_bg: Color,

    // Generic text.
    pub fg_primary: Color,
    pub fg_secondary: Color,

    // Blue/cyan highlights (branch name, hunk headers, accent links).
    pub accent: Color,

    // "reef" title badge.
    pub badge_fg: Color,
    pub badge_bg: Color,
}

impl Theme {
    /// Dark preset — byte-identical to reef's pre-theme hardcoded colors so
    /// existing users see zero visual change.
    pub const fn dark() -> Self {
        Self {
            is_dark: true,
            chrome_bg: Color::Rgb(30, 30, 40),
            chrome_fg: Color::White,
            chrome_muted_fg: Color::DarkGray,
            chrome_active_bg: Color::Rgb(60, 60, 80),
            chrome_active_fg: Color::White,
            border: Color::DarkGray,
            selection_bg: Color::Rgb(40, 60, 100),
            hover_bg: Color::Rgb(40, 40, 50),
            search_match: Color::Rgb(80, 70, 30),
            search_current: Color::Rgb(180, 140, 40),
            added_bg: Color::Rgb(0, 40, 0),
            removed_bg: Color::Rgb(60, 0, 0),
            added_accent: Color::Green,
            removed_accent: Color::Red,
            error_bg: Color::Rgb(60, 0, 0),
            warn_bg: Color::Rgb(180, 140, 40),
            fg_primary: Color::White,
            fg_secondary: Color::DarkGray,
            accent: Color::Cyan,
            badge_fg: Color::Black,
            badge_bg: Color::Blue,
        }
    }

    /// Light preset — grounded in GitHub Light so diff reds/greens preserve
    /// syntect OneHalfLight legibility.
    pub const fn light() -> Self {
        Self {
            is_dark: false,
            chrome_bg: Color::Rgb(246, 248, 250),
            chrome_fg: Color::Rgb(36, 41, 47),
            chrome_muted_fg: Color::Rgb(101, 109, 118),
            chrome_active_bg: Color::Rgb(221, 244, 255),
            chrome_active_fg: Color::Rgb(9, 105, 218),
            border: Color::Rgb(208, 215, 222),
            selection_bg: Color::Rgb(221, 244, 255),
            hover_bg: Color::Rgb(234, 238, 242),
            search_match: Color::Rgb(255, 240, 170),
            search_current: Color::Rgb(255, 210, 50),
            added_bg: Color::Rgb(230, 255, 237),
            removed_bg: Color::Rgb(255, 235, 233),
            added_accent: Color::Rgb(26, 127, 55),
            removed_accent: Color::Rgb(207, 34, 46),
            error_bg: Color::Rgb(255, 235, 233),
            warn_bg: Color::Rgb(255, 220, 120),
            fg_primary: Color::Rgb(36, 41, 47),
            fg_secondary: Color::Rgb(101, 109, 118),
            accent: Color::Rgb(9, 105, 218),
            badge_fg: Color::Rgb(255, 255, 255),
            badge_bg: Color::Rgb(9, 105, 218),
        }
    }

    /// Pref override + terminal-background detection. Runs once in `main.rs`
    /// before raw mode is enabled.
    pub fn resolve() -> Self {
        match crate::prefs::get("ui.theme").as_deref() {
            Some("dark") => return Self::dark(),
            Some("light") => return Self::light(),
            _ => {}
        }

        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Self::dark();
        }

        // `QueryOptions` is marked `#[non_exhaustive]` so we can't struct-init
        // it directly — start from `default()` and override only what we need.
        let mut opts = terminal_colorsaurus::QueryOptions::default();
        opts.timeout = Duration::from_millis(100);
        match terminal_colorsaurus::theme_mode(opts) {
            Ok(terminal_colorsaurus::ThemeMode::Light) => Self::light(),
            _ => Self::dark(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_is_dark_light_is_not() {
        assert!(Theme::dark().is_dark);
        assert!(!Theme::light().is_dark);
    }

    #[test]
    fn dark_and_light_differ_on_chrome() {
        let d = Theme::dark();
        let l = Theme::light();
        assert_ne!(d.chrome_bg, l.chrome_bg);
        assert_ne!(d.chrome_fg, l.chrome_fg);
        assert_ne!(d.added_bg, l.added_bg);
        assert_ne!(d.removed_bg, l.removed_bg);
        assert_ne!(d.selection_bg, l.selection_bg);
        assert_ne!(d.hover_bg, l.hover_bg);
    }

    #[test]
    fn dark_preserves_legacy_hardcodes() {
        // Guard against accidental drift from the pre-theme dark look.
        let d = Theme::dark();
        assert_eq!(d.chrome_bg, Color::Rgb(30, 30, 40));
        assert_eq!(d.selection_bg, Color::Rgb(40, 60, 100));
        assert_eq!(d.hover_bg, Color::Rgb(40, 40, 50));
        assert_eq!(d.added_bg, Color::Rgb(0, 40, 0));
        assert_eq!(d.removed_bg, Color::Rgb(60, 0, 0));
    }
}
