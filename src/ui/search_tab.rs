//! Tab::Search left panel renderer (persistent global-search view).
//!
//! Draws the same three regions as the Space+F overlay — input, result
//! list, footer — but without a popup block, so it fills the tab's left
//! pane. State comes from `app.global_search`, which is shared with the
//! overlay: pinning the overlay via Alt/Ctrl+Enter is a state-preserving
//! hand-off to this panel.
//!
//! Right panel (`file_preview_panel`) handles the preview; see `ui::mod::render`.

use crate::app::App;
use crate::ui::global_search_panel;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

pub fn render_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let input_focused = app.global_search.tab_input_focused;

    // Outer block with a mode-aware title: list mode surfaces the "press /
    // to search" hint where the user is most likely to look first; input
    // mode drops the hint so the title doesn't nag while typing.
    let title_text = if input_focused {
        " 🔎 Search ".to_string()
    } else {
        " 🔎 Search · / to search ".to_string()
    };
    let border_style = if input_focused {
        Style::default().fg(th.accent)
    } else {
        Style::default().fg(th.border)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            title_text,
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    // Delegate to the overlay's row renderer. It reads/writes `last_view_h`
    // on `app.global_search`, which is fine: only one of {overlay, tab} has
    // focus at a time, and both contexts paginate by the same view height.
    global_search_panel::render_body(f, app, inner, input_focused);
}

/// Render a blank placeholder centered in `area`. Used before the user
/// types anything — the overlay's main body already handles empty-state
/// messages, so this is just here for composability.
pub fn render_empty_hint(f: &mut Frame, area: Rect, hint: &str, th: &crate::ui::theme::Theme) {
    if area.height == 0 {
        return;
    }
    let row_y = area.y + area.height / 2;
    let line = Line::from(Span::styled(
        hint.to_string(),
        Style::default()
            .fg(th.fg_secondary)
            .add_modifier(Modifier::ITALIC),
    ));
    f.render_widget(line, Rect::new(area.x, row_y, area.width, 1));
}
