use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use reef::{
    TuiApp, input,
    ui::{preview, theme::Theme},
};
use reef_app::AppCommand;
use test_support::{CwdGuard, HomeGuard, spreadsheet_fixtures, tempdir_repo};

static LOCK: Mutex<()> = Mutex::new(());

#[test]
fn spreadsheet_navigation_renders_cell_values_and_switches_sheets_in_focused_mode() {
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _) = tempdir_repo();
    let _home = HomeGuard::enter(tmp.path());
    let _cwd = CwdGuard::enter(tmp.path());
    std::fs::write(tmp.path().join("sample.xlsx"), &spreadsheet_fixtures()[0].1).unwrap();
    let mut app = TuiApp::new(Theme::dark(), None);
    app.engine
        .dispatch(AppCommand::LoadPreview("sample.xlsx".into()));
    let wake = app.engine.worker_wake_receiver();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.engine.preview_content().is_none() {
        assert!(Instant::now() < deadline, "preview timed out");
        app.tick();
        let _ = wake.recv_timeout(Duration::from_millis(20));
    }
    app.enter_focused_preview();
    let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
    let draw = |terminal: &mut Terminal<TestBackend>, app: &mut TuiApp| {
        terminal
            .draw(|frame| preview::render(frame, app, frame.area(), true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    let rendered = draw(&mut terminal, &mut app);
    assert!(rendered.contains("Name"));
    assert!(rendered.contains("C3: Name"));
    input::handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app);
    input::handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &mut app);
    assert!(draw(&mut terminal, &mut app).contains("D4: 42.5"));
    input::handle_key(
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        &mut app,
    );
    assert_eq!(app.engine.spreadsheet_sheet(), 1);
    assert!(draw(&mut terminal, &mut app).contains("A1: Second sheet"));
    input::handle_key(
        KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE),
        &mut app,
    );
    assert!(draw(&mut terminal, &mut app).contains("Empty worksheet"));
}
