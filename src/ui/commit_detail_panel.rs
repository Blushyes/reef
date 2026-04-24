//! Graph tab's right editor — commit metadata + Changed-files list (tree or
//! flat) + inline diff for the currently-selected file.

use crate::app::{App, DiffHighlighted, DiffLayout, DiffMode, LineTokens};
use crate::git::tree::{self as gtree, Node};
use crate::git::{DiffContent, FileEntry, FileStatus, LineTag};
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::ui::git_graph_panel;
use crate::ui::highlight::StyledToken;
use crate::ui::mouse::ClickAction;
use crate::ui::text::{clip_spans, overlay_match_highlight};
use crate::ui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

// Matches the Git-tab diff panel's gutter widths (" NNNNN  NNNNN  " / " NNNNN ").
const DIFF_GUTTER_WIDTH: usize = 15;
const SBS_GUTTER_WIDTH: usize = 7;

pub fn render(f: &mut Frame, app: &mut App, area: Rect, _focused: bool) {
    let theme = app.theme;
    app.last_commit_detail_view_h = area.height;

    // Layout split — Unified uses a single h_scroll across all rows and builds
    // rows at virtual_w (= viewport + h_scroll) so pad/content extends past
    // the viewport for clip_spans to reveal. SBS uses two independent
    // h_scrolls (left & right halves) applied per-half at render time; rows
    // build at virtual_w = area.width (non-SBS rows don't h-scroll in SBS mode).
    let is_sbs = matches!(app.commit_detail.diff_layout, DiffLayout::SideBySide);
    let h_unified = if is_sbs {
        0
    } else {
        app.commit_detail.diff_h_scroll
    };
    let virtual_w = (area.width as usize)
        .saturating_add(h_unified)
        .min(u16::MAX as usize) as u16;
    let rows = build_rows(app, virtual_w, area.width, &theme);
    let total = rows.len();

    let max_scroll = total.saturating_sub(area.height as usize);
    if app.commit_detail.scroll > max_scroll {
        app.commit_detail.scroll = max_scroll;
    }
    let scroll = app.commit_detail.scroll;
    let visible = rows.iter().skip(scroll).take(area.height as usize);

    // Clamp the relevant h_scrolls to the widest content in view.
    if !is_sbs {
        let max_visible_w: usize = visible.clone().map(|r| r.content_width).max().unwrap_or(0);
        let max_h = max_visible_w.saturating_sub(area.width as usize);
        if app.commit_detail.diff_h_scroll > max_h {
            app.commit_detail.diff_h_scroll = max_h;
        }
    } else {
        // SBS: clamp each half against the widest corresponding side in view.
        let half_w = (area.width.saturating_sub(1) / 2) as usize;
        let right_w = (area.width as usize).saturating_sub(half_w + 1);
        let left_body_w = half_w.saturating_sub(SBS_GUTTER_WIDTH);
        let right_body_w = right_w.saturating_sub(SBS_GUTTER_WIDTH);
        let (max_left_tw, max_right_tw) =
            visible
                .clone()
                .filter_map(|r| r.sbs.as_ref())
                .fold((0usize, 0usize), |(l, r), p| {
                    (
                        l.max(UnicodeWidthStr::width(p.left_text.as_str())),
                        r.max(UnicodeWidthStr::width(p.right_text.as_str())),
                    )
                });
        let max_left_h = max_left_tw.saturating_sub(left_body_w);
        let max_right_h = max_right_tw.saturating_sub(right_body_w);
        if app.commit_detail.sbs_left_h_scroll > max_left_h {
            app.commit_detail.sbs_left_h_scroll = max_left_h;
        }
        if app.commit_detail.sbs_right_h_scroll > max_right_h {
            app.commit_detail.sbs_right_h_scroll = max_right_h;
        }
    }

    let h_unified = app.commit_detail.diff_h_scroll;
    let sbs_left_h = app.commit_detail.sbs_left_h_scroll;
    let sbs_right_h = app.commit_detail.sbs_right_h_scroll;

    for (i, row) in rows
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .enumerate()
    {
        let y = area.y + i as u16;
        let row_idx = scroll + i;
        let hover = crate::ui::hover::is_hover(app, area, y);

        if let Some(sbs) = &row.sbs {
            // SBS rows still get search highlighting: ranges are flat byte
            // offsets into the row's searchable_rows concat, so we split
            // them into left-body-local and right-body-local ranges and
            // apply overlay per half before clipping.
            let (ranges, cur) = app
                .search
                .ranges_on_row(SearchTarget::CommitDetail, row_idx);
            render_sbs_row_live(
                f,
                area,
                y,
                sbs,
                sbs_left_h,
                sbs_right_h,
                hover,
                app.theme.hover_bg,
                &ranges,
                cur,
                &theme,
            );
            continue;
        }

        // Standard (Unified / non-diff) row — single clip_spans pipeline.
        // In SBS mode we render these with h=0 (no horizontal scroll on
        // header / files / hunk rows — to read long message, switch to
        // Unified via `m`).
        let h = if is_sbs { 0 } else { h_unified };

        let (ranges, cur) = app
            .search
            .ranges_on_row(SearchTarget::CommitDetail, row_idx);

        let base: Vec<(Style, String)> = row
            .spans
            .iter()
            .map(|s| (s.style, s.text.clone()))
            .collect();
        let overlaid = if ranges.is_empty() {
            base
        } else {
            overlay_match_highlight(base, &ranges, cur, theme.search_match, theme.search_current)
        };
        let final_tokens: Vec<(Style, String)> = overlaid
            .into_iter()
            .map(|(style, text)| {
                (
                    crate::ui::hover::apply(style, hover, app.theme.hover_bg),
                    text,
                )
            })
            .collect();
        let clipped = clip_spans(&final_tokens, h, area.width as usize);
        f.render_widget(Line::from(clipped), Rect::new(area.x, y, area.width, 1));

        // Hit registry: project logical spans onto screen after h_scroll.
        let mut x_logical: usize = 0;
        for span in &row.spans {
            let w = UnicodeWidthStr::width(span.text.as_str());
            if w == 0 {
                continue;
            }
            let start = x_logical;
            let end = x_logical + w;
            x_logical = end;

            let (cmd, args) = match (&span.click, &row.row_click) {
                (Some(c), _) => (Some(c.0.clone()), Some(c.1.clone())),
                (None, Some(r)) => (Some(r.0.clone()), Some(r.1.clone())),
                (None, None) => (None, None),
            };
            let Some(cmd) = cmd else {
                continue;
            };

            let vis_start = start.max(h);
            let vis_end = end.min(h.saturating_add(area.width as usize));
            if vis_start >= vis_end {
                continue;
            }
            let screen_x = area.x.saturating_add((vis_start - h) as u16);
            let screen_w = (vis_end - vis_start) as u16;

            app.hit_registry.register_row(
                screen_x,
                y,
                screen_w,
                ClickAction::GitCommand {
                    command: cmd,
                    args: args.unwrap_or(Value::Null),
                    dbl_command: None,
                    dbl_args: None,
                },
            );
        }
    }
}

/// Render an SBS row with independent per-half h_scroll. Each half —
/// left (old version) and right (new version) — is built into its own
/// token stream, clipped by its own `skip_cols`, padded to half/right_w,
/// and composed with a stable `│` separator in between. Search matches
/// are split across the two halves by flat-row byte offset so highlights
/// survive the per-half clip pipeline.
#[allow(clippy::too_many_arguments)]
fn render_sbs_row_live(
    f: &mut Frame,
    area: Rect,
    y: u16,
    sbs: &SbsParts,
    left_h: usize,
    right_h: usize,
    hover: bool,
    hover_bg: Color,
    ranges: &[Range<usize>],
    cur: Option<Range<usize>>,
    theme: &Theme,
) {
    let half_w = area.width.saturating_sub(1) / 2;
    let right_w = area.width.saturating_sub(half_w + 1);
    let left_body_w = (half_w as usize).saturating_sub(SBS_GUTTER_WIDTH);
    let right_body_w = (right_w as usize).saturating_sub(SBS_GUTTER_WIDTH);

    let (left_fg, left_bg) = side_style(sbs.left_tag, theme);
    let (right_fg, right_bg) = side_style(sbs.right_tag, theme);

    // Byte offsets of the two body spans inside the searchable flat row:
    //   [lg:7][left_text][│:3][rg:7][right_text]
    // `searchable_rows` joins spans.text in the same order, so ranges from
    // `search::ranges_on_row` resolve to these offsets.
    let left_body_byte_start = SBS_GUTTER_WIDTH;
    let left_body_byte_end = left_body_byte_start + sbs.left_text.len();
    let sep_byte_end = left_body_byte_end + "│".len();
    let right_body_byte_start = sep_byte_end + SBS_GUTTER_WIDTH;
    let right_body_byte_end = right_body_byte_start + sbs.right_text.len();
    let split = split_sbs_ranges(
        ranges,
        cur.as_ref(),
        left_body_byte_start,
        left_body_byte_end,
        right_body_byte_start,
        right_body_byte_end,
    );
    let SbsRanges {
        left: left_ranges,
        left_cur,
        right: right_ranges,
        right_cur,
    } = split;

    let apply_hover = |s: Style| crate::ui::hover::apply(s, hover, hover_bg);

    let l_gutter_style = apply_hover(style_with_bg(
        Style::default().fg(theme.fg_secondary),
        left_bg,
    ));
    let l_body_base = style_with_bg(Style::default().fg(left_fg), left_bg);
    let l_pad_style = apply_hover(style_with_bg(Style::default(), left_bg));
    let r_gutter_style = apply_hover(style_with_bg(
        Style::default().fg(theme.fg_secondary),
        right_bg,
    ));
    let r_body_base = style_with_bg(Style::default().fg(right_fg), right_bg);
    let r_pad_style = apply_hover(style_with_bg(Style::default(), right_bg));
    let sep_style = apply_hover(Style::default().fg(theme.fg_secondary));

    // Left body: base → overlay match highlight → hover → clip.
    // When syntax tokens exist, use them with the row's bg overlaid per-token
    // so added/removed shading still fills the half. Falls back to a single
    // plain-fg span otherwise. Arc-wrapped tokens deref transparently here.
    let left_base_tokens: Vec<(Style, String)> = match &sbs.left_tokens {
        Some(toks) if !toks.is_empty() => toks
            .iter()
            .map(|(s, t)| (style_with_bg(*s, left_bg), t.clone()))
            .collect(),
        _ => vec![(l_body_base, sbs.left_text.clone())],
    };
    let left_overlaid = if left_ranges.is_empty() {
        left_base_tokens
    } else {
        overlay_match_highlight(
            left_base_tokens,
            &left_ranges,
            left_cur,
            theme.search_match,
            theme.search_current,
        )
    };
    let left_hovered: Vec<(Style, String)> = left_overlaid
        .into_iter()
        .map(|(s, t)| (apply_hover(s), t))
        .collect();
    let left_clipped = clip_spans(&left_hovered, left_h, left_body_w);
    let left_clipped_w: usize = left_clipped
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let left_pad_w = left_body_w.saturating_sub(left_clipped_w);

    // Right body: mirror of left.
    let right_base_tokens: Vec<(Style, String)> = match &sbs.right_tokens {
        Some(toks) if !toks.is_empty() => toks
            .iter()
            .map(|(s, t)| (style_with_bg(*s, right_bg), t.clone()))
            .collect(),
        _ => vec![(r_body_base, sbs.right_text.clone())],
    };
    let right_overlaid = if right_ranges.is_empty() {
        right_base_tokens
    } else {
        overlay_match_highlight(
            right_base_tokens,
            &right_ranges,
            right_cur,
            theme.search_match,
            theme.search_current,
        )
    };
    let right_hovered: Vec<(Style, String)> = right_overlaid
        .into_iter()
        .map(|(s, t)| (apply_hover(s), t))
        .collect();
    let right_clipped = clip_spans(&right_hovered, right_h, right_body_w);
    let right_clipped_w: usize = right_clipped
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let right_pad_w = right_body_w.saturating_sub(right_clipped_w);

    let mut out: Vec<Span> = Vec::with_capacity(8);
    out.push(Span::styled(
        format!(" {} ", fmt_diff_lineno(sbs.left_no)),
        l_gutter_style,
    ));
    out.extend(left_clipped);
    out.push(Span::styled(" ".repeat(left_pad_w), l_pad_style));
    out.push(Span::styled("│".to_string(), sep_style));
    out.push(Span::styled(
        format!(" {} ", fmt_diff_lineno(sbs.right_no)),
        r_gutter_style,
    ));
    out.extend(right_clipped);
    out.push(Span::styled(" ".repeat(right_pad_w), r_pad_style));

    f.render_widget(Line::from(out), Rect::new(area.x, y, area.width, 1));
}

struct SbsRanges {
    left: Vec<Range<usize>>,
    left_cur: Option<Range<usize>>,
    right: Vec<Range<usize>>,
    right_cur: Option<Range<usize>>,
}

/// Split search ranges (flat byte offsets into the SBS row's searchable
/// text) into left-body-local and right-body-local ranges. Matches inside
/// gutters / separator, or straddling boundaries, are dropped — they
/// don't correspond to visible body content. Byte offsets are passed
/// through unchanged (ranges keep their byte-offset semantics inside
/// each half's text).
fn split_sbs_ranges(
    ranges: &[Range<usize>],
    cur: Option<&Range<usize>>,
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> SbsRanges {
    let to_side = |r: &Range<usize>| -> Option<(bool, Range<usize>)> {
        if r.start >= left_start && r.end <= left_end {
            Some((true, (r.start - left_start)..(r.end - left_start)))
        } else if r.start >= right_start && r.end <= right_end {
            Some((false, (r.start - right_start)..(r.end - right_start)))
        } else {
            None
        }
    };

    let mut out = SbsRanges {
        left: Vec::new(),
        left_cur: None,
        right: Vec::new(),
        right_cur: None,
    };
    for r in ranges {
        if let Some((is_left, local)) = to_side(r) {
            if is_left {
                out.left.push(local);
            } else {
                out.right.push(local);
            }
        }
    }
    if let Some(c) = cur {
        if let Some((is_left, local)) = to_side(c) {
            if is_left {
                out.left_cur = Some(local);
            } else {
                out.right_cur = Some(local);
            }
        }
    }
    out
}

pub fn handle_key(app: &mut App, key: &str) -> bool {
    match key {
        "m" => {
            app.commit_detail.diff_layout = match app.commit_detail.diff_layout {
                DiffLayout::Unified => DiffLayout::SideBySide,
                DiffLayout::SideBySide => DiffLayout::Unified,
            };
            crate::prefs::set(
                "commit.diff_layout",
                match app.commit_detail.diff_layout {
                    DiffLayout::Unified => "unified",
                    DiffLayout::SideBySide => "side_by_side",
                },
            );
            true
        }
        "f" => {
            app.commit_detail.diff_mode = match app.commit_detail.diff_mode {
                DiffMode::Compact => DiffMode::FullFile,
                DiffMode::FullFile => DiffMode::Compact,
            };
            crate::prefs::set(
                "commit.diff_mode",
                match app.commit_detail.diff_mode {
                    DiffMode::Compact => "compact",
                    DiffMode::FullFile => "full_file",
                },
            );
            app.reload_commit_file_diff();
            true
        }
        "t" => {
            app.commit_detail.files_tree_mode = !app.commit_detail.files_tree_mode;
            crate::prefs::set(
                "commit.files_tree_mode",
                if app.commit_detail.files_tree_mode {
                    "true"
                } else {
                    "false"
                },
            );
            true
        }
        _ => false,
    }
}

pub fn handle_command(app: &mut App, id: &str, args: &Value) -> bool {
    match id {
        "git.selectCommitFile" => {
            let oid = args.get("oid").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.is_empty() {
                return true;
            }
            // In range mode, keep the range intact — file clicks load the
            // range diff (routed inside `load_commit_file_diff`). The `oid`
            // arg on range rows is just the "newest" marker; we deliberately
            // don't compare it against `selected_commit` because that would
            // spuriously flip us into single-select.
            if !app.git_graph.is_range() {
                if oid.is_empty() {
                    return true;
                }
                if app.git_graph.selected_commit.as_deref() != Some(oid) {
                    if let Some(idx) = app.git_graph.find_row_by_oid(oid) {
                        app.git_graph.selected_idx = idx;
                    }
                    app.git_graph.selected_commit = Some(oid.to_string());
                    app.load_commit_detail();
                }
            }
            app.load_commit_file_diff(path);
            true
        }
        "git.toggleCommitDir" => {
            if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
                if !app.commit_detail.files_collapsed.remove(path) {
                    app.commit_detail.files_collapsed.insert(path.to_string());
                }
            }
            true
        }
        "git.toggleCommitFilesView" => {
            handle_key(app, "t");
            true
        }
        _ => false,
    }
}

pub fn scroll(app: &mut App, delta: i32) {
    let s = &mut app.commit_detail.scroll;
    *s = if delta < 0 {
        s.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        s.saturating_add(delta as usize)
    };
}

// ─── Row model ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct RowSpan {
    text: String,
    style: Style,
    click: Option<(String, Value)>,
}

#[derive(Debug, Default)]
struct Row {
    spans: Vec<RowSpan>,
    row_click: Option<(String, Value)>,
    /// Effective content width excluding trailing pad / decorative fill.
    /// Defaults to the sum of span widths (correct for rows without pad);
    /// rows that fill out to `virtual_w` (unified diff content, SBS halves,
    /// the `─`-separator) override this via `with_content_width` so the
    /// h_scroll clamp in `render` converges instead of tracking virtual_w.
    content_width: usize,
    /// Marks an SBS content row — rendered via a per-half clip pipeline
    /// (each half has its own h_scroll) instead of the flat row clip_spans
    /// path. `spans` for SBS rows is still populated with a flat concat
    /// so `searchable_rows` can produce the same plain text as before.
    sbs: Option<SbsParts>,
}

#[derive(Debug, Clone)]
struct SbsParts {
    left_tag: LineTag,
    left_no: Option<u32>,
    left_text: String,
    /// Syntect-colored tokens for `left_text` — populated when syntax
    /// highlighting is available for the file. Concatenating token texts
    /// must yield exactly `left_text` (search byte offsets rely on this).
    /// `None` falls back to a single plain-fg span at render time. `Arc` so
    /// pairing / render clone is O(1) on large diffs.
    left_tokens: Option<LineTokens>,
    right_tag: LineTag,
    right_no: Option<u32>,
    right_text: String,
    right_tokens: Option<LineTokens>,
}

impl RowSpan {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
            click: None,
        }
    }
    fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            click: None,
        }
    }
    fn on_click(mut self, cmd: &str, args: Value) -> Self {
        self.click = Some((cmd.to_string(), args));
        self
    }
}

impl Row {
    fn new(spans: Vec<RowSpan>) -> Self {
        let content_width = spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.text.as_str()))
            .sum();
        Self {
            spans,
            row_click: None,
            content_width,
            sbs: None,
        }
    }
    fn blank() -> Self {
        Row::new(vec![RowSpan::plain("")])
    }
    fn on_click(mut self, cmd: &str, args: Value) -> Self {
        self.row_click = Some((cmd.to_string(), args));
        self
    }
    fn with_content_width(mut self, w: usize) -> Self {
        self.content_width = w;
        self
    }
    fn with_sbs(mut self, parts: SbsParts) -> Self {
        self.sbs = Some(parts);
        self
    }
}

fn from_ratatui_span(span: Span<'static>) -> RowSpan {
    RowSpan {
        text: span.content.into_owned(),
        style: span.style,
        click: None,
    }
}

// ─── Row construction ─────────────────────────────────────────────────────────

/// `width` is the *virtual* width (viewport + h_scroll) — it controls how
/// far pad/decoration extends to the right so `clip_spans` can reveal more
/// content as `h_scroll` grows. `display_w` is the *real* viewport width,
/// stable across frames; SBS's `half` / `│` position must use it (not
/// `width`) or the separator drifts every frame as the user over-scrolls
/// past max_h, producing visible jitter at the right edge.
fn build_rows(app: &App, width: u16, display_w: u16, theme: &Theme) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let cd = &app.commit_detail;
    let max_msg = (width as usize).saturating_sub(4);
    let max_path = (width as usize).saturating_sub(6);

    // Range mode and single mode share the bottom half (files list + inline
    // diff) but have different headers. Collect the common data — (files
    // slice, stable oid for selectCommitFile clicks) — then emit the
    // appropriate header, and hand off to the shared tail.
    let (files_source, stable_oid): (&[FileEntry], &str) = match (&cd.range_detail, &cd.detail) {
        (Some(range), _) => {
            append_range_header(&mut rows, range, max_msg, theme);
            (range.files.as_slice(), range.newest_oid.as_str())
        }
        (None, Some(detail)) => {
            append_commit_header(&mut rows, app, detail, max_msg, theme);
            (detail.files.as_slice(), detail.info.oid.as_str())
        }
        (None, None) => {
            rows.push(Row::new(vec![RowSpan::styled(
                t(Msg::CommitDetailEmpty),
                Style::default().fg(theme.fg_secondary),
            )]));
            return rows;
        }
    };

    let view_label = if cd.files_tree_mode {
        t(Msg::ViewTree)
    } else {
        t(Msg::ViewList)
    };
    rows.push(Row::new(vec![
        RowSpan::styled(
            crate::i18n::changed_files_header(files_source.len()),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        RowSpan::styled(
            crate::i18n::view_toggle_hint(view_label),
            Style::default().fg(theme.fg_secondary),
        )
        .on_click("git.toggleCommitFilesView", Value::Null),
    ]));

    let ctx = CommitFilesCtx {
        selected_file: cd.file_diff.as_ref().map(|d| d.path.as_str()),
        sel_bg: theme.selection_bg,
        max_path,
        commit_oid: stable_oid,
        collapsed: &cd.files_collapsed,
        fg: theme.fg_primary,
    };

    if cd.files_tree_mode {
        let nodes = gtree::build(files_source);
        render_commit_file_tree(&nodes, 1, &ctx, &mut rows, theme);
    } else {
        for file in files_source {
            rows.push(commit_file_row(file, &file.path, "  ", &ctx));
        }
    }

    // Inline diff section: only rendered in the 2-col fallback. When the
    // Graph tab is in 3-col mode the diff lives in its own right column
    // (rendered by `ui::mod::render_graph_diff_column`), so skipping it
    // here avoids a stale duplicate row list and shrinks `scroll`'s clamp
    // back to just metadata + files.
    if !app.graph_uses_three_col()
        && let Some(file_diff) = &cd.file_diff
    {
        rows.push(Row::blank());
        rows.push(diff_header_row(
            &file_diff.path,
            cd.diff_layout,
            cd.diff_mode,
            width,
            theme,
        ));
        rows.push(diff_separator_row(width, theme));
        append_diff_rows(
            &mut rows,
            &file_diff.diff,
            file_diff.highlighted.as_ref(),
            cd.diff_layout,
            width,
            display_w,
            theme,
        );
    }

    rows
}

/// Single-commit header: oid/author/date/refs + message body. Unchanged from
/// the original inline version — just extracted so range-mode can substitute
/// `append_range_header`.
fn append_commit_header(
    rows: &mut Vec<Row>,
    app: &App,
    detail: &crate::git::CommitDetail,
    max_msg: usize,
    theme: &Theme,
) {
    let info = &detail.info;

    rows.push(Row::new(vec![
        RowSpan::styled(t(Msg::CommitLabel), Style::default().fg(theme.fg_secondary)),
        RowSpan::styled(
            info.oid.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    rows.push(Row::new(vec![
        RowSpan::styled(t(Msg::AuthorLabel), Style::default().fg(theme.fg_secondary)),
        RowSpan::styled(
            format!("{} <{}>", info.author_name, info.author_email),
            Style::default().fg(theme.fg_primary),
        ),
    ]));
    rows.push(Row::new(vec![
        RowSpan::styled(t(Msg::DateLabel), Style::default().fg(theme.fg_secondary)),
        RowSpan::styled(
            format_timestamp(info.time),
            Style::default().fg(theme.fg_primary),
        ),
    ]));

    if let Some(labels) = app.git_graph.ref_map.get(&info.oid) {
        let mut spans: Vec<RowSpan> = vec![RowSpan::styled(
            t(Msg::RefsLabel),
            Style::default().fg(theme.fg_secondary),
        )];
        for label in labels {
            spans.push(from_ratatui_span(git_graph_panel::ref_label_span(label)));
            spans.push(RowSpan::plain(" "));
        }
        rows.push(Row::new(spans));
    }

    rows.push(Row::blank());

    for raw in detail.message.lines() {
        // content_width uses the *raw* line width (pre-truncation) so clamp
        // converges to the real end of the message; otherwise content_width
        // would equal virtual_w while the line is truncated, and max_h
        // would track the user's h_scroll indefinitely — then snap back
        // when the line finally fits (the "jitter" you see at the right edge).
        let raw_w = UnicodeWidthStr::width(raw);
        let mut msg = raw.to_string();
        truncate_in_place(&mut msg, max_msg);
        rows.push(
            Row::new(vec![
                RowSpan::plain("    "),
                RowSpan::styled(msg, Style::default().fg(theme.fg_primary)),
            ])
            .with_content_width(4 + raw_w),
        );
    }

    rows.push(Row::blank());
}

/// Range-mode header: "N commits: oldest..newest" plus a clickable list of
/// subjects. Each commit row emits `git.graph.focusCommit` — a zoom-in
/// command that always collapses the range back to single-select on the
/// target commit regardless of visual mode. Plain `git.selectCommit`
/// behaves differently inside visual mode (it extends the endpoint), which
/// is the wrong semantic for this list.
fn append_range_header(
    rows: &mut Vec<Row>,
    range: &crate::app::RangeDetail,
    max_msg: usize,
    theme: &Theme,
) {
    let oldest_short: String = range.oldest_oid.chars().take(7).collect();
    let newest_short: String = range.newest_oid.chars().take(7).collect();

    rows.push(Row::new(vec![
        RowSpan::styled(
            crate::i18n::range_header_count(range.commit_count),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        RowSpan::styled(
            format!(" {}..{}", oldest_short, newest_short),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::DIM),
        ),
    ]));
    rows.push(Row::new(vec![RowSpan::styled(
        t(Msg::RangeHint),
        Style::default().fg(theme.fg_secondary),
    )]));
    rows.push(Row::blank());

    // Commit list: newest first (matches the graph panel's order). Click
    // any row → collapse range to single on that commit.
    for commit in &range.commits {
        let short: String = commit.short_oid.clone();
        let mut subject = commit.subject.clone();
        // Reserve room for the short oid + separator + author column.
        let subject_budget = max_msg.saturating_sub(short.len() + 16);
        truncate_in_place(&mut subject, subject_budget);
        let raw_w = UnicodeWidthStr::width(subject.as_str()) + short.len() + 3;

        rows.push(
            Row::new(vec![
                RowSpan::plain("  "),
                RowSpan::styled(
                    short,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::DIM),
                ),
                RowSpan::plain(" "),
                RowSpan::styled(subject, Style::default().fg(theme.fg_primary)),
            ])
            .on_click(
                "git.graph.focusCommit",
                serde_json::json!({ "oid": commit.oid }),
            )
            .with_content_width(raw_w),
        );
    }

    rows.push(Row::blank());
}

struct CommitFilesCtx<'a> {
    selected_file: Option<&'a str>,
    sel_bg: Color,
    max_path: usize,
    commit_oid: &'a str,
    collapsed: &'a std::collections::HashSet<String>,
    fg: Color,
}

fn commit_file_row(
    file: &FileEntry,
    display_path: &str,
    indent: &str,
    ctx: &CommitFilesCtx,
) -> Row {
    let status_color = match file.status {
        FileStatus::Modified => Color::Yellow,
        FileStatus::Added => Color::Green,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Untracked => Color::Green,
    };
    // content_width uses the raw (pre-truncation) path width so clamp
    // converges on the full path instead of chasing virtual_w.
    let indent_w = UnicodeWidthStr::width(indent);
    let full_display_w = UnicodeWidthStr::width(display_path);
    let mut display = display_path.to_string();
    truncate_in_place(&mut display, ctx.max_path);

    let is_selected = ctx.selected_file == Some(file.path.as_str());
    let base_bg = if is_selected { Some(ctx.sel_bg) } else { None };

    let spans = vec![
        RowSpan::styled(indent.to_string(), apply_bg(Style::default(), base_bg)),
        RowSpan::styled(
            format!("{} ", file.status.label()),
            apply_bg(Style::default().fg(status_color), base_bg),
        ),
        RowSpan::styled(display, apply_bg(Style::default().fg(ctx.fg), base_bg)),
    ];

    Row::new(spans)
        .on_click(
            "git.selectCommitFile",
            serde_json::json!({ "oid": ctx.commit_oid, "path": file.path }),
        )
        .with_content_width(indent_w + 2 + full_display_w)
}

fn render_commit_file_tree(
    nodes: &BTreeMap<String, Node>,
    depth: usize,
    ctx: &CommitFilesCtx,
    rows: &mut Vec<Row>,
    theme: &Theme,
) {
    let mut entries: Vec<(&String, &Node)> = nodes.iter().collect();
    entries.sort_by(|a, b| {
        let a_dir = matches!(a.1, Node::Dir { .. });
        let b_dir = matches!(b.1, Node::Dir { .. });
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        }
    });

    for (name, node) in entries {
        match node {
            Node::Dir { path, children } => {
                let is_collapsed = ctx.collapsed.contains(path);
                let arrow = if is_collapsed { "›" } else { "⌄" };
                let indent = "  ".repeat(depth);
                rows.push(
                    Row::new(vec![
                        RowSpan::plain(indent),
                        RowSpan::styled(
                            format!("{} ", arrow),
                            Style::default().fg(theme.fg_secondary),
                        ),
                        RowSpan::styled(
                            format!("{}/", name),
                            Style::default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                    .on_click("git.toggleCommitDir", serde_json::json!({ "path": path })),
                );
                if !is_collapsed {
                    render_commit_file_tree(children, depth + 1, ctx, rows, theme);
                }
            }
            Node::File(entry) => {
                let basename = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                let indent = "  ".repeat(depth);
                rows.push(commit_file_row(entry, basename, &indent, ctx));
            }
        }
    }
}

// ─── Inline diff rendering ────────────────────────────────────────────────────

fn diff_header_row(
    path: &str,
    layout: DiffLayout,
    mode: DiffMode,
    width: u16,
    theme: &Theme,
) -> Row {
    let layout_label = match layout {
        DiffLayout::Unified => t(Msg::LayoutUnified),
        DiffLayout::SideBySide => t(Msg::LayoutSideBySide),
    };
    let mode_label = match mode {
        DiffMode::Compact => t(Msg::ModeCompact),
        DiffMode::FullFile => t(Msg::ModeFullFile),
    };
    let tag_str = crate::i18n::diff_mode_hint(layout_label, mode_label);
    let tag_w = UnicodeWidthStr::width(tag_str.as_str());
    let path_max = (width as usize).saturating_sub(tag_w);
    let path_display = truncate_to_display_width(path, path_max).to_string();
    // content_width uses the full path width so clamp converges independent
    // of virtual_w (see commit message row for the full rationale).
    let full_path_w = UnicodeWidthStr::width(path);

    Row::new(vec![
        RowSpan::styled(
            path_display,
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ),
        RowSpan::styled(tag_str, Style::default().fg(theme.fg_secondary)),
    ])
    .with_content_width(full_path_w + tag_w)
}

fn diff_separator_row(width: u16, theme: &Theme) -> Row {
    // Purely decorative — fills to `width` so it always spans the viewport,
    // but its "content" is zero. Excluding it from the h_scroll clamp keeps
    // virtual_w from pinning max_h to itself.
    Row::new(vec![RowSpan::styled(
        "─".repeat(width as usize),
        Style::default().fg(theme.fg_secondary),
    )])
    .with_content_width(0)
}

fn append_diff_rows(
    rows: &mut Vec<Row>,
    diff: &DiffContent,
    highlighted: Option<&DiffHighlighted>,
    layout: DiffLayout,
    width: u16,
    display_w: u16,
    theme: &Theme,
) {
    match layout {
        DiffLayout::Unified => append_unified_diff(rows, diff, highlighted, width, theme),
        // SBS uses display_w for the stable separator position (so the `│`
        // doesn't drift when virtual_w changes), and width (= virtual_w) for
        // the right-side pad fill so bg extends to the viewport's right edge.
        DiffLayout::SideBySide => {
            append_side_by_side_diff(rows, diff, highlighted, width, display_w, theme)
        }
    }
}

fn append_unified_diff(
    rows: &mut Vec<Row>,
    diff: &DiffContent,
    highlighted: Option<&DiffHighlighted>,
    width: u16,
    theme: &Theme,
) {
    let added_bg = theme.added_bg;
    let removed_bg = theme.removed_bg;

    for (hi, hunk) in diff.hunks.iter().enumerate() {
        if hi > 0 {
            rows.push(Row::new(vec![RowSpan::styled(
                format!(" {:>5}  {:>5}  ⋯", "", ""),
                Style::default().fg(theme.fg_secondary),
            )]));
        }
        rows.push(Row::new(vec![RowSpan::styled(
            format!(" {:>5}  {:>5}  {}", "", "", hunk.header),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::DIM),
        )]));

        let hunk_tokens = highlighted.and_then(|h| h.get(hi));

        for (li, line) in hunk.lines.iter().enumerate() {
            let (prefix, fg, bg) = match line.tag {
                LineTag::Added => ("+", theme.added_accent, Some(added_bg)),
                LineTag::Removed => ("-", theme.removed_accent, Some(removed_bg)),
                LineTag::Context => (" ", theme.fg_primary, None),
            };
            let old_no = fmt_diff_lineno(line.old_lineno);
            let new_no = fmt_diff_lineno(line.new_lineno);
            let max_text = (width as usize).saturating_sub(DIFF_GUTTER_WIDTH);

            let gutter_style = Style::default().fg(theme.fg_secondary);
            let mark_style = Style::default().fg(fg);
            let (g, m, pad_style) = match bg {
                Some(bg) => (
                    gutter_style.bg(bg),
                    mark_style.bg(bg),
                    Style::default().bg(bg),
                ),
                None => (gutter_style, mark_style, Style::default()),
            };

            // Content tokens: syntect-colored when available, else a single
            // plain-fg token. `bg` is applied per-token so added/removed bg
            // spans the full line. Pad at end fills out to virtual_w.
            let content_tokens =
                content_tokens_for_line(&line.content, hunk_tokens.and_then(|t| t.get(li)), fg, bg);
            let clipped = clip_spans(&content_tokens, 0, max_text);
            let used: usize = clipped
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let pad = max_text.saturating_sub(used);

            // Content width excludes the trailing pad (which fills out to
            // `virtual_w` so diff bg spans the viewport). Uses the *original*
            // line.content width — `content` above may have been trimmed by
            // `max_text`, but we want clamp to converge to the real line.
            // The gutter format below is " NNNNN  NNNNN  " = 15 cols, matching
            // DIFF_GUTTER_WIDTH and the git tab's diff panel; an earlier off-by-one
            // (14-col gutter via 1 trailing space) made content_width skew by 1
            // and contributed to right-edge misalignment.
            let line_content_w = DIFF_GUTTER_WIDTH
                .saturating_add(2)
                .saturating_add(UnicodeWidthStr::width(line.content.as_str()));

            let mut spans: Vec<RowSpan> = Vec::with_capacity(2 + clipped.len() + 1);
            spans.push(RowSpan::styled(format!(" {}  {}  ", old_no, new_no), g));
            spans.push(RowSpan::styled(format!("{} ", prefix), m));
            for span in clipped {
                spans.push(RowSpan::styled(span.content.to_string(), span.style));
            }
            spans.push(RowSpan::styled(" ".repeat(pad), pad_style));

            rows.push(Row::new(spans).with_content_width(line_content_w));
        }
    }
}

/// Build styled-token representation of a single diff line's content. When
/// per-line syntax tokens are present, returns them with the diff row's `bg`
/// overlaid so added/removed backgrounds still paint the full line width.
/// Otherwise falls back to a single plain-fg token. Takes the line's tokens
/// directly (not the nested `DiffHighlighted`) so callers can hoist the
/// per-hunk lookup out of the inner loop.
fn content_tokens_for_line(
    content: &str,
    line_tokens: Option<&LineTokens>,
    fallback_fg: Color,
    bg: Option<Color>,
) -> Vec<StyledToken> {
    match line_tokens {
        Some(toks) if !toks.is_empty() => toks
            .iter()
            .map(|(s, t)| (apply_bg(*s, bg), t.clone()))
            .collect(),
        _ => {
            let style = apply_bg(Style::default().fg(fallback_fg), bg);
            vec![(style, content.to_string())]
        }
    }
}

fn append_side_by_side_diff(
    rows: &mut Vec<Row>,
    diff: &DiffContent,
    highlighted: Option<&DiffHighlighted>,
    _virtual_w: u16,
    _display_w: u16,
    theme: &Theme,
) {
    for (hi, hunk) in diff.hunks.iter().enumerate() {
        if hi > 0 {
            rows.push(Row::new(vec![RowSpan::styled(
                format!(" {:>5}  ⋯", ""),
                Style::default().fg(theme.fg_secondary),
            )]));
        }
        rows.push(Row::new(vec![RowSpan::styled(
            format!(" {:>5}  {}", "", hunk.header),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::DIM),
        )]));

        let hunk_tokens = highlighted.and_then(|h| h.get(hi));
        for parts in pair_hunk_lines(hunk, hunk_tokens) {
            rows.push(build_sbs_row(parts, theme));
        }
    }
}

/// Build an SBS row as a marker-plus-searchable-spans: `spans` holds a flat
/// "left_gutter | left_text | │ | right_gutter | right_text" concatenation
/// so `searchable_rows` keeps working and `content_width` reflects the
/// flat text width (used by Unified-style clamp — which is inactive in
/// SBS mode, but keeping the field sensible is cheaper than special-casing).
/// The `sbs` field is the source of truth for rendering; the render
/// function detects it and takes a per-half clip pipeline instead.
fn build_sbs_row(parts: SbsParts, theme: &Theme) -> Row {
    let (left_fg, left_bg) = side_style(parts.left_tag, theme);
    let (right_fg, right_bg) = side_style(parts.right_tag, theme);

    let left_gutter_style = style_with_bg(Style::default().fg(theme.fg_secondary), left_bg);
    let left_body_style = style_with_bg(Style::default().fg(left_fg), left_bg);
    let right_gutter_style = style_with_bg(Style::default().fg(theme.fg_secondary), right_bg);
    let right_body_style = style_with_bg(Style::default().fg(right_fg), right_bg);

    let spans = vec![
        RowSpan::styled(
            format!(" {} ", fmt_diff_lineno(parts.left_no)),
            left_gutter_style,
        ),
        RowSpan::styled(parts.left_text.clone(), left_body_style),
        RowSpan::styled("│".to_string(), Style::default().fg(theme.fg_secondary)),
        RowSpan::styled(
            format!(" {} ", fmt_diff_lineno(parts.right_no)),
            right_gutter_style,
        ),
        RowSpan::styled(parts.right_text.clone(), right_body_style),
    ];
    Row::new(spans).with_sbs(parts)
}

fn style_with_bg(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(c) => style.bg(c),
        None => style,
    }
}

fn side_style(tag: LineTag, theme: &Theme) -> (Color, Option<Color>) {
    match tag {
        LineTag::Added => (theme.added_accent, Some(theme.added_bg)),
        LineTag::Removed => (theme.removed_accent, Some(theme.removed_bg)),
        LineTag::Context => (theme.fg_primary, None),
    }
}

fn pair_hunk_lines(
    hunk: &crate::git::DiffHunk,
    hunk_tokens: Option<&Vec<LineTokens>>,
) -> Vec<SbsParts> {
    let mut rows = Vec::new();
    // Pending removed entries carry their per-line tokens so the left half
    // keeps its syntax highlighting when paired with a later Added line.
    let mut pending_removed: Vec<(Option<u32>, String, Option<LineTokens>)> = Vec::new();
    let tokens_for =
        |li: usize| -> Option<LineTokens> { hunk_tokens.and_then(|t| t.get(li)).cloned() };

    for (li, line) in hunk.lines.iter().enumerate() {
        match line.tag {
            LineTag::Removed => {
                pending_removed.push((line.old_lineno, line.content.clone(), tokens_for(li)));
            }
            LineTag::Added => {
                let added_tokens = tokens_for(li);
                if let Some((old_no, old_text, old_tokens)) =
                    (!pending_removed.is_empty()).then(|| pending_removed.remove(0))
                {
                    rows.push(SbsParts {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        left_tokens: old_tokens,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                        right_tokens: added_tokens,
                    });
                } else {
                    rows.push(SbsParts {
                        left_tag: LineTag::Context,
                        left_no: None,
                        left_text: String::new(),
                        left_tokens: None,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                        right_tokens: added_tokens,
                    });
                }
            }
            LineTag::Context => {
                for (old_no, old_text, old_tokens) in pending_removed.drain(..) {
                    rows.push(SbsParts {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        left_tokens: old_tokens,
                        right_tag: LineTag::Context,
                        right_no: None,
                        right_text: String::new(),
                        right_tokens: None,
                    });
                }
                let ctx_tokens = tokens_for(li);
                rows.push(SbsParts {
                    left_tag: LineTag::Context,
                    left_no: line.old_lineno,
                    left_text: line.content.clone(),
                    left_tokens: ctx_tokens.clone(),
                    right_tag: LineTag::Context,
                    right_no: line.new_lineno,
                    right_text: line.content.clone(),
                    right_tokens: ctx_tokens,
                });
            }
        }
    }

    for (old_no, old_text, old_tokens) in pending_removed.drain(..) {
        rows.push(SbsParts {
            left_tag: LineTag::Removed,
            left_no: old_no,
            left_text: old_text,
            left_tokens: old_tokens,
            right_tag: LineTag::Context,
            right_no: None,
            right_text: String::new(),
            right_tokens: None,
        });
    }

    rows
}

// ─── Utility helpers ──────────────────────────────────────────────────────────

fn apply_bg(style: Style, bg: Option<Color>) -> Style {
    match bg {
        Some(c) => style.bg(c),
        None => style,
    }
}

fn truncate_in_place(s: &mut String, max: usize) {
    if max == 0 {
        s.clear();
        return;
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return;
    }
    let kept: String = chars.into_iter().take(max.saturating_sub(1)).collect();
    *s = format!("{}…", kept);
}

fn truncate_to_display_width(s: &str, max_width: usize) -> &str {
    let mut width = 0;
    for (i, c) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw > max_width {
            return &s[..i];
        }
        width += cw;
    }
    s
}

fn fmt_diff_lineno(n: Option<u32>) -> String {
    n.map(|v| format!("{:>5}", v))
        .unwrap_or_else(|| "     ".to_string())
}

fn format_timestamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let h = tod / 3600;
    let m = (tod % 3600) / 60;
    let s = tod % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

// ─── Search support ───────────────────────────────────────────────────────────

/// Flattens the panel's row stream into plain text — one string per rendered
/// row. Used by `crate::search` so match row indices line up 1:1 with
/// `commit_detail.scroll` and with what the panel draws. Both width params
/// are passed as `u16::MAX` so row construction skips display-width
/// truncation and the searchable text mirrors the underlying data.
pub fn searchable_rows(app: &App) -> Vec<String> {
    let rows = build_rows(app, u16::MAX, u16::MAX, &app.theme);
    rows.into_iter()
        .map(|r| {
            r.spans
                .into_iter()
                .map(|s| s.text)
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn row_new_sums_span_widths() {
        let row = Row::new(vec![RowSpan::plain("hello"), RowSpan::plain(" world")]);
        assert_eq!(row.content_width, 11);
    }

    #[test]
    fn content_tokens_falls_back_when_no_highlight() {
        let out = content_tokens_for_line("let x = 1;", None, Color::Red, None);
        // One token, plain fg, unchanged text.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "let x = 1;");
    }

    #[test]
    fn content_tokens_applies_bg_to_highlighted_tokens() {
        let syntax_a = Style::default().fg(Color::Blue);
        let syntax_b = Style::default().fg(Color::Green);
        let line_tokens: LineTokens = Arc::new(vec![
            (syntax_a, "let ".to_string()),
            (syntax_b, "x".to_string()),
        ]);
        let out =
            content_tokens_for_line("let x", Some(&line_tokens), Color::Red, Some(Color::Yellow));
        // Tokens preserved, bg overlaid on each.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].1, "let ");
        assert_eq!(out[1].1, "x");
        assert_eq!(out[0].0.bg, Some(Color::Yellow));
        assert_eq!(out[1].0.bg, Some(Color::Yellow));
    }

    /// SBS pairing must thread per-line tokens to the right half: when an
    /// Added line is paired with a pending Removed line, both `left_tokens`
    /// and `right_tokens` must survive to the `SbsParts` (otherwise the
    /// side that lost its tokens renders as an unstyled plain-fg row).
    #[test]
    fn pair_hunk_lines_threads_tokens_through_pairing() {
        use crate::git::{DiffHunk, DiffLine};
        let hunk = DiffHunk {
            header: "@@ -1,1 +1,1 @@".to_string(),
            lines: vec![
                DiffLine {
                    tag: LineTag::Removed,
                    content: "old".to_string(),
                    old_lineno: Some(1),
                    new_lineno: None,
                },
                DiffLine {
                    tag: LineTag::Added,
                    content: "new".to_string(),
                    old_lineno: None,
                    new_lineno: Some(1),
                },
            ],
        };
        let tok_removed = Arc::new(vec![(Style::default().fg(Color::Red), "old".to_string())]);
        let tok_added = Arc::new(vec![(Style::default().fg(Color::Green), "new".to_string())]);
        let hunk_tokens = vec![tok_removed.clone(), tok_added.clone()];
        let parts = pair_hunk_lines(&hunk, Some(&hunk_tokens));
        assert_eq!(parts.len(), 1);
        let p = &parts[0];
        // Arc identity: clones should share refcount, proving O(1) pairing.
        assert!(Arc::ptr_eq(p.left_tokens.as_ref().unwrap(), &tok_removed));
        assert!(Arc::ptr_eq(p.right_tokens.as_ref().unwrap(), &tok_added));
    }

    /// End-of-hunk `pending_removed` flush must carry its Arc tokens so a
    /// hunk that ends in `-` lines (with no trailing Context or Added) still
    /// renders the left half syntax-colored.
    #[test]
    fn pair_hunk_lines_end_of_hunk_flush_preserves_tokens() {
        use crate::git::{DiffHunk, DiffLine};
        let hunk = DiffHunk {
            header: "@@ -1,1 +1,0 @@".to_string(),
            lines: vec![DiffLine {
                tag: LineTag::Removed,
                content: "gone".to_string(),
                old_lineno: Some(1),
                new_lineno: None,
            }],
        };
        let tok = Arc::new(vec![(Style::default().fg(Color::Red), "gone".to_string())]);
        let hunk_tokens = vec![tok.clone()];
        let parts = pair_hunk_lines(&hunk, Some(&hunk_tokens));
        assert_eq!(parts.len(), 1);
        assert!(Arc::ptr_eq(parts[0].left_tokens.as_ref().unwrap(), &tok));
        assert!(parts[0].right_tokens.is_none());
    }

    #[test]
    fn row_with_content_width_overrides_auto_value() {
        // Auto-sum would be 5 + 100 = 105 because of the trailing pad.
        // Mirrors how diff / SBS rows suppress pad from the h_scroll clamp.
        let row = Row::new(vec![
            RowSpan::plain("hello"),
            RowSpan::plain(" ".repeat(100)),
        ])
        .with_content_width(5);
        assert_eq!(row.content_width, 5);
    }

    /// Regression test for the h_scroll clamp that previously used
    /// `sum(span widths)` per row: since `build_rows` pads diff/SBS lines
    /// out to `virtual_w`, raw-sum clamp always yielded `max_h = h_input`,
    /// letting the user scroll forever. With `content_width` the clamp
    /// reflects the real longest line and converges.
    #[test]
    fn clamp_via_content_width_ignores_trailing_pad() {
        let rows = [
            Row::new(vec![RowSpan::plain("author: alice")]),
            // Simulates a diff content row built at virtual_w=500:
            // gutter(15) + prefix(2) + content(30) + pad(453).
            // Raw-sum would be 500; content_width = 47.
            Row::new(vec![
                RowSpan::plain(" 12345  12345  "),
                RowSpan::plain("+ "),
                RowSpan::plain("x".repeat(30)),
                RowSpan::plain(" ".repeat(453)),
            ])
            .with_content_width(DIFF_GUTTER_WIDTH + 2 + 30),
        ];

        let area_width: usize = 80;
        let max_visible_w = rows.iter().map(|r| r.content_width).max().unwrap_or(0);
        let max_h = max_visible_w.saturating_sub(area_width);

        // Content (47 cols) fits in viewport (80 cols): no room to scroll.
        // If the clamp regressed to raw-sum this would be 500 - 80 = 420.
        assert_eq!(max_h, 0);
    }

    #[test]
    fn diff_separator_row_has_zero_content_width() {
        let theme = crate::ui::theme::Theme::dark();
        let row = diff_separator_row(5000, &theme);
        assert_eq!(
            row.content_width, 0,
            "decorative separator must not gate h_scroll clamp"
        );
    }

    /// SBS rows carry the `sbs` marker so render dispatches to the per-half
    /// clip pipeline (independent left & right h_scrolls), not the flat
    /// clip_spans path.
    #[test]
    fn sbs_row_carries_sbs_marker() {
        let theme = crate::ui::theme::Theme::dark();
        let parts = SbsParts {
            left_tag: LineTag::Context,
            left_no: Some(1),
            left_text: "old".to_string(),
            left_tokens: None,
            right_tag: LineTag::Context,
            right_no: Some(2),
            right_text: "new".to_string(),
            right_tokens: None,
        };
        let row = build_sbs_row(parts.clone(), &theme);
        let marker = row.sbs.as_ref().expect("SBS row must carry sbs marker");
        assert_eq!(marker.left_text, "old");
        assert_eq!(marker.right_text, "new");
    }

    /// SBS search highlight regression test: ranges in a flat row string
    /// ("lg | left | │ | rg | right") must map to body-local ranges on
    /// the correct side, and in-gutter / on-separator / straddling matches
    /// must be dropped (the render pipeline has nothing to highlight there).
    #[test]
    fn sbs_ranges_split_maps_to_correct_half() {
        // left_text.len() = 10, right_text.len() = 15.
        // Byte layout:
        //   [0, 7)  lg
        //   [7, 17) left_text
        //   [17, 20) │ (3 bytes)
        //   [20, 27) rg
        //   [27, 42) right_text
        let left_start = 7;
        let left_end = 17;
        let right_start = 20 + 7; // 27
        let right_end = right_start + 15; // 42

        let match_in_left = 9..12; // inside left body
        let match_in_right = 30..35; // inside right body
        let match_in_gutter = 3..5; // inside left gutter → dropped
        let match_on_sep = 17..18; // on separator → dropped
        let match_straddle = 15..25; // crosses body/sep/gutter → dropped

        let ranges = vec![
            match_in_left.clone(),
            match_in_right.clone(),
            match_in_gutter,
            match_on_sep,
            match_straddle,
        ];
        let cur = Some(match_in_right.clone());

        let split = split_sbs_ranges(
            &ranges,
            cur.as_ref(),
            left_start,
            left_end,
            right_start,
            right_end,
        );

        assert_eq!(split.left, vec![(9 - 7)..(12 - 7)]);
        assert_eq!(split.right, vec![(30 - 27)..(35 - 27)]);
        assert_eq!(split.right_cur, Some((30 - 27)..(35 - 27)));
        assert!(split.left_cur.is_none());
    }

    /// Independent clamps: the left and right SBS h_scroll are computed from
    /// their respective text widths — max_left_h depends on left_tw, max_right_h
    /// depends on right_tw, and the two don't cross-contaminate.
    #[test]
    fn sbs_independent_clamp_per_half() {
        // display_w=80 → half=39, right_w=40, left_body_w=32, right_body_w=33.
        let left_body_w: usize = 32;
        let right_body_w: usize = 33;
        let parts = [
            SbsParts {
                left_tag: LineTag::Context,
                left_no: Some(1),
                left_text: "x".repeat(100), // long left
                left_tokens: None,
                right_tag: LineTag::Context,
                right_no: Some(1),
                right_text: "y".repeat(50), // shorter right
                right_tokens: None,
            },
            SbsParts {
                left_tag: LineTag::Context,
                left_no: Some(2),
                left_text: "z".repeat(10), // short left
                left_tokens: None,
                right_tag: LineTag::Context,
                right_no: Some(2),
                right_text: "w".repeat(200), // very long right
                right_tokens: None,
            },
        ];

        let (max_left_tw, max_right_tw) = parts.iter().fold((0usize, 0usize), |(l, r), p| {
            (
                l.max(UnicodeWidthStr::width(p.left_text.as_str())),
                r.max(UnicodeWidthStr::width(p.right_text.as_str())),
            )
        });
        let max_left_h = max_left_tw.saturating_sub(left_body_w);
        let max_right_h = max_right_tw.saturating_sub(right_body_w);

        assert_eq!(
            max_left_h,
            100 - left_body_w,
            "left clamp uses only left_tw"
        );
        assert_eq!(
            max_right_h,
            200 - right_body_w,
            "right clamp uses only right_tw"
        );
        assert_ne!(
            max_left_h, max_right_h,
            "left/right clamps must be independent"
        );
    }
}
