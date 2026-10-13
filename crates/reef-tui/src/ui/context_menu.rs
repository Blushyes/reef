//! Shared context-menu rendering, geometry, and hit regions.
//!
//! Callers supply declarative Item/Separator rows. Selection indices count
//! items only; this component maps them to visible rows, including scroll offsets.

use crate::TuiApp as App;
use crate::ui::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear};
use reef_core::menu::{MenuEntry, menu_items};
use std::borrow::Cow;

pub(crate) struct ContextMenuItem {
    pub label: Cow<'static, str>,
    pub enabled: bool,
    pub action: ClickAction,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ContextMenuPlacement {
    Below((u16, u16)),
    Adjacent((u16, u16)),
    Submenu { parent: Rect, row: u16 },
}

pub(crate) fn render_rows(
    f: &mut Frame,
    app: &mut App,
    screen: Rect,
    selected: usize,
    rows: &[MenuEntry<ContextMenuItem>],
    close_action: ClickAction,
    placement: ContextMenuPlacement,
) -> (Rect, Option<u16>) {
    let th = app.theme;
    let max_label_w = menu_items(rows)
        .map(|row| unicode_width::UnicodeWidthStr::width(row.label.as_ref()))
        .max()
        .unwrap_or(0);
    let popup_w = (max_label_w as u16 + 6).min(screen.width);
    let popup_h = (rows.len() as u16 + 2).min(screen.height);
    let area = context_menu_area(screen, popup_w, popup_h, placement);

    if !matches!(placement, ContextMenuPlacement::Submenu { .. }) {
        for screen_y in screen.y..screen.y + screen.height {
            app.hit_registry
                .register_row(screen.x, screen_y, screen.width, close_action.clone());
        }
    }

    app.hit_registry.register(area, close_action);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let selected_line = rows
        .iter()
        .enumerate()
        .filter_map(|(line, entry)| entry.as_item().map(|_| line))
        .nth(selected);
    let offset = selected_line.map_or(0, |line| menu_scroll_offset(line, inner.height as usize));
    let mut item_index = 0;
    for (line, entry) in rows.iter().enumerate() {
        let is_selected = entry.as_item().is_some() && item_index == selected;
        item_index += usize::from(entry.as_item().is_some());
        if line < offset {
            continue;
        }
        let y = inner.y + (line - offset) as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let row = match entry {
            MenuEntry::Item(row) => row,
            MenuEntry::Separator => {
                let rule = format!("  {}  ", "─".repeat(inner.width.saturating_sub(4) as usize));
                f.render_widget(
                    Line::from(Span::styled(
                        rule,
                        Style::default().fg(th.border).bg(th.chrome_bg),
                    )),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                app.hit_registry.register_row(
                    inner.x,
                    y,
                    inner.width,
                    ClickAction::ContextMenuSeparator,
                );
                continue;
            }
        };
        let is_hovered = app.hover_row == Some(y)
            && app
                .hover_col
                .map(|c| c >= inner.x && c < inner.x + inner.width)
                .unwrap_or(false);
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
    (
        area,
        selected_line.map(|line| inner.y + (line - offset) as u16),
    )
}

fn menu_scroll_offset(selected: usize, visible_height: usize) -> usize {
    selected.saturating_sub(visible_height.saturating_sub(1))
}

fn context_menu_area(
    screen: Rect,
    popup_w: u16,
    popup_h: u16,
    placement: ContextMenuPlacement,
) -> Rect {
    let (anchor, adjacent) = match placement {
        ContextMenuPlacement::Below(anchor) => (anchor, false),
        ContextMenuPlacement::Adjacent(anchor) => (anchor, true),
        ContextMenuPlacement::Submenu { parent, row } => {
            let right = parent.right().saturating_sub(1);
            let x = if right.saturating_add(popup_w) <= screen.right() {
                right
            } else {
                parent
                    .x
                    .saturating_sub(popup_w.saturating_sub(1))
                    .max(screen.x)
            };
            let y = row
                .saturating_sub(1)
                .clamp(screen.y, screen.bottom().saturating_sub(popup_h));
            return Rect::new(x, y, popup_w, popup_h);
        }
    };
    let screen_right = screen.x + screen.width;
    let screen_bottom = screen.y + screen.height;
    let anchor_x = anchor.0.clamp(screen.x, screen_right.saturating_sub(1));
    let anchor_y = anchor.1.clamp(screen.y, screen_bottom.saturating_sub(1));
    let x = if adjacent {
        let right = anchor_x.saturating_add(1);
        if right.saturating_add(popup_w) <= screen_right {
            right
        } else if anchor_x >= screen.x.saturating_add(popup_w) {
            anchor_x - popup_w
        } else {
            right.clamp(screen.x, screen_right.saturating_sub(popup_w))
        }
    } else {
        anchor_x.clamp(screen.x, screen_right.saturating_sub(popup_w))
    };
    let below = anchor_y.saturating_add(1);
    let y = if below.saturating_add(popup_h) <= screen_bottom {
        below
    } else if anchor_y >= screen.y.saturating_add(popup_h) {
        anchor_y - popup_h
    } else {
        below.clamp(screen.y, screen_bottom.saturating_sub(popup_h))
    };
    Rect::new(x, y, popup_w, popup_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submenu_aligns_with_parent_row_and_flips_at_right_edge() {
        let screen = Rect::new(0, 0, 80, 24);
        for (parent, expected) in [
            (Rect::new(4, 4, 24, 14), Rect::new(27, 12, 21, 8)),
            (Rect::new(56, 4, 24, 14), Rect::new(36, 12, 21, 8)),
        ] {
            assert_eq!(
                context_menu_area(
                    screen,
                    21,
                    8,
                    ContextMenuPlacement::Submenu { parent, row: 13 }
                ),
                expected
            );
        }
    }

    #[test]
    fn context_menu_prefers_row_below_anchor() {
        let area = context_menu_area(
            Rect::new(0, 0, 80, 24),
            18,
            4,
            ContextMenuPlacement::Below((12, 7)),
        );

        assert_eq!(area, Rect::new(12, 8, 18, 4));
    }

    #[test]
    fn context_menu_flips_above_anchor_near_bottom() {
        let area = context_menu_area(
            Rect::new(0, 0, 80, 24),
            18,
            4,
            ContextMenuPlacement::Adjacent((12, 22)),
        );

        assert_eq!(area, Rect::new(13, 18, 18, 4));
    }

    #[test]
    fn context_menu_clamps_to_offset_screen_bounds() {
        let area = context_menu_area(
            Rect::new(5, 3, 20, 10),
            8,
            4,
            ContextMenuPlacement::Adjacent((30, 12)),
        );

        assert_eq!(area, Rect::new(16, 8, 8, 4));
    }

    #[test]
    fn context_menu_flips_left_of_anchor_near_right_edge() {
        let area = context_menu_area(
            Rect::new(0, 0, 80, 24),
            18,
            4,
            ContextMenuPlacement::Adjacent((78, 7)),
        );

        assert_eq!(area, Rect::new(60, 8, 18, 4));
    }

    #[test]
    fn adjacent_menu_offsets_from_anchor() {
        let area = context_menu_area(
            Rect::new(0, 0, 80, 24),
            18,
            4,
            ContextMenuPlacement::Adjacent((12, 7)),
        );

        assert_eq!(area, Rect::new(13, 8, 18, 4));
    }
}
