//! Hosts picker overlay (bound to Ctrl+O; see `crate::hosts_picker`).
//!
//! Renders on top of the normal UI when the hosts picker overlay is active.
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

use crate::TuiApp as App;
use crate::ui::mouse::ClickAction;
use reef_app::InputMode;

pub fn render(f: &mut Frame, app: &mut App, screen: Rect) {
    let th = app.theme;

    let popup_w = 68u16.min(screen.width.saturating_sub(4).max(20));
    let popup_h = 22u16.min(screen.height.saturating_sub(4).max(8));
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w, popup_h);

    app.hosts_picker_popup_area = Some(area);

    f.render_widget(Clear, area);

    let snapshot = app.engine.snapshot().hosts_picker;

    let title = match snapshot.input_mode {
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
    let input_text = snapshot.input_text.as_str();
    let prompt = match snapshot.input_mode {
        InputMode::Search => "› ",
        InputMode::Path => "➜ ",
    };
    let prompt_w = unicode_width::UnicodeWidthStr::width(prompt) as u16;
    let input_line = Line::from(vec![
        Span::styled(prompt, Style::default().fg(th.accent)),
        Span::styled(input_text.to_string(), Style::default().fg(th.fg_primary)),
    ]);
    let input_rect = Rect::new(inner.x, input_y, inner.width, 1);
    f.render_widget(input_line, input_rect);
    // Visible caret. Width-aware so multi-byte chars don't desync,
    // clamped so a very long buffer doesn't park the caret past the
    // popup's right border.
    let cursor_clamped = snapshot.cursor.min(input_text.len());
    let cursor_w = unicode_width::UnicodeWidthStr::width(&input_text[..cursor_clamped]) as u16;
    let max_caret_offset = inner.width.saturating_sub(prompt_w + 1);
    let caret_x = inner.x + prompt_w + cursor_w.min(max_caret_offset);
    f.set_cursor_position((caret_x, input_y));

    // Separator line between input and list.
    let sep = "─".repeat(inner.width as usize);
    let sep_line = Line::from(Span::styled(sep, Style::default().fg(th.border)));
    f.render_widget(sep_line, Rect::new(inner.x, sep_y, inner.width, 1));

    // Visible rows. The picker's computed list drives both the render
    // and the selected_idx clamp; we pull once per frame.
    for (row_idx, row) in app.engine.hosts_picker_rows(0, list_h as usize) {
        let y = list_y + row_idx as u16;
        let is_selected = row_idx == snapshot.selected_idx;

        let left_style = if is_selected {
            Style::default()
                .fg(th.chrome_bg)
                .bg(th.accent)
                .add_modifier(Modifier::BOLD)
        } else if row.recent {
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
            Span::styled(format!(" {} ", row.left), left_style),
            Span::styled(format!(" {} ", row.right), right_style),
        ]);
        let row_rect = Rect::new(inner.x, y, inner.width, 1);
        f.render_widget(line, row_rect);

        // Register click zone so a mouse click commits that row.
        app.hit_registry
            .register(row_rect, ClickAction::HostsPickerSelect(row_idx));
    }

    if snapshot.row_count == 0 {
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
        let footer = match snapshot.input_mode {
            InputMode::Search => "  Enter connect · Ctrl+P path mode · Esc cancel",
            InputMode::Path => "  Enter connect · Esc back to filter",
        };
        let footer_line = Line::from(Span::styled(footer, Style::default().fg(Color::DarkGray)));
        f.render_widget(footer_line, Rect::new(inner.x, footer_y, inner.width, 1));
    }
}
