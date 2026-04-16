use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use ratatui::Frame;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    match &app.preview_content {
        None => render_empty(f, inner),
        Some(preview) if preview.is_binary => render_binary(f, inner, &preview.file_path),
        Some(preview) => {
            let file_path = preview.file_path.clone();
            let lines = preview.lines.clone();
            render_content(f, app, inner, &file_path, &lines);
        }
    }
}

fn render_empty(f: &mut Frame, area: Rect) {
    if area.height < 1 { return; }
    let msg = Line::from(Span::styled(
        "选择一个文件预览内容",
        Style::default().fg(Color::DarkGray),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(20) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

fn render_binary(f: &mut Frame, area: Rect, path: &str) {
    if area.height < 2 { return; }
    let header = Line::from(Span::styled(
        path,
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    ));
    f.render_widget(header, Rect::new(area.x, area.y, area.width, 1));

    let msg = Line::from(Span::styled(
        "(binary file)",
        Style::default().fg(Color::DarkGray),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(14) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

fn render_content(f: &mut Frame, app: &mut App, area: Rect, path: &str, lines: &[String]) {
    let mut y = area.y;
    let max_y = area.y + area.height;

    // File header
    if y < max_y {
        let header = Line::from(Span::styled(
            path,
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(header, Rect::new(area.x, y, area.width, 1));
        y += 1;
    }

    // Separator
    if y < max_y {
        let sep = Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(sep, Rect::new(area.x, y, area.width, 1));
        y += 1;
    }

    let content_height = (max_y - y) as usize;
    let max_scroll = lines.len().saturating_sub(content_height);
    app.preview_scroll = app.preview_scroll.min(max_scroll);

    let gutter_w = 6usize; // " NNNNN "
    let content_w = (area.width as usize).saturating_sub(gutter_w);

    for (i, line) in lines.iter().skip(app.preview_scroll).enumerate() {
        let cy = y + i as u16;
        if cy >= max_y { break; }
        let lineno = app.preview_scroll + i + 1;

        let display = truncate_to_width(line, content_w);

        let rendered = Line::from(vec![
            Span::styled(
                format!("{:>5} ", lineno),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(display, Style::default().fg(Color::Gray)),
        ]);
        f.render_widget(rendered, Rect::new(area.x, cy, area.width, 1));
    }
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
