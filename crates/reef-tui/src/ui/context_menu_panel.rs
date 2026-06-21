//! Right-click context-menu overlay for the Files-tab tree.
//!
//! Rendered LAST in `ui::render` (after help popup, before palette
//! overlays) so it floats above everything — the `HitTestRegistry`'s
//! reverse-order hit testing does the rest. A panel-wide "fallthrough
//! close" zone is registered underneath the menu, so any left-click
//! that misses a menu row closes the menu cleanly instead of leaking
//! to the tree behind it.
//!
//! Keyboard navigation is handled in `input::handle_key_tree_context_menu`;
//! this module only renders.

use crate::app::App;
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    if !app.tree_context_menu.active {
        return;
    }
    let th = app.theme;

    // Menu width derived from longest label + padding. Add 2 for the
    // border, 2 for the inside padding. Clamp to screen width so a
    // narrow terminal doesn't push the menu off the right edge.
    let items: Vec<&'static str> = app
        .tree_context_menu
        .items
        .iter()
        .map(crate::i18n::tree_context_menu_label)
        .collect();
    let max_label_w = items
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(*s))
        .max()
        .unwrap_or(0);
    let popup_w = (max_label_w as u16 + 4 + 2).min(screen.width);
    let popup_h = app.tree_context_menu.items.len() as u16 + 2;
    let popup_h = popup_h.min(screen.height);

    let (anchor_x, anchor_y) = app.tree_context_menu.anchor;
    // Clamp so the menu stays fully on-screen even when the click
    // landed near the right/bottom edge. Prefer the click position
    // when there's room, fall back to shifting left/up otherwise.
    let x = anchor_x.min(screen.x + screen.width.saturating_sub(popup_w));
    let y = anchor_y.min(screen.y + screen.height.saturating_sub(popup_h));
    let area = Rect::new(x, y, popup_w, popup_h);

    // Panel-wide fallthrough: any click anywhere on screen that doesn't
    // land on a menu row closes the menu. Registered FIRST so the
    // per-row zones shadow it via the hit-registry's late-wins ordering.
    // Use the full screen so even clicks on the status bar dismiss.
    for sy in screen.y..screen.y + screen.height {
        app.hit_registry.register_row(
            screen.x,
            sy,
            screen.width,
            ClickAction::TreeContextMenuClose,
        );
    }

    // Clear under the popup so the menu's rows don't show stale
    // characters from the tree / preview panels.
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Render each item row with hover/selected highlight.
    let clipboard_empty = app.file_clipboard.is_empty();
    for (i, item) in app.tree_context_menu.items.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let label = crate::i18n::tree_context_menu_label(item);
        let enabled = item.is_enabled(clipboard_empty);
        let is_hovered = app.hover_row == Some(y)
            && app
                .hover_col
                .map(|c| c >= inner.x && c < inner.x + inner.width)
                .unwrap_or(false);
        let is_selected = i == app.tree_context_menu.selected;
        let bg = if is_selected || is_hovered {
            th.selection_bg
        } else {
            th.chrome_bg
        };
        let fg = if enabled {
            th.fg_primary
        } else {
            th.fg_secondary
        };
        let mut fg_style = Style::default().fg(fg).bg(bg);
        if !enabled {
            fg_style = fg_style.add_modifier(Modifier::DIM);
        }
        let padded_label = format!("  {}  ", label);
        let used = unicode_width::UnicodeWidthStr::width(padded_label.as_str());
        let mut s = padded_label;
        if (inner.width as usize) > used {
            s.push_str(&" ".repeat(inner.width as usize - used));
        }
        f.render_widget(
            Line::from(Span::styled(
                s,
                fg_style.add_modifier(if is_selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );

        // Disabled items still register their hit zones — but
        // dispatch checks `is_enabled` again on click and short-
        // circuits, so the click is silently swallowed.
        app.hit_registry.register_row(
            inner.x,
            y,
            inner.width,
            ClickAction::TreeContextMenuItem(item.clone()),
        );
    }
}
