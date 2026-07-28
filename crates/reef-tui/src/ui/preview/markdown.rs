use crate::TuiApp as App;
use crate::ui::mouse::ClickAction;
use crate::ui::preview::chrome::render_card_header;
use crate::ui::text::{clip_spans, overlay_match_highlight, overlay_selection_highlight, spaces};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use reef_app::{AppCommand, FindTarget, SearchTarget};
use reef_core::preview::PreviewDocument as PreviewContent;
use std::borrow::Cow;
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MarkdownVisualRow {
    pub logical_row: usize,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Default)]
pub(crate) struct MarkdownLayoutCache {
    preview_id: usize,
    width: usize,
    rows: Vec<MarkdownVisualRow>,
}

impl MarkdownLayoutCache {
    pub(crate) fn ensure(
        &mut self,
        preview_id: usize,
        markdown: &reef_core::markdown::MarkdownPreview,
        width: usize,
    ) -> bool {
        if self.preview_id == preview_id && self.width == width {
            return false;
        }

        self.preview_id = preview_id;
        self.width = width;
        self.rows.clear();

        if let Some(model) = markdown.render_model.as_ref() {
            for (logical_row, (spans, text)) in model.rows.iter().zip(&model.text_rows).enumerate()
            {
                if row_wraps(spans) {
                    self.push_wrapped_row(logical_row, text, width);
                } else {
                    self.rows.push(MarkdownVisualRow {
                        logical_row,
                        byte_start: 0,
                        byte_end: text.len(),
                    });
                }
            }
        } else {
            for (logical_row, text) in markdown.source.lines().enumerate() {
                self.push_wrapped_row(logical_row, text, width);
            }
        }

        true
    }

    fn push_wrapped_row(&mut self, logical_row: usize, text: &str, width: usize) {
        self.rows
            .extend(
                wrap_text_ranges(text, width)
                    .into_iter()
                    .map(|range| MarkdownVisualRow {
                        logical_row,
                        byte_start: range.start,
                        byte_end: range.end,
                    }),
            );
    }

    pub(crate) fn matches(&self, preview_id: usize) -> bool {
        self.preview_id == preview_id
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn row(&self, visual_row: usize) -> Option<MarkdownVisualRow> {
        self.rows.get(visual_row).copied()
    }

    pub(crate) fn first_visual_row(&self, logical_row: usize) -> usize {
        self.rows
            .partition_point(|row| row.logical_row < logical_row)
            .min(self.rows.len().saturating_sub(1))
    }

    pub(crate) fn max_scroll(&self, view_height: usize) -> usize {
        self.rows.len().saturating_sub(view_height)
    }
}

fn row_wraps(row: &[reef_core::markdown::MarkdownSpan]) -> bool {
    use reef_core::markdown::MarkdownRole;

    !row.iter().any(|span| {
        matches!(
            span.style.role,
            MarkdownRole::CodeBlockHeader | MarkdownRole::CodeBlockText | MarkdownRole::Border
        )
    })
}

fn wrap_text_ranges(text: &str, max_width: usize) -> Vec<Range<usize>> {
    if text.is_empty() || max_width == 0 {
        return std::iter::once(0..text.len()).collect();
    }

    let mut ranges = Vec::new();
    let mut start = 0usize;

    while start < text.len() {
        let mut width = 0usize;
        let mut last_whitespace = None;
        let mut overflow = None;

        for (offset, ch) in text[start..].char_indices() {
            let byte = start + offset;
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + char_width > max_width {
                overflow = Some(byte);
                break;
            }
            if ch.is_whitespace() {
                last_whitespace = Some(byte);
            }
            width += char_width;
        }

        let Some(overflow_at) = overflow else {
            ranges.push(start..text.len());
            break;
        };

        let overflow_is_whitespace = text[overflow_at..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace);
        let end = if overflow_is_whitespace {
            overflow_at
        } else {
            last_whitespace
                .filter(|whitespace| *whitespace > start)
                .unwrap_or(overflow_at)
        };
        if end == start {
            let char_len = text[start..].chars().next().map_or(0, char::len_utf8);
            ranges.push(start..start + char_len);
            start += char_len;
            continue;
        }

        ranges.push(start..end);
        start = end;
        while start < text.len() {
            let ch = text[start..].chars().next().expect("start is in bounds");
            if !ch.is_whitespace() {
                break;
            }
            start += ch.len_utf8();
        }
    }

    ranges
}

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
    app.layout.last_preview_view_h = content_height as u16;
    let content_w = area.width as usize;
    let preview_id = preview as *const PreviewContent as usize;
    let layout_changed = app.markdown_layout.ensure(preview_id, markdown, content_w);
    sync_visual_scroll(app, layout_changed, content_height);

    let Some(model) = markdown.render_model.as_ref() else {
        render_source_only(f, app, area, markdown, y, content_height);
        return;
    };

    let max_visible_w = (app.markdown_visual_scroll..app.markdown_layout.len())
        .filter_map(|visual_row| app.markdown_layout.row(visual_row))
        .take(content_height)
        .filter_map(|visual_row| {
            model.text_rows.get(visual_row.logical_row).map(|text| {
                UnicodeWidthStr::width(&text[visual_row.byte_start..visual_row.byte_end])
            })
        })
        .max()
        .unwrap_or(0);
    let max_h = max_visible_w.saturating_sub(content_w);
    app.engine
        .dispatch(AppCommand::ClampPreviewHorizontalScroll(max_h));
    let h = app.engine.preview_h_scroll();

    app.last_preview_content_origin = None;
    app.last_markdown_content_origin = Some((area.x, y));
    let selection = app.preview_selection;

    for i in 0..content_height {
        let cy = y + i as u16;
        if cy >= max_y {
            break;
        }
        let Some(visual_row) = app.markdown_layout.row(app.markdown_visual_scroll + i) else {
            break;
        };
        let Some(row) = model.rows.get(visual_row.logical_row) else {
            break;
        };
        let visual_range = visual_row.byte_start..visual_row.byte_end;
        let visible_spans = slice_row_spans(row, &visual_range);
        let base_tokens: Vec<(Style, Cow<'_, str>)> = visible_spans
            .iter()
            .scan(0usize, |pos, span| {
                let start = *pos;
                *pos += UnicodeWidthStr::width(span.text);
                let mut style = span_style(span.source, &th);
                if span.source.link.is_some()
                    && span_hovered(app, area.x, cy, start, *pos, h, content_w)
                {
                    style = link_hover_style(style, &th);
                }
                Some((style, Cow::Borrowed(span.text)))
            })
            .collect();
        let (ranges, cur) = if app.engine.find_widget().target == Some(FindTarget::FilePreview) {
            app.engine
                .find_widget()
                .ranges_on_row(FindTarget::FilePreview, visual_row.logical_row)
        } else {
            app.engine
                .search()
                .ranges_on_row(SearchTarget::FilePreview, visual_row.logical_row)
        };
        let visible_ranges = intersect_ranges(&ranges, &visual_range);
        let visible_current = cur.and_then(|range| intersect_range(&range, &visual_range));
        let tokens = if visible_ranges.is_empty() {
            base_tokens
        } else {
            overlay_match_highlight(
                base_tokens,
                &visible_ranges,
                visible_current,
                th.search_match,
                th.search_current,
            )
        };
        let tokens = match selection.as_ref().and_then(|s| {
            markdown
                .text_for_row(visual_row.logical_row)
                .and_then(|text| s.line_byte_range(visual_row.logical_row, text))
                .and_then(|range| intersect_range(&range, &visual_range))
        }) {
            Some(r) if r.start < r.end => overlay_selection_highlight(tokens, r),
            _ => tokens,
        };
        let mut spans = clip_spans(&tokens, h, content_w);
        pad_row_bg(&mut spans, content_w, row_bg(row, &th));
        register_links(app, &visible_spans, area.x, cy, h, content_w);
        f.render_widget(Line::from(spans), Rect::new(area.x, cy, area.width, 1));
    }
}

fn sync_visual_scroll(app: &mut App, layout_changed: bool, content_height: usize) {
    let engine_scroll = app.engine.preview_scroll();
    let max_scroll = app.markdown_layout.max_scroll(content_height);

    if layout_changed || app.markdown_engine_scroll_seen != Some(engine_scroll) {
        app.markdown_visual_scroll = if engine_scroll == usize::MAX {
            max_scroll
        } else {
            app.markdown_layout
                .first_visual_row(engine_scroll)
                .min(max_scroll)
        };
    } else {
        app.markdown_visual_scroll = app.markdown_visual_scroll.min(max_scroll);
    }

    let logical_scroll = app
        .markdown_layout
        .row(app.markdown_visual_scroll)
        .map_or(0, |row| row.logical_row);
    if engine_scroll != logical_scroll {
        app.engine
            .dispatch(AppCommand::SetPreviewVerticalScroll(logical_scroll));
    }
    app.markdown_engine_scroll_seen = Some(logical_scroll);
}

fn render_source_only(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    markdown: &reef_core::markdown::MarkdownPreview,
    y: u16,
    content_height: usize,
) {
    let th = app.theme;
    let max_y = area.y + area.height;

    let content_w = area.width as usize;
    let max_visible_w = (app.markdown_visual_scroll..app.markdown_layout.len())
        .filter_map(|visual_row| app.markdown_layout.row(visual_row))
        .take(content_height)
        .filter_map(|visual_row| {
            markdown.text_for_row(visual_row.logical_row).map(|text| {
                UnicodeWidthStr::width(&text[visual_row.byte_start..visual_row.byte_end])
            })
        })
        .max()
        .unwrap_or(0);
    app.engine
        .dispatch(AppCommand::ClampPreviewHorizontalScroll(
            max_visible_w.saturating_sub(content_w),
        ));
    let h = app.engine.preview_h_scroll();

    app.last_preview_content_origin = None;
    app.last_markdown_content_origin = Some((area.x, y));
    let selection = app.preview_selection;

    for visible_offset in 0..content_height {
        let Some(visual_row) = app
            .markdown_layout
            .row(app.markdown_visual_scroll + visible_offset)
        else {
            break;
        };
        let Some(text) = markdown.text_for_row(visual_row.logical_row) else {
            break;
        };
        let visual_range = visual_row.byte_start..visual_row.byte_end;
        let visible_text = &text[visual_range.clone()];
        let cy = y + visible_offset as u16;
        if cy >= max_y {
            break;
        }
        let base_tokens = vec![(
            Style::default().fg(th.fg_primary),
            Cow::Borrowed(visible_text),
        )];
        let (ranges, current) = if app.engine.find_widget().target == Some(FindTarget::FilePreview)
        {
            app.engine
                .find_widget()
                .ranges_on_row(FindTarget::FilePreview, visual_row.logical_row)
        } else {
            app.engine
                .search()
                .ranges_on_row(SearchTarget::FilePreview, visual_row.logical_row)
        };
        let visible_ranges = intersect_ranges(&ranges, &visual_range);
        let visible_current = current.and_then(|range| intersect_range(&range, &visual_range));
        let tokens = if visible_ranges.is_empty() {
            base_tokens
        } else {
            overlay_match_highlight(
                base_tokens,
                &visible_ranges,
                visible_current,
                th.search_match,
                th.search_current,
            )
        };
        let tokens = match selection
            .as_ref()
            .and_then(|selection| selection.line_byte_range(visual_row.logical_row, text))
            .and_then(|range| intersect_range(&range, &visual_range))
        {
            Some(range) if range.start < range.end => overlay_selection_highlight(tokens, range),
            _ => tokens,
        };
        let spans = clip_spans(&tokens, h, content_w);
        f.render_widget(Line::from(spans), Rect::new(area.x, cy, area.width, 1));
    }
}

#[derive(Clone, Copy)]
struct VisibleMarkdownSpan<'a> {
    source: &'a reef_core::markdown::MarkdownSpan,
    text: &'a str,
}

fn slice_row_spans<'a>(
    row: &'a [reef_core::markdown::MarkdownSpan],
    range: &Range<usize>,
) -> Vec<VisibleMarkdownSpan<'a>> {
    let mut offset = 0usize;
    row.iter()
        .filter_map(|span| {
            let span_start = offset;
            let span_end = span_start + span.text.len();
            offset = span_end;
            let start = range.start.max(span_start);
            let end = range.end.min(span_end);
            (start < end).then(|| VisibleMarkdownSpan {
                source: span,
                text: &span.text[start - span_start..end - span_start],
            })
        })
        .collect()
}

fn intersect_ranges(ranges: &[Range<usize>], visual: &Range<usize>) -> Vec<Range<usize>> {
    ranges
        .iter()
        .filter_map(|range| intersect_range(range, visual))
        .collect()
}

fn intersect_range(range: &Range<usize>, visual: &Range<usize>) -> Option<Range<usize>> {
    let start = range.start.max(visual.start);
    let end = range.end.min(visual.end);
    (start < end).then(|| start - visual.start..end - visual.start)
}

fn register_links(
    app: &mut App,
    row: &[VisibleMarkdownSpan<'_>],
    x: u16,
    y: u16,
    h_scroll: usize,
    content_w: usize,
) {
    let mut pos = 0usize;
    for span in row {
        let width = UnicodeWidthStr::width(span.text);
        if let Some(link) = span.source.link.as_ref() {
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
    use reef_core::markdown::build_markdown_preview;

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

    #[test]
    fn prose_wraps_at_words_and_wide_character_boundaries() {
        assert_eq!(
            wrap_text_ranges("alpha beta gamma", 10),
            vec![0..10, 11..16]
        );
        assert_eq!(wrap_text_ranges("甲乙丙丁", 4), vec![0..6, 6..12]);
    }

    #[test]
    fn prose_layout_expands_one_logical_row_into_visual_rows() {
        let markdown =
            build_markdown_preview("README.md", "alpha beta gamma\n").expect("markdown preview");
        let mut cache = MarkdownLayoutCache::default();

        cache.ensure(1, &markdown, 10);

        assert_eq!(
            cache.rows,
            vec![
                MarkdownVisualRow {
                    logical_row: 0,
                    byte_start: 0,
                    byte_end: 10,
                },
                MarkdownVisualRow {
                    logical_row: 0,
                    byte_start: 11,
                    byte_end: 16,
                },
            ]
        );
    }

    #[test]
    fn tables_and_code_blocks_keep_natural_width() {
        let markdown = build_markdown_preview(
            "README.md",
            "| first | second |\n| --- | --- |\n| value | value |\n\n```text\nabcdefgh\n```\n",
        )
        .expect("markdown preview");
        let model = markdown.render_model.as_ref().expect("render model");
        let mut cache = MarkdownLayoutCache::default();

        cache.ensure(1, &markdown, 4);

        assert_eq!(cache.len(), model.rows.len());
        assert!(
            model
                .text_rows
                .iter()
                .any(|text| UnicodeWidthStr::width(text.as_str()) > 4)
        );
        assert_eq!(
            cache
                .rows
                .iter()
                .map(|row| row.logical_row)
                .collect::<Vec<_>>(),
            (0..model.rows.len()).collect::<Vec<_>>()
        );
    }
}
