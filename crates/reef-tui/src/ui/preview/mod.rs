pub mod binary;
pub mod chrome;
pub mod image;
pub mod markdown;
pub mod structured;
pub mod text;
pub mod video;

use crate::TuiApp as App;
use crate::i18n::{Msg, t};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding};
use reef_core::preview::PreviewBody;

pub fn render(f: &mut Frame, app: &mut App, area: Rect, focused: bool) {
    let block = Block::default().padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.last_preview_rect = Some(inner);

    let preview = match app.engine.preview_content() {
        None => {
            render_empty(f, app, inner);
            return;
        }
        Some(preview) => preview,
    };

    match &preview.body {
        PreviewBody::Text(_)
            if app.engine.structured_preview_mode() == reef_app::StructuredPreviewMode::Tree
                && app.engine.structured_preview_document().is_some() =>
        {
            structured::render(f, app, inner, &preview, focused);
        }
        PreviewBody::Text(_) => text::render(f, app, inner, &preview, focused),
        PreviewBody::Markdown(markdown) => {
            markdown::render(f, app, inner, &preview, markdown, focused);
        }
        PreviewBody::Image(img) => image::render(f, app, inner, &preview.path, img, focused),
        // Videos load as binaries — nothing decodes them during preview
        // build — so the video card is selected on the detected kind rather
        // than on a distinct body variant.
        PreviewBody::Binary(info) if reef_app::preview_is_video(&preview) => {
            video::render(f, app, inner, &preview.path, info, focused);
        }
        PreviewBody::Binary(info) => binary::render(f, app, inner, &preview.path, info, focused),
        PreviewBody::Database(info) => {
            crate::ui::db_preview::render(f, app, inner, &preview.path, info, focused);
        }
    }
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
