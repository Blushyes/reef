//! Quick-open palette overlay (bound to Space-P; see `crate::quick_open`).
//!
//! Rendered on top of the normal UI when `app.quick_open.active` is true.
//! Three regions inside the popup: a single-row input line, a list of
//! matches, and a right-aligned counter footer. The only state this panel
//! writes back to `App` is `quick_open.last_view_h` (used by PageUp/PageDown
//! step sizing) and `quick_open.scroll` (kept in sync with `selected`); the
//! matching itself lives in `crate::quick_open`.
//!
//! Highlight strategy: nucleo reports `indices` as character positions in
//! the full display path. We render the path as "basename + dir" — basename
//! first, parent dir dimmed to the right — so indices need mapping to the
//! correct segment. `build_row_line` does that mapping char-by-char so a
//! query like "uiftp" lights up 'u', 'i', 'f', 't', 'p' across both the
//! directory and basename columns.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};
use std::collections::HashSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::quick_open::{Candidate, MatchEntry};
use crate::ui::mouse::ClickAction;
use crate::ui::theme::Theme;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    let th = app.theme;

    let popup_w = 82u16.min(screen.width.saturating_sub(4).max(20));
    let popup_h = 24u16.min(screen.height.saturating_sub(4).max(8));
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w, popup_h);

    // Stash bounds so `quick_open::handle_mouse` can decide "inside popup
    // vs. click-away dismiss". Overwritten every frame so it stays in sync
    // with terminal resizes.
    app.quick_open.last_popup_area = Some(area);

    f.render_widget(Clear, area);

    let recent = app.quick_open.query.is_empty() && !app.quick_open.mru.is_empty();
    let title = if recent {
        " Quick Open · recent "
    } else {
        " Quick Open "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            title,
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    let input_y = inner.y;
    let sep_y = inner.y + 1;
    let list_y = inner.y + 2;
    // Reserve the last row for the footer when there's room; otherwise let
    // the list use every available row. This mirrors how `render_help`
    // degrades on tiny screens.
    let has_footer = inner.height >= 5;
    let list_h = if has_footer {
        inner.height.saturating_sub(3)
    } else {
        inner.height.saturating_sub(2)
    };
    let footer_y = inner.y + inner.height.saturating_sub(1);

    app.quick_open.last_view_h = list_h;

    // ── Input row ──────────────────────────────────────────────────────
    let prompt_spans = vec![
        Span::styled(
            "> ",
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            app.quick_open.query.clone(),
            Style::default().fg(th.fg_primary),
        ),
    ];
    f.render_widget(
        Line::from(prompt_spans),
        Rect::new(inner.x, input_y, inner.width, 1),
    );
    // Blinking cursor — same trick `render_search_prompt` uses. Using
    // UnicodeWidthStr so cursor lands between CJK/wide chars correctly.
    let cursor_w = UnicodeWidthStr::width(&app.quick_open.query[..app.quick_open.cursor]) as u16;
    let cursor_x = inner.x + 2 + cursor_w.min(inner.width.saturating_sub(3));
    f.set_cursor_position((cursor_x, input_y));

    // ── Separator row ──────────────────────────────────────────────────
    f.render_widget(
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().fg(th.border),
        )),
        Rect::new(inner.x, sep_y, inner.width, 1),
    );

    // ── List ───────────────────────────────────────────────────────────
    if app.quick_open.matches.is_empty() {
        let msg = if app.quick_open.query.is_empty() {
            "Type to search files…"
        } else {
            "No matching files"
        };
        f.render_widget(
            Line::from(Span::styled(
                msg,
                Style::default()
                    .fg(th.fg_secondary)
                    .add_modifier(Modifier::ITALIC),
            )),
            Rect::new(inner.x, list_y, inner.width, 1),
        );
    } else {
        // Keep the selection in view. Same pattern as file_tree_panel — only
        // adjust scroll when selection moved outside the window, so the user
        // can scroll independently (Future: wire mouse wheel on popup).
        let sel = app.quick_open.selected;
        if sel < app.quick_open.scroll {
            app.quick_open.scroll = sel;
        } else if list_h > 0 && sel >= app.quick_open.scroll + list_h as usize {
            app.quick_open.scroll = sel + 1 - list_h as usize;
        }
        let scroll = app.quick_open.scroll;

        for row in 0..list_h as usize {
            let match_idx = scroll + row;
            let Some(m) = app.quick_open.matches.get(match_idx) else {
                break;
            };
            let Some(cand) = app.quick_open.index.get(m.idx) else {
                continue;
            };
            let is_sel = match_idx == sel;
            let y = list_y + row as u16;
            let line = build_row_line(cand, m, is_sel, inner.width, &th);
            f.render_widget(line, Rect::new(inner.x, y, inner.width, 1));
            // Register the row as a click zone. Registered after the
            // underlying panels drew theirs, so `hit_test` (which scans
            // in reverse) picks up the palette zone first on overlap.
            app.hit_registry.register_row(
                inner.x,
                y,
                inner.width,
                ClickAction::QuickOpenSelect(match_idx),
            );
        }
    }

    // ── Footer (N/M counter) ───────────────────────────────────────────
    if has_footer {
        let cur = if app.quick_open.matches.is_empty() {
            0
        } else {
            app.quick_open.selected + 1
        };
        let text = format!("{} / {}", cur, app.quick_open.matches.len());
        let w = UnicodeWidthStr::width(text.as_str()) as u16;
        let fx = inner.x + inner.width.saturating_sub(w);
        f.render_widget(
            Line::from(Span::styled(text, Style::default().fg(th.fg_secondary))),
            Rect::new(fx, footer_y, w, 1),
        );
    }
}

/// Render one result row. Layout:
/// `<bg> basename   dir/ <fill> `
/// — basename bold, dir dim, matched chars in `indices` in accent color.
/// The whole row gets `selection_bg` when selected.
fn build_row_line(
    cand: &Candidate,
    m: &MatchEntry,
    is_sel: bool,
    width: u16,
    th: &Theme,
) -> Line<'static> {
    let bg = if is_sel {
        th.selection_bg
    } else {
        Color::Reset
    };

    let display = &cand.display;
    // Byte offset where basename starts; also acts as "dir byte length".
    let basename_start_byte = display.rfind('/').map(|i| i + 1).unwrap_or(0);
    let basename = &display[basename_start_byte..];
    let dir = &display[..basename_start_byte];
    // Char count of the dir prefix; `indices` are char offsets into the
    // full display so we need a char boundary, not the byte one.
    let basename_start_char = display[..basename_start_byte].chars().count();

    let basename_hl: HashSet<usize> = m
        .indices
        .iter()
        .filter_map(|&i| {
            let i = i as usize;
            if i >= basename_start_char {
                Some(i - basename_start_char)
            } else {
                None
            }
        })
        .collect();
    let dir_hl: HashSet<usize> = m
        .indices
        .iter()
        .filter_map(|&i| {
            let i = i as usize;
            if i < basename_start_char {
                Some(i)
            } else {
                None
            }
        })
        .collect();

    let basename_base = Style::default()
        .fg(th.fg_primary)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let dir_base = Style::default().fg(th.fg_secondary).bg(bg);
    let hl_bg = if is_sel {
        th.search_current
    } else {
        th.search_match
    };
    let hl = Style::default()
        .fg(th.fg_primary)
        .bg(hl_bg)
        .add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span<'static>> = Vec::new();

    // Leading single-space so the selection bar doesn't hug the border.
    spans.push(Span::styled(" ".to_string(), Style::default().bg(bg)));

    // Basename, char-by-char so we can apply per-char highlight style.
    let mut basename_w: usize = 0;
    for (ci, c) in basename.chars().enumerate() {
        let style = if basename_hl.contains(&ci) {
            hl
        } else {
            basename_base
        };
        spans.push(Span::styled(c.to_string(), style));
        basename_w += UnicodeWidthChar::width(c).unwrap_or(0);
    }

    // Pad basename column to ~40 cols (or narrower on small screens) so the
    // dir column visually aligns across rows.
    let name_col: usize = (width as usize).saturating_sub(10).clamp(10, 40);
    let used = 1 + basename_w;
    let pad = name_col.saturating_sub(used) + 2; // +2: always have a gap
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));

    // Dir, char-by-char for matching char highlight.
    let mut dir_w: usize = 0;
    for (ci, c) in dir.chars().enumerate() {
        let style = if dir_hl.contains(&ci) { hl } else { dir_base };
        spans.push(Span::styled(c.to_string(), style));
        dir_w += UnicodeWidthChar::width(c).unwrap_or(0);
    }

    // Trailing fill so the selection bg reaches the right edge.
    let used_total = used + pad + dir_w;
    let fill = (width as usize).saturating_sub(used_total);
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), Style::default().bg(bg)));
    }

    Line::from(spans)
}
