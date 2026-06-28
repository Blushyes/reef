//! Multi-candidate goto-definition popup.
//!
//! Shown when `gd` / Ctrl+click resolves a single identifier to more
//! than one in-file definition (trait method with multiple impl
//! blocks, shadowed binding, same-name overloads). UX mirrors
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
use unicode_width::UnicodeWidthStr;

/// Maximum candidate snippet length we attempt to render before
/// truncating with `…`. Mirrors the cap in `nav::intrafile::snippet_for`
/// so rows don't reflow unexpectedly across rebuilds.
const MAX_ROW_TEXT_W: u16 = 80;

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
        let row_text = format_row(cand);
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
        let style = Style::default()
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
        let mut s = row_text;
        let used = UnicodeWidthStr::width(s.as_str());
        if (body_w as usize) > used {
            s.push_str(&" ".repeat(body_w as usize - used));
        }
        f.render_widget(
            Line::from(Span::styled(s, style)),
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

/// Format one candidate as ` L<line> <snippet>`, truncated with `…` to
/// `MAX_ROW_TEXT_W`. Line numbers are 1-based for display so they match
/// the preview gutter. Built only for the visible window.
fn format_row(c: &reef_core::nav::Location) -> String {
    let s = format!(" L{:<5} {}", c.line + 1, c.snippet);
    if (UnicodeWidthStr::width(s.as_str()) as u16) <= MAX_ROW_TEXT_W {
        return s;
    }
    let mut truncated = String::new();
    let mut acc = 0u16;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if acc + w + 1 > MAX_ROW_TEXT_W {
            break;
        }
        truncated.push(ch);
        acc += w;
    }
    truncated.push('…');
    truncated
}

pub(crate) fn candidates_max_width(candidates: &[reef_core::nav::Location]) -> u16 {
    candidates.iter().map(row_display_width).max().unwrap_or(0) as u16
}

/// Display width of `format_row(c)` computed *without* building the
/// string — used to size the popup across every candidate while only
/// the visible rows pay for actual formatting. The `{:<5}` line field
/// pads to at least 5 columns; the total is capped at `MAX_ROW_TEXT_W`
/// to match `format_row`'s truncation.
fn row_display_width(c: &reef_core::nav::Location) -> usize {
    // Decimal digit count of the 1-based line number; `{:<5}` pads it to
    // at least 5 columns. `c.line + 1 >= 1`, so `ilog10` never hits its
    // zero precondition.
    let line_w = ((c.line + 1).ilog10() as usize + 1).max(5);
    // " L" + line field + " " + snippet.
    let w = 2 + line_w + 1 + UnicodeWidthStr::width(c.snippet.as_str());
    w.min(MAX_ROW_TEXT_W as usize)
}
