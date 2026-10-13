//! Adapts the file-tree menu declarations and sort settings for the shared renderer.

use crate::TuiApp as App;
use crate::ui::context_menu::{ContextMenuItem, ContextMenuPlacement, render_rows};
use crate::ui::mouse::ClickAction;
use ratatui::{Frame, layout::Rect};
use reef_app::ContextMenuItem as I;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    if !app.engine.tree_context_menu_active() {
        return;
    }
    let clipboard_empty = app.engine.file_clipboard_empty();
    let rows: Vec<_> = app
        .engine
        .tree_context_menu_entries()
        .iter()
        .map(|entry| {
            entry.map(|item| ContextMenuItem {
                label: crate::i18n::tree_context_menu_label(item).into(),
                enabled: item.is_enabled(clipboard_empty),
                action: ClickAction::TreeContextMenuItem(item.clone()),
            })
        })
        .collect();
    let anchor = app.engine.tree_context_menu_anchor();
    let selected = app.engine.tree_context_menu_selected();
    let (parent, row) = render_rows(
        f,
        app,
        screen,
        selected,
        &rows,
        ClickAction::TreeContextMenuClose,
        ContextMenuPlacement::Below(anchor),
    );
    if let (Some(selected), Some(row)) = (app.engine.tree_context_menu_sort_selected(), row) {
        let sort = app.engine.file_tree_sort();
        let rows: Vec<_> = I::SORT_ITEMS
            .iter()
            .map(|entry| {
                entry.map(|item| {
                    let checked = match item {
                        I::SortField(field) => sort.field == *field,
                        I::SortDirection(direction) => sort.direction == *direction,
                        I::FoldersFirst => sort.folders_first,
                        _ => false,
                    };
                    ContextMenuItem {
                        label: format!(
                            "{} {}",
                            if checked { "✓" } else { " " },
                            crate::i18n::tree_context_menu_label(item)
                        )
                        .into(),
                        enabled: true,
                        action: ClickAction::TreeContextMenuItem(item.clone()),
                    }
                })
            })
            .collect();
        render_rows(
            f,
            app,
            screen,
            selected,
            &rows,
            ClickAction::TreeContextMenuClose,
            ContextMenuPlacement::Submenu { parent, row },
        );
    }
}
