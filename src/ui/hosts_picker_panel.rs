//! Hosts picker overlay (bound to Ctrl+O; see `crate::hosts_picker`).
//!
//! Renders on top of the normal UI when `app.hosts_picker.active` is true.
//! Three regions inside the popup: a single-row input line (filter or
//! literal path), the visible-row list (recent targets + parsed
//! `~/.ssh/config` entries), and a help footer. Registers a `ClickAction`
//! zone per row so clicking commits. Deliberately simpler than the
//! quick-open panel — no fuzzy highlight, no per-row h-scroll.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};

use crate::app::App;
use crate::hosts_picker::{InputMode, PickerRow};
use crate::ui::mouse::ClickAction;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    let th = app.theme;

    let popup_w = 68u16.min(screen.width.saturating_sub(4).max(20));
    let popup_h = 22u16.min(screen.height.saturating_sub(4).max(8));
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w, popup_h);

    app.hosts_picker.last_popup_area = Some(area);

    f.render_widget(Clear, area);

    let title = match app.hosts_picker.input_mode {
        InputMode::Search => " Connect to · filter ",
        InputMode::Path => " Connect to · [user@]host[:path] ",
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            title,
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
    let input_text = match app.hosts_picker.input_mode {
        InputMode::Search => app.hosts_picker.filter.clone(),
        InputMode::Path => app.hosts_picker.path_buffer.clone(),
    };
    let prompt = match app.hosts_picker.input_mode {
        InputMode::Search => "› ",
        InputMode::Path => "➜ ",
    };
    let input_line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(th.accent)),
        Span::styled(input_text, Style::default().fg(th.fg_primary)),
    ]);
    let input_rect = Rect::new(inner.x, input_y, inner.width, 1);
    f.render_widget(input_line, input_rect);

    // Separator line between input and list.
    let sep = "─".repeat(inner.width as usize);
    let sep_line = Line::from(Span::styled(sep, Style::default().fg(th.border)));
    f.render_widget(sep_line, Rect::new(inner.x, sep_y, inner.width, 1));

    // Visible rows. The picker's computed list drives both the render
    // and the selected_idx clamp; we pull once per frame.
    let rows = app.hosts_picker.visible_rows();

    for (row_idx, row) in rows.iter().enumerate().take(list_h as usize) {
        let y = list_y + row_idx as u16;
        let is_selected = row_idx == app.hosts_picker.selected_idx;
        let (left, right, is_recent) = match row {
            PickerRow::Recent(t) => (t.to_arg(), "recent".to_string(), true),
            PickerRow::Entry(h) => {
                let right = match (&h.hostname, &h.user) {
                    (Some(host), Some(user)) => format!("{user}@{host}"),
                    (Some(host), None) => host.clone(),
                    (None, Some(user)) => format!("{user}@"),
                    (None, None) => String::new(),
                };
                (h.alias.clone(), right, false)
            }
        };

        let left_style = if is_selected {
            Style::default()
                .fg(th.chrome_bg)
                .bg(th.accent)
                .add_modifier(Modifier::BOLD)
        } else if is_recent {
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

        // Register click zone so a mouse click commits that row.
        app.hit_registry
            .register(row_rect, ClickAction::HostsPickerSelect(row_idx));
    }

    if rows.is_empty() {
        let empty = Line::from(Span::styled(
            "  no matches — press Ctrl+P to type a target",
            Style::default()
                .fg(th.chrome_muted_fg)
                .add_modifier(Modifier::ITALIC),
        ));
        f.render_widget(empty, Rect::new(inner.x, list_y, inner.width, 1));
    }

    if has_footer {
        let footer_y = inner.y + inner.height - 1;
        let footer = match app.hosts_picker.input_mode {
            InputMode::Search => "  Enter connect · Ctrl+P path mode · Esc cancel",
            InputMode::Path => "  Enter connect · Esc back to filter",
        };
        let footer_line = Line::from(Span::styled(footer, Style::default().fg(Color::DarkGray)));
        f.render_widget(footer_line, Rect::new(inner.x, footer_y, inner.width, 1));
    }
}
