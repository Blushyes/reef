use crate::app::App;
use crate::git::{DiffContent, LineTag};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use ratatui::Frame;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match &app.diff_content {
        None => render_empty(f, inner),
        Some(diff) => render_diff(f, app, inner, diff),
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
    let x = area.x + area.width.saturating_sub(22) / 2; // rough centering
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

fn render_diff(f: &mut Frame, app: &App, area: Rect, diff: &DiffContent) {
    let mut y = area.y;
    let max_y = area.y + area.height;

    // File header
    if y >= max_y {
        return;
    }
    let header = Line::from(vec![
        Span::styled(
            &diff.file_path,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(header, Rect::new(area.x, y, area.width, 1));
    y += 1;

    // Separator
    if y >= max_y {
        return;
    }
    let sep = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(sep, Rect::new(area.x, y, area.width, 1));
    y += 1;

    // Build all diff lines first, then apply scroll
    let mut all_lines: Vec<DiffDisplayLine> = Vec::new();

    for (i, hunk) in diff.hunks.iter().enumerate() {
        // Hunk separator (except first)
        if i > 0 {
            all_lines.push(DiffDisplayLine::Separator);
        }

        // Hunk header
        all_lines.push(DiffDisplayLine::HunkHeader(hunk.header.clone()));

        for line in &hunk.lines {
            all_lines.push(DiffDisplayLine::Content {
                tag: line.tag,
                old_lineno: line.old_lineno,
                new_lineno: line.new_lineno,
                text: line.content.clone(),
            });
        }
    }

    // Apply scroll
    let scroll = app.diff_scroll;
    let visible_lines = all_lines.iter().skip(scroll);

    for dl in visible_lines {
        if y >= max_y {
            break;
        }

        match dl {
            DiffDisplayLine::Separator => {
                let sep_line = Line::from(Span::styled(
                    format!(" {:>5}  {:>5}  {}", "", "", "⋯"),
                    Style::default().fg(Color::DarkGray),
                ));
                f.render_widget(sep_line, Rect::new(area.x, y, area.width, 1));
            }
            DiffDisplayLine::HunkHeader(header) => {
                let line = Line::from(Span::styled(
                    format!(" {}", header),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::DIM),
                ));
                f.render_widget(line, Rect::new(area.x, y, area.width, 1));
            }
            DiffDisplayLine::Content {
                tag,
                old_lineno,
                new_lineno,
                text,
            } => {
                let (prefix, fg, bg) = match tag {
                    LineTag::Added => ("+", Color::Green, Color::Rgb(0, 40, 0)),
                    LineTag::Removed => ("-", Color::Red, Color::Rgb(60, 0, 0)),
                    LineTag::Context => (" ", Color::Gray, Color::Reset),
                };

                let old_num = old_lineno
                    .map(|n| format!("{:>5}", n))
                    .unwrap_or_else(|| "     ".to_string());
                let new_num = new_lineno
                    .map(|n| format!("{:>5}", n))
                    .unwrap_or_else(|| "     ".to_string());

                // Truncate text to fit
                let gutter_width = 15; // " XXXXX  XXXXX  "
                let max_text = (area.width as usize).saturating_sub(gutter_width);
                let display_text = if text.len() > max_text {
                    &text[..max_text]
                } else {
                    text
                };

                let line = Line::from(vec![
                    Span::styled(
                        format!(" {}  {} ", old_num, new_num),
                        Style::default().fg(Color::DarkGray).bg(bg),
                    ),
                    Span::styled(
                        format!("{} ", prefix),
                        Style::default().fg(fg).bg(bg),
                    ),
                    Span::styled(
                        display_text.to_string(),
                        Style::default().fg(fg).bg(bg),
                    ),
                    // Fill rest of line with bg color
                    Span::styled(
                        " ".repeat(
                            max_text
                                .saturating_sub(display_text.len())
                                .min(area.width as usize),
                        ),
                        Style::default().bg(bg),
                    ),
                ]);
                f.render_widget(line, Rect::new(area.x, y, area.width, 1));
            }
        }
        y += 1;
    }
}

enum DiffDisplayLine {
    Separator,
    HunkHeader(String),
    Content {
        tag: LineTag,
        old_lineno: Option<u32>,
        new_lineno: Option<u32>,
        text: String,
    },
}
