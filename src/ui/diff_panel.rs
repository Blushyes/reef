use crate::app::{App, DiffHighlighted, DiffLayout, DiffMode, LineTokens};
use crate::git::{DiffContent, DiffHunk, LineTag};
use crate::i18n::{Msg, t};
use crate::search::{SearchState, SearchTarget};
use crate::ui::highlight::StyledToken;
use crate::ui::selection::{DiffHit, DiffRowText, DiffSelection, DiffSide};
use crate::ui::text::{
    clip_spans, overlay_match_highlight, overlay_selection_highlight, truncate_to_width,
};
use crate::ui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

/// Scroll + viewport state held by whichever layer owns a diff panel
/// instance. Git tab stores these at App top level; Graph tab's 3-col
/// layout will use its own copy inside `commit_detail`. Passed in by
/// mutable ref so the render's clamping reaches back through to the
/// owner, and the next frame sees the pinned values.
pub struct DiffView<'a> {
    pub scroll: &'a mut usize,
    pub h_scroll: &'a mut usize,
    pub sbs_left_h_scroll: &'a mut usize,
    pub sbs_right_h_scroll: &'a mut usize,
    pub last_view_h: &'a mut u16,
}

/// Git-tab entry point. Thin wrapper around `render_diff` that sources
/// everything from `App`. Graph tab will call `render_diff` directly
/// with its own scroll fields once the 3-column layout lands.
pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Cache the panel rect for the mouse-selection handler's point-in-rect
    // gate. Both Git-tab Diff and Graph-tab 3-col Diff call `render_diff`,
    // but only Git-tab enters through this wrapper; the Graph wrapper in
    // `ui::mod.rs` caches its own rect the same way. One cache slot serves
    // both since at most one tab renders Diff at a time.
    app.last_diff_rect = Some(inner);

    let Some(d) = app.diff_content.take() else {
        render_empty(f, inner, &app.theme);
        return;
    };
    let selection = app.diff_selection;
    render_diff(
        f,
        inner,
        &d.diff,
        d.highlighted.as_ref(),
        app.diff_layout,
        app.diff_mode,
        app.theme,
        &app.search,
        SearchTarget::Diff,
        selection.as_ref(),
        &mut DiffView {
            scroll: &mut app.diff_scroll,
            h_scroll: &mut app.diff_h_scroll,
            sbs_left_h_scroll: &mut app.sbs_left_h_scroll,
            sbs_right_h_scroll: &mut app.sbs_right_h_scroll,
            last_view_h: &mut app.last_diff_view_h,
        },
        &mut app.last_diff_hit,
    );
    app.diff_content = Some(d);
}

/// Pure diff renderer — no `App` dependency. Callers own the scroll state
/// and pass search + theme by reference. Both Git tab (via `render`) and
/// Graph tab's future 3-col right column use this.
///
/// `search_target` decides which of the caller's `/` matches the renderer
/// will overlay on the content; pass `SearchTarget::Diff` for the Git tab
/// and `SearchTarget::GraphDiff` once that variant lands.
#[allow(clippy::too_many_arguments)]
pub fn render_diff(
    f: &mut Frame,
    area: Rect,
    diff: &DiffContent,
    highlighted: Option<&DiffHighlighted>,
    layout: DiffLayout,
    mode: DiffMode,
    theme: Theme,
    search: &SearchState,
    search_target: SearchTarget,
    selection: Option<&DiffSelection>,
    view: &mut DiffView<'_>,
    hit_slot: &mut Option<DiffHit>,
) {
    match layout {
        DiffLayout::Unified => render_unified(
            f,
            area,
            diff,
            highlighted,
            mode,
            theme,
            search,
            search_target,
            selection,
            view,
            hit_slot,
        ),
        DiffLayout::SideBySide => render_side_by_side(
            f,
            area,
            diff,
            highlighted,
            mode,
            theme,
            search,
            search_target,
            selection,
            view,
            hit_slot,
        ),
    }
}

fn render_empty(f: &mut Frame, area: Rect, theme: &Theme) {
    if area.height < 1 {
        return;
    }
    let msg = Line::from(Span::styled(
        t(Msg::DiffEmpty),
        Style::default().fg(theme.fg_secondary),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(22) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

// ─── Unified view ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_unified(
    f: &mut Frame,
    area: Rect,
    diff: &DiffContent,
    highlighted: Option<&DiffHighlighted>,
    mode: DiffMode,
    theme: Theme,
    search: &SearchState,
    search_target: SearchTarget,
    selection: Option<&DiffSelection>,
    view: &mut DiffView<'_>,
    hit_slot: &mut Option<DiffHit>,
) {
    let mut y = area.y;
    let max_y = area.y + area.height;

    render_file_header(f, area, diff, DiffLayout::Unified, mode, &theme, &mut y, max_y);

    // Build all display lines. Content rows pick up per-line syntect tokens
    // when available so the render path can emit syntax-colored spans
    // instead of a single plain-fg span.
    let mut all_lines: Vec<UnifiedLine> = Vec::new();
    for (hi, hunk) in diff.hunks.iter().enumerate() {
        if hi > 0 {
            all_lines.push(UnifiedLine::Separator);
        }
        all_lines.push(UnifiedLine::HunkHeader(hunk.header.clone()));
        let hunk_tokens = highlighted.and_then(|h| h.get(hi));
        for (li, line) in hunk.lines.iter().enumerate() {
            all_lines.push(UnifiedLine::Content {
                tag: line.tag,
                old_lineno: line.old_lineno,
                new_lineno: line.new_lineno,
                text: line.content.clone(),
                tokens: hunk_tokens.and_then(|t| t.get(li)).cloned(),
            });
        }
    }

    // ` XXXXX  XXXXX ` (14 cols line-number gutter) + `+ ` (2 cols prefix)
    // = 16 cols before body. Constants here match the span math in
    // `render_unified_line`; update both together if the gutter layout
    // changes.
    const GUTTER_AND_PREFIX: usize = 16;
    let gutter_width = 15usize; // legacy clamp target (body starts 1 col earlier than math says — preserved)
    let content_w = (area.width as usize).saturating_sub(gutter_width);
    let visible_rows = max_y.saturating_sub(y) as usize;
    // Remember viewport height for search-jump centering.
    *view.last_view_h = visible_rows as u16;

    // Clamp vertical scroll so we can't scroll past the last displayable row.
    // Must come before the horizontal max-width calc below, which itself
    // uses `skip(*view.scroll)` and would see an empty slice if we let
    // the offset run past the end.
    let max_scroll = all_lines.len().saturating_sub(visible_rows);
    *view.scroll = (*view.scroll).min(max_scroll);

    // Clamp horizontal scroll against the widest Content line currently in view.
    let max_visible_w: usize = all_lines
        .iter()
        .skip(*view.scroll)
        .take(visible_rows)
        .filter_map(|dl| match dl {
            UnifiedLine::Content { text, .. } => Some(UnicodeWidthStr::width(text.as_str())),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let max_h = max_visible_w.saturating_sub(content_w);
    *view.h_scroll = (*view.h_scroll).min(max_h);
    let h = *view.h_scroll;
    let content_y = y;

    // Snapshot rows + geometry so the mouse-selection handler can translate
    // a terminal `(col, row)` hit into `(side, row_idx, byte_offset)` without
    // touching the diff data structure. Rebuilt every frame.
    let diff_rows: Vec<DiffRowText> = all_lines
        .iter()
        .map(|dl| match dl {
            UnifiedLine::Separator => DiffRowText::Separator,
            UnifiedLine::HunkHeader(h) => DiffRowText::Header(h.clone()),
            UnifiedLine::Content { text, .. } => DiffRowText::Unified(text.clone()),
        })
        .collect();
    *hit_slot = Some(DiffHit {
        layout: DiffLayout::Unified,
        content_y,
        content_x_unified: area.x.saturating_add(GUTTER_AND_PREFIX as u16),
        content_x_left: 0,
        content_x_right: 0,
        right_start_x: 0,
        scroll: *view.scroll,
        h_scroll: h,
        sbs_left_h_scroll: 0,
        sbs_right_h_scroll: 0,
        rows: diff_rows,
    });

    let scroll = *view.scroll;
    for (offset, dl) in all_lines.iter().skip(scroll).enumerate() {
        if y >= max_y {
            break;
        }
        let row_idx = scroll + offset;
        // `collect_rows(Diff)` in `search.rs` uses the exact same
        // `UnifiedLine` order, so row_idx matches 1:1 with match row indices.
        let (ranges, cur) = search.ranges_on_row(search_target, row_idx);
        let sel_range = selection
            .filter(|s| s.side == DiffSide::Unified)
            .and_then(|s| {
                let txt = match dl {
                    UnifiedLine::Separator => "",
                    UnifiedLine::HunkHeader(h) => h.as_str(),
                    UnifiedLine::Content { text, .. } => text.as_str(),
                };
                s.sel.line_byte_range(row_idx, txt)
            });
        render_unified_line(f, area, y, dl, h, &theme, &ranges, cur, sel_range);
        y += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_unified_line(
    f: &mut Frame,
    area: Rect,
    y: u16,
    dl: &UnifiedLine,
    h_scroll: usize,
    theme: &Theme,
    match_ranges: &[Range<usize>],
    current_range: Option<Range<usize>>,
    selection_range: Option<Range<usize>>,
) {
    match dl {
        UnifiedLine::Separator => {
            let line = Line::from(Span::styled(
                format!(" {:>5}  {:>5}  ⋯", "", ""),
                Style::default().fg(theme.fg_secondary),
            ));
            f.render_widget(line, Rect::new(area.x, y, area.width, 1));
        }
        UnifiedLine::HunkHeader(header) => {
            // Hunk headers can match too (e.g. search `@@` or a function name
            // that shows up in the hunk context). Apply overlay to the header
            // text after the leading space.
            let base_style = Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::DIM);
            let prefix = Span::styled(" ", base_style);
            let header_tokens = vec![(base_style, header.clone())];
            let tokens = if match_ranges.is_empty() {
                header_tokens
            } else {
                overlay_match_highlight(
                    header_tokens,
                    match_ranges,
                    current_range,
                    theme.search_match,
                    theme.search_current,
                )
            };
            let tokens = match selection_range {
                Some(r) if r.start < r.end => overlay_selection_highlight(tokens, r),
                _ => tokens,
            };
            let mut spans = vec![prefix];
            for (style, text) in tokens {
                spans.push(Span::styled(text, style));
            }
            let line = Line::from(spans);
            f.render_widget(line, Rect::new(area.x, y, area.width, 1));
        }
        UnifiedLine::Content {
            tag,
            old_lineno,
            new_lineno,
            text,
            tokens: syntax_tokens,
        } => {
            let (prefix, fg, bg) = line_style(*tag, theme);
            let old_num = fmt_lineno(*old_lineno);
            let new_num = fmt_lineno(*new_lineno);

            let gutter_width = 15usize; // " XXXXX  XXXXX  "
            let max_text = (area.width as usize).saturating_sub(gutter_width);

            // Overlay search matches before horizontal-clipping so ranges map
            // onto unshifted text. `clip_spans` then handles h_scroll without
            // caring that the token stream was split.
            let body_style = Style::default().fg(fg).bg(bg);
            let base_tokens: Vec<StyledToken> = match syntax_tokens {
                Some(toks) if !toks.is_empty() => {
                    toks.iter().map(|(s, t)| (s.bg(bg), t.clone())).collect()
                }
                _ => vec![(body_style, text.clone())],
            };
            let tokens = if match_ranges.is_empty() {
                base_tokens
            } else {
                overlay_match_highlight(
                    base_tokens,
                    match_ranges,
                    current_range,
                    theme.search_match,
                    theme.search_current,
                )
            };
            // Selection overlay stacks on top of search overlay — both use
            // `overlay_selection_highlight`'s REVERSED trick so the colors
            // compose cleanly with the per-tag background.
            let tokens = match selection_range {
                Some(r) if r.start < r.end => overlay_selection_highlight(tokens, r),
                _ => tokens,
            };
            let body_spans = clip_spans(&tokens, h_scroll, max_text);
            let body_w: usize = body_spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = max_text.saturating_sub(body_w);

            let mut spans = vec![
                Span::styled(
                    format!(" {}  {} ", old_num, new_num),
                    Style::default().fg(theme.fg_secondary).bg(bg),
                ),
                Span::styled(format!("{} ", prefix), Style::default().fg(fg).bg(bg)),
            ];
            spans.extend(body_spans);
            spans.push(Span::styled(
                " ".repeat(pad.min(area.width as usize)),
                Style::default().bg(bg),
            ));
            let line = Line::from(spans);
            f.render_widget(line, Rect::new(area.x, y, area.width, 1));
        }
    }
}

enum UnifiedLine {
    Separator,
    HunkHeader(String),
    Content {
        tag: LineTag,
        old_lineno: Option<u32>,
        new_lineno: Option<u32>,
        text: String,
        /// Syntect-colored tokens for this line (when a syntax resolved).
        /// Concatenating token texts yields `text`; render path overlays
        /// the row's bg on each token. `Arc` so per-hunk `tokens_for` pass
        /// is O(1) per line.
        tokens: Option<LineTokens>,
    },
}

// ─── Side-by-side view ───────────────────────────────────────────────────────

struct SbsRow {
    left_tag: LineTag,
    left_no: Option<u32>,
    left_text: String,
    /// Syntect tokens for `left_text` (when a syntax resolved); concatenating
    /// token texts yields `left_text`. `None` falls back to a single plain-fg
    /// span at render time. `Arc` so pairing / render clones are O(1).
    left_tokens: Option<LineTokens>,
    right_tag: LineTag,
    right_no: Option<u32>,
    right_text: String,
    right_tokens: Option<LineTokens>,
}

enum SbsDisplayLine {
    Separator,
    HunkHeader(String),
    Row(SbsRow),
}

fn build_sbs_lines(hunk: &DiffHunk, hunk_tokens: Option<&Vec<LineTokens>>) -> Vec<SbsDisplayLine> {
    let mut rows: Vec<SbsDisplayLine> = Vec::new();
    // Carry tokens alongside each pending removal so a later Added pairing
    // keeps the left half's syntax highlighting.
    let mut pending_removed: Vec<(Option<u32>, String, Option<LineTokens>)> = Vec::new();
    let tokens_for =
        |li: usize| -> Option<LineTokens> { hunk_tokens.and_then(|t| t.get(li)).cloned() };

    rows.push(SbsDisplayLine::HunkHeader(hunk.header.clone()));

    for (li, line) in hunk.lines.iter().enumerate() {
        match line.tag {
            LineTag::Removed => {
                pending_removed.push((line.old_lineno, line.content.clone(), tokens_for(li)));
            }
            LineTag::Added => {
                let added_tokens = tokens_for(li);
                if let Some((old_no, old_text, old_tokens)) = pending_removed.first().cloned() {
                    pending_removed.remove(0);
                    rows.push(SbsDisplayLine::Row(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        left_tokens: old_tokens,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                        right_tokens: added_tokens,
                    }));
                } else {
                    // Added with no paired removal
                    rows.push(SbsDisplayLine::Row(SbsRow {
                        left_tag: LineTag::Context,
                        left_no: None,
                        left_text: String::new(),
                        left_tokens: None,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                        right_tokens: added_tokens,
                    }));
                }
            }
            LineTag::Context => {
                // Flush pending removed (no matching additions)
                for (old_no, old_text, old_tokens) in pending_removed.drain(..) {
                    rows.push(SbsDisplayLine::Row(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        left_tokens: old_tokens,
                        right_tag: LineTag::Context,
                        right_no: None,
                        right_text: String::new(),
                        right_tokens: None,
                    }));
                }
                // Context appears on both sides
                let ctx_tokens = tokens_for(li);
                rows.push(SbsDisplayLine::Row(SbsRow {
                    left_tag: LineTag::Context,
                    left_no: line.old_lineno,
                    left_text: line.content.clone(),
                    left_tokens: ctx_tokens.clone(),
                    right_tag: LineTag::Context,
                    right_no: line.new_lineno,
                    right_text: line.content.clone(),
                    right_tokens: ctx_tokens,
                }));
            }
        }
    }

    // Flush any remaining pending removed
    for (old_no, old_text, old_tokens) in pending_removed.drain(..) {
        rows.push(SbsDisplayLine::Row(SbsRow {
            left_tag: LineTag::Removed,
            left_no: old_no,
            left_text: old_text,
            left_tokens: old_tokens,
            right_tag: LineTag::Context,
            right_no: None,
            right_text: String::new(),
            right_tokens: None,
        }));
    }

    rows
}

#[allow(clippy::too_many_arguments)]
fn render_side_by_side(
    f: &mut Frame,
    area: Rect,
    diff: &DiffContent,
    highlighted: Option<&DiffHighlighted>,
    mode: DiffMode,
    theme: Theme,
    search: &SearchState,
    _search_target: SearchTarget,
    selection: Option<&DiffSelection>,
    view: &mut DiffView<'_>,
    hit_slot: &mut Option<DiffHit>,
) {
    let mut y = area.y;
    let max_y = area.y + area.height;

    render_file_header(
        f,
        area,
        diff,
        DiffLayout::SideBySide,
        mode,
        &theme,
        &mut y,
        max_y,
    );
    // TODO(search-sbs): SBS layout doesn't overlay `/` match highlights
    // on its rows — only Unified does. Row index inside `build_sbs_lines`
    // diverges from `unified_display_rows` (it pairs Removed/Added into
    // single rows), so wiring search in needs a parallel row collector
    // before the renderer can light up matches. `search` is threaded
    // through to keep the signature honest once that lands.
    let _ = search;

    // Build all display lines
    let mut all_lines: Vec<SbsDisplayLine> = Vec::new();
    for (hi, hunk) in diff.hunks.iter().enumerate() {
        if hi > 0 {
            all_lines.push(SbsDisplayLine::Separator);
        }
        let hunk_tokens = highlighted.and_then(|h| h.get(hi));
        all_lines.extend(build_sbs_lines(hunk, hunk_tokens));
    }

    // Half width: leave 1 col for center divider
    let half_w = (area.width.saturating_sub(1)) / 2;
    let right_w = area.width.saturating_sub(half_w + 1);

    // Gutter: " XXXXX " = 7 cols per side
    let gutter = 7usize;
    let left_content_w = (half_w as usize).saturating_sub(gutter);
    let right_content_w = (right_w as usize).saturating_sub(gutter);
    let visible_rows = max_y.saturating_sub(y) as usize;
    *view.last_view_h = visible_rows as u16;

    // Clamp vertical scroll so we can't scroll past the last displayable row.
    let max_scroll = all_lines.len().saturating_sub(visible_rows);
    *view.scroll = (*view.scroll).min(max_scroll);

    // Clamp each side's horizontal scroll against that side's widest text
    // in view. Using a shared scroll would force both halves to pan in
    // lockstep — rarely what you want when the two versions have very
    // different line widths (think rename / large rewrite).
    let max_left_w: usize = all_lines
        .iter()
        .skip(*view.scroll)
        .take(visible_rows)
        .filter_map(|dl| match dl {
            SbsDisplayLine::Row(row) => Some(UnicodeWidthStr::width(row.left_text.as_str())),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let max_right_w: usize = all_lines
        .iter()
        .skip(*view.scroll)
        .take(visible_rows)
        .filter_map(|dl| match dl {
            SbsDisplayLine::Row(row) => Some(UnicodeWidthStr::width(row.right_text.as_str())),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let max_h_left = max_left_w.saturating_sub(left_content_w);
    let max_h_right = max_right_w.saturating_sub(right_content_w);
    *view.sbs_left_h_scroll = (*view.sbs_left_h_scroll).min(max_h_left);
    *view.sbs_right_h_scroll = (*view.sbs_right_h_scroll).min(max_h_right);
    let h_left = *view.sbs_left_h_scroll;
    let h_right = *view.sbs_right_h_scroll;
    let content_y = y;

    // Snapshot rows + geometry for the selection handler. Each SBS row
    // carries both halves so `DiffRowText::text_for(side)` can pick the
    // right one at copy time. Separator + HunkHeader rows don't have a
    // per-side split — the mouse handler treats them as neutral.
    let diff_rows: Vec<DiffRowText> = all_lines
        .iter()
        .map(|dl| match dl {
            SbsDisplayLine::Separator => DiffRowText::Separator,
            SbsDisplayLine::HunkHeader(h) => DiffRowText::Header(h.clone()),
            SbsDisplayLine::Row(r) => DiffRowText::Sbs {
                left: r.left_text.clone(),
                right: r.right_text.clone(),
            },
        })
        .collect();
    let gutter = 7u16;
    *hit_slot = Some(DiffHit {
        layout: DiffLayout::SideBySide,
        content_y,
        content_x_unified: 0,
        content_x_left: area.x.saturating_add(gutter),
        // Right half starts 1 col after the divider; content starts another
        // gutter in. `right_start_x` marks the divider/right-half boundary
        // for `DiffHit::side_for_column`.
        right_start_x: area.x.saturating_add(half_w + 1),
        content_x_right: area.x.saturating_add(half_w + 1 + gutter),
        scroll: *view.scroll,
        h_scroll: 0,
        sbs_left_h_scroll: h_left,
        sbs_right_h_scroll: h_right,
        rows: diff_rows,
    });

    let scroll = *view.scroll;
    for (offset, dl) in all_lines.iter().skip(scroll).enumerate() {
        if y >= max_y {
            break;
        }
        let row_idx = scroll + offset;
        // Per-half selection range: only light up the half that owns the
        // selection's anchor side; the other half renders untouched.
        let (sel_left, sel_right) = match selection {
            Some(s) => match s.side {
                DiffSide::SbsLeft => {
                    if let SbsDisplayLine::Row(row) = dl {
                        (
                            s.sel.line_byte_range(row_idx, row.left_text.as_str()),
                            None,
                        )
                    } else {
                        (None, None)
                    }
                }
                DiffSide::SbsRight => {
                    if let SbsDisplayLine::Row(row) = dl {
                        (
                            None,
                            s.sel.line_byte_range(row_idx, row.right_text.as_str()),
                        )
                    } else {
                        (None, None)
                    }
                }
                DiffSide::Unified => (None, None),
            },
            None => (None, None),
        };
        match dl {
            SbsDisplayLine::Separator => {
                let line = Line::from(Span::styled(
                    format!(
                        " {:>5}  ⋯{}",
                        "",
                        " ".repeat(area.width.saturating_sub(10) as usize)
                    ),
                    Style::default().fg(theme.fg_secondary),
                ));
                f.render_widget(line, Rect::new(area.x, y, area.width, 1));
            }
            SbsDisplayLine::HunkHeader(header) => {
                let line = Line::from(Span::styled(
                    format!(" {}", header),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::DIM),
                ));
                f.render_widget(line, Rect::new(area.x, y, area.width, 1));
            }
            SbsDisplayLine::Row(row) => {
                render_sbs_row(
                    f, area, y, row, half_w, right_w, h_left, h_right, &theme, sel_left, sel_right,
                );
            }
        }
        y += 1;
    }
}

#[allow(clippy::too_many_arguments)]
fn render_sbs_row(
    f: &mut Frame,
    area: Rect,
    y: u16,
    row: &SbsRow,
    half_w: u16,
    right_w: u16,
    h_scroll_left: usize,
    h_scroll_right: usize,
    theme: &Theme,
    sel_left: Option<Range<usize>>,
    sel_right: Option<Range<usize>>,
) {
    // Gutter: " XXXXX " = 7 cols
    let gutter = 7usize;

    // ── Left half ──
    let left_content_w = (half_w as usize).saturating_sub(gutter);
    let (_, left_fg, left_bg) = line_style(row.left_tag, theme);
    let left_no = fmt_lineno(row.left_no);
    let left_body =
        build_sbs_body_tokens(&row.left_text, row.left_tokens.as_ref(), left_fg, left_bg);
    let left_body = match sel_left {
        Some(r) if r.start < r.end => overlay_selection_highlight(left_body, r),
        _ => left_body,
    };
    let left_spans = clip_spans(&left_body, h_scroll_left, left_content_w);
    let left_used: usize = left_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let left_pad = left_content_w.saturating_sub(left_used);

    let mut left_line_spans = vec![Span::styled(
        format!(" {} ", left_no),
        Style::default().fg(theme.fg_secondary).bg(left_bg),
    )];
    left_line_spans.extend(left_spans);
    left_line_spans.push(Span::styled(
        " ".repeat(left_pad),
        Style::default().bg(left_bg),
    ));
    f.render_widget(Line::from(left_line_spans), Rect::new(area.x, y, half_w, 1));

    // ── Divider ──
    let div_x = area.x + half_w;
    let div_line = Line::from(Span::styled("│", Style::default().fg(theme.fg_secondary)));
    f.render_widget(div_line, Rect::new(div_x, y, 1, 1));

    // ── Right half ──
    let right_content_w = (right_w as usize).saturating_sub(gutter);
    let (_, right_fg, right_bg) = line_style(row.right_tag, theme);
    let right_no = fmt_lineno(row.right_no);
    let right_body = build_sbs_body_tokens(
        &row.right_text,
        row.right_tokens.as_ref(),
        right_fg,
        right_bg,
    );
    let right_body = match sel_right {
        Some(r) if r.start < r.end => overlay_selection_highlight(right_body, r),
        _ => right_body,
    };
    let right_spans = clip_spans(&right_body, h_scroll_right, right_content_w);
    let right_used: usize = right_spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_pad = right_content_w.saturating_sub(right_used);

    let mut right_line_spans = vec![Span::styled(
        format!(" {} ", right_no),
        Style::default().fg(theme.fg_secondary).bg(right_bg),
    )];
    right_line_spans.extend(right_spans);
    right_line_spans.push(Span::styled(
        " ".repeat(right_pad),
        Style::default().bg(right_bg),
    ));
    f.render_widget(
        Line::from(right_line_spans),
        Rect::new(div_x + 1, y, right_w, 1),
    );
}

/// Pick the token stream for one SBS body: syntect tokens (bg overlaid per-token)
/// when available, else a single plain-fg span with bg. `Arc` lets the caller
/// pass a per-line handle it got cheaply from the hunk token table.
fn build_sbs_body_tokens(
    text: &str,
    tokens: Option<&LineTokens>,
    fg: Color,
    bg: Color,
) -> Vec<StyledToken> {
    match tokens {
        Some(toks) if !toks.is_empty() => toks.iter().map(|(s, t)| (s.bg(bg), t.clone())).collect(),
        _ => vec![(Style::default().fg(fg).bg(bg), text.to_string())],
    }
}

// ─── Shared helpers ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_file_header(
    f: &mut Frame,
    area: Rect,
    diff: &DiffContent,
    layout: DiffLayout,
    mode: DiffMode,
    theme: &Theme,
    y: &mut u16,
    max_y: u16,
) {
    if *y >= max_y {
        return;
    }

    let layout_label = match layout {
        DiffLayout::Unified => t(Msg::LayoutUnified),
        DiffLayout::SideBySide => t(Msg::LayoutSideBySide),
    };
    let mode_label = match mode {
        DiffMode::Compact => t(Msg::ModeCompact),
        DiffMode::FullFile => t(Msg::ModeFullFile),
    };

    let tag_str = crate::i18n::diff_mode_hint(layout_label, mode_label);
    let tag_len = UnicodeWidthStr::width(tag_str.as_str()) as u16;
    let path_max = area.width.saturating_sub(tag_len) as usize;
    let path_display = truncate_to_width(&diff.file_path, path_max);

    let header = Line::from(vec![
        Span::styled(
            path_display.to_string(),
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(tag_str, Style::default().fg(theme.fg_secondary)),
    ]);
    f.render_widget(header, Rect::new(area.x, *y, area.width, 1));
    *y += 1;

    // Separator
    if *y >= max_y {
        return;
    }
    let sep = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(theme.fg_secondary),
    ));
    f.render_widget(sep, Rect::new(area.x, *y, area.width, 1));
    *y += 1;
}

fn line_style(tag: LineTag, theme: &Theme) -> (&'static str, Color, Color) {
    match tag {
        LineTag::Added => ("+", theme.added_accent, theme.added_bg),
        LineTag::Removed => ("-", theme.removed_accent, theme.removed_bg),
        LineTag::Context => (" ", theme.fg_primary, Color::Reset),
    }
}

fn fmt_lineno(n: Option<u32>) -> String {
    n.map(|v| format!("{:>5}", v))
        .unwrap_or_else(|| "     ".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DiffHunk, DiffLine, LineTag};
    use std::sync::Arc;

    fn make_line(
        tag: LineTag,
        content: &str,
        old_no: Option<u32>,
        new_no: Option<u32>,
    ) -> DiffLine {
        DiffLine {
            tag,
            content: content.to_string(),
            old_lineno: old_no,
            new_lineno: new_no,
        }
    }

    fn make_hunk(header: &str, lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            header: header.to_string(),
            lines,
        }
    }

    fn count_rows(v: &[SbsDisplayLine]) -> usize {
        v.iter()
            .filter(|l| matches!(l, SbsDisplayLine::Row(_)))
            .count()
    }

    fn get_rows(v: &[SbsDisplayLine]) -> Vec<&SbsRow> {
        v.iter()
            .filter_map(|l| {
                if let SbsDisplayLine::Row(r) = l {
                    Some(r)
                } else {
                    None
                }
            })
            .collect()
    }

    // ── line_style ───────────────────────────────────────────────────────────

    #[test]
    fn line_style_added() {
        let theme = Theme::dark();
        let (prefix, fg, _bg) = line_style(LineTag::Added, &theme);
        assert_eq!(prefix, "+");
        assert_eq!(fg, Color::Green);
    }

    #[test]
    fn line_style_removed() {
        let theme = Theme::dark();
        let (prefix, fg, _bg) = line_style(LineTag::Removed, &theme);
        assert_eq!(prefix, "-");
        assert_eq!(fg, Color::Red);
    }

    #[test]
    fn line_style_context() {
        let theme = Theme::dark();
        let (prefix, _fg, _bg) = line_style(LineTag::Context, &theme);
        assert_eq!(prefix, " ");
    }

    // ── fmt_lineno ───────────────────────────────────────────────────────────

    #[test]
    fn fmt_lineno_none_is_five_spaces() {
        assert_eq!(fmt_lineno(None), "     ");
    }

    #[test]
    fn fmt_lineno_small_is_right_aligned() {
        assert_eq!(fmt_lineno(Some(1)), "    1");
        assert_eq!(fmt_lineno(Some(42)), "   42");
    }

    #[test]
    fn fmt_lineno_large_fills_field() {
        assert_eq!(fmt_lineno(Some(99999)), "99999");
    }

    // ── build_sbs_lines ──────────────────────────────────────────────────────

    #[test]
    fn build_sbs_lines_starts_with_hunk_header() {
        let hunk = make_hunk("@@ -1,2 +1,2 @@", vec![]);
        let lines = build_sbs_lines(&hunk, None);
        assert!(matches!(lines.first(), Some(SbsDisplayLine::HunkHeader(_))));
    }

    #[test]
    fn build_sbs_lines_context_appears_on_both_sides() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Context, "same", Some(1), Some(1))],
        );
        let lines = build_sbs_lines(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_tag, LineTag::Context);
        assert_eq!(rows[0].right_tag, LineTag::Context);
        assert_eq!(rows[0].left_text, "same");
        assert_eq!(rows[0].right_text, "same");
    }

    #[test]
    fn build_sbs_lines_add_only_has_empty_left() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Added, "new line", None, Some(1))],
        );
        let lines = build_sbs_lines(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_tag, LineTag::Context);
        assert!(rows[0].left_text.is_empty());
        assert_eq!(rows[0].right_tag, LineTag::Added);
        assert_eq!(rows[0].right_text, "new line");
    }

    #[test]
    fn build_sbs_lines_remove_only_has_empty_right() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Removed, "old line", Some(1), None)],
        );
        let lines = build_sbs_lines(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_tag, LineTag::Removed);
        assert_eq!(rows[0].left_text, "old line");
        assert_eq!(rows[0].right_tag, LineTag::Context);
        assert!(rows[0].right_text.is_empty());
    }

    #[test]
    fn build_sbs_lines_remove_then_add_are_paired() {
        let hunk = make_hunk(
            "@@ @@",
            vec![
                make_line(LineTag::Removed, "old", Some(1), None),
                make_line(LineTag::Added, "new", None, Some(1)),
            ],
        );
        let lines = build_sbs_lines(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_text, "old");
        assert_eq!(rows[0].right_text, "new");
    }

    #[test]
    fn build_sbs_lines_multiple_context_rows() {
        let hunk = make_hunk(
            "@@ @@",
            vec![
                make_line(LineTag::Context, "line1", Some(1), Some(1)),
                make_line(LineTag::Context, "line2", Some(2), Some(2)),
            ],
        );
        assert_eq!(count_rows(&build_sbs_lines(&hunk, None)), 2);
    }

    /// Paired Remove/Add with tokens must surface them on both halves.
    /// Uses `Arc::ptr_eq` to verify the pairing didn't clone the inner Vec —
    /// clones should just bump the refcount.
    #[test]
    fn build_sbs_lines_paired_threads_tokens() {
        let hunk = make_hunk(
            "@@ @@",
            vec![
                make_line(LineTag::Removed, "old", Some(1), None),
                make_line(LineTag::Added, "new", None, Some(1)),
            ],
        );
        let tok_removed = Arc::new(vec![(Style::default().fg(Color::Red), "old".to_string())]);
        let tok_added = Arc::new(vec![(Style::default().fg(Color::Green), "new".to_string())]);
        let hunk_tokens = vec![tok_removed.clone(), tok_added.clone()];
        let lines = build_sbs_lines(&hunk, Some(&hunk_tokens));
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert!(Arc::ptr_eq(
            rows[0].left_tokens.as_ref().unwrap(),
            &tok_removed
        ));
        assert!(Arc::ptr_eq(
            rows[0].right_tokens.as_ref().unwrap(),
            &tok_added
        ));
    }

    /// End-of-hunk flush (a hunk ending in `-` lines with no trailing Context
    /// or Added) must still carry the removed line's tokens to the left half.
    /// Regression guard for the final `pending_removed.drain(..)` loop.
    #[test]
    fn build_sbs_lines_end_of_hunk_flush_preserves_tokens() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Removed, "gone", Some(1), None)],
        );
        let tok = Arc::new(vec![(Style::default().fg(Color::Red), "gone".to_string())]);
        let hunk_tokens = vec![tok.clone()];
        let lines = build_sbs_lines(&hunk, Some(&hunk_tokens));
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert!(Arc::ptr_eq(rows[0].left_tokens.as_ref().unwrap(), &tok));
        assert!(rows[0].right_tokens.is_none());
    }

    /// Context rows clone the token Arc into both halves — they share the
    /// same inner Vec (both identity-equal to the original).
    #[test]
    fn build_sbs_lines_context_shares_tokens_across_halves() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Context, "same", Some(1), Some(1))],
        );
        let tok = Arc::new(vec![(Style::default(), "same".to_string())]);
        let hunk_tokens = vec![tok.clone()];
        let lines = build_sbs_lines(&hunk, Some(&hunk_tokens));
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert!(Arc::ptr_eq(rows[0].left_tokens.as_ref().unwrap(), &tok));
        assert!(Arc::ptr_eq(rows[0].right_tokens.as_ref().unwrap(), &tok));
    }
}
