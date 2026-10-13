use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Terminal, backend::TestBackend};
use reef::ui::{mouse::ClickAction, theme::Theme};
use reef::{TuiApp as App, input, ui};
use reef_app::{AppCommand, ContextMenuItem as Item};
use reef_core::file_tree::{
    FileTreeSort, FileTreeSortDirection as Direction, FileTreeSortField as Field,
};
use reef_core::menu::menu_items;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};
use test_support::{CwdGuard, HomeGuard, tempdir_repo, write_file};

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn with_app(test: impl FnOnce(&mut App)) {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let home = tempfile::TempDir::new().unwrap();
    let _home = HomeGuard::enter(home.path());
    let (workspace, repo) = tempdir_repo();
    for name in ["a-dir/child.rs", "z-dir/child.rs", "a.txt", "z.txt"] {
        write_file(&repo, name, "sample");
    }
    for (path, seconds) in [("a-dir", 10), ("a.txt", 20), ("z-dir", 30), ("z.txt", 40)] {
        std::fs::File::open(workspace.path().join(path))
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
            )
            .unwrap();
    }
    let _cwd = CwdGuard::enter(workspace.path());
    let mut app = App::new(Theme::dark(), None);
    settle_tree(&mut app);
    test(&mut app);
}

fn settle_tree(app: &mut App) {
    let timeout = Instant::now() + Duration::from_secs(5);
    let wake = app.engine.worker_wake_receiver();
    loop {
        app.tick();
        if !app.engine.state.file_tree_load.loading && !app.engine.state.file_tree_load.stale {
            return;
        }
        assert!(
            Instant::now() < timeout,
            "file tree did not finish rebuilding"
        );
        let deadline = app
            .engine
            .next_deadline()
            .map_or(timeout, |deadline| deadline.min(timeout));
        let _ = wake.recv_deadline(deadline);
    }
}

fn key(app: &mut App, code: KeyCode) {
    input::handle_key(KeyEvent::new(code, KeyModifiers::NONE), app);
}

fn open_sort_with_keyboard(app: &mut App) {
    app.open_tree_context_menu(None, (4, 4));
    let index = menu_items(app.engine.tree_context_menu_entries())
        .position(|item| *item == Item::SortBy)
        .unwrap();
    for _ in 0..index {
        key(app, KeyCode::Down);
    }
    key(app, KeyCode::Right);
    assert_eq!(
        app.engine.tree_context_menu_current(),
        Some(Item::SortField(Field::Name))
    );
}

fn choose(app: &mut App, item: Item) {
    open_sort_with_keyboard(app);
    let index = menu_items(Item::SORT_ITEMS)
        .position(|candidate| *candidate == item)
        .unwrap();
    for _ in 0..index {
        key(app, KeyCode::Down);
    }
    key(app, KeyCode::Enter);
    assert!(!app.engine.tree_context_menu_active());
    settle_tree(app);
}

fn paths(app: &App) -> Vec<String> {
    app.engine
        .file_tree_entries()
        .iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn keyboard_sort_reorders_directories_and_files_and_persists_settings() {
    with_app(|app| {
        app.engine
            .dispatch(AppCommand::SelectVisibleFileTreePath("a.txt".into()));
        choose(app, Item::SortField(Field::Modified));
        choose(app, Item::SortDirection(Direction::Descending));
        assert_eq!(paths(app), ["z-dir", "a-dir", "z.txt", "a.txt"]);
        choose(app, Item::FoldersFirst);
        assert_eq!(paths(app), ["z.txt", "z-dir", "a.txt", "a-dir"]);
        assert_eq!(
            app.engine.state.file_tree.selected_path().as_deref(),
            Some(Path::new("a.txt"))
        );
        let expected = FileTreeSort {
            field: Field::Modified,
            direction: Direction::Descending,
            folders_first: false,
        };
        assert_eq!(app.engine.file_tree_sort(), expected);
        assert_eq!(reef_io::prefs::file_tree_sort(), expected);
        let restored = App::new(Theme::dark(), None);
        assert_eq!(restored.engine.file_tree_sort(), expected);
    });
}

#[test]
fn keyboard_returns_to_parent_then_closes_menu() {
    with_app(|app| {
        open_sort_with_keyboard(app);
        key(app, KeyCode::Up);
        assert_eq!(
            app.engine.tree_context_menu_current(),
            Some(Item::FoldersFirst)
        );
        key(app, KeyCode::Left);
        assert!(app.engine.tree_context_menu_active());
        assert_eq!(app.engine.tree_context_menu_current(), Some(Item::SortBy));
        key(app, KeyCode::Enter);
        assert_eq!(app.engine.tree_context_menu_sort_selected(), Some(0));
        key(app, KeyCode::Down);
        key(app, KeyCode::Down);
        assert_eq!(
            app.engine.tree_context_menu_current(),
            Some(Item::SortField(Field::Modified))
        );
        key(app, KeyCode::Down);
        assert_eq!(
            app.engine.tree_context_menu_current(),
            Some(Item::SortDirection(Direction::Ascending))
        );
        key(app, KeyCode::Down);
        key(app, KeyCode::Down);
        assert_eq!(
            app.engine.tree_context_menu_current(),
            Some(Item::FoldersFirst)
        );
        key(app, KeyCode::Esc);
        assert_eq!(app.engine.tree_context_menu_current(), Some(Item::SortBy));
        key(app, KeyCode::Esc);
        assert!(!app.engine.tree_context_menu_active());
    });
}

fn render(app: &mut App, terminal: &mut Terminal<TestBackend>) {
    terminal.draw(|frame| ui::render(frame, app)).unwrap();
}

fn menu_position(app: &App, item: Item, width: u16, height: u16) -> (u16, u16) {
    for y in 0..height {
        for x in 0..width {
            if matches!(app.hit_registry.hit_test(x, y), Some(ClickAction::TreeContextMenuItem(candidate)) if candidate == item)
            {
                return (x, y);
            }
        }
    }
    panic!("menu item {item:?} has no hit region");
}

fn mouse(
    app: &mut App,
    terminal: &Terminal<TestBackend>,
    kind: MouseEventKind,
    position: (u16, u16),
) {
    input::handle_mouse(
        MouseEvent {
            kind,
            column: position.0,
            row: position.1,
            modifiers: KeyModifiers::NONE,
        },
        app,
        terminal,
    );
}

#[test]
fn mouse_submenu_flips_left_and_keeps_parent_actions_clickable() {
    with_app(|app| {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.open_tree_context_menu(None, (78, 22));
        render(app, &mut terminal);
        let sort = menu_position(app, Item::SortBy, 80, 24);
        let new_folder = menu_position(app, Item::NewFolder, 80, 24);
        let reveal = menu_position(app, Item::RevealInFinder, 80, 24);
        assert_eq!(sort.1, new_folder.1 + 2);
        assert_eq!(reveal.1, sort.1 + 2);
        mouse(
            app,
            &terminal,
            MouseEventKind::Down(MouseButton::Left),
            (sort.0, sort.1 - 1),
        );
        assert!(app.engine.tree_context_menu_active());
        assert!(!app.engine.tree_edit_active());
        mouse(app, &terminal, MouseEventKind::Moved, sort);
        assert!(app.engine.tree_context_menu_sort_selected().is_some());
        render(app, &mut terminal);
        let kind = menu_position(app, Item::SortField(Field::Kind), 80, 24);
        assert!(kind.0 < sort.0, "submenu should open left of its parent");
        let modified = menu_position(app, Item::SortField(Field::Modified), 80, 24);
        let ascending = menu_position(app, Item::SortDirection(Direction::Ascending), 80, 24);
        let descending = menu_position(app, Item::SortDirection(Direction::Descending), 80, 24);
        let folders_first = menu_position(app, Item::FoldersFirst, 80, 24);
        assert_eq!(ascending.1, modified.1 + 2);
        assert_eq!(folders_first.1, descending.1 + 2);
        for position in [
            (ascending.0, ascending.1 - 1),
            (folders_first.0, folders_first.1 - 1),
        ] {
            assert!(matches!(
                app.hit_registry.hit_test(position.0, position.1),
                Some(ClickAction::ContextMenuSeparator)
            ));
            mouse(app, &terminal, MouseEventKind::Moved, position);
            mouse(
                app,
                &terminal,
                MouseEventKind::Down(MouseButton::Left),
                position,
            );
            assert_eq!(app.engine.tree_context_menu_sort_selected(), Some(0));
            assert_eq!(app.engine.file_tree_sort(), FileTreeSort::default());
        }
        menu_position(app, Item::NewFile, 80, 24);
        mouse(
            app,
            &terminal,
            MouseEventKind::Down(MouseButton::Left),
            kind,
        );
        assert_eq!(app.engine.file_tree_sort().field, Field::Kind);
        assert!(!app.engine.tree_context_menu_active());
        settle_tree(app);

        app.open_tree_context_menu(None, (4, 4));
        render(app, &mut terminal);
        let sort = menu_position(app, Item::SortBy, 80, 24);
        mouse(
            app,
            &terminal,
            MouseEventKind::Down(MouseButton::Left),
            sort,
        );
        render(app, &mut terminal);
        let new_file = menu_position(app, Item::NewFile, 80, 24);
        mouse(app, &terminal, MouseEventKind::Moved, new_file);
        assert_eq!(app.engine.tree_context_menu_sort_selected(), None);
        assert_eq!(app.engine.tree_context_menu_current(), Some(Item::NewFile));
        mouse(
            app,
            &terminal,
            MouseEventKind::Down(MouseButton::Left),
            (79, 0),
        );
        assert!(!app.engine.tree_context_menu_active());
        assert!(!app.engine.tree_edit_active());
    });
}
