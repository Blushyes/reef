use crate::TuiApp as App;
use crate::preview_context_menu::PreviewContextMenuItem;
use crate::ui::context_menu_panel::{ContextMenuRow, render_rows};
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    if !app.preview_context_menu.active || !app.preview_has_selectable_text() {
        return;
    }

    let rows = PreviewContextMenuItem::ALL.map(|item| ContextMenuRow {
        label: crate::i18n::preview_context_menu_label(item),
        enabled: item != PreviewContextMenuItem::Copy || app.preview_context_copy_enabled(),
        action: ClickAction::PreviewContextMenuItem(item),
    });
    let anchor = app.preview_context_menu.anchor;
    let selected = app.preview_context_menu.selected;
    render_rows(
        f,
        app,
        screen,
        anchor,
        selected,
        &rows,
        ClickAction::PreviewContextMenuClose,
    );
}
