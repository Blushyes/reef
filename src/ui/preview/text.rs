use crate::app::App;
use crate::find_widget::FindTarget;
use crate::search::SearchTarget;
use crate::ui::preview::chrome::render_card_header;
use crate::ui::text::{clip_spans, overlay_match_highlight, overlay_selection_highlight};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent};
use unicode_width::UnicodeWidthStr;

pub(in crate::ui) fn render(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    preview: &PreviewContent,
    focused: bool,
) {
    let (lines, highlighted) = match &preview.body {
        PreviewBody::Text(text) => (&text.lines, &text.highlighted),
        _ => return,
    };
    let th = app.theme;
    let max_y = area.y + area.height;

    let match_count =
        if app.search.target == Some(SearchTarget::FilePreview) && !app.search.matches.is_empty() {
            let total = app.search.matches.len();
            let cur = app.search.current.map(|i| i + 1).unwrap_or(0);
            Some((cur, total))
        } else {
            None
        };
    let y = render_card_header(f, area, &preview.path, &th, focused, match_count);

    let content_height = (max_y - y) as usize;
    app.last_preview_view_h = content_height as u16;
    let max_scroll = lines.len().saturating_sub(content_height);
    app.preview_scroll = app.preview_scroll.min(max_scroll);

    let gutter_w = 6usize;
    let content_w = (area.width as usize).saturating_sub(gutter_w);

    let max_visible_w: usize = lines
        .iter()
        .skip(app.preview_scroll)
        .take(content_height)
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0);
    let max_h = max_visible_w.saturating_sub(content_w);
    app.preview_h_scroll = app.preview_h_scroll.min(max_h);
    let h = app.preview_h_scroll;

    app.last_preview_content_origin = Some((area.x + gutter_w as u16, y, gutter_w as u16));
    let selection = app.preview_selection;

    for (i, line) in lines.iter().skip(app.preview_scroll).enumerate() {
        let cy = y + i as u16;
        if cy >= max_y {
            break;
        }
        let real_idx = app.preview_scroll + i;
        let lineno = real_idx + 1;

        let gutter = Span::styled(
            format!("{lineno:>5} "),
            Style::default().fg(th.fg_secondary),
        );

        let base_tokens: Vec<(Style, std::borrow::Cow<'_, str>)> =
            match highlighted.as_ref().and_then(|hh| hh.get(real_idx)) {
                Some(tokens) => tokens
                    .iter()
                    .map(|token| {
                        (
                            crate::ui::highlight::to_ratatui_style(token.style),
                            std::borrow::Cow::Borrowed(token.text.as_str()),
                        )
                    })
                    .collect(),
                None => vec![(
                    Style::default().fg(th.fg_primary),
                    std::borrow::Cow::Borrowed(line.as_str()),
                )],
            };
        let (mut ranges, mut cur) = if app.find_widget.target == Some(FindTarget::FilePreview) {
            app.find_widget
                .ranges_on_row(FindTarget::FilePreview, real_idx)
        } else {
            app.search
                .ranges_on_row(SearchTarget::FilePreview, real_idx)
        };
        if let Some(hl) = app.preview_highlight.as_ref() {
            if preview.path == hl.path.to_string_lossy()
                && hl.row == real_idx
                && hl.byte_range.start < hl.byte_range.end
            {
                ranges.push(hl.byte_range.clone());
                if cur.is_none() {
                    cur = Some(hl.byte_range.clone());
                }
            }
        }
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
        let tokens = match selection
            .as_ref()
            .and_then(|s| s.line_byte_range(real_idx, line))
        {
            Some(r) if r.start < r.end => overlay_selection_highlight(tokens, r),
            _ => tokens,
        };
        let tokens = match app.ctrl_hover_target.as_ref() {
            Some((row, range)) if *row == real_idx && range.start < range.end => {
                crate::ui::text::overlay_ctrl_hover_underline(tokens, range.clone(), th.accent)
            }
            _ => tokens,
        };

        let mut spans = vec![gutter];
        spans.extend(clip_spans(&tokens, h, content_w));

        f.render_widget(Line::from(spans), Rect::new(area.x, cy, area.width, 1));
    }
}
