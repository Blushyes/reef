pub mod diff_panel;
pub mod file_panel;

use crate::app::App;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

pub fn render(f: &mut Frame, app: &mut App) {
    let size = f.area();

    // Clear hit registry for this frame
    app.hit_registry.clear();

    // Main layout: title bar (1) + body + status bar (1)
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Min(3),   // body
            Constraint::Length(1), // status
        ])
        .split(size);

    render_title_bar(f, app, main_layout[0]);

    // Body: left panel + split line (1 col) + right panel
    let left_width = (main_layout[1].width as u32 * app.split_percent as u32 / 100) as u16;
    let left_width = left_width.max(10).min(main_layout[1].width.saturating_sub(20));

    let body_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_width),
            Constraint::Min(20),
        ])
        .split(main_layout[1]);

    file_panel::render(f, app, body_layout[0]);

    // Register drag zone on the split border (the rightmost column of left panel)
    let split_x = body_layout[0].x + body_layout[0].width.saturating_sub(1);
    app.hit_registry.register(
        Rect::new(split_x, body_layout[0].y, 2, body_layout[0].height),
        crate::mouse::ClickAction::StartDragSplit,
    );

    diff_panel::render(f, app, body_layout[1]);

    render_status_bar(f, app, main_layout[2]);
}

fn render_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let repo_name = app.repo.workdir_name();
    let branch = app.repo.branch_name();

    let title = Line::from(vec![
        Span::styled(
            " gv ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(Color::Rgb(30, 30, 40))),
        Span::styled(
            &repo_name,
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(30, 30, 40))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} {}", "⎇", branch),
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::Rgb(30, 30, 40)),
        ),
        // Fill rest of title bar
        Span::styled(
            " ".repeat(area.width.saturating_sub(repo_name.len() as u16 + branch.len() as u16 + 10) as usize),
            Style::default().bg(Color::Rgb(30, 30, 40)),
        ),
    ]);
    f.render_widget(title, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let staged_count = app.staged_files.len();
    let unstaged_count = app.unstaged_files.len();

    if app.select_mode {
        // Select mode — full-width indicator
        let hint = Line::from(vec![
            Span::styled(
                " SELECT ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  拖拽鼠标选择文字，按 v 退出选择模式",
                Style::default().fg(Color::Yellow).bg(Color::Rgb(30, 30, 40)),
            ),
        ]);
        f.render_widget(hint, area);
        return;
    }

    let status = Line::from(vec![
        Span::styled(
            format!(" 暂存: {} ", staged_count),
            Style::default().fg(Color::Green).bg(Color::Rgb(30, 30, 40)),
        ),
        Span::styled(
            format!(" 更改: {} ", unstaged_count),
            Style::default().fg(Color::Yellow).bg(Color::Rgb(30, 30, 40)),
        ),
        Span::styled(
            " ".repeat(area.width.saturating_sub(30) as usize),
            Style::default().bg(Color::Rgb(30, 30, 40)),
        ),
        Span::styled(
            " q:退出 Tab:切换 ↑↓:导航 s:暂存 u:取消暂存 v:选择文字 ",
            Style::default()
                .fg(Color::DarkGray)
                .bg(Color::Rgb(30, 30, 40)),
        ),
    ]);
    f.render_widget(status, area);
}
