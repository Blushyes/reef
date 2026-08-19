//! A WAL-mode SQLite database must not make reef reload itself.
//!
//! SQLite rewrites a WAL database's `-shm` index on every open —
//! including the read-only opens a preview does. That sidecar lands in
//! the watched workdir, so without filtering it the preview marks
//! itself stale, re-reads the database, rewrites the sidecar, and never
//! settles: the grid resets its scroll a couple of times a second and
//! the whole file is re-read for nothing.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use reef::TuiApp as App;
use reef::ui;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use test_support::{CwdGuard, HomeGuard, force_en_lang, tempdir_repo};

static CWD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn previewing_a_wal_database_settles() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let (tmp, _raw) = tempdir_repo();
    let home = tempfile::TempDir::new().unwrap();
    let _h = HomeGuard::enter(home.path());
    let _g = CwdGuard::enter(tmp.path());

    let conn = rusqlite::Connection::open(tmp.path().join("qb.db")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE step_run (id INTEGER PRIMARY KEY, prompt TEXT);
        WITH RECURSIVE seq(n) AS (
            SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 60
        )
        INSERT INTO step_run(id, prompt)
            SELECT n, '把本段的动作拆成六个镜头,每个镜头单独描述。' FROM seq;
        "#,
    )
    .unwrap();
    drop(conn);

    let mut app = App::new(Theme::dark(), None);
    app.refresh_file_tree();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        app.tick();
        if !app.engine.state.file_tree_load.loading && !app.engine.state.file_tree_load.stale {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let idx = app
        .engine
        .state
        .file_tree
        .entries
        .iter()
        .position(|e| e.name == "qb.db")
        .expect("qb.db in tree");
    app.engine.state.file_tree.selected = idx;
    app.load_preview();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        app.tick();
        if app.engine.state.preview_content.is_some() && !app.engine.state.preview_load.loading {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.engine.state.preview_content.is_some(),
        "preview never loaded"
    );

    // Open a cell, scroll away from it, then sit idle and draw. The
    // preview must not reload, and neither the scroll nor the cell may
    // move under the user.
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    app.engine
        .dispatch(reef_app::AppCommand::DbLoadCell { row: 0, column: 1 });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        app.tick();
        if app
            .engine
            .db_preview()
            .and_then(|state| state.cell.as_ref())
            .is_some_and(|cell| cell.value.is_some())
        {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    app.engine.dispatch(reef_app::AppCommand::PreviewScroll(10));
    let scrolled_to = app.engine.preview_scroll();
    assert!(scrolled_to > 0, "the grid did not scroll");

    let revision = app.engine.state.preview_source_revision;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        app.tick();
        terminal.draw(|f| ui::render(f, &mut app)).unwrap();
        thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(
        app.engine.state.preview_source_revision, revision,
        "the preview reloaded itself while idle — a sidecar write is being \
         treated as a workspace change"
    );
    assert_eq!(
        app.engine.preview_scroll(),
        scrolled_to,
        "the grid scroll was reset under the user"
    );
    assert!(
        app.engine
            .db_preview()
            .and_then(|state| state.cell.as_ref())
            .is_some_and(|cell| cell.value.is_some()),
        "the opened cell did not survive idling"
    );
}
