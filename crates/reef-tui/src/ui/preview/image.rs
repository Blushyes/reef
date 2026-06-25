use crate::TuiApp as App;
use crate::i18n::{Msg, t};
use crate::ui::preview::chrome::render_card_header;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui_image::StatefulImage;
use reef_core::preview::ImagePreview;
use unicode_width::UnicodeWidthStr;

const MIN_META_HEIGHT: u16 = 5;

pub(in crate::ui) fn render(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    path: &str,
    img: &ImagePreview,
    focused: bool,
) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;
    let max_y = area.y + area.height;
    let mut y = render_card_header(f, area, path, &th, focused, None);

    let wants_meta = area.height >= MIN_META_HEIGHT;
    if wants_meta && y < max_y {
        f.render_widget(
            Line::from(Span::styled(
                img.meta_line.as_str(),
                Style::default().fg(th.fg_secondary),
            )),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
        if y < max_y {
            y += 1;
        }
    }

    if y >= max_y {
        return;
    }
    let image_area = Rect::new(area.x, y, area.width, max_y - y);
    if image_area.height < 1 || image_area.width < 1 {
        return;
    }
    match app.preview_image_protocol.as_mut() {
        Some(proto) => {
            let widget = StatefulImage::default();
            f.render_stateful_widget(widget, image_area, proto);
        }
        None => {
            let msg = Line::from(Span::styled(
                t(Msg::PreviewImageUnavailable),
                Style::default().fg(th.fg_secondary),
            ));
            let cy = image_area.y + image_area.height / 2;
            let cx = image_area.x
                + image_area
                    .width
                    .saturating_sub(UnicodeWidthStr::width(t(Msg::PreviewImageUnavailable)) as u16)
                    / 2;
            f.render_widget(msg, Rect::new(cx, cy, image_area.width, 1));
        }
    }
}
