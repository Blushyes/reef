use crate::app::App;
use crate::file_tree::PreviewContent;
use crate::i18n::{Msg, t};
use crate::search::SearchTarget;
use crate::ui::text::{clip_spans, overlay_match_highlight};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let preview = match app.preview_content.take() {
        None => {
            render_empty(f, app, inner);
            return;
        }
        Some(preview) => preview,
    };

    if preview.is_binary {
        render_binary(f, app, inner, &preview.file_path);
    } else {
        render_content(f, app, inner, &preview);
    }

    app.preview_content = Some(preview);
}

fn render_empty(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 1 {
        return;
    }
    let msg = Line::from(Span::styled(
        t(Msg::PreviewEmpty),
        Style::default().fg(app.theme.fg_secondary),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(20) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

fn render_binary(f: &mut Frame, app: &App, area: Rect, path: &str) {
    if area.height < 2 {
        return;
    }
    let th = app.theme;
    let header = Line::from(Span::styled(
        path,
        Style::default()
            .fg(th.fg_primary)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(header, Rect::new(area.x, area.y, area.width, 1));

    let msg = Line::from(Span::styled(
        "(binary file)",
        Style::default().fg(th.fg_secondary),
    ));
    let y = area.y + area.height / 2;
    let x = area.x + area.width.saturating_sub(14) / 2;
    f.render_widget(msg, Rect::new(x, y, area.width, 1));
}

fn render_content(f: &mut Frame, app: &mut App, area: Rect, preview: &PreviewContent) {
    let th = app.theme;
    let mut y = area.y;
    let max_y = area.y + area.height;

    // File header
    if y < max_y {
        let header = Line::from(Span::styled(
            preview.file_path.as_str(),
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(header, Rect::new(area.x, y, area.width, 1));
        y += 1;
    }

    // Separator
    if y < max_y {
        let sep = Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(th.fg_secondary),
        ));
        f.render_widget(sep, Rect::new(area.x, y, area.width, 1));
        y += 1;
    }

    let content_height = (max_y - y) as usize;
    // Cache the content viewport height so search-jump can center matches.
    app.last_preview_view_h = content_height as u16;
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
            Style::default().fg(th.fg_secondary),
        );

        // Unified path for both the syntect-tokenized case and the plain-text
        // fallback: build a token vec, overlay any search matches for this row,
        // then clip horizontally. Keeps horizontal-scroll and search highlight
        // independent of whether syntax tokens were produced.
        let base_tokens: Vec<(Style, String)> =
            match preview.highlighted.as_ref().and_then(|hh| hh.get(real_idx)) {
                Some(tokens) => tokens.clone(),
                None => vec![(Style::default().fg(th.fg_primary), line.clone())],
            };
        let (mut ranges, mut cur) = app
            .search
            .ranges_on_row(SearchTarget::FilePreview, real_idx);
        // `global_search::accept` stashes a single-row highlight at the
        // matching line so we can light it up once the async preview lands.
        // Applied alongside the `/` search ranges using the same overlay
        // helper — the existing "current match" slot is natural, since there
        // is only ever one global-search highlight per preview.
        if let Some(hl) = app.preview_highlight.as_ref() {
            if preview.file_path == hl.path.to_string_lossy() && hl.row == real_idx {
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

        let mut spans = vec![gutter];
        spans.extend(clip_spans(&tokens, h, content_w));

        let rendered = Line::from(spans);
        f.render_widget(rendered, Rect::new(area.x, cy, area.width, 1));
    }
}
