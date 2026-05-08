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
use crate::global_search::{MAX_RESULTS, MatchHit, SearchPanelFocus};
use crate::ui::mouse::ClickAction;
use crate::ui::theme::Theme;

/// Width (in display cols) of the per-row checkbox column when
/// `replace_open` is true: `"[✓] "` / `"[ ] "` are 4 cells. Zero when
/// closed so the layout is byte-identical to today's search-only render.
const CHECKBOX_COL_W: u16 = 4;
/// Width of the find-input prompt (`"> "` / `"▾ "`). The prompt's
/// leading 2 cells double as the replace-toggle hit target on
/// Tab::Search — clicking the arrow expands/collapses the replace row,
/// matching VSCode's chevron-in-input affordance without burning extra
/// columns on a separate icon.
const PROMPT_COL_W: u16 = 2;

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
/// tab passes `app.global_search.input_focused()`.
///
/// Writes `app.global_search.last_view_h` so PageUp/PageDown (overlay and
/// tab both bind them) get a correct step size, and registers one
/// `GlobalSearchSelect` hit-test row per visible result.
pub fn render_body(f: &mut Frame, app: &mut App, inner: Rect, input_focused: bool) {
    let th = app.theme;
    if inner.height < 3 {
        return;
    }

    // Layout budgets shift by ±1 row when the user toggles the Replace
    // input on. `toggle_in_use` gates whether the prompt arrow doubles
    // as a replace-toggle button — overlay is single-input + transient,
    // so its prompt is just a prompt; Tab::Search makes the prompt
    // clickable.
    let replace_open = app.global_search.replace_open;
    let toggle_in_use = !app.global_search.active; // overlay sets `active=true`
    let header_rows: u16 = if replace_open { 2 } else { 1 };

    let find_y = inner.y;
    let replace_y = inner.y + 1;
    let sep_y = inner.y + header_rows;
    let list_y = inner.y + header_rows + 1;
    let has_footer = inner.height >= header_rows + 4;
    let list_h = if has_footer {
        inner.height.saturating_sub(header_rows + 2)
    } else {
        inner.height.saturating_sub(header_rows + 1)
    };
    let footer_y = inner.y + inner.height.saturating_sub(1);

    app.global_search.last_view_h = list_h;

    // ── Find input row ────────────────────────────────────────────────
    // Overlay has only one input row, so any input-focus there is by
    // definition Find. Tab::Search uses the focus enum to disambiguate.
    let find_focused = input_focused
        && (!toggle_in_use || matches!(app.global_search.focus, SearchPanelFocus::FindInput));
    // Prompt arrow doubles as the replace-toggle on Tab::Search:
    // `> ` when collapsed, `▾ ` (U+25BE BLACK DOWN-POINTING SMALL
    // TRIANGLE) when the replace row is showing. Picked from a few
    // candidates:
    //   - `v` (letter) looks like a typo
    //   - `▼` (U+25BC) is too visually heavy next to ASCII `>`
    //   - `⌄` (U+2304) renders as 0-width or a missing-glyph box on
    //     several common terminal fonts (Apple Terminal, older tmux
    //     setups). `▾` is in Geometric Shapes (U+25xx), broadly
    //     supported across monospace fonts.
    // The overlay always shows `> ` because it has no replace mode.
    let prompt_glyph = if toggle_in_use && replace_open {
        "▾ "
    } else {
        "> "
    };
    let prompt_style = if find_focused {
        Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(th.fg_secondary)
    };
    let query_style = if find_focused {
        Style::default().fg(th.fg_primary)
    } else {
        Style::default().fg(th.fg_secondary)
    };
    f.render_widget(
        Line::from(vec![
            Span::styled(prompt_glyph.to_string(), prompt_style),
            Span::styled(app.global_search.query.clone(), query_style),
        ]),
        Rect::new(inner.x, find_y, inner.width, 1),
    );
    if find_focused {
        let cursor_w =
            UnicodeWidthStr::width(&app.global_search.query[..app.global_search.cursor]) as u16;
        let cursor_x = inner.x + PROMPT_COL_W + cursor_w.min(inner.width.saturating_sub(3));
        f.set_cursor_position((cursor_x, find_y));
    }
    // Hit-test order matters: register the narrower toggle target FIRST
    // so its 2-cell zone wins over the row-wide focus zone.
    if toggle_in_use {
        app.hit_registry.register_row(
            inner.x,
            find_y,
            PROMPT_COL_W,
            ClickAction::SearchToggleReplace,
        );
    }
    if !find_focused && toggle_in_use {
        // Click anywhere on the find row past the prompt arrow → focus
        // the find input. Avoids making users hunt for `/` or `i`.
        let body_x = inner.x + PROMPT_COL_W;
        let body_w = inner.width.saturating_sub(PROMPT_COL_W);
        app.hit_registry
            .register_row(body_x, find_y, body_w, ClickAction::GlobalSearchFocusInput);
    }

    // ── Replace input row ─────────────────────────────────────────────
    if replace_open {
        let replace_focused = matches!(app.global_search.focus, SearchPanelFocus::ReplaceInput);
        let r_prompt_style = if replace_focused {
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.fg_secondary)
        };
        let r_query_style = if replace_focused {
            Style::default().fg(th.fg_primary)
        } else {
            Style::default().fg(th.fg_secondary)
        };
        let placeholder = crate::i18n::t(crate::i18n::Msg::ReplaceWithPlaceholder);
        let body: Span<'static> = if app.global_search.replace_text.is_empty() && !replace_focused {
            Span::styled(
                placeholder.to_string(),
                Style::default()
                    .fg(th.fg_secondary)
                    .add_modifier(Modifier::ITALIC),
            )
        } else {
            Span::styled(app.global_search.replace_text.clone(), r_query_style)
        };
        f.render_widget(
            Line::from(vec![Span::styled("↪ ", r_prompt_style), body]),
            Rect::new(inner.x, replace_y, inner.width, 1),
        );
        if replace_focused {
            let cursor_w = UnicodeWidthStr::width(
                &app.global_search.replace_text[..app.global_search.replace_cursor],
            ) as u16;
            let cursor_x = inner.x + PROMPT_COL_W + cursor_w.min(inner.width.saturating_sub(3));
            f.set_cursor_position((cursor_x, replace_y));
        } else {
            app.hit_registry.register_row(
                inner.x,
                replace_y,
                inner.width,
                ClickAction::GlobalSearchFocusReplaceInput,
            );
        }
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
            let included = app.global_search.is_match_included(hit_idx);
            let y = list_y + row as u16;
            let row_area = Rect::new(inner.x, y, inner.width, 1);
            let hover = crate::ui::hover::is_hover(app, row_area, y);

            // Checkbox column (only in replace mode). Sits at the left
            // edge of the row, before the leading-pad space the natural
            // row layout already includes. Click toggles the per-match
            // exclusion; the rest of the row keeps its existing
            // "select hit" hit-test.
            let checkbox_w = if replace_open { CHECKBOX_COL_W } else { 0 };
            if replace_open {
                let glyph = if included { "[✓] " } else { "[ ] " };
                let style = if included {
                    Style::default().fg(th.accent)
                } else {
                    Style::default().fg(th.fg_secondary)
                };
                f.render_widget(
                    Line::from(Span::styled(glyph.to_string(), style)),
                    Rect::new(inner.x, y, checkbox_w, 1),
                );
                app.hit_registry.register_row(
                    inner.x,
                    y,
                    checkbox_w,
                    ClickAction::SearchToggleMatch(hit_idx),
                );
            }

            let body_x = inner.x + checkbox_w;
            let body_w = inner.width.saturating_sub(checkbox_w);
            let line = build_row_line(hit, is_sel, hover, body_w, h_scroll, included, &th);
            f.render_widget(line, Rect::new(body_x, y, body_w, 1));
            app.hit_registry.register_row(
                body_x,
                y,
                body_w,
                ClickAction::GlobalSearchSelect(hit_idx),
            );
        }
    }

    // ── Footer ─────────────────────────────────────────────────────────
    if has_footer {
        let base_text = render_footer_text(&app.global_search, loading);

        if replace_open {
            // Replace footer adds a count + clickable Apply button. Lay
            // it out as: `<base>  ·  N <suffix>  ·  [Apply]`. The Apply
            // span gets its own hit-test so the rest of the footer is
            // inert.
            let included_count = app.global_search.included_count();
            let suffix = crate::i18n::t(crate::i18n::Msg::ReplaceCountSuffix);
            let count_text = format!(" · {included_count} {suffix} · ");
            let in_flight = app.replace_load.loading;
            let apply_label = if in_flight {
                let hint = crate::i18n::t(crate::i18n::Msg::ReplacingHint);
                match app.global_search.replace_progress {
                    Some((done, total)) if total > 0 => format!(" {hint} {done}/{total} "),
                    _ => format!(" {hint} "),
                }
            } else {
                format!(" {} ", crate::i18n::t(crate::i18n::Msg::ApplyReplace))
            };
            let count_w = UnicodeWidthStr::width(count_text.as_str()) as u16;
            let apply_w = UnicodeWidthStr::width(apply_label.as_str()) as u16;
            let base_w = UnicodeWidthStr::width(base_text.as_str()) as u16;
            let total_w = base_w + count_w + apply_w;
            let fx = inner.x + inner.width.saturating_sub(total_w);

            let base_span = Span::styled(base_text, Style::default().fg(th.fg_secondary));
            let count_span = Span::styled(count_text, Style::default().fg(th.fg_secondary));
            let apply_can_fire = !in_flight && included_count > 0;
            let apply_style = if apply_can_fire {
                Style::default()
                    .fg(th.chrome_active_fg)
                    .bg(th.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(th.fg_secondary)
                    .add_modifier(Modifier::DIM)
            };
            let apply_span = Span::styled(apply_label, apply_style);

            f.render_widget(
                Line::from(vec![base_span, count_span, apply_span]),
                Rect::new(fx, footer_y, total_w, 1),
            );
            if apply_can_fire {
                app.hit_registry.register_row(
                    fx + base_w + count_w,
                    footer_y,
                    apply_w,
                    ClickAction::SearchApplyReplace,
                );
            }
        } else {
            let w = UnicodeWidthStr::width(base_text.as_str()) as u16;
            let fx = inner.x + inner.width.saturating_sub(w);
            f.render_widget(
                Line::from(Span::styled(
                    base_text,
                    Style::default().fg(th.fg_secondary),
                )),
                Rect::new(fx, footer_y, w, 1),
            );
        }
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
    included: bool,
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

    let dim = !included;
    let mut path_base = with_row_bg(Style::default().fg(th.fg_secondary));
    let mut text_base = with_row_bg(Style::default().fg(th.fg_primary));
    let mut sep_style = with_row_bg(Style::default().fg(th.border));
    let pad_style = with_row_bg(Style::default());
    let hl_bg = if is_sel {
        th.search_current
    } else {
        th.search_match
    };
    let mut hl = Style::default()
        .fg(th.fg_primary)
        .bg(hl_bg)
        .add_modifier(Modifier::BOLD);
    if dim {
        // Visually mark excluded rows: strikethrough + dim foreground so
        // the user can scan a result list and immediately see which
        // matches Apply will skip. The match-highlight bg stays so the
        // pattern is still readable.
        let strike = Modifier::CROSSED_OUT;
        path_base = path_base.add_modifier(strike).fg(th.fg_secondary);
        text_base = text_base.add_modifier(strike).fg(th.fg_secondary);
        sep_style = sep_style.add_modifier(strike);
        hl = hl.add_modifier(strike);
    }

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
