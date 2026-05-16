//! VSCode-style floating find widget. Anchored to the upper-right of
//! the active content panel (`app.last_preview_rect` for Files / Search
//! tabs, `app.last_diff_rect` for Git / Graph tabs). Renders as a flat
//! solid-color slab — no border, just `chrome_active_bg` filled across
//! a top pad row + content row + bottom pad row — so it reads as an
//! inset toolbar with breathing room rather than a framed modal.
//!
//! Hover affordances: while the widget is open, the `app.hover_col` /
//! `app.hover_row` slots fed by `MouseEventKind::Moved` light up the
//! cell under the cursor so the user can see exactly what the next
//! click will hit.
//!
//! PR A renders only the find row (no Replace expansion); the chevron
//! and replace input row are reserved for PR B.

use crate::app::App;
use crate::find_widget::{FindTarget, FindWidgetState};
use crate::ui::mouse::ClickAction;
use crate::ui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;
use unicode_width::UnicodeWidthStr;

/// Target visible width of the widget. Caller clamps against the anchor
/// panel; very narrow panels degrade gracefully (toggles + counter still
/// fit at 36 cols, the query area becomes a small editor).
const TARGET_WIDTH: u16 = 64;
const MIN_WIDTH: u16 = 36;
/// Total visible height: top blank pad + content row + bottom blank pad.
const WIDGET_HEIGHT: u16 = 3;
/// Horizontal padding inside the chip — keeps glyphs from sitting
/// flush against the painted edges.
const H_PAD: u16 = 2;

const TOGGLE_LEN: u16 = 4; // " Aa " etc.
const BUTTON_LEN: u16 = 3; // " ↑ "
const COUNTER_LEN: u16 = 11; // " 1234/1234 "

pub fn render(f: &mut Frame, app: &mut App) {
    if !app.find_widget.active {
        return;
    }
    let Some(target) = app.find_widget.target else {
        return;
    };
    // Pick the anchor rect from whichever content panel hosts the
    // widget's target. Falling through with `None` is the right call
    // for transient states (tab mid-switch) — the widget will simply
    // not draw this frame.
    let anchor = match target {
        FindTarget::FilePreview => app.last_preview_rect,
        FindTarget::DiffUnified
        | FindTarget::DiffSbsLeft
        | FindTarget::DiffSbsRight
        | FindTarget::GraphDiffUnified
        | FindTarget::GraphDiffSbsLeft
        | FindTarget::GraphDiffSbsRight => app.last_diff_rect,
    };
    let Some(anchor) = anchor else {
        return;
    };
    if anchor.width < MIN_WIDTH || anchor.height < WIDGET_HEIGHT {
        return;
    }

    let width = TARGET_WIDTH.min(anchor.width).max(MIN_WIDTH);
    // Right-anchor flush to the panel's right edge.
    let x = anchor.x + anchor.width.saturating_sub(width);
    let y = anchor.y;
    let rect = Rect::new(x, y, width, WIDGET_HEIGHT);
    app.find_widget.last_widget_rect = Some(rect);

    let th = app.theme;
    let chip_bg = th.chrome_active_bg;

    // Wipe glyphs behind the widget so the underlying panel doesn't
    // bleed through. No border — just a flat solid-color slab spanning
    // top pad / content / bottom pad rows.
    f.render_widget(Clear, rect);
    let fill_style = Style::default().bg(chip_bg);
    let fill = " ".repeat(width as usize);
    for row_y in y..y + WIDGET_HEIGHT {
        f.render_widget(
            Line::from(Span::styled(fill.clone(), fill_style)),
            Rect::new(x, row_y, width, 1),
        );
    }

    // Content sits on the middle row; outer rows are pure padding.
    let content_y = y + 1;
    render_find_row(f, app, &th, rect, content_y);
}

fn render_find_row(f: &mut Frame, app: &mut App, th: &Theme, area: Rect, content_y: u16) {
    let usable_x = area.x + H_PAD;
    let usable_end = area.x + area.width - H_PAD;

    // Right-to-left allocation: the buttons and counter are fixed-width;
    // the query input absorbs whatever's left after them.
    let close_x = usable_end - BUTTON_LEN;
    let down_x = close_x - BUTTON_LEN;
    let up_x = down_x - BUTTON_LEN;
    let counter_x = up_x.saturating_sub(COUNTER_LEN);
    let regex_x = counter_x.saturating_sub(TOGGLE_LEN);
    let word_x = regex_x.saturating_sub(TOGGLE_LEN);
    let case_x = word_x.saturating_sub(TOGGLE_LEN);
    let query_x = usable_x;
    let query_w = case_x.saturating_sub(query_x).saturating_sub(1);

    // ─── Query input ───
    let query_text = visible_query_text(
        &app.find_widget.query,
        app.find_widget.cursor,
        query_w as usize,
    );
    let query_style = Style::default()
        .fg(th.chrome_active_fg)
        .bg(th.chrome_active_bg);
    let placeholder_style = Style::default()
        .fg(th.chrome_muted_fg)
        .bg(th.chrome_active_bg)
        .add_modifier(Modifier::ITALIC);
    let query_span = if app.find_widget.query.is_empty() {
        Span::styled(
            format!("{:<w$}", "Find", w = query_w as usize),
            placeholder_style,
        )
    } else {
        Span::styled(
            format!("{:<w$}", query_text, w = query_w as usize),
            query_style,
        )
    };
    f.render_widget(
        Line::from(query_span),
        Rect::new(query_x, content_y, query_w, 1),
    );

    // Cursor lands at column = display width of query[..cursor]
    let cursor_col = UnicodeWidthStr::width(
        &app.find_widget.query[..app.find_widget.cursor.min(app.find_widget.query.len())],
    ) as u16;
    let cursor_x_actual = query_x + cursor_col.min(query_w.saturating_sub(1));
    f.set_cursor_position((cursor_x_actual, content_y));

    let hover = (app.hover_col, app.hover_row);

    let toggles: [(&str, u16, bool, ClickAction); 3] = [
        (
            "Aa",
            case_x,
            app.find_widget.match_case,
            ClickAction::FindWidgetToggleCase,
        ),
        (
            "ab",
            word_x,
            app.find_widget.whole_word,
            ClickAction::FindWidgetToggleWord,
        ),
        (
            ".*",
            regex_x,
            app.find_widget.regex,
            ClickAction::FindWidgetToggleRegex,
        ),
    ];
    let buttons: [(&str, u16, ClickAction); 3] = [
        (" ↑ ", up_x, ClickAction::FindWidgetPrev),
        (" ↓ ", down_x, ClickAction::FindWidgetNext),
        (" × ", close_x, ClickAction::FindWidgetClose),
    ];

    for (label, x, on, _) in &toggles {
        paint_toggle(
            f,
            th,
            label,
            *x,
            content_y,
            *on,
            hover_in(hover, *x, content_y, TOGGLE_LEN),
        );
    }

    let counter_text = match &app.find_widget.regex_error {
        Some(_) => " regex! ".to_string(),
        None => format_counter(&app.find_widget),
    };
    let counter_padded = format!("{:^w$}", counter_text, w = COUNTER_LEN as usize);
    let counter_fg = if app.find_widget.regex_error.is_some() {
        th.removed_accent
    } else if app.find_widget.matches.is_empty() {
        th.chrome_muted_fg
    } else {
        th.chrome_active_fg
    };
    f.render_widget(
        Line::from(Span::styled(
            counter_padded,
            Style::default().fg(counter_fg).bg(th.chrome_active_bg),
        )),
        Rect::new(counter_x, content_y, COUNTER_LEN, 1),
    );

    for (glyph, x, _) in &buttons {
        paint_button(
            f,
            th,
            glyph,
            *x,
            content_y,
            hover_in(hover, *x, content_y, BUTTON_LEN),
        );
    }

    let registry = &mut app.hit_registry;
    for (_, x, _, action) in &toggles {
        registry.register_row(*x, content_y, TOGGLE_LEN, action.clone());
    }
    for (_, x, action) in &buttons {
        registry.register_row(*x, content_y, BUTTON_LEN, action.clone());
    }
}

/// Mouse-hover hit test for a single-row strip starting at `(x, y)` with
/// the given width. `app.hover_col / hover_row` are the last MouseMoved
/// position; missing values just return false (no hover indication).
fn hover_in(hover: (Option<u16>, Option<u16>), x: u16, y: u16, width: u16) -> bool {
    matches!(hover, (Some(c), Some(r)) if r == y && c >= x && c < x + width)
}

fn paint_toggle(f: &mut Frame, th: &Theme, label: &str, x: u16, y: u16, on: bool, hover: bool) {
    // Four visual states:
    // - On  + hover → reversed accent fill (extra "you're on this") punch
    // - On         → accent bg, chip fg
    // - Off + hover → chrome_bg fill so off cells "light up" on hover
    //                 without committing to the accent color
    // - Off        → fully blended into chip background
    let style = match (on, hover) {
        (true, true) => Style::default()
            .fg(th.chrome_active_bg)
            .bg(th.accent)
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED),
        (true, false) => Style::default()
            .fg(th.chrome_active_bg)
            .bg(th.accent)
            .add_modifier(Modifier::BOLD),
        (false, true) => Style::default().fg(th.chrome_active_fg).bg(th.chrome_bg),
        (false, false) => Style::default()
            .fg(th.chrome_muted_fg)
            .bg(th.chrome_active_bg),
    };
    let text = format!(" {} ", label);
    f.render_widget(
        Line::from(Span::styled(text, style)),
        Rect::new(x, y, TOGGLE_LEN, 1),
    );
}

fn paint_button(f: &mut Frame, th: &Theme, glyph: &str, x: u16, y: u16, hover: bool) {
    // Hover swaps the cell to a slightly darker `chrome_bg` so it
    // visually pops against the chip surface, with bold glyph for
    // the "this is clickable now" cue.
    let style = if hover {
        Style::default()
            .fg(th.chrome_active_fg)
            .bg(th.chrome_bg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(th.chrome_active_fg)
            .bg(th.chrome_active_bg)
    };
    f.render_widget(
        Line::from(Span::styled(glyph.to_string(), style)),
        Rect::new(x, y, BUTTON_LEN, 1),
    );
}

/// Pick what slice of the query string to show inside the given visible
/// width. We keep the cursor in view by sliding a window over the
/// string when it would otherwise overflow.
fn visible_query_text(query: &str, cursor: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let total = UnicodeWidthStr::width(query);
    if total <= width {
        return query.to_string();
    }
    // Cursor-anchored sliding window. Show as much of the prefix that
    // ends at the cursor as fits; if the cursor is near the start,
    // anchor at start.
    let prefix = &query[..cursor.min(query.len())];
    let prefix_w = UnicodeWidthStr::width(prefix);
    if prefix_w < width {
        // Cursor near start — show from byte 0, truncate tail.
        let mut acc = 0usize;
        let mut end = 0usize;
        for (i, c) in query.char_indices() {
            let cw = UnicodeWidthStr::width(c.to_string().as_str());
            if acc + cw > width {
                break;
            }
            acc += cw;
            end = i + c.len_utf8();
        }
        query[..end].to_string()
    } else {
        // Show width chars ending at cursor.
        let mut acc = 0usize;
        let mut start = prefix.len();
        for (i, c) in prefix.char_indices().rev() {
            let cw = UnicodeWidthStr::width(c.to_string().as_str());
            if acc + cw > width.saturating_sub(1) {
                break;
            }
            acc += cw;
            start = i;
        }
        prefix[start..].to_string()
    }
}

fn format_counter(state: &FindWidgetState) -> String {
    if state.matches.is_empty() {
        if state.query.is_empty() {
            " ".to_string()
        } else {
            "No results".to_string()
        }
    } else {
        let total = state.matches.len();
        let cur = state.current.map(|i| i + 1).unwrap_or(0);
        format!("{}/{}", cur, total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_no_query_is_blank() {
        let s = FindWidgetState::default();
        assert_eq!(format_counter(&s).trim(), "");
    }

    #[test]
    fn counter_query_no_matches() {
        let s = FindWidgetState {
            query: "x".to_string(),
            ..FindWidgetState::default()
        };
        assert_eq!(format_counter(&s), "No results");
    }

    #[test]
    fn counter_with_matches() {
        let s = FindWidgetState {
            matches: vec![
                crate::search::MatchLoc {
                    row: 0,
                    byte_range: 0..1,
                },
                crate::search::MatchLoc {
                    row: 0,
                    byte_range: 2..3,
                },
            ],
            current: Some(0),
            ..FindWidgetState::default()
        };
        assert_eq!(format_counter(&s), "1/2");
    }

    #[test]
    fn visible_query_short_fits_entirely() {
        assert_eq!(visible_query_text("foo", 3, 20), "foo");
    }

    #[test]
    fn visible_query_long_window_anchors_to_cursor_tail() {
        // 30-char string, cursor at the end, 10-char window.
        let q: String = "abcdefghijklmnopqrstuvwxyz0123".to_string();
        let v = visible_query_text(&q, q.len(), 10);
        // Tail end stays visible.
        assert!(q.ends_with(&v));
        assert!(UnicodeWidthStr::width(v.as_str()) <= 10);
    }

    #[test]
    fn visible_query_long_cursor_at_start_anchors_to_start() {
        let q = "abcdefghijklmnopqrstuvwxyz".to_string();
        let v = visible_query_text(&q, 0, 5);
        assert!(q.starts_with(&v));
        assert!(UnicodeWidthStr::width(v.as_str()) <= 5);
    }
}
