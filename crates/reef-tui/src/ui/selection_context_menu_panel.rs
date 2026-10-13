use crate::TuiApp as App;
use crate::selection_context_menu::SelectionContextMenuItem;
use crate::ui::context_menu::{ContextMenuItem, ContextMenuPlacement, render_rows};
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    if !app.selection_context_menu_has_text() {
        return;
    }

    let rows = SelectionContextMenuItem::ALL.map(|entry| {
        entry.map(|item| ContextMenuItem {
            label: crate::i18n::selection_context_menu_label(*item).into(),
            enabled: *item != SelectionContextMenuItem::Copy
                || app.selection_context_copy_enabled(),
            action: ClickAction::SelectionContextMenuItem(*item),
        })
    });
    render_rows(
        f,
        app,
        screen,
        app.selection_context_menu.selected(),
        &rows,
        ClickAction::SelectionContextMenuClose,
        ContextMenuPlacement::Adjacent(app.selection_context_menu.anchor()),
    );
}
