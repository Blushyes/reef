//! Global-search palette overlay (Space+F; see `crate::global_search`).
//!
//! Bigger overlay than `quick_open_panel` because each row shows two columns
//! (`path:line` + the matching line text) and the list benefits from more
//! vertical room. Shape otherwise mirrors quick-open: input row → separator
//! → list → footer, with the popup's bounds stashed back into
//! `GlobalSearchState.last_popup_area` so the mouse handler can distinguish
//! "click inside popup" from "click-away dismiss".

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::global_search::{MAX_RESULTS, MatchHit};
use crate::ui::mouse::ClickAction;
use crate::ui::theme::Theme;

/// Overlay entry point — centred popup with border, calling `render_body`
/// on the inner rect. The popup itself stashes its bounds so the mouse
/// handler can tell "click inside" from "click-away dismiss".
pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    let th = app.theme;

    // Wider and taller than quick-open because each row is two columns.
    let popup_w = 120u16.min(
        (screen.width.saturating_mul(9) / 10)
            .saturating_sub(2)
            .max(30),
    );
    let popup_h = 32u16.min((screen.height.saturating_mul(3) / 4).max(10));
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w, popup_h);

    app.global_search.last_popup_area = Some(area);

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            " Global Search ",
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Overlay is always input-focused — there's no "browse results" mode
    // for a transient popup; closing it is the analogue.
    render_body(f, app, inner, /* input_focused */ true);
}

/// Shared search-results body (input row → separator → list → footer).
/// Drawn by both the overlay and the `search_tab` sidebar on whatever
/// rectangle they want; `inner` is the usable area, already stripped of any
/// border/padding the caller owns.
///
/// `input_focused` controls the visible cues (cursor, prompt colour,
/// empty-state hint) without changing which keys are handled — that's the
/// caller's job in `input::handle_key_search`. Overlay passes true; the
/// tab passes `app.global_search.tab_input_focused`.
///
/// Writes `app.global_search.last_view_h` so PageUp/PageDown (overlay and
/// tab both bind them) get a correct step size, and registers one
/// `GlobalSearchSelect` hit-test row per visible result.
pub fn render_body(f: &mut Frame, app: &mut App, inner: Rect, input_focused: bool) {
    let th = app.theme;
    if inner.height < 3 {
        return;
    }

    let input_y = inner.y;
    let sep_y = inner.y + 1;
    let list_y = inner.y + 2;
    let has_footer = inner.height >= 5;
    let list_h = if has_footer {
        inner.height.saturating_sub(3)
    } else {
        inner.height.saturating_sub(2)
    };
    let footer_y = inner.y + inner.height.saturating_sub(1);

    app.global_search.last_view_h = list_h;

    // ── Input row ──────────────────────────────────────────────────────
    // Prompt glyph signals focus: accent+bold when input is active, dim
    // when in list mode. Same signal the cursor gives, but visible even on
    // terminals that hide the caret.
    let prompt_style = if input_focused {
        Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(th.fg_secondary)
    };
    let query_style = if input_focused {
        Style::default().fg(th.fg_primary)
    } else {
        Style::default().fg(th.fg_secondary)
    };
    let prompt_spans = vec![
        Span::styled("> ", prompt_style),
        Span::styled(app.global_search.query.clone(), query_style),
    ];
    f.render_widget(
        Line::from(prompt_spans),
        Rect::new(inner.x, input_y, inner.width, 1),
    );
    // Blinking cursor only when the input is focused — hiding it in list
    // mode makes the mode legible even without looking at colours.
    if input_focused {
        let cursor_w =
            UnicodeWidthStr::width(&app.global_search.query[..app.global_search.cursor]) as u16;
        let cursor_x = inner.x + 2 + cursor_w.min(inner.width.saturating_sub(3));
        f.set_cursor_position((cursor_x, input_y));
    } else {
        // Clicking the prompt row while in list mode should focus the
        // input — avoid making users hunt for `/` or `i`. Overlay is
        // always input-focused, so this only registers in the tab case.
        app.hit_registry.register_row(
            inner.x,
            input_y,
            inner.width,
            ClickAction::GlobalSearchFocusInput,
        );
    }

    // ── Separator row ──────────────────────────────────────────────────
    f.render_widget(
        Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            Style::default().fg(th.border),
        )),
        Rect::new(inner.x, sep_y, inner.width, 1),
    );

    // ── List ───────────────────────────────────────────────────────────
    let loading = app.global_search_load.loading;
    if app.global_search.results.is_empty() {
        let msg = if app.global_search.query.is_empty() {
            if input_focused {
                "Type to search file contents…"
            } else {
                "Press / or i to start searching"
            }
        } else if loading {
            "Scanning…"
        } else {
            "No matches"
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
        let sel = app.global_search.selected;
        if sel < app.global_search.scroll {
            app.global_search.scroll = sel;
        } else if list_h > 0 && sel >= app.global_search.scroll + list_h as usize {
            app.global_search.scroll = sel + 1 - list_h as usize;
        }
        let scroll = app.global_search.scroll;

        // Clamp to MAX_H_SCROLL so a fast trackpad flick can't leave the
        // row permanently blank. Write it back so the next render reads
        // the clamped value (mouse handlers don't clamp themselves).
        app.global_search.results_h_scroll = app
            .global_search
            .results_h_scroll
            .min(crate::global_search::MAX_H_SCROLL);
        let h_scroll = app.global_search.results_h_scroll;
        for row in 0..list_h as usize {
            let hit_idx = scroll + row;
            let Some(hit) = app.global_search.results.get(hit_idx) else {
                break;
            };
            let is_sel = hit_idx == sel;
            let y = list_y + row as u16;
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            let hover = crate::ui::hover::is_hover(app, row_area, y);
            let line = build_row_line(hit, is_sel, hover, inner.width, h_scroll, &th);
            f.render_widget(line, row_area);
            app.hit_registry.register_row(
                inner.x,
                y,
                inner.width,
                ClickAction::GlobalSearchSelect(hit_idx),
            );
        }
    }

    // ── Footer ─────────────────────────────────────────────────────────
    if has_footer {
        let text = render_footer_text(&app.global_search, loading);
        let w = UnicodeWidthStr::width(text.as_str()) as u16;
        let fx = inner.x + inner.width.saturating_sub(w);
        f.render_widget(
            Line::from(Span::styled(text, Style::default().fg(th.fg_secondary))),
            Rect::new(fx, footer_y, w, 1),
        );
    }
}

/// Build the right-aligned footer line. Shows the selection cursor (`N /
/// M`), an h-scroll indicator when the list has been panned, a
/// `scanning…` hint while a worker is in flight, and a `1000+ (refine)`
/// tail when the worker truncated. `loading` comes from
/// `App.global_search_load.loading` — passed in so this stays a pure
/// function of the search's public state.
fn render_footer_text(state: &crate::global_search::GlobalSearchState, loading: bool) -> String {
    let cur = if state.results.is_empty() {
        0
    } else {
        state.selected + 1
    };
    let total = state.results.len();
    let mut s = format!("{cur} / {total}");
    if state.results_h_scroll > 0 {
        // `←` signals "content shifted left, Home to reset" — both the
        // direction of the shift and the escape hatch.
        s.push_str(&format!(" · ←{}", state.results_h_scroll));
    }
    if loading {
        s.push_str(" · scanning…");
    }
    if state.truncated {
        s.push_str(&format!(" · {MAX_RESULTS}+ (refine)"));
    }
    s
}

/// Fixed path-column width. Must stay constant per render so whole-row
/// h-scroll behaves predictably across rows — if different rows allocated
/// different path widths, the `│` separator would zig-zag as you scroll.
const PATH_COL_W: usize = 40;

/// Render one result row as a Line whose layout is:
/// `<space> <path:line padded-to-40> <" │ "> <line_text>`
///
/// The row's NATURAL content is built first (no clipping, no smart shift),
/// then the whole thing is sliced by `clip_row_to_viewport` to apply
/// `h_scroll` + `viewport_w`. That makes the scroll uniform: at h_scroll=5
/// every row loses the same 5 cols from the left (starting with the leading
/// pad, then eating into the path), rather than only the line-text column
/// shifting per-row.
///
/// Row-level background is three-tier: `selection_bg` on the chosen row
/// wins, then `hover_bg` for mouse-over, else terminal default. The match
/// highlight's own bg always overrides these — the accent colour is the
/// point of the row.
fn build_row_line(
    hit: &MatchHit,
    is_sel: bool,
    hover: bool,
    viewport_w: u16,
    h_scroll: usize,
    th: &Theme,
) -> Line<'static> {
    let row_bg: Option<Color> = if is_sel {
        Some(th.selection_bg)
    } else if hover {
        Some(th.hover_bg)
    } else {
        None
    };
    let with_row_bg = |s: Style| -> Style {
        match row_bg {
            Some(bg) => s.bg(bg),
            None => s,
        }
    };

    let path_base = with_row_bg(Style::default().fg(th.fg_secondary));
    let text_base = with_row_bg(Style::default().fg(th.fg_primary));
    let sep_style = with_row_bg(Style::default().fg(th.border));
    let pad_style = with_row_bg(Style::default());
    let hl_bg = if is_sel {
        th.search_current
    } else {
        th.search_match
    };
    let hl = Style::default()
        .fg(th.fg_primary)
        .bg(hl_bg)
        .add_modifier(Modifier::BOLD);

    // Build the full natural row as a Span stream. No clipping here.
    let mut full: Vec<Span<'static>> = Vec::new();

    // Leading pad so the selection bar has breathing room.
    full.push(Span::styled(" ".to_string(), pad_style));

    // Path column. Fixed PATH_COL_W so the separator aligns across rows;
    // format_path_column collapses long dirs to `…/basename:line`.
    let path_display = format_path_column(&hit.display, hit.line + 1, PATH_COL_W);
    let path_w = UnicodeWidthStr::width(path_display.as_str());
    full.push(Span::styled(path_display, path_base));
    if path_w < PATH_COL_W {
        full.push(Span::styled(" ".repeat(PATH_COL_W - path_w), pad_style));
    }

    // Separator.
    full.push(Span::styled(" │ ".to_string(), sep_style));

    // Line text — walk char-by-char so we can style the match range.
    // Per-char Spans are fine at this list size; batching is a later opt.
    let has_match = hit.byte_range.start != hit.byte_range.end;
    for (bi, c) in hit.line_text.char_indices() {
        let in_match = has_match && bi >= hit.byte_range.start && bi < hit.byte_range.end;
        let style = if in_match { hl } else { text_base };
        full.push(Span::styled(c.to_string(), style));
    }

    // Slice the whole row by (h_scroll, viewport_w). Trailing right-pad
    // keeps the row bg filled to the right edge.
    let clipped = clip_row_to_viewport(&full, h_scroll, viewport_w as usize, pad_style);
    Line::from(clipped)
}

/// Clip a natural-width Span stream to a horizontal viewport
/// `[h_scroll, h_scroll + max_w)`. Right-pads with `fill_style` so the row
/// background reaches the right edge even when content is shorter than the
/// viewport. Operates on display columns (UnicodeWidthChar), drops any
/// wide-char that straddles the left boundary rather than rendering half a
/// glyph.
fn clip_row_to_viewport(
    spans: &[Span<'static>],
    h_scroll: usize,
    max_w: usize,
    fill_style: Style,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    if max_w == 0 {
        return out;
    }
    let mut col_pos: usize = 0; // position in the natural row
    let mut used_w: usize = 0; // position in the viewport
    'outer: for span in spans {
        for c in span.content.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            // Fully before the viewport — drop.
            if col_pos + cw <= h_scroll {
                col_pos += cw;
                continue;
            }
            // Wide char whose left edge lives in the skip zone: drop (can't
            // render half a CJK glyph without corrupting the grid).
            if col_pos < h_scroll {
                col_pos += cw;
                continue;
            }
            // Past the right edge of the viewport — stop everything.
            if used_w + cw > max_w {
                break 'outer;
            }
            out.push(Span::styled(c.to_string(), span.style));
            used_w += cw;
            col_pos += cw;
        }
    }
    let fill = max_w.saturating_sub(used_w);
    if fill > 0 {
        out.push(Span::styled(" ".repeat(fill), fill_style));
    }
    out
}

/// Pack `display:line` into at most `width` cols. When it doesn't fit we
/// collapse the leading directories into `…/basename:line`, preserving the
/// information the user is most likely to recognise (basename + line #).
fn format_path_column(display: &str, line: usize, width: usize) -> String {
    let full = format!("{display}:{line}");
    let full_w = UnicodeWidthStr::width(full.as_str());
    if full_w <= width {
        return full;
    }
    // Collapse dirs: "deeply/nested/path/foo.rs:42" → "…/foo.rs:42"
    let (_, basename) = display.rsplit_once('/').unwrap_or(("", display));
    let collapsed = format!("…/{basename}:{line}");
    if UnicodeWidthStr::width(collapsed.as_str()) <= width {
        return collapsed;
    }
    // Still too long (long basename, narrow popup). Chop from the front.
    // `full.chars().rev().take(width-1)` then reverse keeps the tail so
    // `:line` stays visible.
    let mut take = width.saturating_sub(1);
    let mut out = String::new();
    let mut it = full.chars().rev();
    while take > 0 {
        if let Some(c) = it.next() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if cw > take {
                break;
            }
            out.insert(0, c);
            take -= cw;
        } else {
            break;
        }
    }
    format!("…{out}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_path_column_fits_full_when_short() {
        let s = format_path_column("src/app.rs", 42, 30);
        assert_eq!(s, "src/app.rs:42");
    }

    #[test]
    fn format_path_column_collapses_long_dir() {
        let s = format_path_column("a/b/c/d/e/file.rs", 100, 15);
        assert!(s.contains("file.rs"));
        assert!(s.contains(":100"));
        assert!(s.starts_with('…'));
    }

    /// Walk the emitted spans back into a plain string — handy for asserting
    /// on visible content without caring about style. Includes the trailing
    /// right-pad for readers that need to assert exact viewport widths;
    /// call `.trim_end()` at the callsite if you don't care.
    fn rendered(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Build a trivial test row: plain spans of ASCII only, one span.
    fn span(text: &str) -> Span<'static> {
        Span::styled(text.to_string(), Style::default())
    }

    #[test]
    fn clip_row_no_scroll_renders_from_start() {
        let row = vec![span("hello world")];
        let out = clip_row_to_viewport(&row, 0, 20, Style::default());
        let joined = rendered(&out);
        // 11 chars of content + 9 cols of fill = 20.
        assert_eq!(joined.chars().count(), 20);
        assert!(joined.starts_with("hello world"));
    }

    #[test]
    fn clip_row_h_scroll_drops_left_cols() {
        let row = vec![span("0123456789abcdef")];
        let out = clip_row_to_viewport(&row, 5, 8, Style::default());
        let joined = rendered(&out);
        // Drop cols 0..5 (chars '0'..'4'), render cols 5..13 ('5'..'c').
        assert_eq!(joined.trim_end(), "56789abc");
    }

    #[test]
    fn clip_row_truncates_right_when_content_longer_than_viewport() {
        let row = vec![span("0123456789abcdef")];
        let out = clip_row_to_viewport(&row, 0, 5, Style::default());
        let joined = rendered(&out);
        // Exactly 5 chars, no leading pad, no ellipsis (plain clip).
        assert_eq!(joined, "01234");
    }

    #[test]
    fn clip_row_fills_to_viewport_when_content_shorter() {
        let row = vec![span("abc")];
        let out = clip_row_to_viewport(&row, 0, 8, Style::default());
        let joined = rendered(&out);
        assert_eq!(joined.chars().count(), 8);
        assert_eq!(&joined[..3], "abc");
    }

    #[test]
    fn clip_row_handles_wide_chars_across_boundary() {
        // 你 is 2 display cols. h_scroll=1 falls INSIDE 你 → drop it.
        let row = vec![span("你b")];
        let out = clip_row_to_viewport(&row, 1, 5, Style::default());
        let joined = rendered(&out);
        // Should NOT show half of 你; show 'b' then fill.
        assert_eq!(joined.trim_end(), "b");
    }

    #[test]
    fn clip_row_walks_multiple_spans() {
        // Two spans ("path" + "text") — ensure we walk into the second.
        let row = vec![span("abc"), span("def"), span("ghi")];
        let out = clip_row_to_viewport(&row, 2, 5, Style::default());
        let joined = rendered(&out);
        // Drop "ab", render "cdefg".
        assert_eq!(joined.trim_end(), "cdefg");
    }

    #[test]
    fn clip_row_zero_viewport_emits_nothing() {
        let row = vec![span("anything")];
        let out = clip_row_to_viewport(&row, 0, 0, Style::default());
        assert!(out.is_empty());
    }
}
