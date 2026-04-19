use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use reef::app::App;
use reef::i18n;
use reef::ui::theme::Theme;
use reef::ui::toast::Toast;
use reef::{editor, input, ui};
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

    // Probe terminal background BEFORE raw mode / alt-screen so the OSC 11
    // reply doesn't fragment onto the TUI. `Theme::resolve` also honours the
    // `ui.theme` pref override and a non-TTY fallback.
    let theme = Theme::resolve();

    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // App init
    let mut app = App::new(theme);

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

                    // `v` toggles select mode, which flips crossterm's mouse capture
                    // so the terminal's native text-selection works. Kept inline
                    // because it's the only input that needs to poke the terminal
                    // backend mid-loop.
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
                        input::handle_key(key, &mut app);
                    }
                }
                Event::Mouse(mouse) => {
                    if !app.select_mode {
                        input::handle_mouse(mouse, &mut app, &terminal);
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

        // Handle an edit request *after* event drain so the terminal is
        // idle. Suspends the TUI, runs `$EDITOR`, resumes. Mouse capture
        // tracks `select_mode` — off in select mode, on otherwise.
        if let Some(path) = app.pending_edit.take() {
            let mouse_was_on = !app.select_mode;
            if let Err(e) = editor::launch(&mut terminal, &path, mouse_was_on) {
                app.toasts
                    .push(Toast::error(i18n::edit_open_failed(&e.to_string())));
            }
            // Pick up on-disk changes immediately rather than waiting for
            // the fs-watcher debounce.
            app.refresh_file_tree();
            app.load_preview();
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
