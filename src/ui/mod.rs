pub mod diff_panel;
pub mod file_panel;

use crate::app::App;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding};
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

    // Help overlay (rendered last, on top of everything)
    if app.show_help {
        render_help(f, size);
    }
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
            " q:退出 Tab:切换 ↑↓:导航 s:暂存 u:取消暂存 m:左右视图 f:全量diff v:选择文字 h:帮助 ",
            Style::default()
                .fg(Color::DarkGray)
                .bg(Color::Rgb(30, 30, 40)),
        ),
    ]);
    f.render_widget(status, area);
}

fn render_help(f: &mut Frame, screen: Rect) {
    // Shortcuts table: (key, description)
    let entries: &[(&str, &str)] = &[
        ("q / Ctrl+C",  "退出"),
        ("Tab",         "切换焦点面板（文件 ↔ Diff）"),
        ("↑ / k",       "向上导航 / 向上滚动"),
        ("↓ / j",       "向下导航 / 向下滚动"),
        ("PageUp",      "快速向上翻页"),
        ("PageDown",    "快速向下翻页"),
        ("s",           "暂存当前选中文件"),
        ("u",           "取消暂存当前选中文件"),
        ("r",           "刷新文件状态"),
        ("m",           "切换 Diff 布局（上下 ↔ 左右）"),
        ("f",           "切换 Diff 模式（局部 ↔ 全量）"),
        ("v",           "进入文字选择模式（禁用鼠标捕获）"),
        ("h",           "显示 / 关闭此帮助"),
        ("任意键",       "关闭帮助"),
    ];

    let popup_w = 54u16;
    let popup_h = entries.len() as u16 + 4; // border(2) + title(1) + blank(1) + entries

    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w.min(screen.width), popup_h.min(screen.height));

    // Clear background behind popup
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(Span::styled(
            " 快捷键帮助 ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut row_y = inner.y;
    for (key, desc) in entries {
        if row_y >= inner.y + inner.height {
            break;
        }
        let line = Line::from(vec![
            Span::styled(
                format!("{:<14}", key),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(*desc, Style::default().fg(Color::White)),
        ]);
        f.render_widget(line, Rect::new(inner.x, row_y, inner.width, 1));
        row_y += 1;
    }
}
