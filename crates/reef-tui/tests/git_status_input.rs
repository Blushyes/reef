use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reef::TuiApp as App;
use reef::input;
use reef::ui::theme::Theme;
use reef_app::{AppCommand, AppPanel as Panel, AppTab as Tab, SelectedFile};
use reef_core::git::{FileEntry, FileStatus};
use std::sync::Mutex;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn git_entry(path: &str) -> FileEntry {
    FileEntry {
        path: path.to_owned(),
        status: FileStatus::Modified,
        additions: 0,
        deletions: 0,
    }
}

#[test]
fn selecting_git_file_blurs_commit_editor_and_restores_arrow_navigation() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (temp, _repo) = test_support::tempdir_repo();
    let _cwd = CwdGuard::enter(temp.path());
    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.engine.state.unstaged_files = vec![git_entry("a.rs"), git_entry("b.rs")];
    app.engine.state.git_status.tree_mode = false;
    app.engine.dispatch(AppCommand::ToggleStatusTreeMode);
    app.engine.state.selected_file = Some(SelectedFile {
        path: "a.rs".to_owned(),
        is_staged: false,
    });
    app.engine.state.git_status.commit_editing = true;

    app.select_file("a.rs", false);
    input::handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app);

    assert!(!app.engine.state.git_status.commit_editing);
    assert_eq!(
        app.engine.state.selected_file,
        Some(SelectedFile {
            path: "b.rs".to_owned(),
            is_staged: false,
        })
    );
}

#[test]
fn graph_changed_files_support_arrow_navigation_without_losing_focus() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (temp, _repo) = test_support::tempdir_repo();
    let _cwd = CwdGuard::enter(temp.path());
    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Graph);
    app.engine.state.git_graph.selected_commit = Some("deadbeef".to_owned());
    app.engine.state.commit_detail.detail = Some(reef_core::git::CommitDetail {
        info: reef_core::git::CommitInfo {
            oid: "deadbeef".to_owned(),
            short_oid: "deadbee".to_owned(),
            parents: Vec::new(),
            author_name: "Tester".to_owned(),
            author_email: "test@example.com".to_owned(),
            time: 0,
            subject: "test".to_owned(),
        },
        message: "test".to_owned(),
        committer_name: "Tester".to_owned(),
        committer_time: 0,
        files: vec![git_entry("src/a.rs"), git_entry("src/b.rs")],
    });
    app.engine.dispatch(AppCommand::ToggleCommitFilesTreeMode);
    app.set_active_panel(Panel::Commit);
    app.load_commit_file_diff("src/a.rs");

    assert_eq!(app.engine.state.active_panel, Panel::Commit);

    input::handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app);

    assert_eq!(
        app.engine.state.commit_detail.selected_file.as_deref(),
        Some("src/b.rs")
    );
    assert_eq!(app.engine.state.active_panel, Panel::Commit);
    assert!(app.engine.state.commit_file_diff_load.loading);
}
