use crate::ui::focus::header_title_style;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

pub fn render_card_header(
    f: &mut Frame,
    area: Rect,
    path: &str,
    theme: &crate::ui::theme::Theme,
    focused: bool,
    match_count: Option<(usize, usize)>,
) -> u16 {
    let mut y = area.y;
    let max_y = area.y + area.height;
    if y >= max_y {
        return y;
    }

    let title_style = header_title_style(theme, focused);
    let count_text = match_count.map(|(cur, total)| format!("[{cur}/{total}]"));
    let count_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);

    let path_w = UnicodeWidthStr::width(path) as u16;
    let count_w = count_text
        .as_deref()
        .map(|c| UnicodeWidthStr::width(c) as u16)
        .unwrap_or(0);
    let fits_counter =
        count_text.is_some() && path_w.saturating_add(1).saturating_add(count_w) <= area.width;

    let title_rect = Rect::new(area.x, y, area.width, 1);
    if fits_counter {
        let count = count_text.as_deref().unwrap();
        let pad_w = area.width.saturating_sub(path_w).saturating_sub(count_w);
        let pad = " ".repeat(pad_w as usize);
        f.render_widget(
            Line::from(vec![
                Span::styled(path, title_style),
                Span::raw(pad),
                Span::styled(count, count_style),
            ]),
            title_rect,
        );
    } else {
        f.render_widget(Line::from(Span::styled(path, title_style)), title_rect);
    }
    y += 1;
    if y < max_y {
        let sep_color = if focused {
            theme.accent
        } else {
            theme.fg_secondary
        };
        f.render_widget(
            Line::from(Span::styled(
                "─".repeat(area.width as usize),
                Style::default().fg(sep_color),
            )),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }
    y
}
