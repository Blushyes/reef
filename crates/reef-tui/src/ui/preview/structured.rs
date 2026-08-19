use std::borrow::Cow;

use crate::TuiApp as App;
use crate::ui::mouse::ClickAction;
use crate::ui::preview::chrome::{StructuredHeaderOptions, render_structured_card_header};
use crate::ui::text::clip_spans;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use reef_app::AppCommand;
use reef_core::preview::PreviewDocument;
use reef_core::structured_data::{JsonOutlineRow, JsonSegment, JsonSegmentRole};
use unicode_width::UnicodeWidthStr;

pub(in crate::ui) fn render(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    preview: &PreviewDocument,
    focused: bool,
) {
    let Some(document) = app.engine.structured_preview_document() else {
        return;
    };
    let outline = document.outline();
    let theme = app.theme;
    let y = render_structured_card_header(
        f,
        area,
        &preview.path,
        &theme,
        focused,
        StructuredHeaderOptions {
            match_count: preview_match_count(app),
            mode: app.engine.structured_preview_mode(),
        },
        &mut app.hit_registry,
    );
    let max_y = area.y + area.height;
    let content_height = max_y.saturating_sub(y) as usize;
    app.layout.last_preview_view_h = content_height as u16;
    app.last_preview_content_origin = None;

    let max_scroll = outline.row_count().saturating_sub(content_height);
    app.engine
        .dispatch(AppCommand::ClampPreviewVerticalScroll(max_scroll));

    let gutter_width = 6usize;
    let content_width = (area.width as usize).saturating_sub(gutter_width);
    let max_horizontal = outline
        .content_width_columns()
        .saturating_sub(content_width);
    app.engine
        .dispatch(AppCommand::ClampPreviewHorizontalScroll(max_horizontal));
    let horizontal_scroll = app.engine.preview_h_scroll();

    for row in outline
        .rows()
        .iter()
        .skip(app.engine.preview_scroll())
        .take(content_height)
    {
        let screen_y = y + (row.line_number - app.engine.preview_scroll() - 1) as u16;
        render_row(
            f,
            app,
            area,
            screen_y,
            row,
            horizontal_scroll,
            content_width,
        );
    }
}

fn render_row(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    screen_y: u16,
    row: &JsonOutlineRow,
    horizontal_scroll: usize,
    content_width: usize,
) {
    let theme = app.theme;
    let gutter = Span::styled(
        format!("{:>5} ", row.line_number),
        Style::default().fg(theme.fg_secondary),
    );
    let indent_columns = row.depth * 4;
    let prefix_columns = segment_width(&row.prefix_segments);
    let mut tokens = vec![(Style::default(), Cow::Owned(" ".repeat(indent_columns)))];
    tokens.extend(styled_segments(&row.prefix_segments, &theme));
    if let Some(disclosure) = &row.disclosure {
        tokens.push((
            Style::default().fg(theme.fg_secondary),
            Cow::Borrowed(if disclosure.collapsed { "▸ " } else { "▾ " }),
        ));

        let disclosure_column = indent_columns + prefix_columns;
        if disclosure_column >= horizontal_scroll
            && disclosure_column < horizontal_scroll.saturating_add(content_width)
        {
            app.hit_registry.register_row(
                area.x + 6 + (disclosure_column - horizontal_scroll) as u16,
                screen_y,
                1,
                ClickAction::ToggleStructuredPreviewNode(disclosure.node_id.clone()),
            );
        }
    }
    tokens.extend(styled_segments(&row.suffix_segments, &theme));

    let mut spans = vec![gutter];
    spans.extend(clip_spans(&tokens, horizontal_scroll, content_width));
    f.render_widget(
        Line::from(spans),
        Rect::new(area.x, screen_y, area.width, 1),
    );
}

fn styled_segments<'a>(
    segments: &'a [JsonSegment],
    theme: &crate::ui::theme::Theme,
) -> impl Iterator<Item = (Style, Cow<'a, str>)> {
    segments.iter().map(|segment| {
        let color = match segment.role {
            JsonSegmentRole::Key => theme.accent,
            JsonSegmentRole::String => Color::Green,
            JsonSegmentRole::Number => Color::Yellow,
            JsonSegmentRole::Boolean => Color::Magenta,
            JsonSegmentRole::Null => theme.fg_secondary,
            JsonSegmentRole::Punctuation => theme.fg_primary,
        };
        (
            Style::default().fg(color),
            Cow::Borrowed(segment.text.as_str()),
        )
    })
}

fn segment_width(segments: &[JsonSegment]) -> usize {
    segments
        .iter()
        .map(|segment| UnicodeWidthStr::width(segment.text.as_str()))
        .sum()
}

fn preview_match_count(app: &App) -> Option<(usize, usize)> {
    let search = app.engine.search();
    if search.target == Some(reef_app::SearchTarget::FilePreview) && !search.matches.is_empty() {
        Some((
            search.current.map(|index| index + 1).unwrap_or_default(),
            search.matches.len(),
        ))
    } else {
        None
    }
}
