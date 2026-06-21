//! Convention: focused → `theme.accent`; unfocused → `theme.fg_secondary`.
//! Title rows keep `BOLD` in both states so the typographic weight doesn't
//! jiggle when focus moves.

use crate::ui::theme::Theme;
use ratatui::style::{Modifier, Style};

pub fn header_title_style(theme: &Theme, focused: bool) -> Style {
    let fg = if focused {
        theme.accent
    } else {
        theme.fg_secondary
    };
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_title_style_changes_color_with_focus() {
        let th = Theme::dark();
        let f = header_title_style(&th, true);
        let u = header_title_style(&th, false);
        // The whole point of the helper: focused must be visually distinct.
        // We assert via the resolved fg color, not the Style struct, because
        // ratatui's Style has more fields (bg, modifiers) we don't care about.
        assert_ne!(f.fg, u.fg, "focused/unfocused titles must differ");
        assert_eq!(f.fg, Some(th.accent));
        assert_eq!(u.fg, Some(th.fg_secondary));
    }

    #[test]
    fn header_title_style_keeps_bold_in_both_states() {
        let th = Theme::dark();
        // BOLD is preserved across focus state so the row weight doesn't
        // jiggle when the user Tab/Shift+Tabs between panels.
        for focused in [true, false] {
            let s = header_title_style(&th, focused);
            assert!(
                s.add_modifier.contains(Modifier::BOLD),
                "BOLD must be set when focused={focused}"
            );
        }
    }

    #[test]
    fn header_title_style_works_for_light_theme_too() {
        // Light theme uses different RGB but the same focused/unfocused
        // contract; guarding against accidental theme drift.
        let th = Theme::light();
        let f = header_title_style(&th, true);
        let u = header_title_style(&th, false);
        assert_ne!(f.fg, u.fg);
    }
}
