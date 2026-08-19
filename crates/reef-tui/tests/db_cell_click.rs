//! End-to-end mouse path for opening a SQLite cell: render a frame so
//! the data grid registers its per-cell hit zones, push a real
//! `Down(Left)` through `input::handle_mouse`, then keep stepping to
//! confirm the opened cell survives — a cell that flashes and vanishes
//! means something downstream is clearing `DbPreviewState::cell`.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use reef::TuiApp as App;
use reef::ui;
use reef::ui::mouse::ClickAction;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use test_support::{CwdGuard, HomeGuard, force_en_lang, tempdir_repo};

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn seed(tmp_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(tmp_path.join("cells.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE step_run (id INTEGER PRIMARY KEY, prompt TEXT);
        INSERT INTO step_run(id, prompt) VALUES
            (1, '06,不得遗漏中间任何一条。相邻两条之间必须保持连续,不得跳跃。'),
            (2, '08,把本段的动作拆成六个镜头。');
        "#,
    )
    .unwrap();
}

/// Enough rows that the grid scrolls, for the scroll-ownership test.
fn seed_many_rows(tmp_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(tmp_path.join("cells.db")).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE step_run (id INTEGER PRIMARY KEY, prompt TEXT);
        WITH RECURSIVE seq(n) AS (
            SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 40
        )
        INSERT INTO step_run(id, prompt)
            SELECT n, '把本段的动作拆成六个镜头,每个镜头单独描述。' FROM seq;
        "#,
    )
    .unwrap();
}

fn wait_for<F: Fn(&mut App) -> bool>(app: &mut App, what: &str, done: F) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        app.tick();
        if done(app) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn render(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
    terminal.draw(|f| ui::render(f, app)).expect("draw");
    let buf = terminal.backend().buffer();
    (0..buf.area().height)
        .map(|y| {
            (0..buf.area().width)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn clicking_a_grid_cell_opens_a_pane_that_stays_open() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    seed(tmp.path());
    let home = tempfile::TempDir::new().unwrap();
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.refresh_file_tree();
    wait_for(&mut app, "file tree", |app| {
        !app.engine.state.file_tree_load.loading && !app.engine.state.file_tree_load.stale
    });
    let idx = app
        .engine
        .state
        .file_tree
        .entries
        .iter()
        .position(|e| e.name == "cells.db")
        .expect("cells.db in tree");
    app.engine.state.file_tree.selected = idx;
    app.load_preview();
    wait_for(&mut app, "preview", |app| {
        !app.engine.state.preview_load.loading
            && app.engine.state.preview_schedule.is_none()
            && app.engine.state.preview_content.is_some()
    });

    let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
    render(&mut app, &mut terminal);

    // Find a registered cell zone the way a user's pointer would.
    let mut target = None;
    'scan: for y in 0..24u16 {
        for x in 0..110u16 {
            if let Some(ClickAction::DbSelectCell { row, column }) = app.hit_registry.hit_test(x, y)
                && row == 0
                && column == 1
            {
                target = Some((x, y));
                break 'scan;
            }
        }
    }
    let (x, y) = target.expect("the data grid registered a click zone for row 0 / column 1");

    reef::input::handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        &mut app,
        &terminal,
    );
    assert!(
        app.engine
            .db_preview()
            .is_some_and(|state| state.cell.is_some()),
        "the click did not open a cell"
    );

    // The value arrives asynchronously; the cell must survive every
    // step in between, not just the first frame.
    wait_for(&mut app, "cell value", |app| {
        assert!(
            app.engine
                .db_preview()
                .is_some_and(|state| state.cell.is_some()),
            "the opened cell was cleared while its value was in flight"
        );
        app.engine
            .db_preview()
            .and_then(|state| state.cell.as_ref())
            .is_some_and(|cell| cell.value.is_some())
    });

    let output = render(&mut app, &mut terminal);
    assert!(
        output.contains("TEXT ·"),
        "value pane missing after the click:\n{output}"
    );

    // Idle steps + redraws must not close it either.
    for _ in 0..20 {
        app.tick();
        thread::sleep(Duration::from_millis(10));
    }
    let output = render(&mut app, &mut terminal);
    assert!(
        app.engine
            .db_preview()
            .is_some_and(|state| state.cell.is_some()),
        "the opened cell was cleared while idling:\n{output}"
    );

    // A refresh of the same `.db` — what the fs watcher triggers every
    // time anything writes to the database — must not close the pane.
    let revision_before = app.engine.state.preview_source_revision;
    {
        let conn =
            rusqlite::Connection::open(std::env::current_dir().unwrap().join("cells.db")).unwrap();
        conn.execute("INSERT INTO step_run(id, prompt) VALUES (3, 'new row')", [])
            .unwrap();
    }
    app.engine.state.preview_load.mark_stale();
    app.load_preview();
    wait_for(&mut app, "preview refresh", |app| {
        !app.engine.state.preview_load.loading && app.engine.state.preview_schedule.is_none()
    });
    for _ in 0..20 {
        app.tick();
        thread::sleep(Duration::from_millis(10));
    }
    let output = render(&mut app, &mut terminal);
    assert_ne!(
        app.engine.state.preview_source_revision, revision_before,
        "the refresh did not actually reload the database preview"
    );
    assert!(
        app.engine
            .db_preview()
            .and_then(|state| state.cell.as_ref())
            .is_some_and(|cell| cell.value.is_some()),
        "the opened cell did not survive a preview refresh:\n{output}"
    );
}

#[test]
fn scrolling_the_grid_is_not_dragged_back_to_the_opened_cell() {
    // Revealing the cell cursor belongs to a cursor move. Re-running it
    // every frame fights the wheel: the user scrolls, the next render
    // yanks the grid back, and the grid reads as janky.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    seed_many_rows(tmp.path());
    let home = tempfile::TempDir::new().unwrap();
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.refresh_file_tree();
    wait_for(&mut app, "file tree", |app| {
        !app.engine.state.file_tree_load.loading && !app.engine.state.file_tree_load.stale
    });
    let idx = app
        .engine
        .state
        .file_tree
        .entries
        .iter()
        .position(|e| e.name == "cells.db")
        .expect("cells.db in tree");
    app.engine.state.file_tree.selected = idx;
    app.load_preview();
    wait_for(&mut app, "preview", |app| {
        !app.engine.state.preview_load.loading
            && app.engine.state.preview_schedule.is_none()
            && app.engine.state.preview_content.is_some()
    });

    let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
    render(&mut app, &mut terminal);

    // Open the top row's TEXT cell, then scroll far past it.
    app.engine
        .dispatch(reef_app::AppCommand::DbLoadCell { row: 0, column: 1 });
    wait_for(&mut app, "cell value", |app| {
        app.engine
            .db_preview()
            .and_then(|state| state.cell.as_ref())
            .is_some_and(|cell| cell.value.is_some())
    });
    render(&mut app, &mut terminal);

    app.engine.dispatch(reef_app::AppCommand::PreviewScroll(20));
    let scrolled_to = app.engine.preview_scroll();
    assert!(scrolled_to > 0, "the grid did not scroll at all");

    // Redraws and idle steps must leave the user's scroll alone.
    for _ in 0..5 {
        render(&mut app, &mut terminal);
        app.tick();
    }
    assert_eq!(
        app.engine.preview_scroll(),
        scrolled_to,
        "the grid was dragged back to the opened cell"
    );

    // Moving the cursor, on the other hand, must bring it back into view.
    app.engine
        .dispatch(reef_app::AppCommand::DbMoveCell { d_row: 1, d_col: 0 });
    render(&mut app, &mut terminal);
    assert!(
        app.engine.preview_scroll() < scrolled_to,
        "a cursor move did not scroll the grid back to the cursor"
    );
}
