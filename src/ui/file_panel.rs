use crate::app::App;
use crate::git::FileStatus;
use crate::mouse::ClickAction;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding};
use ratatui::Frame;

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut y = inner.y;
    let max_y = inner.y + inner.height;

    // === Staged section ===
    if !app.staged_files.is_empty() {
        if y >= max_y {
            return;
        }

        // Section header
        let arrow = if app.staged_collapsed { "▶" } else { "▼" };
        let count = app.staged_files.len();
        let header = Line::from(vec![
            Span::styled(
                format!("{} ", arrow),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "暂存的更改",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", count),
                Style::default().fg(Color::Green),
            ),
        ]);
        f.render_widget(header, Rect::new(inner.x, y, inner.width, 1));

        // Register click for toggle
        app.hit_registry
            .register_row(inner.x, y, inner.width, ClickAction::ToggleStaged);
        y += 1;

        if !app.staged_collapsed {
            for file in app.staged_files.clone() {
                if y >= max_y {
                    break;
                }
                render_file_row(f, app, inner, y, &file.path, file.status, true);
                y += 1;
            }
        }

        // Spacer
        if y < max_y {
            y += 1;
        }
    }

    // === Unstaged section ===
    if y >= max_y {
        return;
    }

    let arrow = if app.unstaged_collapsed { "▶" } else { "▼" };
    let count = app.unstaged_files.len();
    let header = Line::from(vec![
        Span::styled(format!("{} ", arrow), Style::default().fg(Color::White)),
        Span::styled(
            "更改",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", count),
            Style::default().fg(Color::Blue),
        ),
    ]);
    f.render_widget(header, Rect::new(inner.x, y, inner.width, 1));
    app.hit_registry
        .register_row(inner.x, y, inner.width, ClickAction::ToggleUnstaged);
    y += 1;

    if !app.unstaged_collapsed {
        for file in app.unstaged_files.clone() {
            if y >= max_y {
                break;
            }
            render_file_row(f, app, inner, y, &file.path, file.status, false);
            y += 1;
        }
    }
}

fn render_file_row(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    y: u16,
    path: &str,
    status: FileStatus,
    is_staged: bool,
) {
    let is_selected = app
        .selected_file
        .as_ref()
        .map(|s| s.path == path && s.is_staged == is_staged)
        .unwrap_or(false);

    let is_hovered = app.hover_row == Some(y);

    // Background style
    let bg = if is_selected {
        Color::Rgb(40, 60, 100)
    } else if is_hovered {
        Color::Rgb(40, 40, 50)
    } else {
        Color::Reset
    };

    let status_color = match status {
        FileStatus::Modified => Color::Yellow,
        FileStatus::Added => Color::Green,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Untracked => Color::Green,
    };

    // Button: [+] for unstaged, [-] for staged
    let button_label = if is_staged { "−" } else { "+" };
    let button_color = if is_staged {
        Color::Red
    } else {
        Color::Green
    };

    // Truncate path to fit
    let status_label = status.label();
    // Layout: "  filename.ext    M [+]"
    //          2 + path + gap + status(1) + space + button(3)
    let reserved = 8; // "  " + " M" + " [+]"
    let max_path_len = (area.width as usize).saturating_sub(reserved);
    let display_path = if path.len() > max_path_len {
        // Show the end of the path with "..."
        let start = path.len() - max_path_len.saturating_sub(3);
        format!("...{}", &path[start..])
    } else {
        path.to_string()
    };

    let path_width = max_path_len;
    let padded_path = format!("{:<width$}", display_path, width = path_width);

    let line = Line::from(vec![
        Span::styled("  ", Style::default().bg(bg)),
        Span::styled(padded_path, Style::default().fg(Color::White).bg(bg)),
        Span::styled(
            format!(" {}", status_label),
            Style::default().fg(status_color).bg(bg),
        ),
        Span::styled(" ", Style::default().bg(bg)),
        Span::styled(
            button_label,
            Style::default()
                .fg(button_color)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(bg)),
    ]);

    f.render_widget(line, Rect::new(area.x, y, area.width, 1));

    // Register clickable regions
    // Whole row → select file
    app.hit_registry.register_row(
        area.x,
        y,
        area.width.saturating_sub(3),
        ClickAction::SelectFile {
            path: path.to_string(),
            staged: is_staged,
        },
    );

    // Button region → stage/unstage (last 3 columns)
    let button_x = area.x + area.width.saturating_sub(3);
    let button_action = if is_staged {
        ClickAction::UnstageFile(path.to_string())
    } else {
        ClickAction::StageFile(path.to_string())
    };
    app.hit_registry
        .register_row(button_x, y, 3, button_action);
}
