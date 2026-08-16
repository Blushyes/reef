//! Shared right-click context-menu renderer plus the Files-tree adapter.
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

use crate::TuiApp as App;
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};

pub(crate) struct ContextMenuRow {
    pub label: &'static str,
    pub enabled: bool,
    pub action: ClickAction,
}

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    if !app.engine.tree_context_menu_active() {
        return;
    }
    let clipboard_empty = app.engine.file_clipboard_empty();
    let rows: Vec<_> = app
        .engine
        .tree_context_menu_items()
        .iter()
        .map(|item| ContextMenuRow {
            label: crate::i18n::tree_context_menu_label(item),
            enabled: item.is_enabled(clipboard_empty),
            action: ClickAction::TreeContextMenuItem(item.clone()),
        })
        .collect();
    let anchor = app.engine.tree_context_menu_anchor();
    let selected = app.engine.tree_context_menu_selected();
    render_rows(
        f,
        app,
        screen,
        anchor,
        selected,
        &rows,
        ClickAction::TreeContextMenuClose,
    );
}

pub(crate) fn render_rows(
    f: &mut Frame,
    app: &mut App,
    screen: Rect,
    anchor: (u16, u16),
    selected: usize,
    rows: &[ContextMenuRow],
    close_action: ClickAction,
) {
    let th = app.theme;
    let max_label_w = rows
        .iter()
        .map(|row| unicode_width::UnicodeWidthStr::width(row.label))
        .max()
        .unwrap_or(0);
    let popup_w = (max_label_w as u16 + 6).min(screen.width);
    let popup_h = (rows.len() as u16 + 2).min(screen.height);
    let area = context_menu_area(screen, anchor, popup_w, popup_h);

    for screen_y in screen.y..screen.y + screen.height {
        app.hit_registry
            .register_row(screen.x, screen_y, screen.width, close_action.clone());
    }

    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    for (i, row) in rows.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let is_hovered = app.hover_row == Some(y)
            && app
                .hover_col
                .map(|c| c >= inner.x && c < inner.x + inner.width)
                .unwrap_or(false);
        let is_selected = i == selected;
        let bg = if is_selected || is_hovered {
            th.selection_bg
        } else {
            th.chrome_bg
        };
        let fg = if row.enabled {
            th.fg_primary
        } else {
            th.fg_secondary
        };
        let mut fg_style = Style::default().fg(fg).bg(bg);
        if !row.enabled {
            fg_style = fg_style.add_modifier(Modifier::DIM);
        }
        let padded_label = format!("  {}  ", row.label);
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

        app.hit_registry
            .register_row(inner.x, y, inner.width, row.action.clone());
    }
}

fn context_menu_area(screen: Rect, anchor: (u16, u16), popup_w: u16, popup_h: u16) -> Rect {
    let max_x = screen.x + screen.width.saturating_sub(popup_w);
    let x = anchor.0.clamp(screen.x, max_x);
    let screen_bottom = screen.y + screen.height;
    let below = anchor.1.saturating_add(1);
    let y = if below.saturating_add(popup_h) <= screen_bottom {
        below
    } else if anchor.1 >= screen.y.saturating_add(popup_h) {
        anchor.1 - popup_h
    } else {
        below.clamp(screen.y, screen_bottom.saturating_sub(popup_h))
    };
    Rect::new(x, y, popup_w, popup_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_menu_prefers_row_below_anchor() {
        let area = context_menu_area(Rect::new(0, 0, 80, 24), (12, 7), 18, 4);

        assert_eq!(area, Rect::new(12, 8, 18, 4));
    }

    #[test]
    fn context_menu_flips_above_anchor_near_bottom() {
        let area = context_menu_area(Rect::new(0, 0, 80, 24), (12, 22), 18, 4);

        assert_eq!(area, Rect::new(12, 18, 18, 4));
    }

    #[test]
    fn context_menu_clamps_to_offset_screen_bounds() {
        let area = context_menu_area(Rect::new(5, 3, 20, 10), (30, 12), 8, 4);

        assert_eq!(area, Rect::new(17, 8, 8, 4));
    }
}
