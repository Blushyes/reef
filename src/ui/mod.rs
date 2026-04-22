pub mod commit_detail_panel;
pub mod context_menu_panel;
pub mod diff_panel;
pub mod file_preview_panel;
pub mod file_tree_panel;
pub mod git_graph_panel;
pub mod git_status_panel;
pub mod global_search_panel;
pub mod highlight;
pub mod hosts_picker_panel;
pub mod hover;
pub mod mouse;
pub mod quick_open_panel;
pub mod search_tab;
pub mod text;
pub mod theme;
pub mod toast;

use crate::app::{App, Tab};
use crate::i18n::{Msg, t};
use crate::ui::mouse::ClickAction;
use crate::ui::toast::ToastLevel;
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
            if !app.backend.has_repo() {
                render_no_repo(f, app, body_layout[0]);
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
        Tab::Search => {
            search_tab::render_sidebar(f, app, body_layout[0]);
            file_preview_panel::render(f, app, body_layout[1]);
        }
    }

    render_status_bar(f, app, main_layout[3]);

    if app.show_help {
        render_help(f, app, size);
    }

    // Render last so the palette overlays help if both are somehow active
    // (shouldn't happen in practice — opening one dismisses the other via
    // input-priority — but the ordering here is a belt-and-braces guard).
    // Global-search after quick-open: if both flags were somehow true the
    // later render wins on overlap, and having global-search on top matches
    // its priority in input dispatch.
    if app.quick_open.active {
        quick_open_panel::render(f, app, size);
    }
    if app.global_search.active {
        global_search_panel::render(f, app, size);
    }
    if app.hosts_picker.active {
        hosts_picker_panel::render(f, app, size);
    }
    // Context menu overlay renders last so it sits above the help
    // popup and any other in-panel chrome. The menu itself is scoped
    // to the Files tab but we don't gate on `active_tab` here: the
    // menu can only be opened from a right-click while the Files tab
    // is active, and `tree_edit.active` implicitly prevents concurrent
    // opens. Once shown, the menu should stay visible even if the
    // user tab-switches (unlikely — input is gated) so the render
    // check only looks at `tree_context_menu.active`.
    if app.tree_context_menu.active {
        context_menu_panel::render(f, app, size);
    }
}

/// Full-width message shown in the Git tab when not inside a git repository.
fn render_no_repo(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.border));
    let msg = Paragraph::new(Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            t(Msg::NoRepoTitle),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            t(Msg::NoRepoHint),
            Style::default().fg(th.fg_secondary),
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
        .border_style(Style::default().fg(app.theme.border));
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
        .border_style(Style::default().fg(app.theme.border));
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
    let th = app.theme;
    let bg = th.chrome_bg;
    let tabs = Tab::ALL;

    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;

    for (i, tab) in tabs.iter().enumerate() {
        let label = tab.label();
        let is_active = app.active_tab == *tab;
        let style = if is_active {
            Style::default()
                .fg(th.chrome_active_fg)
                .bg(th.chrome_active_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.chrome_muted_fg).bg(bg)
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
                Style::default().fg(th.chrome_muted_fg).bg(bg),
            ));
            x += 1;
        }
    }

    // Fill rest of row
    let remaining = (area.width as usize).saturating_sub(x.saturating_sub(area.x) as usize);
    let keys_hint = t(Msg::TabBarHint);
    let pad = remaining.saturating_sub(UnicodeWidthStr::width(keys_hint));
    spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
    spans.push(Span::styled(
        keys_hint,
        Style::default().fg(th.chrome_muted_fg).bg(bg),
    ));

    f.render_widget(Line::from(spans), area);
}

fn render_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let repo_name = if app.backend.has_repo() {
        app.workdir_name.as_str()
    } else {
        "—"
    };
    let branch = app.branch_name.as_str();

    let title = Line::from(vec![
        Span::styled(
            " reef ",
            Style::default()
                .fg(th.badge_fg)
                .bg(th.badge_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(th.chrome_bg)),
        Span::styled(
            repo_name,
            Style::default()
                .fg(th.chrome_fg)
                .bg(th.chrome_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if branch.is_empty() {
                String::new()
            } else {
                format!("  ⎇ {}", branch)
            },
            Style::default().fg(th.accent).bg(th.chrome_bg),
        ),
        Span::styled(
            " ".repeat(
                area.width
                    .saturating_sub(repo_name.len() as u16 + branch.len() as u16 + 10)
                    as usize,
            ),
            Style::default().bg(th.chrome_bg),
        ),
    ]);
    f.render_widget(title, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;

    // Search prompt has the highest priority — while active it fully owns the
    // status row so the user can see what they're typing.
    if app.search.active {
        render_search_prompt(f, app, area);
        return;
    }

    // Post-search "n/N hint" indicator: when a search session is dormant but
    // still has matches, show a compact counter so the user remembers they
    // can keep stepping.
    if !app.search.active && !app.search.matches.is_empty() {
        render_search_dormant(f, app, area);
        return;
    }

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
                t(Msg::SelectModeHint),
                Style::default().fg(Color::Yellow).bg(th.chrome_bg),
            ),
        ]);
        f.render_widget(hint, area);
        return;
    }

    // Place-mode modal indicator. Mirrors the select-mode pattern: a loud
    // badge in the accent color so the user can't miss that a mode is
    // active, plus a hint describing how to commit or cancel. When a
    // copy is actively running the hint swaps to a copying indicator so
    // the status bar proves the worker is still alive on big transfers.
    if app.place_mode.active {
        let copying = app.file_copy_load.loading;
        let (badge_text, hint_text) = if copying {
            (" 📋 COPYING ", crate::i18n::place_mode_copying_banner())
        } else {
            (
                " 📋 PLACE ",
                crate::i18n::place_mode_status_hint().to_string(),
            )
        };
        let hint = Line::from(vec![
            Span::styled(
                badge_text,
                Style::default()
                    .fg(th.chrome_bg)
                    .bg(th.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(hint_text, Style::default().fg(th.accent).bg(th.chrome_bg)),
            Span::styled(
                " ".repeat(area.width.saturating_sub(80) as usize),
                Style::default().bg(th.chrome_bg),
            ),
        ]);
        f.render_widget(hint, area);
        return;
    }

    // Tree delete-confirm: status bar becomes a red ⚠ prompt. Mirrors
    // the select/place mode takeover pattern. Confirm key routing
    // lives in `input::handle_key_tree_delete_confirm` — here we
    // just draw.
    if let Some(pending) = app.tree_delete_confirm.as_ref() {
        let prompt = crate::i18n::tree_delete_confirm_prompt(
            &pending.display_name,
            pending.is_dir,
            pending.hard,
        );
        let badge = if pending.hard {
            " ⚠ DELETE "
        } else {
            " 🗑 TRASH "
        };
        let hint = Line::from(vec![
            Span::styled(
                badge,
                Style::default()
                    .fg(Color::White)
                    .bg(th.error_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                prompt,
                Style::default()
                    .fg(th.fg_primary)
                    .bg(th.chrome_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ".repeat(area.width.saturating_sub(80) as usize),
                Style::default().bg(th.chrome_bg),
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
        None => match app.activity_message() {
            Some(msg) => (format!("  {} ", msg), Color::Cyan),
            None => (String::new(), Color::Cyan),
        },
    };

    let status = Line::from(vec![
        Span::styled(notif, Style::default().fg(notif_color).bg(th.chrome_bg)),
        Span::styled(
            " ".repeat(area.width.saturating_sub(60) as usize),
            Style::default().bg(th.chrome_bg),
        ),
        Span::styled(
            t(Msg::StatusBarHint),
            Style::default().fg(th.chrome_muted_fg).bg(th.chrome_bg),
        ),
    ]);
    f.render_widget(status, area);
}

fn render_search_prompt(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let prefix = if app.search.backwards { '?' } else { '/' };
    let query = app.search.query.as_str();

    // Build the right-side counter / status text.
    let right = match (
        app.search.matches.len(),
        app.search.current,
        app.search.wrap_msg,
        query.is_empty(),
    ) {
        (0, _, _, true) => String::new(),
        (0, _, _, false) => t(Msg::SearchNoMatch).to_string(),
        (n, Some(i), wrap, _) => crate::i18n::search_counter(i, n, wrap),
        _ => String::new(),
    };

    let prompt_text = format!("{}{}", prefix, query);
    let right_width = UnicodeWidthStr::width(right.as_str()) as u16;

    // Draw background fill for the whole row first.
    let fill = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        Style::default().bg(th.chrome_bg),
    ));
    f.render_widget(fill, area);

    // Prompt on the left.
    let prompt_style = Style::default()
        .fg(th.fg_primary)
        .bg(th.chrome_bg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(
        Line::from(Span::styled(prompt_text.clone(), prompt_style)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    // Counter on the right.
    if right_width > 0 && right_width < area.width {
        let rx = area.x + area.width.saturating_sub(right_width);
        let right_style = Style::default().fg(th.fg_secondary).bg(th.chrome_bg);
        f.render_widget(
            Line::from(Span::styled(right, right_style)),
            Rect::new(rx, area.y, right_width, 1),
        );
    }

    // Blinking terminal cursor at the current insertion point — lets the user
    // see where new chars will land without a static `█` glyph.
    let prefix_w = 1u16; // '/' and '?' are always narrow.
    let cursor_w =
        UnicodeWidthStr::width(&app.search.query[..app.search.cursor.min(app.search.query.len())])
            as u16;
    let cursor_x = area.x + (prefix_w + cursor_w).min(area.width.saturating_sub(1));
    f.set_cursor_position((cursor_x, area.y));
}

fn render_search_dormant(f: &mut Frame, app: &App, area: Rect) {
    let th = app.theme;
    let prefix = if app.search.backwards { '?' } else { '/' };
    let counter = match app.search.current {
        Some(i) => crate::i18n::search_dormant_with_counter(
            prefix,
            &app.search.query,
            i,
            app.search.matches.len(),
        ),
        None => format!(" {}{} ", prefix, app.search.query),
    };
    let counter_w = UnicodeWidthStr::width(counter.as_str()) as u16;
    let fill = Line::from(Span::styled(
        " ".repeat(area.width as usize),
        Style::default().bg(th.chrome_bg),
    ));
    f.render_widget(fill, area);
    let style = Style::default().fg(th.fg_secondary).bg(th.chrome_bg);
    f.render_widget(
        Line::from(Span::styled(counter, style)),
        Rect::new(area.x, area.y, counter_w.min(area.width), 1),
    );
}

fn render_help(f: &mut Frame, app: &App, screen: Rect) {
    let th = app.theme;
    let core_entries: &[(&str, &str)] = &[
        ("q / Ctrl+C", t(Msg::HelpQuit)),
        ("Tab", t(Msg::HelpSwitchTab)),
        ("Shift+Tab", t(Msg::HelpSwitchPanel)),
        ("1 … 9", t(Msg::HelpJumpTab)),
        ("↑ / k / Ctrl+P", t(Msg::HelpNavUp)),
        ("↓ / j / Ctrl+N", t(Msg::HelpNavDown)),
        ("PageUp", t(Msg::HelpPageUp)),
        ("PageDown", t(Msg::HelpPageDown)),
        ("← / →", t(Msg::HelpHScroll)),
        ("Shift+← / Shift+→", t(Msg::HelpHScrollFast)),
        ("Home / End", t(Msg::HelpHomeEnd)),
        (t(Msg::HelpKeyMouseHScroll), t(Msg::HelpMouseHScroll)),
        ("s / u", t(Msg::HelpStageUnstage)),
        ("d → y", t(Msg::HelpDiscard)),
        ("m", t(Msg::HelpDiffLayout)),
        ("f", t(Msg::HelpDiffMode)),
        ("t", t(Msg::HelpToggleView)),
        ("r", t(Msg::HelpRefresh)),
        ("v", t(Msg::HelpSelectMode)),
        ("h", t(Msg::HelpShowHelp)),
        ("Space p", t(Msg::HelpQuickOpen)),
        ("Space f", t(Msg::HelpGlobalSearch)),
        (t(Msg::HelpKeyDragDrop), t(Msg::HelpDragDrop)),
        ("F2", t(Msg::HelpRenameEntry)),
        ("d / Del / ⌫", t(Msg::HelpDeleteEntry)),
        ("Shift+D / Shift+Del", t(Msg::HelpHardDeleteEntry)),
        (t(Msg::HelpKeyRightClick), t(Msg::HelpRightClickMenu)),
        (t(Msg::HelpKeyAnyKey), t(Msg::HelpAnyKey)),
    ];

    let popup_w = 72u16;
    let popup_h = core_entries.len() as u16 + 4;
    let x = screen.x + screen.width.saturating_sub(popup_w) / 2;
    let y = screen.y + screen.height.saturating_sub(popup_h) / 2;
    let area = Rect::new(x, y, popup_w.min(screen.width), popup_h.min(screen.height));

    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent))
        .title(Span::styled(
            t(Msg::HelpTitle),
            Style::default()
                .fg(th.fg_primary)
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
            Span::styled(*desc, Style::default().fg(th.fg_primary)),
        ]);
        f.render_widget(line, Rect::new(inner.x, row_y, inner.width, 1));
        row_y += 1;
    }
}
