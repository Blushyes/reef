pub mod binary;
pub mod chrome;
pub mod image;
pub mod markdown;
pub mod text;

use crate::TuiApp as App;
use crate::i18n::{Msg, t};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use reef_core::preview::PreviewBody;
use std::path::Path;
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.last_preview_rect = Some(inner);

    let loading_target = app
        .engine
        .preview_scheduled_path()
        .or_else(|| app.engine.preview_in_flight_path());
    let show_loading = match (&loading_target, app.engine.preview_content_ref()) {
        (Some(target), Some(current)) => current.path != target.to_string_lossy(),
        (Some(_), None) => true,
        _ => false,
    };
    if show_loading {
        render_loading(f, app, inner, loading_target.as_ref().unwrap(), focused);
        return;
    }

    let preview = match app.engine.preview_content() {
        None => {
            render_empty(f, app, inner);
            return;
        }
        Some(preview) => preview,
    };

    match &preview.body {
        PreviewBody::Text(_) => text::render(f, app, inner, &preview, focused),
        PreviewBody::Markdown(markdown) => {
            markdown::render(f, app, inner, &preview, markdown, focused);
        }
        PreviewBody::Image(img) => image::render(f, app, inner, &preview.path, img, focused),
        PreviewBody::Binary(info) => binary::render(f, app, inner, &preview.path, info, focused),
        PreviewBody::Database(info) => {
            crate::ui::db_preview::render(f, app, inner, &preview.path, info, focused);
        }
    }
}

fn render_loading(f: &mut Frame, app: &App, area: Rect, target: &Path, focused: bool) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;
    let max_y = area.y + area.height;
    let y = chrome::render_card_header(f, area, &target.to_string_lossy(), &th, focused, None);
    if y >= max_y {
        return;
    }
    let msg = t(Msg::PreviewLoading);
    let msg_w = UnicodeWidthStr::width(msg) as u16;
    let cy = y + (max_y - y) / 2;
    let cx = area.x + area.width.saturating_sub(msg_w) / 2;
    f.render_widget(
        Line::from(Span::styled(msg, Style::default().fg(th.fg_secondary))),
        Rect::new(cx, cy, area.width.saturating_sub(cx - area.x), 1),
    );
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
