pub mod commit_detail_panel;
pub mod diff_panel;
pub mod file_preview_panel;
pub mod file_tree_panel;
pub mod git_graph_panel;
pub mod git_status_panel;
pub mod hover;
pub mod text;

use crate::app::{App, Tab};
use crate::mouse::ClickAction;
use crate::toast::ToastLevel;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App) {
    let size = f.area();
    app.hit_registry.clear();

    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // tab bar
            Constraint::Min(3),    // body
            Constraint::Length(1), // status
        ])
        .split(size);

    render_title_bar(f, app, main_layout[0]);
    render_tab_bar(f, app, main_layout[1]);

    // Body: left + right
    let left_width = (main_layout[2].width as u32 * app.split_percent as u32 / 100) as u16;
    let left_width = left_width
        .max(10)
        .min(main_layout[2].width.saturating_sub(20));

    let body_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_width), Constraint::Min(20)])
        .split(main_layout[2]);

    // Drag zone on the split border
    let split_x = body_layout[0].x + body_layout[0].width.saturating_sub(1);
    app.hit_registry.register(
        Rect::new(split_x, body_layout[0].y, 2, body_layout[0].height),
        ClickAction::StartDragSplit,
    );

    match app.active_tab {
        Tab::Git => {
            if app.repo.is_none() {
                render_no_repo(f, body_layout[0]);
            } else {
                render_git_sidebar(f, app, body_layout[0]);
                render_git_editor(f, app, body_layout[1]);
            }
        }
        Tab::Files => {
            file_tree_panel::render(f, app, body_layout[0]);
            file_preview_panel::render(f, app, body_layout[1]);
        }
        Tab::Graph => {
            render_graph_sidebar(f, app, body_layout[0]);
            render_graph_editor(f, app, body_layout[1]);
        }
    }

    render_status_bar(f, app, main_layout[3]);

    if app.show_help {
        render_help(f, size);
    }
}

/// Full-width message shown in the Git tab when not inside a git repository.
fn render_no_repo(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let msg = Paragraph::new(Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Not a git repository",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Run `git init` to initialise one, or open reef inside a git repo.",
            Style::default().fg(Color::DarkGray),
        )),
    ]))
    .alignment(ratatui::layout::Alignment::Center)
    .block(block);
    f.render_widget(msg, area);
}

/// Git tab's left sidebar — inline host-native status panel.
fn render_git_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );
    let focused = matches!(app.active_panel, crate::app::Panel::Files);
    git_status_panel::render(f, app, padded, focused);
}

/// Git tab's right editor — host-native diff panel.
fn render_git_editor(f: &mut Frame, app: &mut App, area: Rect) {
    diff_panel::render(f, app, area);
}

/// Graph tab's left sidebar — inline commit-graph panel.
fn render_graph_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let padded = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(1),
        inner.height,
    );
    let focused = matches!(app.active_panel, crate::app::Panel::Files);
    git_graph_panel::render(f, app, padded, focused);
}

/// Graph tab's right editor — inline commit-detail panel.
fn render_graph_editor(f: &mut Frame, app: &mut App, area: Rect) {
    let inner = Rect::new(
        area.x + 1,
        area.y,
        area.width.saturating_sub(1),
        area.height,
    );
    let focused = matches!(app.active_panel, crate::app::Panel::Diff);
    commit_detail_panel::render(f, app, inner, focused);
}

fn render_tab_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let bg = Color::Rgb(30, 30, 40);
    let tabs = Tab::ALL;

    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;

    for (i, tab) in tabs.iter().enumerate() {
        let label = tab.label();
        let is_active = app.active_tab == *tab;
        let style = if is_active {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(60, 60, 80))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray).bg(bg)
        };
        let span = Span::styled(label, style);
        // Hit-registry uses terminal columns (display width), not bytes — tab
        // labels contain wide glyphs (📁, ⑂) so UnicodeWidthStr::width is
        // required to keep click zones aligned with the rendered text.
        let w = UnicodeWidthStr::width(label) as u16;

        app.hit_registry
            .register_row(x, area.y, w, ClickAction::SwitchTab(*tab));
        x += w;
        spans.push(span);

        // Separator between tabs
        if i < tabs.len() - 1 {
            spans.push(Span::styled(
                "│",
                Style::default().fg(Color::DarkGray).bg(bg),
            ));
            x += 1;
        }
    }

    // Fill rest of row
    let remaining = (area.width as usize).saturating_sub(x.saturating_sub(area.x) as usize);
    let keys_hint = " 1:Files 2:Git 3:Graph";
    let pad = remaining.saturating_sub(UnicodeWidthStr::width(keys_hint));
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    spans.push(Span::styled(
        keys_hint,
        Style::default().fg(Color::DarkGray).bg(bg),
    ));

    f.render_widget(Line::from(spans), area);
}

fn render_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let repo_name = app
        .repo
        .as_ref()
        .map(|r| r.workdir_name())
        .unwrap_or_else(|| "—".to_string());
    let branch = app
        .repo
        .as_ref()
        .map(|r| r.branch_name())
        .unwrap_or_default();

    let title = Line::from(vec![
        Span::styled(
            " reef ",
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
            if branch.is_empty() {
                String::new()
            } else {
                format!("  ⎇ {}", branch)
            },
            Style::default().fg(Color::Cyan).bg(Color::Rgb(30, 30, 40)),
        ),
        Span::styled(
            " ".repeat(
                area.width
                    .saturating_sub(repo_name.len() as u16 + branch.len() as u16 + 10)
                    as usize,
            ),
            Style::default().bg(Color::Rgb(30, 30, 40)),
        ),
    ]);
    f.render_widget(title, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    if app.select_mode {
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
                Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::Rgb(30, 30, 40)),
            ),
        ]);
        f.render_widget(hint, area);
        return;
    }

    // Show the most recent toast (push success/failure etc.) inline.
    let (notif, notif_color) = match app.toasts.last() {
        Some(t) => (
            format!("  {} ", t.message),
            match t.level {
                ToastLevel::Error => Color::Red,
                ToastLevel::Warn => Color::Yellow,
                ToastLevel::Info => Color::Cyan,
            },
        ),
        None => (String::new(), Color::Cyan),
    };

    let status = Line::from(vec![
        Span::styled(
            notif,
            Style::default().fg(notif_color).bg(Color::Rgb(30, 30, 40)),
        ),
        Span::styled(
            " ".repeat(area.width.saturating_sub(60) as usize),
            Style::default().bg(Color::Rgb(30, 30, 40)),
        ),
        Span::styled(
            " q:退出 Tab:切换 s:暂存 u:取消 r:刷新 h:帮助 ",
            Style::default()
                .fg(Color::DarkGray)
                .bg(Color::Rgb(30, 30, 40)),
        ),
    ]);
    f.render_widget(status, area);
}

fn render_help(f: &mut Frame, screen: Rect) {
    let core_entries: &[(&str, &str)] = &[
        ("q / Ctrl+C", "退出"),
        ("Tab", "切换顶部标签页（Files ↔ Git ↔ Graph）"),
        ("Shift+Tab", "切换焦点面板（侧边栏 ↔ 编辑区）"),
        ("1 … 9", "跳转到第 N 个标签页"),
        ("↑ / k", "向上导航 / 向上滚动"),
        ("↓ / j", "向下导航 / 向下滚动"),
        ("PageUp", "快速向上翻页"),
        ("PageDown", "快速向下翻页"),
        ("← / →", "横向滚动（Diff/预览 面板聚焦时）"),
        ("Shift+← / Shift+→", "横向快速滚动（10 列）"),
        ("Home / End", "回到行首 / 跳到行尾"),
        ("Shift+滚轮 / 触控板横划", "鼠标横向滚动"),
        ("s / u", "暂存 / 取消暂存（Git tab）"),
        ("d → y", "还原工作树文件（Git tab）"),
        ("m", "切换 Diff 布局（上下 ↔ 左右）"),
        ("f", "切换 Diff 模式（局部 ↔ 全量）"),
        ("t", "切换列表 / 树形视图"),
        ("r", "刷新"),
        ("v", "文字选择模式"),
        ("h", "显示 / 关闭此帮助"),
        ("任意键", "关闭帮助"),
    ];

    let popup_w = 72u16;
    let popup_h = core_entries.len() as u16 + 4;
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w.min(screen.width), popup_h.min(screen.height));

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(Span::styled(
            " 快捷键帮助 ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let key_col = 24usize;
    let mut row_y = inner.y;

    for (key, desc) in core_entries {
        if row_y >= inner.y + inner.height {
            break;
        }
        let line = Line::from(vec![
            Span::styled(
                format!("{:<width$}", key, width = key_col),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(*desc, Style::default().fg(Color::White)),
        ]);
        f.render_widget(line, Rect::new(inner.x, row_y, inner.width, 1));
        row_y += 1;
    }
}
