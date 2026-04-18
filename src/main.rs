use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use reef::app::{App, Panel, Tab};
use reef::mouse;
use reef::ui;
use std::io;
use std::panic;
use std::time::{Duration, Instant};

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Set up panic hook to restore terminal
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(panic_info);
    }));

    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // App init
    let mut app = App::new();

    // Main loop
    loop {
        app.tick();
        terminal.draw(|f| ui::render(f, &mut app))?;

        // Block until at least one event arrives (or 16ms timeout for ~60fps)
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }

        // Snapshot selection before processing events
        let sel_before = app
            .selected_file
            .as_ref()
            .map(|s| (s.path.clone(), s.is_staged));

        // Drain ALL pending events so rapid key repeats coalesce — only one
        // render + diff-load runs per frame, regardless of how many events fired.
        loop {
            match event::read()? {
                Event::Key(key) => {
                    // Crossterm emits Press + Release (and Repeat on hold) for every
                    // physical keystroke on Windows Terminal, in terminals that enable
                    // the kitty keyboard protocol, and on some macOS configurations.
                    // Without this filter each keypress runs handle_key twice, so j/k
                    // navigate 2 rows instead of 1 (and 3+ when held).
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }

                    // v toggles select mode regardless of active panel
                    if key.code == KeyCode::Char('v') && key.modifiers.is_empty() {
                        app.select_mode = !app.select_mode;
                        if app.select_mode {
                            execute!(terminal.backend_mut(), DisableMouseCapture)?;
                        } else {
                            execute!(terminal.backend_mut(), EnableMouseCapture)?;
                        }
                    } else if app.show_help {
                        app.show_help = false;
                    } else {
                        handle_key(key, &mut app);
                    }
                }
                Event::Mouse(mouse) => {
                    if !app.select_mode {
                        handle_mouse(mouse, &mut app, &terminal);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
            // Stop draining if no more events are immediately available
            if !event::poll(Duration::from_millis(0))? {
                break;
            }
        }

        // After draining, sync selection state
        let sel_after = app
            .selected_file
            .as_ref()
            .map(|s| (s.path.clone(), s.is_staged));
        if sel_after != sel_before {
            app.load_diff();
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

fn handle_key(key: event::KeyEvent, app: &mut App) {
    // Global keys (work on all tabs)
    match key.code {
        KeyCode::Char('q') => {
            app.should_quit = true;
            return;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
            return;
        }
        KeyCode::Char(c) if matches!(c, '1'..='9') => {
            let idx = (c as u8 - b'1') as usize;
            if let Some(&tab) = Tab::ALL.get(idx) {
                app.active_tab = tab;
            }
            return;
        }
        KeyCode::Tab => {
            let tabs = Tab::ALL;
            let cur = tabs.iter().position(|&t| t == app.active_tab).unwrap_or(0);
            app.active_tab = tabs[(cur + 1) % tabs.len()];
            return;
        }
        KeyCode::BackTab => {
            app.active_panel = match app.active_panel {
                Panel::Files => Panel::Diff,
                Panel::Diff => Panel::Files,
            };
            return;
        }
        KeyCode::Char('h') => {
            app.show_help = true;
            return;
        }
        _ => {}
    }

    match app.active_tab {
        Tab::Git => handle_key_git(key, app),
        Tab::Files => handle_key_files(key, app),
        Tab::Graph => handle_key_graph(key, app),
    }
}

fn handle_key_graph(key: event::KeyEvent, app: &mut App) {
    use reef::ui::{commit_detail_panel, git_graph_panel};
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => {
                git_graph_panel::handle_key(app, "k");
            }
            Panel::Diff => commit_detail_panel::scroll(app, -1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => {
                git_graph_panel::handle_key(app, "j");
            }
            Panel::Diff => commit_detail_panel::scroll(app, 1),
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                for _ in 0..10 {
                    git_graph_panel::handle_key(app, "k");
                }
            }
            Panel::Diff => commit_detail_panel::scroll(app, -20),
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                for _ in 0..10 {
                    git_graph_panel::handle_key(app, "j");
                }
            }
            Panel::Diff => commit_detail_panel::scroll(app, 20),
        },
        KeyCode::Char('r') => {
            // `r` on the graph sidebar = force a graph cache refresh
            app.git_graph.cache_key = None;
            app.refresh_graph();
        }
        // m/f/t target the commit-detail panel regardless of focus.
        KeyCode::Char('m') => {
            commit_detail_panel::handle_key(app, "m");
        }
        KeyCode::Char('f') => {
            commit_detail_panel::handle_key(app, "f");
        }
        KeyCode::Char('t') => {
            commit_detail_panel::handle_key(app, "t");
        }
        _ => {}
    }
}

fn handle_key_git(key: event::KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => app.navigate_files(-1),
            Panel::Diff => {
                app.diff_scroll = app.diff_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => app.navigate_files(1),
            Panel::Diff => {
                app.diff_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => app.navigate_files(-10),
            Panel::Diff => {
                app.diff_scroll = app.diff_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => app.navigate_files(10),
            Panel::Diff => {
                app.diff_scroll += 20;
            }
        },
        KeyCode::Left if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.diff_h_scroll = app.diff_h_scroll.saturating_sub(step);
        }
        KeyCode::Right if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.diff_h_scroll = app.diff_h_scroll.saturating_add(step);
        }
        KeyCode::Home if app.active_panel == Panel::Diff => {
            app.diff_h_scroll = 0;
        }
        KeyCode::End if app.active_panel == Panel::Diff => {
            app.diff_h_scroll = usize::MAX; // render 自动钳到实际最大值
        }
        KeyCode::Char('s') => {
            reef::ui::git_status_panel::handle_key(app, "s");
        }
        KeyCode::Char('u') => {
            reef::ui::git_status_panel::handle_key(app, "u");
        }
        KeyCode::Char('d') => {
            reef::ui::git_status_panel::handle_key(app, "d");
        }
        KeyCode::Char('y') => {
            reef::ui::git_status_panel::handle_key(app, "y");
        }
        KeyCode::Char('n') => {
            reef::ui::git_status_panel::handle_key(app, "n");
        }
        KeyCode::Esc => {
            reef::ui::git_status_panel::handle_key(app, "Escape");
        }
        KeyCode::Char('r') => {
            reef::ui::git_status_panel::handle_key(app, "r");
        }
        KeyCode::Char('t') => {
            reef::ui::git_status_panel::handle_key(app, "t");
        }
        KeyCode::Char('m') => {
            app.toggle_diff_layout();
        }
        KeyCode::Char('f') => {
            app.toggle_diff_mode();
        }
        _ => {}
    }
}

fn handle_key_files(key: event::KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-1);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll = app.preview_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(1);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-10);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll = app.preview_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(10);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll += 20;
            }
        },
        KeyCode::Left if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.preview_h_scroll = app.preview_h_scroll.saturating_sub(step);
        }
        KeyCode::Right if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.preview_h_scroll = app.preview_h_scroll.saturating_add(step);
        }
        KeyCode::Home if app.active_panel == Panel::Diff => {
            app.preview_h_scroll = 0;
        }
        KeyCode::End if app.active_panel == Panel::Diff => {
            app.preview_h_scroll = usize::MAX;
        }
        KeyCode::Enter => {
            let idx = app.file_tree.selected;
            if let Some(entry) = app.file_tree.entries.get(idx) {
                if entry.is_dir {
                    app.file_tree.toggle_expand(idx);
                }
            }
            app.load_preview();
        }
        KeyCode::Char('r') => {
            app.refresh_file_tree();
        }
        _ => {}
    }
}

fn handle_mouse<B: ratatui::backend::Backend>(
    mouse: MouseEvent,
    app: &mut App,
    terminal: &Terminal<B>,
) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();
            let is_double = matches!(
                app.last_click,
                Some((t, c, r))
                    if c == mouse.column
                        && r == mouse.row
                        && now.duration_since(t) < DOUBLE_CLICK_WINDOW
            );

            if let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row) {
                // On double-click: if the region carries a dbl action, swap to it
                // and run through handle_action so host-side side effects fire.
                let effective = if is_double {
                    if let crate::mouse::ClickAction::GitCommand {
                        dbl_command: Some(ref cmd),
                        ref dbl_args,
                        ..
                    } = action
                    {
                        crate::mouse::ClickAction::GitCommand {
                            command: cmd.clone(),
                            args: dbl_args.clone().unwrap_or(serde_json::Value::Null),
                            dbl_command: None,
                            dbl_args: None,
                        }
                    } else {
                        action
                    }
                } else {
                    action
                };
                app.handle_action(effective);
            }

            // Reset tracking on every genuine second click so triple-clicks don't chain.
            app.last_click = if is_double {
                None
            } else {
                Some((now, mouse.column, mouse.row))
            };
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.dragging_split = false;
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.dragging_split {
                let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
                if total_width > 0 {
                    let percent = (mouse.column * 100 / total_width).clamp(10, 80);
                    app.split_percent = percent;
                }
            }
        }
        MouseEventKind::ScrollUp => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            // Shift + 滚轮 = 横向滚动（兼容不发 ScrollLeft/Right 的终端）
            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                apply_horizontal_scroll(app, mouse.column, total_width, -3);
                return;
            }
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left {
                        reef::ui::git_status_panel::scroll(app, -3);
                    } else {
                        app.diff_scroll = app.diff_scroll.saturating_sub(3);
                    }
                }
                Tab::Files => {
                    if is_left {
                        app.tree_scroll = app.tree_scroll.saturating_sub(3);
                    } else {
                        app.preview_scroll = app.preview_scroll.saturating_sub(3);
                    }
                }
                Tab::Graph => {
                    if is_left {
                        reef::ui::git_graph_panel::scroll(app, -3);
                    } else {
                        reef::ui::commit_detail_panel::scroll(app, -3);
                    }
                }
            }
        }
        MouseEventKind::ScrollDown => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                apply_horizontal_scroll(app, mouse.column, total_width, 3);
                return;
            }
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left {
                        reef::ui::git_status_panel::scroll(app, 3);
                    } else {
                        app.diff_scroll += 3;
                    }
                }
                Tab::Files => {
                    if is_left {
                        app.tree_scroll += 3;
                    } else {
                        app.preview_scroll += 3;
                    }
                }
                Tab::Graph => {
                    if is_left {
                        reef::ui::git_graph_panel::scroll(app, 3);
                    } else {
                        reef::ui::commit_detail_panel::scroll(app, 3);
                    }
                }
            }
        }
        MouseEventKind::ScrollLeft => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            apply_horizontal_scroll(app, mouse.column, total_width, -3);
        }
        MouseEventKind::ScrollRight => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            apply_horizontal_scroll(app, mouse.column, total_width, 3);
        }
        MouseEventKind::Moved => {
            app.hover_row = Some(mouse.row);
            app.hover_col = Some(mouse.column);
        }
        _ => {}
    }
}

/// Apply a horizontal-scroll delta (in display columns) to the preview / diff
/// panel under the cursor. Events landing on the left sidebar are ignored.
/// Graph tab doesn't support horizontal scroll yet — the commit-detail rows
/// already truncate to the panel width; if long-line viewing becomes a real
/// need, wire up `CommitDetailState.diff_h_scroll` here.
fn apply_horizontal_scroll(app: &mut App, column: u16, total_width: u16, delta: i32) {
    let split_x = total_width * app.split_percent / 100;
    let is_left = column < split_x;
    if is_left {
        return;
    }
    let target = match app.active_tab {
        Tab::Files => &mut app.preview_h_scroll,
        Tab::Git => &mut app.diff_h_scroll,
        Tab::Graph => return,
    };
    *target = if delta < 0 {
        target.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        target.saturating_add(delta as usize)
    };
}
