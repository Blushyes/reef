use crate::app::App;
use crate::git::FileStatus;
use crate::mouse::ClickAction;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding};
use unicode_width::UnicodeWidthStr;

/// A logical row in the file panel (before scroll is applied).
enum PanelRow {
    StagedHeader {
        collapsed: bool,
        count: usize,
    },
    UnstagedHeader {
        collapsed: bool,
        count: usize,
    },
    File {
        path: String,
        status: FileStatus,
        staged: bool,
    },
    Spacer,
    Empty,
}

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Build the full logical row list (ignoring scroll)
    let mut rows: Vec<PanelRow> = Vec::new();

    // Staged section — only shown when there are staged files
    if !app.staged_files.is_empty() {
        rows.push(PanelRow::StagedHeader {
            collapsed: app.staged_collapsed,
            count: app.staged_files.len(),
        });
        if !app.staged_collapsed {
            for file in &app.staged_files {
                rows.push(PanelRow::File {
                    path: file.path.clone(),
                    status: file.status,
                    staged: true,
                });
            }
        }
        rows.push(PanelRow::Spacer);
    }

    // Unstaged section — always shown
    rows.push(PanelRow::UnstagedHeader {
        collapsed: app.unstaged_collapsed,
        count: app.unstaged_files.len(),
    });
    if !app.unstaged_collapsed {
        for file in &app.unstaged_files {
            rows.push(PanelRow::File {
                path: file.path.clone(),
                status: file.status,
                staged: false,
            });
        }
        if app.unstaged_files.is_empty() {
            rows.push(PanelRow::Empty);
        }
    }

    // Clamp scroll offset to valid range
    let total_rows = rows.len();
    let visible_height = inner.height as usize;
    let max_scroll = total_rows.saturating_sub(visible_height);
    app.file_scroll = app.file_scroll.min(max_scroll);
    let scroll = app.file_scroll;

    // Render only the visible rows
    let mut screen_y = inner.y;
    let max_y = inner.y + inner.height;

    for row in rows.iter().skip(scroll) {
        if screen_y >= max_y {
            break;
        }
        match row {
            PanelRow::StagedHeader { collapsed, count } => {
                let arrow = if *collapsed { "›" } else { "⌄" };
                let line = Line::from(vec![
                    Span::styled(format!("{} ", arrow), Style::default().fg(Color::White)),
                    Span::styled(
                        "暂存的更改",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", count), Style::default().fg(Color::Green)),
                ]);
                f.render_widget(line, Rect::new(inner.x, screen_y, inner.width, 1));
                app.hit_registry.register_row(
                    inner.x,
                    screen_y,
                    inner.width,
                    ClickAction::ToggleStaged,
                );
            }
            PanelRow::UnstagedHeader { collapsed, count } => {
                let arrow = if *collapsed { "›" } else { "⌄" };
                let line = Line::from(vec![
                    Span::styled(format!("{} ", arrow), Style::default().fg(Color::White)),
                    Span::styled(
                        "更改",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {}", count), Style::default().fg(Color::Blue)),
                ]);
                f.render_widget(line, Rect::new(inner.x, screen_y, inner.width, 1));
                app.hit_registry.register_row(
                    inner.x,
                    screen_y,
                    inner.width,
                    ClickAction::ToggleUnstaged,
                );
            }
            PanelRow::File {
                path,
                status,
                staged,
            } => {
                render_file_row(f, app, inner, screen_y, path, *status, *staged);
            }
            PanelRow::Spacer => {
                // Empty row — no rendering needed
            }
            PanelRow::Empty => {
                let line = Line::from(Span::styled(
                    "  无文件",
                    Style::default().fg(Color::DarkGray),
                ));
                f.render_widget(line, Rect::new(inner.x, screen_y, inner.width, 1));
            }
        }
        screen_y += 1;
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

    let button_label = if is_staged { "−" } else { "+" };
    let button_color = if is_staged { Color::Red } else { Color::Green };

    let status_label = status.label();
    let reserved = 8;
    let max_path_len = (area.width as usize).saturating_sub(reserved);
    let path_display_width = UnicodeWidthStr::width(path);
    let display_path = if path_display_width > max_path_len {
        let truncated = truncate_start_to_width(path, max_path_len.saturating_sub(3));
        format!("...{}", truncated)
    } else {
        path.to_string()
    };

    let display_path_width = UnicodeWidthStr::width(display_path.as_str());
    let pad = max_path_len.saturating_sub(display_path_width);
    let padded_path = format!("{}{}", display_path, " ".repeat(pad));

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

    // Whole row (except last 3 cols) → select file
    app.hit_registry.register_row(
        area.x,
        y,
        area.width.saturating_sub(3),
        ClickAction::SelectFile {
            path: path.to_string(),
            staged: is_staged,
        },
    );

    // Last 3 cols → stage / unstage button
    let button_x = area.x + area.width.saturating_sub(3);
    let button_action = if is_staged {
        ClickAction::UnstageFile(path.to_string())
    } else {
        ClickAction::StageFile(path.to_string())
    };
    app.hit_registry.register_row(button_x, y, 3, button_action);
}

/// Take the last `max_width` display columns of a string.
fn truncate_start_to_width(s: &str, max_width: usize) -> &str {
    let total = UnicodeWidthStr::width(s);
    if total <= max_width {
        return s;
    }
    let skip = total - max_width;
    let mut width = 0;
    for (i, c) in s.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        width += cw;
        if width >= skip {
            let next = i + c.len_utf8();
            return &s[next..];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::truncate_start_to_width;

    #[test]
    fn truncate_start_within_limit_returns_whole() {
        assert_eq!(truncate_start_to_width("hello", 10), "hello");
    }

    #[test]
    fn truncate_start_exact_limit_returns_whole() {
        assert_eq!(truncate_start_to_width("hello", 5), "hello");
    }

    #[test]
    fn truncate_start_keeps_tail() {
        // Keep the last 5 columns of "long/path/file.rs" (17 cols)
        assert_eq!(truncate_start_to_width("long/path/file.rs", 5), "le.rs");
    }

    #[test]
    fn truncate_start_cjk_width() {
        // Each CJK char is width 2; "你好世界" is width 8. Keep last 4 → "世界"
        assert_eq!(truncate_start_to_width("你好世界", 4), "世界");
    }
}
