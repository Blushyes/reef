//! Overlay renderer for the Graph tab's branch picker (`b` key).
//!
//! Mirrors `hosts_picker_panel` — bordered popup with a filter row up
//! top, a list of rows in the middle, and a footer. Click zones are
//! registered as `ClickAction::GraphBranchPickerSelect(idx)` so mouse
//! drives the same commit path as Enter.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::graph_branch_picker::{BranchKind, BranchPickerRow};
use crate::ui::mouse::ClickAction;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    let th = app.theme;

    let popup_w = 68u16.min(screen.width.saturating_sub(4).max(20));
    let popup_h = 22u16.min(screen.height.saturating_sub(4).max(8));
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w, popup_h);

    app.graph_branch_picker.last_popup_area = Some(area);

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            " Graph · branch ",
            Style::default()
                .fg(th.fg_primary)
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 {
        return;
    }

    let input_y = inner.y;
    let sep_y = inner.y + 1;
    let list_y = inner.y + 2;
    let has_footer = inner.height >= 5;
    let list_h = if has_footer {
        inner.height.saturating_sub(3)
    } else {
        inner.height.saturating_sub(2)
    };

    // Input row.
    let prompt = "› ";
    let prompt_w = UnicodeWidthStr::width(prompt) as u16;
    let input_line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(th.accent)),
        Span::styled(
            app.graph_branch_picker.filter.clone(),
            Style::default().fg(th.fg_primary),
        ),
    ]);
    f.render_widget(input_line, Rect::new(inner.x, input_y, inner.width, 1));
    // Visible caret — same trick `quick_open_panel` uses. Width-aware
    // so it lands between CJK / wide chars correctly, and clamped to
    // the popup's interior so a very long filter doesn't park the
    // caret past the border.
    let cursor_byte = app
        .graph_branch_picker
        .cursor
        .min(app.graph_branch_picker.filter.len());
    let cursor_w = UnicodeWidthStr::width(&app.graph_branch_picker.filter[..cursor_byte]) as u16;
    let max_caret_offset = inner.width.saturating_sub(prompt_w + 1);
    let caret_x = inner.x + prompt_w + cursor_w.min(max_caret_offset);
    f.set_cursor_position((caret_x, input_y));

    // Separator.
    let sep = "─".repeat(inner.width as usize);
    let sep_line = Line::from(Span::styled(sep, Style::default().fg(th.border)));
    f.render_widget(sep_line, Rect::new(inner.x, sep_y, inner.width, 1));

    let rows = app.graph_branch_picker.visible_rows();

    for (row_idx, row) in rows.iter().enumerate().take(list_h as usize) {
        let y = list_y + row_idx as u16;
        let is_selected = row_idx == app.graph_branch_picker.selected_idx;

        let (left, right, accent) = match row {
            BranchPickerRow::AllRefs => ("[ All refs ]".to_string(), "default".to_string(), true),
            BranchPickerRow::Recent(b) => {
                let prefix = if b.is_head { "* " } else { "  " };
                let kind = match b.kind {
                    BranchKind::Local => "recent · local",
                    BranchKind::Remote => "recent · remote",
                };
                (format!("{prefix}{}", b.display), kind.to_string(), true)
            }
            BranchPickerRow::Branch(b) => {
                let prefix = if b.is_head { "* " } else { "  " };
                let kind = match b.kind {
                    BranchKind::Local => "local",
                    BranchKind::Remote => "remote",
                };
                (format!("{prefix}{}", b.display), kind.to_string(), false)
            }
        };

        let left_style = if is_selected {
            Style::default()
                .fg(th.chrome_bg)
                .bg(th.accent)
                .add_modifier(Modifier::BOLD)
        } else if accent {
            Style::default().fg(th.accent)
        } else {
            Style::default().fg(th.fg_primary)
        };
        let right_style = if is_selected {
            Style::default().fg(th.chrome_bg).bg(th.accent)
        } else {
            Style::default().fg(th.chrome_muted_fg)
        };

        let line = Line::from(vec![
            Span::styled(format!(" {left} "), left_style),
            Span::styled(format!(" {right} "), right_style),
        ]);
        let row_rect = Rect::new(inner.x, y, inner.width, 1);
        f.render_widget(line, row_rect);

        app.hit_registry
            .register(row_rect, ClickAction::GraphBranchPickerSelect(row_idx));
    }

    if rows.is_empty() {
        let empty = Line::from(Span::styled(
            "  no branches match — clear the filter or press Esc",
            Style::default()
                .fg(th.chrome_muted_fg)
                .add_modifier(Modifier::ITALIC),
        ));
        f.render_widget(empty, Rect::new(inner.x, list_y, inner.width, 1));
    }

    if has_footer {
        let footer_y = inner.y + inner.height - 1;
        let footer = "  Enter select · Esc cancel";
        let footer_line = Line::from(Span::styled(footer, Style::default().fg(Color::DarkGray)));
        f.render_widget(footer_line, Rect::new(inner.x, footer_y, inner.width, 1));
    }
}
