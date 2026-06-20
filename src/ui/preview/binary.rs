use crate::app::App;
use crate::i18n::{Msg, t};
use crate::ui::preview::chrome::render_card_header;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use reef_core::preview::{BinaryInfo, BinaryReason};
use unicode_width::UnicodeWidthStr;

pub(in crate::ui) fn render(
    f: &mut Frame,
    app: &App,
    area: Rect,
    path: &str,
    info: &BinaryInfo,
    focused: bool,
) {
    if area.height < 1 {
        return;
    }
    let th = app.theme;
    let max_y = area.y + area.height;
    let mut y = render_card_header(f, area, path, &th, focused, None);

    if y < max_y && !info.meta_line.is_empty() {
        f.render_widget(
            Line::from(Span::styled(
                info.meta_line.as_str(),
                Style::default().fg(th.fg_secondary),
            )),
            Rect::new(area.x, y, area.width, 1),
        );
        y += 1;
    }

    let reason = reason_text(info);
    if y < max_y {
        let cy = y + (max_y - y) / 2;
        let reason_w = UnicodeWidthStr::width(reason.as_str()) as u16;
        let cx = area.x + area.width.saturating_sub(reason_w) / 2;
        f.render_widget(
            Line::from(Span::styled(reason, Style::default().fg(th.fg_secondary))),
            Rect::new(cx, cy, area.width.saturating_sub(cx - area.x), 1),
        );
    }
}

fn reason_text(info: &BinaryInfo) -> String {
    match &info.reason {
        BinaryReason::NonImage | BinaryReason::NullBytes => {
            t(Msg::PreviewBinaryNonImage).to_string()
        }
        BinaryReason::UnsupportedImage => t(Msg::PreviewBinaryUnsupportedImage).to_string(),
        BinaryReason::TooLarge => t(Msg::PreviewBinaryTooLarge).to_string(),
        BinaryReason::DecodeError(msg) => {
            format!("{}: {}", t(Msg::PreviewBinaryDecodeError), msg)
        }
        BinaryReason::Empty => t(Msg::PreviewBinaryEmpty).to_string(),
    }
}
