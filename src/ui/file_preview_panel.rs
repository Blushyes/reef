use crate::app::App;
use crate::file_tree::PreviewContent;
use crate::ui::text::{clip_spans, skip_n_columns, truncate_to_width};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let preview = match app.preview_content.take() {
        None => {
            render_empty(f, inner);
            return;
        }
        Some(preview) => preview,
    };

    if preview.is_binary {
        render_binary(f, inner, &preview.file_path);
    } else {
        render_content(f, app, inner, &preview);
    }

    app.preview_content = Some(preview);
}

fn render_empty(f: &mut Frame, area: Rect) {
    if area.height < 1 {
        return;
    }
    let msg = Line::from(Span::styled(
        "选择一个文件预览内容",
        Style::default().fg(Color::DarkGray),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(20) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

fn render_binary(f: &mut Frame, area: Rect, path: &str) {
    if area.height < 2 {
        return;
    }
    let header = Line::from(Span::styled(
        path,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
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

fn render_content(f: &mut Frame, app: &mut App, area: Rect, preview: &PreviewContent) {
    let mut y = area.y;
    let max_y = area.y + area.height;

    // File header
    if y < max_y {
        let header = Line::from(Span::styled(
            preview.file_path.as_str(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
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
    let max_scroll = preview.lines.len().saturating_sub(content_height);
    app.preview_scroll = app.preview_scroll.min(max_scroll);

    let gutter_w = 6usize; // " NNNNN "
    let content_w = (area.width as usize).saturating_sub(gutter_w);

    // Clamp horizontal scroll against the widest line currently in view.
    let max_visible_w: usize = preview
        .lines
        .iter()
        .skip(app.preview_scroll)
        .take(content_height)
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0);
    let max_h = max_visible_w.saturating_sub(content_w);
    app.preview_h_scroll = app.preview_h_scroll.min(max_h);
    let h = app.preview_h_scroll;

    for (i, line) in preview.lines.iter().skip(app.preview_scroll).enumerate() {
        let cy = y + i as u16;
        if cy >= max_y {
            break;
        }
        let real_idx = app.preview_scroll + i;
        let lineno = real_idx + 1;

        let gutter = Span::styled(
            format!("{:>5} ", lineno),
            Style::default().fg(Color::DarkGray),
        );

        let mut spans = vec![gutter];
        match preview.highlighted.as_ref().and_then(|h| h.get(real_idx)) {
            Some(tokens) => spans.extend(clip_spans(tokens, h, content_w)),
            None => {
                let shifted = skip_n_columns(line, h);
                let display = truncate_to_width(shifted, content_w);
                spans.push(Span::styled(display, Style::default().fg(Color::Gray)));
            }
        }

        let rendered = Line::from(spans);
        f.render_widget(rendered, Rect::new(area.x, cy, area.width, 1));
    }
}
