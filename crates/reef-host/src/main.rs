mod app;
mod git;
mod mouse;
mod plugin;
mod renderer;
mod ui;

use app::{App, Panel};
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
use std::time::Duration;

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

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    // v toggles select mode regardless of active panel
                    if key.code == KeyCode::Char('v')
                        && key.modifiers.is_empty()
                    {
                        app.select_mode = !app.select_mode;
                        if app.select_mode {
                            execute!(terminal.backend_mut(), DisableMouseCapture)?;
                        } else {
                            execute!(terminal.backend_mut(), EnableMouseCapture)?;
                        }
                    } else if app.show_help {
                        // Any key closes help
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
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Tab => {
            app.active_panel = match app.active_panel {
                Panel::Files => Panel::Diff,
                Panel::Diff => Panel::Files,
            };
        }
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
        KeyCode::Char('s') => {
            if let Some(ref sel) = app.selected_file.clone() {
                if !sel.is_staged {
                    app.stage_file(&sel.path);
                }
            }
        }
        KeyCode::Char('u') => {
            if let Some(ref sel) = app.selected_file.clone() {
                if sel.is_staged {
                    app.unstage_file(&sel.path);
                }
            }
        }
        KeyCode::Char('h') => {
            app.show_help = true;
        }
        KeyCode::Char('m') => {
            app.toggle_diff_layout();
        }
        KeyCode::Char('f') => {
            app.toggle_diff_mode();
        }
        KeyCode::Char('r') => {
            // Refresh
            app.refresh_status();
            if app.selected_file.is_some() {
                app.load_diff();
            }
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
            if let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row) {
                app.handle_action(action);
            }
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
            // Determine which panel based on mouse position
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            let split_x = total_width * app.split_percent / 100;
            if mouse.column < split_x {
                app.file_scroll = app.file_scroll.saturating_sub(3);
            } else {
                app.diff_scroll = app.diff_scroll.saturating_sub(3);
            }
        }
        MouseEventKind::ScrollDown => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            let split_x = total_width * app.split_percent / 100;
            if mouse.column < split_x {
                app.file_scroll += 3;
            } else {
                app.diff_scroll += 3;
            }
        }
        MouseEventKind::Moved => {
            app.hover_row = Some(mouse.row);
        }
        _ => {}
    }
}
