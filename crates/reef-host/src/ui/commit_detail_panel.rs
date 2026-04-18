//! Graph tab's right editor — commit metadata + Changed-files list (tree or
//! flat) + inline diff for the currently-selected file.

use crate::app::{App, DiffLayout, DiffMode};
use crate::git::tree::{self as gtree, Node};
use crate::git::{DiffContent, FileEntry, FileStatus, LineTag};
use crate::mouse::ClickAction;
use crate::ui::git_graph_panel;
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
    let rows = build_rows(app, area.width);
    let total = rows.len();

    let max_scroll = total.saturating_sub(area.height as usize);
    if app.commit_detail.scroll > max_scroll {
        app.commit_detail.scroll = max_scroll;
    }
    let scroll = app.commit_detail.scroll;

    for (i, row) in rows
        .iter()
        .skip(scroll)
        .take(area.height as usize)
        .enumerate()
    {
        let y = area.y + i as u16;
        let spans: Vec<Span<'static>> = row
            .spans
            .iter()
            .map(|s| Span::styled(s.text.clone(), s.style))
            .collect();
        f.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));

        let mut x = area.x;
        for span in &row.spans {
            let w = UnicodeWidthStr::width(span.text.as_str()) as u16;
            if w == 0 {
                continue;
            }
            let (cmd, args) = match (&span.click, &row.row_click) {
                (Some(c), _) => (Some(c.0.clone()), Some(c.1.clone())),
                (None, Some(r)) => (Some(r.0.clone()), Some(r.1.clone())),
                (None, None) => (None, None),
            };
            if let Some(cmd) = cmd {
                app.hit_registry.register_row(
                    x,
                    y,
                    w,
                    ClickAction::GitCommand {
                        command: cmd,
                        args: args.unwrap_or(Value::Null),
                        dbl_command: None,
                        dbl_args: None,
                    },
                );
            }
            x = x.saturating_add(w);
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
        Self {
            spans,
            row_click: None,
        }
    }
    fn blank() -> Self {
        Row::new(vec![RowSpan::plain("")])
    }
    fn on_click(mut self, cmd: &str, args: Value) -> Self {
        self.row_click = Some((cmd.to_string(), args));
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

fn build_rows(app: &App, width: u16) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let cd = &app.commit_detail;

    let Some(detail) = &cd.detail else {
        rows.push(Row::new(vec![RowSpan::styled(
            "  选择一个 commit 查看详情",
            Style::default().fg(Color::DarkGray),
        )]));
        return rows;
    };

    let info = &detail.info;
    let max_msg = (width as usize).saturating_sub(4);
    let max_path = (width as usize).saturating_sub(6);

    rows.push(Row::new(vec![
        RowSpan::styled("commit ", Style::default().fg(Color::DarkGray)),
        RowSpan::styled(
            info.oid.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    rows.push(Row::new(vec![
        RowSpan::styled("Author: ", Style::default().fg(Color::DarkGray)),
        RowSpan::styled(
            format!("{} <{}>", info.author_name, info.author_email),
            Style::default().fg(Color::White),
        ),
    ]));
    rows.push(Row::new(vec![
        RowSpan::styled("Date:   ", Style::default().fg(Color::DarkGray)),
        RowSpan::styled(
            format_timestamp(info.time),
            Style::default().fg(Color::White),
        ),
    ]));

    if let Some(labels) = app.git_graph.ref_map.get(&info.oid) {
        let mut spans: Vec<RowSpan> = vec![RowSpan::styled(
            "Refs:   ",
            Style::default().fg(Color::DarkGray),
        )];
        for label in labels {
            spans.push(from_ratatui_span(git_graph_panel::ref_label_span(label)));
            spans.push(RowSpan::plain(" "));
        }
        rows.push(Row::new(spans));
    }

    rows.push(Row::blank());

    for raw in detail.message.lines() {
        let mut msg = raw.to_string();
        truncate_in_place(&mut msg, max_msg);
        rows.push(Row::new(vec![
            RowSpan::plain("    "),
            RowSpan::styled(msg, Style::default().fg(Color::White)),
        ]));
    }

    rows.push(Row::blank());

    let view_label = if cd.files_tree_mode {
        "树形"
    } else {
        "列表"
    };
    rows.push(Row::new(vec![
        RowSpan::styled(
            format!("Changed files ({})", detail.files.len()),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        RowSpan::styled(
            format!("  [{}]  t 切换", view_label),
            Style::default().fg(Color::DarkGray),
        )
        .on_click("git.toggleCommitFilesView", Value::Null),
    ]));

    let ctx = CommitFilesCtx {
        selected_file: cd.file_diff.as_ref().map(|(p, _)| p.as_str()),
        sel_bg: Color::Rgb(40, 60, 100),
        max_path,
        commit_oid: &info.oid,
        collapsed: &cd.files_collapsed,
    };

    if cd.files_tree_mode {
        let nodes = gtree::build(&detail.files);
        render_commit_file_tree(&nodes, 1, &ctx, &mut rows);
    } else {
        for file in &detail.files {
            rows.push(commit_file_row(file, &file.path, "  ", &ctx));
        }
    }

    if let Some((path, diff)) = &cd.file_diff {
        rows.push(Row::blank());
        rows.push(diff_header_row(path, cd.diff_layout, cd.diff_mode, width));
        rows.push(diff_separator_row(width));
        append_diff_rows(&mut rows, diff, cd.diff_layout, width);
    }

    rows
}

struct CommitFilesCtx<'a> {
    selected_file: Option<&'a str>,
    sel_bg: Color,
    max_path: usize,
    commit_oid: &'a str,
    collapsed: &'a std::collections::HashSet<String>,
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
        RowSpan::styled(
            display,
            apply_bg(Style::default().fg(Color::White), base_bg),
        ),
    ];

    Row::new(spans).on_click(
        "git.selectCommitFile",
        serde_json::json!({ "oid": ctx.commit_oid, "path": file.path }),
    )
}

fn render_commit_file_tree(
    nodes: &BTreeMap<String, Node>,
    depth: usize,
    ctx: &CommitFilesCtx,
    rows: &mut Vec<Row>,
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
                            Style::default().fg(Color::DarkGray),
                        ),
                        RowSpan::styled(
                            format!("{}/", name),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                    .on_click("git.toggleCommitDir", serde_json::json!({ "path": path })),
                );
                if !is_collapsed {
                    render_commit_file_tree(children, depth + 1, ctx, rows);
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

fn diff_header_row(path: &str, layout: DiffLayout, mode: DiffMode, width: u16) -> Row {
    let layout_label = match layout {
        DiffLayout::Unified => "上下",
        DiffLayout::SideBySide => "左右",
    };
    let mode_label = match mode {
        DiffMode::Compact => "局部",
        DiffMode::FullFile => "全量",
    };
    let tag_str = format!("  [{}][{}]  m/f 切换", layout_label, mode_label);
    let tag_w = UnicodeWidthStr::width(tag_str.as_str());
    let path_max = (width as usize).saturating_sub(tag_w);
    let path_display = truncate_to_display_width(path, path_max).to_string();

    Row::new(vec![
        RowSpan::styled(
            path_display,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        RowSpan::styled(tag_str, Style::default().fg(Color::DarkGray)),
    ])
}

fn diff_separator_row(width: u16) -> Row {
    Row::new(vec![RowSpan::styled(
        "─".repeat(width as usize),
        Style::default().fg(Color::DarkGray),
    )])
}

fn append_diff_rows(rows: &mut Vec<Row>, diff: &DiffContent, layout: DiffLayout, width: u16) {
    match layout {
        DiffLayout::Unified => append_unified_diff(rows, diff, width),
        DiffLayout::SideBySide => append_side_by_side_diff(rows, diff, width),
    }
}

fn append_unified_diff(rows: &mut Vec<Row>, diff: &DiffContent, width: u16) {
    let added_bg = Color::Rgb(0, 40, 0);
    let removed_bg = Color::Rgb(60, 0, 0);

    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            rows.push(Row::new(vec![RowSpan::styled(
                format!(" {:>5}  {:>5}  ⋯", "", ""),
                Style::default().fg(Color::DarkGray),
            )]));
        }
        rows.push(Row::new(vec![RowSpan::styled(
            format!(" {:>5}  {:>5}  {}", "", "", hunk.header),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
        )]));

        for line in &hunk.lines {
            let (prefix, fg, bg) = match line.tag {
                LineTag::Added => ("+", Color::Green, Some(added_bg)),
                LineTag::Removed => ("-", Color::Red, Some(removed_bg)),
                LineTag::Context => (" ", Color::White, None),
            };
            let old_no = fmt_diff_lineno(line.old_lineno);
            let new_no = fmt_diff_lineno(line.new_lineno);
            let max_text = (width as usize).saturating_sub(DIFF_GUTTER_WIDTH);
            let content = truncate_to_display_width(&line.content, max_text).to_string();
            let pad = max_text.saturating_sub(UnicodeWidthStr::width(content.as_str()));

            let gutter_style = Style::default().fg(Color::DarkGray);
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
            rows.push(Row::new(vec![
                RowSpan::styled(format!(" {}  {} ", old_no, new_no), g),
                RowSpan::styled(format!("{} ", prefix), m),
                RowSpan::styled(content, t),
                RowSpan::styled(" ".repeat(pad), p),
            ]));
        }
    }
}

fn append_side_by_side_diff(rows: &mut Vec<Row>, diff: &DiffContent, width: u16) {
    let half = width.saturating_sub(1) / 2;
    let right_w = width.saturating_sub(half + 1);

    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            rows.push(Row::new(vec![RowSpan::styled(
                format!(" {:>5}  ⋯", ""),
                Style::default().fg(Color::DarkGray),
            )]));
        }
        rows.push(Row::new(vec![RowSpan::styled(
            format!(" {:>5}  {}", "", hunk.header),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
        )]));

        for row in pair_hunk_lines(hunk) {
            rows.push(render_sbs_row(&row, half, right_w));
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

fn render_sbs_row(row: &SbsRow, half_w: u16, right_w: u16) -> Row {
    let added_bg = Color::Rgb(0, 40, 0);
    let removed_bg = Color::Rgb(60, 0, 0);

    let mut spans: Vec<RowSpan> = Vec::new();
    let (left_fg, left_bg) = side_style(row.left_tag, added_bg, removed_bg);
    let (right_fg, right_bg) = side_style(row.right_tag, added_bg, removed_bg);

    push_sbs_half(
        &mut spans,
        row.left_no,
        &row.left_text,
        left_fg,
        left_bg,
        half_w,
    );
    spans.push(RowSpan::styled(
        "│".to_string(),
        Style::default().fg(Color::DarkGray),
    ));
    push_sbs_half(
        &mut spans,
        row.right_no,
        &row.right_text,
        right_fg,
        right_bg,
        right_w,
    );
    Row::new(spans)
}

fn push_sbs_half(
    spans: &mut Vec<RowSpan>,
    lineno: Option<u32>,
    text: &str,
    fg: Color,
    bg: Option<Color>,
    width: u16,
) {
    let content_w = (width as usize).saturating_sub(SBS_GUTTER_WIDTH);
    let trimmed = truncate_to_display_width(text, content_w);
    let pad = content_w.saturating_sub(UnicodeWidthStr::width(trimmed));

    let gutter_style = Style::default().fg(Color::DarkGray);
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

fn side_style(tag: LineTag, added_bg: Color, removed_bg: Color) -> (Color, Option<Color>) {
    match tag {
        LineTag::Added => (Color::Green, Some(added_bg)),
        LineTag::Removed => (Color::Red, Some(removed_bg)),
        LineTag::Context => (Color::White, None),
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
