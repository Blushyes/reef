use crate::app::App;
use crate::search::SearchTarget;
use crate::ui::mouse::ClickAction;
use crate::ui::text::overlay_match_highlight;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(th.border));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );

    let entries = &app.file_tree.entries;
    if entries.is_empty() {
        let msg = Line::from(Span::styled(
            "(empty)",
            Style::default().fg(th.fg_secondary),
        ));
        f.render_widget(msg, Rect::new(padded.x, padded.y, padded.width, 1));
        return;
    }

    // Clamp scroll
    let max_scroll = entries.len().saturating_sub(padded.height as usize);
    app.tree_scroll = app.tree_scroll.min(max_scroll);

    // Ensure selected is visible
    if app.file_tree.selected < app.tree_scroll {
        app.tree_scroll = app.file_tree.selected;
    } else if app.file_tree.selected >= app.tree_scroll + padded.height as usize {
        app.tree_scroll = app
            .file_tree
            .selected
            .saturating_sub(padded.height as usize - 1);
    }

    let scroll = app.tree_scroll;
    let max_y = padded.y + padded.height;

    for (i, entry) in entries.iter().skip(scroll).enumerate() {
        let y = padded.y + i as u16;
        if y >= max_y {
            break;
        }
        let global_idx = scroll + i;
        let is_selected = global_idx == app.file_tree.selected;
        let is_hovered = app.hover_row == Some(y)
            && app
                .hover_col
                .map(|c| c >= area.x && c < area.x + area.width)
                .unwrap_or(false);

        let indent = "  ".repeat(entry.depth);
        let icon = if entry.is_dir {
            if entry.is_expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };

        let name_style = if entry.is_dir {
            Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.fg_primary)
        };

        let bg = if is_selected {
            th.selection_bg
        } else if is_hovered {
            th.hover_bg
        } else {
            Color::Reset
        };

        let mut spans = vec![
            Span::styled(indent.clone(), Style::default().bg(bg)),
            Span::styled(icon, Style::default().fg(th.fg_secondary).bg(bg)),
        ];
        // Overlay search highlights onto the filename span. Collect_rows for
        // the FileTree target emits `entry.name` — byte ranges returned by
        // `ranges_on_row` are directly applicable here.
        let name_base_style = if is_selected || is_hovered {
            name_style.bg(bg)
        } else {
            name_style
        };
        let (ranges, cur) = app.search.ranges_on_row(SearchTarget::FileTree, global_idx);
        if ranges.is_empty() {
            spans.push(Span::styled(entry.name.clone(), name_base_style));
        } else {
            let name_tokens = vec![(name_base_style, entry.name.clone())];
            let overlaid = overlay_match_highlight(
                name_tokens,
                &ranges,
                cur,
                th.search_match,
                th.search_current,
            );
            for (style, text) in overlaid {
                spans.push(Span::styled(text, style));
            }
        }

        // Git status indicator
        if let Some(ch) = entry.git_status {
            let status_color = match ch {
                'M' => Color::Yellow,
                'A' => Color::Green,
                'D' => Color::Red,
                'U' | '?' => Color::Green,
                _ => th.fg_secondary,
            };
            spans.push(Span::styled(
                format!(" {}", ch),
                Style::default().fg(status_color).bg(bg),
            ));
        }

        // Pad remainder
        let content_width: usize =
            indent.len() + icon.len() + entry.name.len() + entry.git_status.map(|_| 2).unwrap_or(0);
        let pad = (padded.width as usize).saturating_sub(content_width);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
        }

        let line = Line::from(spans);
        f.render_widget(line, Rect::new(padded.x, y, padded.width, 1));

        // Register click zone
        app.hit_registry
            .register_row(area.x, y, area.width, ClickAction::TreeClick(global_idx));
    }
}
