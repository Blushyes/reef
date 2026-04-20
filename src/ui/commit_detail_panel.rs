//! Graph tab's right editor — commit metadata + Changed-files list (tree or
//! flat) + inline diff for the currently-selected file.

use crate::app::{App, DiffLayout, DiffMode};
use crate::git::tree::{self as gtree, Node};
use crate::git::{DiffContent, FileEntry, FileStatus, LineTag};
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::ui::git_graph_panel;
use crate::ui::mouse::ClickAction;
use crate::ui::text::{clip_spans, overlay_match_highlight};
use crate::ui::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::collections::BTreeMap;
use unicode_width::UnicodeWidthStr;

// Matches the Git-tab diff panel's gutter widths (" NNNNN  NNNNN  " / " NNNNN ").
const DIFF_GUTTER_WIDTH: usize = 15;
const SBS_GUTTER_WIDTH: usize = 7;

pub fn render(f: &mut Frame, app: &mut App, area: Rect, _focused: bool) {
    let theme = app.theme;
    // Cache viewport so search-jump can center.
    app.last_commit_detail_view_h = area.height;

    // Build rows at a virtual width = viewport + current h_scroll, so content
    // and pad extend far enough right for `clip_spans` below to reveal rows
    // past `area.width`. Mirrors the diff_panel / file_preview_panel strategy.
    let h = app.commit_detail.diff_h_scroll;
    let virtual_w = (area.width as usize)
        .saturating_add(h)
        .min(u16::MAX as usize) as u16;
    let rows = build_rows(app, virtual_w, &theme);
    let total = rows.len();

    let max_scroll = total.saturating_sub(area.height as usize);
    if app.commit_detail.scroll > max_scroll {
        app.commit_detail.scroll = max_scroll;
    }
    let scroll = app.commit_detail.scroll;

    // Clamp h_scroll against the widest row currently in view. `build_rows`
    // fills pad/decoration out to `virtual_w` so the diff bg spans the
    // viewport — using raw span widths here would make max_h track
    // virtual_w and the clamp never converge. `Row.content_width` is the
    // per-row "useful width" that excludes trailing pad.
    let max_visible_w: usize = rows
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .map(|r| r.content_width)
        .max()
        .unwrap_or(0);
    let max_h = max_visible_w.saturating_sub(area.width as usize);
    if app.commit_detail.diff_h_scroll > max_h {
        app.commit_detail.diff_h_scroll = max_h;
    }
    let h = app.commit_detail.diff_h_scroll;

    for (i, row) in rows
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .enumerate()
    {
        let y = area.y + i as u16;
        let row_idx = scroll + i;
        let hover = crate::ui::hover::is_hover(app, area, y);
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

        // Hit registry must account for h_scroll: a span at logical x=200
        // with h=150 renders at screen x=50, not x=200. Intersect each
        // clickable span with [h, h + area.width) and register the clipped
        // on-screen portion.
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
            if oid.is_empty() || path.is_empty() {
                return true;
            }
            if app.git_graph.selected_commit.as_deref() != Some(oid) {
                if let Some(idx) = app.git_graph.rows.iter().position(|r| r.commit.oid == oid) {
                    app.git_graph.selected_idx = idx;
                }
                app.git_graph.selected_commit = Some(oid.to_string());
                app.load_commit_detail();
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
}

fn from_ratatui_span(span: Span<'static>) -> RowSpan {
    RowSpan {
        text: span.content.into_owned(),
        style: span.style,
        click: None,
    }
}

// ─── Row construction ─────────────────────────────────────────────────────────

fn build_rows(app: &App, width: u16, theme: &Theme) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let cd = &app.commit_detail;

    let Some(detail) = &cd.detail else {
        rows.push(Row::new(vec![RowSpan::styled(
            t(Msg::CommitDetailEmpty),
            Style::default().fg(theme.fg_secondary),
        )]));
        return rows;
    };

    let info = &detail.info;
    let max_msg = (width as usize).saturating_sub(4);
    let max_path = (width as usize).saturating_sub(6);

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

    let view_label = if cd.files_tree_mode {
        t(Msg::ViewTree)
    } else {
        t(Msg::ViewList)
    };
    rows.push(Row::new(vec![
        RowSpan::styled(
            crate::i18n::changed_files_header(detail.files.len()),
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
        selected_file: cd.file_diff.as_ref().map(|(p, _)| p.as_str()),
        sel_bg: theme.selection_bg,
        max_path,
        commit_oid: &info.oid,
        collapsed: &cd.files_collapsed,
        fg: theme.fg_primary,
    };

    if cd.files_tree_mode {
        let nodes = gtree::build(&detail.files);
        render_commit_file_tree(&nodes, 1, &ctx, &mut rows, theme);
    } else {
        for file in &detail.files {
            rows.push(commit_file_row(file, &file.path, "  ", &ctx));
        }
    }

    if let Some((path, diff)) = &cd.file_diff {
        rows.push(Row::blank());
        rows.push(diff_header_row(
            path,
            cd.diff_layout,
            cd.diff_mode,
            width,
            theme,
        ));
        rows.push(diff_separator_row(width, theme));
        append_diff_rows(&mut rows, diff, cd.diff_layout, width, theme);
    }

    rows
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
    layout: DiffLayout,
    width: u16,
    theme: &Theme,
) {
    match layout {
        DiffLayout::Unified => append_unified_diff(rows, diff, width, theme),
        DiffLayout::SideBySide => append_side_by_side_diff(rows, diff, width, theme),
    }
}

fn append_unified_diff(rows: &mut Vec<Row>, diff: &DiffContent, width: u16, theme: &Theme) {
    let added_bg = theme.added_bg;
    let removed_bg = theme.removed_bg;

    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
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

        for line in &hunk.lines {
            let (prefix, fg, bg) = match line.tag {
                LineTag::Added => ("+", theme.added_accent, Some(added_bg)),
                LineTag::Removed => ("-", theme.removed_accent, Some(removed_bg)),
                LineTag::Context => (" ", theme.fg_primary, None),
            };
            let old_no = fmt_diff_lineno(line.old_lineno);
            let new_no = fmt_diff_lineno(line.new_lineno);
            let max_text = (width as usize).saturating_sub(DIFF_GUTTER_WIDTH);
            let content = truncate_to_display_width(&line.content, max_text).to_string();
            let pad = max_text.saturating_sub(UnicodeWidthStr::width(content.as_str()));

            let gutter_style = Style::default().fg(theme.fg_secondary);
            let mark_style = Style::default().fg(fg);
            let text_style = Style::default().fg(fg);
            let (g, m, t, p) = match bg {
                Some(bg) => (
                    gutter_style.bg(bg),
                    mark_style.bg(bg),
                    text_style.bg(bg),
                    Style::default().bg(bg),
                ),
                None => (gutter_style, mark_style, text_style, Style::default()),
            };
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
            rows.push(
                Row::new(vec![
                    RowSpan::styled(format!(" {}  {}  ", old_no, new_no), g),
                    RowSpan::styled(format!("{} ", prefix), m),
                    RowSpan::styled(content, t),
                    RowSpan::styled(" ".repeat(pad), p),
                ])
                .with_content_width(line_content_w),
            );
        }
    }
}

fn append_side_by_side_diff(rows: &mut Vec<Row>, diff: &DiffContent, width: u16, theme: &Theme) {
    let half = width.saturating_sub(1) / 2;
    let right_w = width.saturating_sub(half + 1);

    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
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

        for row in pair_hunk_lines(hunk) {
            rows.push(render_sbs_row(&row, half, right_w, theme));
        }
    }
}

struct SbsRow {
    left_tag: LineTag,
    left_no: Option<u32>,
    left_text: String,
    right_tag: LineTag,
    right_no: Option<u32>,
    right_text: String,
}

fn render_sbs_row(row: &SbsRow, half_w: u16, right_w: u16, theme: &Theme) -> Row {
    let mut spans: Vec<RowSpan> = Vec::new();
    let (left_fg, left_bg) = side_style(row.left_tag, theme);
    let (right_fg, right_bg) = side_style(row.right_tag, theme);

    push_sbs_half(
        &mut spans,
        row.left_no,
        &row.left_text,
        left_fg,
        left_bg,
        half_w,
        theme,
    );
    spans.push(RowSpan::styled(
        "│".to_string(),
        Style::default().fg(theme.fg_secondary),
    ));
    push_sbs_half(
        &mut spans,
        row.right_no,
        &row.right_text,
        right_fg,
        right_bg,
        right_w,
        theme,
    );
    // Content width: both halves' gutters + their real text widths + the
    // 1-col separator. Excludes the trailing pad in each half, so clamp
    // converges on the real content instead of `virtual_w`.
    let left_tw = UnicodeWidthStr::width(row.left_text.as_str());
    let right_tw = UnicodeWidthStr::width(row.right_text.as_str());
    let content_w = SBS_GUTTER_WIDTH
        .saturating_add(left_tw)
        .saturating_add(1)
        .saturating_add(SBS_GUTTER_WIDTH)
        .saturating_add(right_tw);
    Row::new(spans).with_content_width(content_w)
}

fn push_sbs_half(
    spans: &mut Vec<RowSpan>,
    lineno: Option<u32>,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    width: u16,
    theme: &Theme,
) {
    let content_w = (width as usize).saturating_sub(SBS_GUTTER_WIDTH);
    let trimmed = truncate_to_display_width(text, content_w);
    let pad = content_w.saturating_sub(UnicodeWidthStr::width(trimmed));

    let gutter_style = Style::default().fg(theme.fg_secondary);
    let body_style = Style::default().fg(fg);
    let (g, b, p) = match bg {
        Some(bg) => (
            gutter_style.bg(bg),
            body_style.bg(bg),
            Style::default().bg(bg),
        ),
        None => (gutter_style, body_style, Style::default()),
    };
    spans.push(RowSpan::styled(format!(" {} ", fmt_diff_lineno(lineno)), g));
    spans.push(RowSpan::styled(trimmed.to_string(), b));
    spans.push(RowSpan::styled(" ".repeat(pad), p));
}

fn side_style(tag: LineTag, theme: &Theme) -> (Color, Option<Color>) {
    match tag {
        LineTag::Added => (theme.added_accent, Some(theme.added_bg)),
        LineTag::Removed => (theme.removed_accent, Some(theme.removed_bg)),
        LineTag::Context => (theme.fg_primary, None),
    }
}

fn pair_hunk_lines(hunk: &crate::git::DiffHunk) -> Vec<SbsRow> {
    let mut rows = Vec::new();
    let mut pending_removed: Vec<(Option<u32>, String)> = Vec::new();

    for line in &hunk.lines {
        match line.tag {
            LineTag::Removed => {
                pending_removed.push((line.old_lineno, line.content.clone()));
            }
            LineTag::Added => {
                if let Some((old_no, old_text)) =
                    (!pending_removed.is_empty()).then(|| pending_removed.remove(0))
                {
                    rows.push(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                    });
                } else {
                    rows.push(SbsRow {
                        left_tag: LineTag::Context,
                        left_no: None,
                        left_text: String::new(),
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                    });
                }
            }
            LineTag::Context => {
                for (old_no, old_text) in pending_removed.drain(..) {
                    rows.push(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        right_tag: LineTag::Context,
                        right_no: None,
                        right_text: String::new(),
                    });
                }
                rows.push(SbsRow {
                    left_tag: LineTag::Context,
                    left_no: line.old_lineno,
                    left_text: line.content.clone(),
                    right_tag: LineTag::Context,
                    right_no: line.new_lineno,
                    right_text: line.content.clone(),
                });
            }
        }
    }

    for (old_no, old_text) in pending_removed.drain(..) {
        rows.push(SbsRow {
            left_tag: LineTag::Removed,
            left_no: old_no,
            left_text: old_text,
            right_tag: LineTag::Context,
            right_no: None,
            right_text: String::new(),
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
/// `commit_detail.scroll` and with what the panel draws. Width is passed as
/// `u16::MAX` so row construction skips display-width truncation and the
/// searchable text mirrors the underlying data.
pub fn searchable_rows(app: &App) -> Vec<String> {
    let rows = build_rows(app, u16::MAX, &app.theme);
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

    #[test]
    fn row_new_sums_span_widths() {
        let row = Row::new(vec![RowSpan::plain("hello"), RowSpan::plain(" world")]);
        assert_eq!(row.content_width, 11);
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
}
