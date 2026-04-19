//! Shared mouse-hover row tinting for the three inline git panels.
//!
//! The subtle hover wash is applied only to spans that don't already carry a
//! background — so selection highlights and banner buttons keep their identity
//! under the cursor. Callers pass the theme's `hover_bg` explicitly so this
//! module stays free of a back-dependency on `ui::theme`.

use crate::app::App;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

/// True when the mouse is over row `y` within `area`.
pub fn is_hover(app: &App, area: Rect, y: u16) -> bool {
    app.hover_row == Some(y)
        && app
            .hover_col
            .map(|c| c >= area.x && c < area.x + area.width)
            .unwrap_or(false)
}

/// Return `style` with the hover bg applied, but only when the span doesn't
/// already have its own bg — that way selection / button chrome keeps its
/// specific color.
pub fn apply(style: Style, hover: bool, hover_bg: Color) -> Style {
    if hover && style.bg.is_none() {
        style.bg(hover_bg)
    } else {
        style
    }
}
