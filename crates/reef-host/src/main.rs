mod app;
mod file_tree;
mod git;
mod mouse;
mod plugin;
mod renderer;
mod ui;

use app::{App, Panel, Tab};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
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
    let mut app = match App::new() {
        Ok(app) => app,
        Err(e) => {
            // Restore terminal before printing error
            disable_raw_mode()?;
            execute!(
                io::stdout(),
                LeaveAlternateScreen,
                DisableMouseCapture
            )?;
            eprintln!("错误: 无法打开 Git 仓库: {}", e);
            eprintln!("请在 Git 仓库目录中运行 reef");
            std::process::exit(1);
        }
    };

    // Main loop
    loop {
        app.tick_plugins();
        terminal.draw(|f| ui::render(f, &mut app))?;

        // Block until at least one event arrives (or 16ms timeout for ~60fps)
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }

        // Snapshot selection before processing events
        let sel_before = app.selected_file.as_ref().map(|s| (s.path.clone(), s.is_staged));

        // Drain ALL pending events so rapid key repeats don't queue plugin commands
        loop {
            match event::read()? {
                Event::Key(key) => {
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
        let sel_after = app.selected_file.as_ref().map(|s| (s.path.clone(), s.is_staged));
        if sel_after != sel_before {
            // Load diff natively in the host (no plugin round-trip)
            app.load_diff();
            // Notify plugin so the sidebar highlights the selected file
            if let Some(ref sel) = app.selected_file.clone() {
                app.plugin_manager.queue_select_file(&sel.path, sel.is_staged);
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Shutdown plugins
    app.plugin_manager.shutdown();

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
        KeyCode::Char('q') => { app.should_quit = true; return; }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true; return;
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
        KeyCode::Char('h') => { app.show_help = true; return; }
        _ => {}
    }

    match app.active_tab {
        Tab::Git => handle_key_git(key, app),
        Tab::Files => handle_key_files(key, app),
    }
}

fn handle_key_git(key: event::KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => app.navigate_files(-1),
            Panel::Diff => { app.diff_scroll = app.diff_scroll.saturating_sub(1); }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => app.navigate_files(1),
            Panel::Diff => { app.diff_scroll += 1; }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => app.navigate_files(-10),
            Panel::Diff => { app.diff_scroll = app.diff_scroll.saturating_sub(20); }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => app.navigate_files(10),
            Panel::Diff => { app.diff_scroll += 20; }
        },
        KeyCode::Char('s') => {
            if !app.route_key_to_plugin("s") {
                if let Some(ref sel) = app.selected_file.clone() {
                    if !sel.is_staged { app.stage_file(&sel.path); }
                }
            }
        }
        KeyCode::Char('u') => {
            if !app.route_key_to_plugin("u") {
                if let Some(ref sel) = app.selected_file.clone() {
                    if sel.is_staged { app.unstage_file(&sel.path); }
                }
            }
        }
        KeyCode::Char('r') => {
            if !app.route_key_to_plugin("r") {
                app.refresh_status();
                if app.selected_file.is_some() { app.load_diff(); }
            }
        }
        KeyCode::Char('t') => { app.route_key_to_plugin("t"); }
        KeyCode::Char('m') => { app.toggle_diff_layout(); }
        KeyCode::Char('f') => { app.toggle_diff_mode(); }
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
            Panel::Diff => { app.preview_scroll = app.preview_scroll.saturating_sub(1); }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(1);
                app.load_preview();
            }
            Panel::Diff => { app.preview_scroll += 1; }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-10);
                app.load_preview();
            }
            Panel::Diff => { app.preview_scroll = app.preview_scroll.saturating_sub(20); }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(10);
                app.load_preview();
            }
            Panel::Diff => { app.preview_scroll += 20; }
        },
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
            app.file_tree.rebuild();
            app.refresh_status(); // re-syncs git decorations
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
                    if let crate::mouse::ClickAction::PluginCommand {
                        dbl_command: Some(ref cmd),
                        ref dbl_args,
                        ..
                    } = action
                    {
                        crate::mouse::ClickAction::PluginCommand {
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
            app.last_click = if is_double { None } else { Some((now, mouse.column, mouse.row)) };
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.dragging_split = false;
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.dragging_split {
                let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
                if total_width > 0 {
                    let percent = (mouse.column as u16 * 100 / total_width).clamp(10, 80);
                    app.split_percent = percent;
                }
            }
        }
        MouseEventKind::ScrollUp => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left { app.file_scroll = app.file_scroll.saturating_sub(3); }
                    else { app.diff_scroll = app.diff_scroll.saturating_sub(3); }
                }
                Tab::Files => {
                    if is_left { app.tree_scroll = app.tree_scroll.saturating_sub(3); }
                    else { app.preview_scroll = app.preview_scroll.saturating_sub(3); }
                }
            }
        }
        MouseEventKind::ScrollDown => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left { app.file_scroll += 3; }
                    else { app.diff_scroll += 3; }
                }
                Tab::Files => {
                    if is_left { app.tree_scroll += 3; }
                    else { app.preview_scroll += 3; }
                }
            }
        }
        MouseEventKind::Moved => {
            app.hover_row = Some(mouse.row);
            app.hover_col = Some(mouse.column);
        }
        _ => {}
    }
}
