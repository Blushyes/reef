//! Multi-candidate goto-definition popup.
//!
//! Shown when `gd` / Ctrl+click resolves a single identifier to more
//! than one definition or reference. UX mirrors
//! `tree_context_menu` — small overlay anchored near the click /
//! keyboard focus, fixed-order list of rows, Up/Down/Enter/Esc plus
//! mouse. Same panel-wide fallthrough close zone keeps stray clicks
//! from leaking through to the preview underneath.

use crate::TuiApp as App;
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Keep enough source context to identify a declaration or reference while
/// bounding the overlay on narrow terminals.
const MAX_ROW_TEXT_W: u16 = 100;
const MAX_PATH_TEXT_W: u16 = 48;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    let Some(popup) = app.engine.nav_candidates() else {
        return;
    };
    if popup.candidates.is_empty() {
        return;
    }
    let th = app.theme;

    let total = popup.candidates.len();
    let visible = popup.visible_rows();
    let scroll = popup.scroll.min(total.saturating_sub(visible));

    // Reserve a column for the scrollbar gutter when the list scrolls.
    let scrollable = total > visible;
    let gutter = if scrollable { 1 } else { 0 };
    let popup_w =
        (popup.max_row_width + 2 /* borders */ + 2 /* h-padding */ + gutter).min(screen.width);
    // Fixed body height: exactly `visible` rows, never grows past
    // MAX_VISIBLE_ROWS regardless of candidate count.
    let popup_h = (visible as u16 + 2/* borders */).min(screen.height);

    // Anchor below the click / cursor row (popup.anchor_row is already
    // +1 from the click row by `compute_nav_popup_anchor`). Clamp so
    // the popup stays fully on-screen — same pattern as
    // `context_menu_panel`.
    let x = popup
        .anchor_col
        .min(screen.x + screen.width.saturating_sub(popup_w));
    let y = popup
        .anchor_row
        .min(screen.y + screen.height.saturating_sub(popup_h));
    let area = Rect::new(x, y, popup_w, popup_h);

    // Panel-wide fallthrough close. Same approach as context_menu_panel
    // — clicks anywhere on screen that miss a row dismiss the popup,
    // preventing leak-through to the preview pane.
    for sy in screen.y..screen.y + screen.height {
        app.hit_registry
            .register_row(screen.x, sy, screen.width, ClickAction::NavCandidatesClose);
    }

    f.render_widget(Clear, area);
    // Title shows position when the list scrolls — "3/42" — so the
    // user knows there's more below / above.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.border));
    let block = if scrollable {
        // Show the visible-row RANGE, not `selected` — the wheel
        // scrolls the window independently of the highlighted row,
        // so a `selected`-based counter would point off-screen.
        let last = (scroll + visible).min(total);
        block.title(Span::styled(
            format!(" {}–{}/{} ", scroll + 1, last, total),
            Style::default().fg(th.fg_secondary),
        ))
    } else {
        block
    };
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Render exactly the `visible` window [scroll, scroll+visible).
    for row_in_view in 0..visible {
        let cand_idx = scroll + row_in_view;
        let Some(cand) = popup.candidates.get(cand_idx) else {
            break;
        };
        let row = candidate_row(cand, &popup.current_path);
        let y = inner.y + row_in_view as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let is_selected = cand_idx == popup.selected;
        let is_hovered = app.hover_row == Some(y)
            && app
                .hover_col
                .map(|c| c >= inner.x && c < inner.x + inner.width)
                .unwrap_or(false);
        let bg = if is_selected || is_hovered {
            th.selection_bg
        } else {
            th.chrome_bg
        };
        let location_style = Style::default()
            .fg(th.accent)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let snippet_style =
            Style::default()
                .fg(th.fg_primary)
                .bg(bg)
                .add_modifier(if is_selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });

        // Body width excludes the scrollbar gutter so the bar doesn't
        // overwrite row text.
        let body_w = inner.width.saturating_sub(gutter);
        let used = row.display_width();
        let padding = body_w as usize - used.min(body_w as usize);
        f.render_widget(
            Line::from(vec![
                Span::styled(" ", location_style),
                Span::styled(row.location, location_style),
                Span::styled("  ", snippet_style),
                Span::styled(row.snippet, snippet_style),
                Span::styled(" ".repeat(padding), snippet_style),
            ]),
            Rect::new(inner.x, y, body_w, 1),
        );

        // Scrollbar thumb in the gutter column: a contiguous block
        // proportional to the visible fraction, positioned by scroll.
        if scrollable {
            let bar_x = inner.x + body_w;
            let thumb_len = ((visible * visible) / total).max(1).min(visible);
            let track = visible.saturating_sub(thumb_len);
            let max_scroll = total.saturating_sub(visible).max(1);
            let thumb_start = (scroll * track) / max_scroll;
            let in_thumb = row_in_view >= thumb_start && row_in_view < thumb_start + thumb_len;
            let (glyph, color) = if in_thumb {
                ("█", th.accent)
            } else {
                ("│", th.border)
            };
            f.render_widget(
                Line::from(Span::styled(
                    glyph,
                    Style::default().fg(color).bg(th.chrome_bg),
                )),
                Rect::new(bar_x, y, 1, 1),
            );
        }

        // Hit zone uses the absolute candidate index so a click on a
        // scrolled-into-view row selects the right candidate.
        app.hit_registry.register_row(
            inner.x,
            y,
            body_w,
            ClickAction::NavCandidateSelect(cand_idx),
        );
    }
}

struct CandidateRow {
    location: String,
    snippet: String,
}

impl CandidateRow {
    fn display_width(&self) -> usize {
        1 + UnicodeWidthStr::width(self.location.as_str())
            + 2
            + UnicodeWidthStr::width(self.snippet.as_str())
    }
}

/// Formats the same hierarchy used by VS Code's references view in a compact
/// TUI row: workspace-relative file, one-based line, then source text.
/// The source text itself exposes `def`, `class`, `fn`, imports, and call sites
/// without introducing language-specific guesses into the navigation model.
fn candidate_row(c: &reef_core::nav::Location, current_path: &Path) -> CandidateRow {
    let path = c.path.as_deref().unwrap_or(current_path);
    let path = truncate_start(&path.to_string_lossy(), MAX_PATH_TEXT_W);
    let location = format!("{}:{}", path, c.line + 1);
    let fixed_width = 1 + UnicodeWidthStr::width(location.as_str()) + 2;
    let snippet_width = (MAX_ROW_TEXT_W as usize).saturating_sub(fixed_width);
    CandidateRow {
        location,
        snippet: truncate_end(&c.snippet, snippet_width as u16),
    }
}

fn truncate_end(text: &str, max_width: u16) -> String {
    if UnicodeWidthStr::width(text) <= max_width as usize {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut truncated = String::new();
    let mut acc = 0u16;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if acc + w + 1 > max_width {
            break;
        }
        truncated.push(ch);
        acc += w;
    }
    truncated.push('…');
    truncated
}

fn truncate_start(text: &str, max_width: u16) -> String {
    if UnicodeWidthStr::width(text) <= max_width as usize {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    let suffix_width = max_width - 1;
    let mut acc = 0u16;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if acc + width > suffix_width {
            break;
        }
        acc += width;
        start = index;
    }
    format!("…{}", &text[start..])
}

pub(crate) fn candidates_max_width(
    candidates: &[reef_core::nav::Location],
    current_path: &Path,
) -> u16 {
    candidates
        .iter()
        .map(|candidate| row_display_width(candidate, current_path))
        .max()
        .unwrap_or(0) as u16
}

/// Display width of `candidate_row(c)` computed *without* building the
/// string — used to size the popup across every candidate while only
/// the visible rows pay for actual formatting. The total is capped at
/// `MAX_ROW_TEXT_W` to match `candidate_row`'s truncation.
fn row_display_width(c: &reef_core::nav::Location, current_path: &Path) -> usize {
    let path = c.path.as_deref().unwrap_or(current_path);
    let path_w =
        UnicodeWidthStr::width(path.to_string_lossy().as_ref()).min(MAX_PATH_TEXT_W as usize);
    let line_w = (c.line + 1).ilog10() as usize + 1;
    // Leading space + path + ':' separator + line + two spaces + snippet.
    let w = 1 + path_w + 1 + line_w + 2 + UnicodeWidthStr::width(c.snippet.as_str());
    w.min(MAX_ROW_TEXT_W as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn location(path: Option<&str>) -> reef_core::nav::Location {
        reef_core::nav::Location {
            path: path.map(PathBuf::from),
            line: 111,
            byte_range: 4..25,
            snippet: "def extract_source_assets(script):".to_owned(),
        }
    }

    #[test]
    fn candidate_row_shows_cross_file_path_line_and_source() {
        let row = candidate_row(
            &location(Some("pipeline/asset_localization/extract.py")),
            Path::new("pipeline/current.py"),
        );

        assert_eq!(
            (row.location.as_str(), row.snippet.as_str()),
            (
                "pipeline/asset_localization/extract.py:112",
                "def extract_source_assets(script):"
            )
        );
    }

    #[test]
    fn candidate_row_uses_current_path_for_intra_file_location() {
        let row = candidate_row(&location(None), Path::new("src/navigation.rs"));

        assert_eq!(row.location, "src/navigation.rs:112");
    }

    #[test]
    fn candidate_row_preserves_filename_when_path_is_too_wide() {
        let row = candidate_row(
            &location(Some(
                "a/very/long/workspace/path/with/many/components/asset_localization/extract.py",
            )),
            Path::new("unused.py"),
        );

        assert!(row.location.starts_with('…') && row.location.ends_with("extract.py:112"));
    }
}
