use crate::app::{App, DiffLayout};
use crate::git::{DiffContent, DiffHunk, LineTag};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match &app.diff_content {
        None => render_empty(f, inner),
        Some(diff) => {
            let diff = diff.clone();
            match app.diff_layout {
                DiffLayout::Unified => render_unified(f, app, inner, &diff),
                DiffLayout::SideBySide => render_side_by_side(f, app, inner, &diff),
            }
        }
    }
}

fn render_empty(f: &mut Frame, area: Rect) {
    if area.height < 1 {
        return;
    }
    let msg = Line::from(Span::styled(
        "选择一个文件查看 diff",
        Style::default().fg(Color::DarkGray),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(22) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

// ─── Unified view ────────────────────────────────────────────────────────────

fn render_unified(f: &mut Frame, app: &App, area: Rect, diff: &DiffContent) {
    let mut y = area.y;
    let max_y = area.y + area.height;

    render_file_header(f, app, area, diff, &mut y, max_y);

    // Build all display lines
    let mut all_lines: Vec<UnifiedLine> = Vec::new();
    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            all_lines.push(UnifiedLine::Separator);
        }
        all_lines.push(UnifiedLine::HunkHeader(hunk.header.clone()));
        for line in &hunk.lines {
            all_lines.push(UnifiedLine::Content {
                tag: line.tag,
                old_lineno: line.old_lineno,
                new_lineno: line.new_lineno,
                text: line.content.clone(),
            });
        }
    }

    let scroll = app.diff_scroll;
    for dl in all_lines.iter().skip(scroll) {
        if y >= max_y {
            break;
        }
        render_unified_line(f, area, y, dl);
        y += 1;
    }
}

fn render_unified_line(f: &mut Frame, area: Rect, y: u16, dl: &UnifiedLine) {
    match dl {
        UnifiedLine::Separator => {
            let line = Line::from(Span::styled(
                format!(" {:>5}  {:>5}  ⋯", "", ""),
                Style::default().fg(Color::DarkGray),
            ));
            f.render_widget(line, Rect::new(area.x, y, area.width, 1));
        }
        UnifiedLine::HunkHeader(header) => {
            let line = Line::from(Span::styled(
                format!(" {}", header),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            ));
            f.render_widget(line, Rect::new(area.x, y, area.width, 1));
        }
        UnifiedLine::Content {
            tag,
            old_lineno,
            new_lineno,
            text,
        } => {
            let (prefix, fg, bg) = line_style(*tag);
            let old_num = fmt_lineno(*old_lineno);
            let new_num = fmt_lineno(*new_lineno);

            let gutter_width = 15usize; // " XXXXX  XXXXX  "
            let max_text = (area.width as usize).saturating_sub(gutter_width);
            let display_text = truncate_to_width(text, max_text);
            let pad = max_text.saturating_sub(UnicodeWidthStr::width(display_text));

            let line = Line::from(vec![
                Span::styled(
                    format!(" {}  {} ", old_num, new_num),
                    Style::default().fg(Color::DarkGray).bg(bg),
                ),
                Span::styled(format!("{} ", prefix), Style::default().fg(fg).bg(bg)),
                Span::styled(display_text.to_string(), Style::default().fg(fg).bg(bg)),
                Span::styled(
                    " ".repeat(pad.min(area.width as usize)),
                    Style::default().bg(bg),
                ),
            ]);
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
    },
}

// ─── Side-by-side view ───────────────────────────────────────────────────────

struct SbsRow {
    left_tag: LineTag,
    left_no: Option<u32>,
    left_text: String,
    right_tag: LineTag,
    right_no: Option<u32>,
    right_text: String,
}

enum SbsDisplayLine {
    Separator,
    HunkHeader(String),
    Row(SbsRow),
}

fn build_sbs_lines(hunk: &DiffHunk) -> Vec<SbsDisplayLine> {
    let mut rows: Vec<SbsDisplayLine> = Vec::new();
    let mut pending_removed: Vec<(Option<u32>, String)> = Vec::new();

    rows.push(SbsDisplayLine::HunkHeader(hunk.header.clone()));

    for line in &hunk.lines {
        match line.tag {
            LineTag::Removed => {
                pending_removed.push((line.old_lineno, line.content.clone()));
            }
            LineTag::Added => {
                if let Some((old_no, old_text)) = pending_removed.first().cloned() {
                    pending_removed.remove(0);
                    rows.push(SbsDisplayLine::Row(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                    }));
                } else {
                    // Added with no paired removal
                    rows.push(SbsDisplayLine::Row(SbsRow {
                        left_tag: LineTag::Context,
                        left_no: None,
                        left_text: String::new(),
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: line.content.clone(),
                    }));
                }
            }
            LineTag::Context => {
                // Flush pending removed (no matching additions)
                for (old_no, old_text) in pending_removed.drain(..) {
                    rows.push(SbsDisplayLine::Row(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        right_tag: LineTag::Context,
                        right_no: None,
                        right_text: String::new(),
                    }));
                }
                // Context appears on both sides
                rows.push(SbsDisplayLine::Row(SbsRow {
                    left_tag: LineTag::Context,
                    left_no: line.old_lineno,
                    left_text: line.content.clone(),
                    right_tag: LineTag::Context,
                    right_no: line.new_lineno,
                    right_text: line.content.clone(),
                }));
            }
        }
    }

    // Flush any remaining pending removed
    for (old_no, old_text) in pending_removed.drain(..) {
        rows.push(SbsDisplayLine::Row(SbsRow {
            left_tag: LineTag::Removed,
            left_no: old_no,
            left_text: old_text,
            right_tag: LineTag::Context,
            right_no: None,
            right_text: String::new(),
        }));
    }

    rows
}

fn render_side_by_side(f: &mut Frame, app: &App, area: Rect, diff: &DiffContent) {
    let mut y = area.y;
    let max_y = area.y + area.height;

    render_file_header(f, app, area, diff, &mut y, max_y);

    // Build all display lines
    let mut all_lines: Vec<SbsDisplayLine> = Vec::new();
    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            all_lines.push(SbsDisplayLine::Separator);
        }
        all_lines.extend(build_sbs_lines(hunk));
    }

    // Half width: leave 1 col for center divider
    let half_w = (area.width.saturating_sub(1)) / 2;
    let right_w = area.width.saturating_sub(half_w + 1);

    let scroll = app.diff_scroll;
    for dl in all_lines.iter().skip(scroll) {
        if y >= max_y {
            break;
        }
        match dl {
            SbsDisplayLine::Separator => {
                let line = Line::from(Span::styled(
                    format!(
                        " {:>5}  ⋯{}",
                        "",
                        " ".repeat(area.width.saturating_sub(10) as usize)
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
                f.render_widget(line, Rect::new(area.x, y, area.width, 1));
            }
            SbsDisplayLine::HunkHeader(header) => {
                let line = Line::from(Span::styled(
                    format!(" {}", header),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                ));
                f.render_widget(line, Rect::new(area.x, y, area.width, 1));
            }
            SbsDisplayLine::Row(row) => {
                render_sbs_row(f, area, y, row, half_w, right_w);
            }
        }
        y += 1;
    }
}

fn render_sbs_row(f: &mut Frame, area: Rect, y: u16, row: &SbsRow, half_w: u16, right_w: u16) {
    // Gutter: " XXXXX " = 7 cols
    let gutter = 7usize;

    // ── Left half ──
    let left_content_w = (half_w as usize).saturating_sub(gutter);
    let (_, left_fg, left_bg) = line_style(row.left_tag);
    let left_no = fmt_lineno(row.left_no);
    let left_text = truncate_to_width(&row.left_text, left_content_w);
    let left_pad = left_content_w.saturating_sub(UnicodeWidthStr::width(left_text));

    let left_line = Line::from(vec![
        Span::styled(
            format!(" {} ", left_no),
            Style::default().fg(Color::DarkGray).bg(left_bg),
        ),
        Span::styled(
            left_text.to_string(),
            Style::default().fg(left_fg).bg(left_bg),
        ),
        Span::styled(" ".repeat(left_pad), Style::default().bg(left_bg)),
    ]);
    f.render_widget(left_line, Rect::new(area.x, y, half_w, 1));

    // ── Divider ──
    let div_x = area.x + half_w;
    let div_line = Line::from(Span::styled("│", Style::default().fg(Color::DarkGray)));
    f.render_widget(div_line, Rect::new(div_x, y, 1, 1));

    // ── Right half ──
    let right_content_w = (right_w as usize).saturating_sub(gutter);
    let (_, right_fg, right_bg) = line_style(row.right_tag);
    let right_no = fmt_lineno(row.right_no);
    let right_text = truncate_to_width(&row.right_text, right_content_w);
    let right_pad = right_content_w.saturating_sub(UnicodeWidthStr::width(right_text));

    let right_line = Line::from(vec![
        Span::styled(
            format!(" {} ", right_no),
            Style::default().fg(Color::DarkGray).bg(right_bg),
        ),
        Span::styled(
            right_text.to_string(),
            Style::default().fg(right_fg).bg(right_bg),
        ),
        Span::styled(" ".repeat(right_pad), Style::default().bg(right_bg)),
    ]);
    f.render_widget(right_line, Rect::new(div_x + 1, y, right_w, 1));
}

// ─── Shared helpers ──────────────────────────────────────────────────────────

fn render_file_header(
    f: &mut Frame,
    app: &App,
    area: Rect,
    diff: &DiffContent,
    y: &mut u16,
    max_y: u16,
) {
    if *y >= max_y {
        return;
    }

    let layout_label = match app.diff_layout {
        DiffLayout::Unified => "上下",
        DiffLayout::SideBySide => "左右",
    };
    let mode_label = match app.diff_mode {
        crate::app::DiffMode::Compact => "局部",
        crate::app::DiffMode::FullFile => "全量",
    };

    let tag_str = format!("  [{}][{}]  m/f 切换", layout_label, mode_label);
    let tag_len = UnicodeWidthStr::width(tag_str.as_str()) as u16;
    let path_max = area.width.saturating_sub(tag_len) as usize;
    let path_display = truncate_to_width(&diff.file_path, path_max);

    let header = Line::from(vec![
        Span::styled(
            path_display.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(tag_str, Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(header, Rect::new(area.x, *y, area.width, 1));
    *y += 1;

    // Separator
    if *y >= max_y {
        return;
    }
    let sep = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(sep, Rect::new(area.x, *y, area.width, 1));
    *y += 1;
}

fn line_style(tag: LineTag) -> (&'static str, Color, Color) {
    match tag {
        LineTag::Added => ("+", Color::Green, Color::Rgb(0, 40, 0)),
        LineTag::Removed => ("-", Color::Red, Color::Rgb(60, 0, 0)),
        LineTag::Context => (" ", Color::Gray, Color::Reset),
    }
}

fn fmt_lineno(n: Option<u32>) -> String {
    n.map(|v| format!("{:>5}", v))
        .unwrap_or_else(|| "     ".to_string())
}

/// Truncate a string to fit within `max_width` display columns.
fn truncate_to_width(s: &str, max_width: usize) -> &str {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{DiffHunk, DiffLine, LineTag};

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
        let (prefix, fg, _bg) = line_style(LineTag::Added);
        assert_eq!(prefix, "+");
        assert_eq!(fg, Color::Green);
    }

    #[test]
    fn line_style_removed() {
        let (prefix, fg, _bg) = line_style(LineTag::Removed);
        assert_eq!(prefix, "-");
        assert_eq!(fg, Color::Red);
    }

    #[test]
    fn line_style_context() {
        let (prefix, _fg, _bg) = line_style(LineTag::Context);
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

    // ── truncate_to_width ────────────────────────────────────────────────────

    #[test]
    fn truncate_to_width_ascii_within_limit() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_width_ascii_exact() {
        assert_eq!(truncate_to_width("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_width_ascii_over_limit() {
        assert_eq!(truncate_to_width("hello world", 5), "hello");
    }

    #[test]
    fn truncate_to_width_cjk() {
        // Each CJK char is width 2; max_width=4 → two chars fit
        assert_eq!(truncate_to_width("你好世界", 4), "你好");
    }

    #[test]
    fn truncate_to_width_cjk_within_limit() {
        assert_eq!(truncate_to_width("你好", 4), "你好");
    }

    // ── build_sbs_lines ──────────────────────────────────────────────────────

    #[test]
    fn build_sbs_lines_starts_with_hunk_header() {
        let hunk = make_hunk("@@ -1,2 +1,2 @@", vec![]);
        let lines = build_sbs_lines(&hunk);
        assert!(matches!(lines.first(), Some(SbsDisplayLine::HunkHeader(_))));
    }

    #[test]
    fn build_sbs_lines_context_appears_on_both_sides() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Context, "same", Some(1), Some(1))],
        );
        let lines = build_sbs_lines(&hunk);
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
        let lines = build_sbs_lines(&hunk);
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
        let lines = build_sbs_lines(&hunk);
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
        let lines = build_sbs_lines(&hunk);
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
        assert_eq!(count_rows(&build_sbs_lines(&hunk)), 2);
    }
}
