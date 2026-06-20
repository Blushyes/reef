use crate::app::App;
use crate::find_widget::FindTarget;
use crate::search::SearchTarget;
use crate::ui::mouse::ClickAction;
use crate::ui::preview::chrome::render_card_header;
use crate::ui::text::{clip_spans, overlay_match_highlight, overlay_selection_highlight, spaces};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use reef_core::preview::PreviewDocument as PreviewContent;
use unicode_width::UnicodeWidthStr;

pub(in crate::ui) fn render(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    preview: &PreviewContent,
    markdown: &reef_core::markdown::MarkdownPreview,
    focused: bool,
) {
    let th = app.theme;
    let max_y = area.y + area.height;
    let y = render_card_header(f, area, &preview.path, &th, focused, None);
    let content_height = (max_y - y) as usize;
    app.last_preview_view_h = content_height as u16;
    let max_scroll = markdown.text_rows.len().saturating_sub(content_height);
    app.preview_scroll = app.preview_scroll.min(max_scroll);

    let content_w = area.width as usize;
    let max_visible_w = markdown
        .rows
        .iter()
        .skip(app.preview_scroll)
        .take(content_height)
        .map(|row| {
            row.iter()
                .map(|s| UnicodeWidthStr::width(s.text.as_str()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let max_h = max_visible_w.saturating_sub(content_w);
    app.preview_h_scroll = app.preview_h_scroll.min(max_h);
    let h = app.preview_h_scroll;

    app.last_preview_content_origin = None;
    app.last_markdown_content_origin = Some((area.x, y));
    let selection = app.preview_selection;

    for (i, row) in markdown.rows.iter().skip(app.preview_scroll).enumerate() {
        let cy = y + i as u16;
        if cy >= max_y {
            break;
        }
        let real_idx = app.preview_scroll + i;
        let base_tokens: Vec<(Style, std::borrow::Cow<'_, str>)> = row
            .iter()
            .scan(0usize, |pos, span| {
                let start = *pos;
                *pos += UnicodeWidthStr::width(span.text.as_str());
                let mut style = span_style(span, &th);
                if span.link.is_some() && span_hovered(app, area.x, cy, start, *pos, h, content_w) {
                    style = link_hover_style(style, &th);
                }
                Some((style, std::borrow::Cow::Borrowed(span.text.as_str())))
            })
            .collect();
        let (ranges, cur) = if app.find_widget.target == Some(FindTarget::FilePreview) {
            app.find_widget
                .ranges_on_row(FindTarget::FilePreview, real_idx)
        } else {
            app.search
                .ranges_on_row(SearchTarget::FilePreview, real_idx)
        };
        let tokens = if ranges.is_empty() {
            base_tokens
        } else {
            overlay_match_highlight(
                base_tokens,
                &ranges,
                cur,
                th.search_match,
                th.search_current,
            )
        };
        let tokens = match selection.as_ref().and_then(|s| {
            markdown
                .text_for_row(real_idx)
                .and_then(|text| s.line_byte_range(real_idx, text))
        }) {
            Some(r) if r.start < r.end => overlay_selection_highlight(tokens, r),
            _ => tokens,
        };
        let mut spans = clip_spans(&tokens, h, content_w);
        pad_row_bg(&mut spans, content_w, row_bg(row, &th));
        register_links(app, row, area.x, cy, h, content_w);
        f.render_widget(Line::from(spans), Rect::new(area.x, cy, area.width, 1));
    }
}

fn register_links(
    app: &mut App,
    row: &[reef_core::markdown::MarkdownSpan],
    x: u16,
    y: u16,
    h_scroll: usize,
    content_w: usize,
) {
    let mut pos = 0usize;
    for span in row {
        let width = UnicodeWidthStr::width(span.text.as_str());
        if let Some(link) = span.link.as_ref() {
            let start = pos.saturating_sub(h_scroll);
            let end = (pos + width).saturating_sub(h_scroll).min(content_w);
            if start < end {
                app.hit_registry.register_row(
                    x + start as u16,
                    y,
                    (end - start) as u16,
                    ClickAction::OpenMarkdownLink(link.clone()),
                );
            }
        }
        pos += width;
    }
}

fn span_hovered(
    app: &App,
    x: u16,
    y: u16,
    start: usize,
    end: usize,
    h_scroll: usize,
    content_w: usize,
) -> bool {
    let Some((hover_x, hover_y)) = app.hover_col.zip(app.hover_row) else {
        return false;
    };
    if hover_y != y {
        return false;
    }
    let screen_start = start.saturating_sub(h_scroll).min(content_w);
    let screen_end = end.saturating_sub(h_scroll).min(content_w);
    screen_start < screen_end
        && hover_x >= x + screen_start as u16
        && hover_x < x + screen_end as u16
}

fn link_hover_style(style: Style, th: &crate::ui::theme::Theme) -> Style {
    style.fg(link_hover_fg(th)).add_modifier(Modifier::BOLD)
}

fn link_hover_fg(th: &crate::ui::theme::Theme) -> Color {
    if th.is_dark {
        Color::Rgb(255, 220, 120)
    } else {
        Color::Rgb(130, 80, 223)
    }
}

fn pad_row_bg<'a>(spans: &mut Vec<Span<'a>>, content_w: usize, bg: Option<Color>) {
    let Some(bg) = bg else {
        return;
    };
    let used = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum::<usize>();
    if used < content_w {
        spans.push(Span::styled(
            spaces(content_w - used),
            Style::default().bg(bg),
        ));
    }
}

fn row_bg(
    row: &[reef_core::markdown::MarkdownSpan],
    th: &crate::ui::theme::Theme,
) -> Option<Color> {
    row.iter()
        .any(|s| {
            matches!(
                s.style.role,
                reef_core::markdown::MarkdownRole::CodeBlockHeader
                    | reef_core::markdown::MarkdownRole::CodeBlockText
            )
        })
        .then_some(code_block_bg(th))
}

fn span_style(span: &reef_core::markdown::MarkdownSpan, th: &crate::ui::theme::Theme) -> Style {
    use reef_core::markdown::MarkdownRole;

    let mut style = match span.style.role {
        MarkdownRole::Normal => Style::default().fg(th.fg_primary),
        MarkdownRole::Heading => Style::default().fg(th.accent).add_modifier(Modifier::BOLD),
        MarkdownRole::Quote => Style::default().fg(th.fg_secondary),
        MarkdownRole::Code => Style::default().fg(th.accent),
        MarkdownRole::CodeBlockHeader => Style::default()
            .fg(code_block_label_fg(th))
            .bg(code_block_bg(th))
            .add_modifier(Modifier::BOLD),
        MarkdownRole::CodeBlockText => Style::default().fg(code_block_fg(th)).bg(code_block_bg(th)),
        MarkdownRole::Link => Style::default()
            .fg(th.accent)
            .add_modifier(Modifier::UNDERLINED),
        MarkdownRole::TableHeader => Style::default()
            .fg(th.fg_primary)
            .add_modifier(Modifier::BOLD),
        MarkdownRole::Border => Style::default().fg(th.fg_secondary),
    };
    if span.style.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if span.style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if let Some(syntax) = span.syntax {
        let syntax_style = crate::ui::highlight::to_ratatui_style(syntax);
        if let Some(fg) = syntax_style.fg {
            style = style.fg(fg);
        }
        style = style.add_modifier(syntax_style.add_modifier);
    }
    if span.link.is_some() {
        style = style.fg(th.accent).add_modifier(Modifier::UNDERLINED);
    }
    style
}

fn code_block_bg(th: &crate::ui::theme::Theme) -> Color {
    if th.is_dark {
        Color::Rgb(36, 38, 46)
    } else {
        Color::Rgb(246, 248, 250)
    }
}

fn code_block_fg(th: &crate::ui::theme::Theme) -> Color {
    if th.is_dark {
        Color::Rgb(230, 232, 238)
    } else {
        th.fg_primary
    }
}

fn code_block_label_fg(th: &crate::ui::theme::Theme) -> Color {
    if th.is_dark {
        Color::Rgb(150, 180, 205)
    } else {
        th.fg_secondary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_code_background_pads_to_full_row() {
        let mut spans = vec![Span::styled(" code ", Style::default())];
        let bg = code_block_bg(&crate::ui::theme::Theme::light());
        pad_row_bg(&mut spans, 10, Some(bg));

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].content.as_ref(), "    ");
        assert_eq!(spans[1].style.bg, Some(bg));
        assert_eq!(bg, Color::Rgb(246, 248, 250));
    }

    #[test]
    fn markdown_link_hover_changes_foreground_color() {
        let th = crate::ui::theme::Theme::dark();
        let base = Style::default().fg(th.accent);
        let hovered = link_hover_style(base, &th);

        assert_ne!(hovered.fg, Some(th.accent));
        assert_eq!(hovered.fg, Some(Color::Rgb(255, 220, 120)));
        assert!(hovered.add_modifier.contains(Modifier::BOLD));
        assert!(hovered.bg.is_none());
    }
}
