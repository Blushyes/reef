//! Selection reveal and independent wheel scrolling through the real TUI.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use reef::{
    TuiApp, input,
    ui::{commit_detail_panel, nav_candidates_popup, theme::Theme},
};
use reef_app::{
    AppCommand, AppPanel, AppTab, CursorPosition, LocationSnapshot, LocationSurface,
    NavCandidateKind, NavCandidatesPopup, NavPeekMode, ScrollPosition,
};
use reef_core::{
    git::{CommitDetail, CommitInfo, FileEntry, FileStatus},
    nav::Location,
};
use test_support::{CwdGuard, HOME_LOCK, HomeGuard, tempdir_repo};

fn screen(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[test]
fn graph_keyboard_selection_reveals_files_in_flat_and_collapsed_tree_views() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let _home = HomeGuard::enter(tmp.path());
    let _cwd = CwdGuard::enter(tmp.path());
    for tree in [false, true] {
        let mut app = TuiApp::new(Theme::dark(), None);
        app.set_active_tab(AppTab::Graph);
        app.engine.state.git_graph.selected_commit = Some("deadbeef".into());
        app.engine.state.commit_detail.detail = Some(CommitDetail {
            info: CommitInfo {
                oid: "deadbeef".into(),
                short_oid: "deadbee".into(),
                parents: vec![],
                author_name: "Tester".into(),
                author_email: "test@example.com".into(),
                time: 0,
                subject: "test".into(),
            },
            message: "A multiline commit\n\nDetails occupy additional rows".into(),
            committer_name: "Tester".into(),
            committer_time: 0,
            files: (0..100)
                .map(|i| FileEntry {
                    path: format!("dir{:02}/file{i:03}.rs", i / 10),
                    status: FileStatus::Modified,
                    additions: 0,
                    deletions: 0,
                })
                .collect(),
        });
        app.engine.state.commit_detail.files_tree_mode = tree;
        app.engine
            .state
            .commit_detail
            .files_collapsed
            .insert("dir02".into());
        app.set_active_panel(AppPanel::Commit);
        app.load_commit_file_diff("dir00/file000.rs");
        let mut terminal = Terminal::new(TestBackend::new(80, 15)).unwrap();
        terminal
            .draw(|f| commit_detail_panel::render(f, &mut app, f.area(), true))
            .unwrap();
        for key in [KeyCode::Down; 40]
            .into_iter()
            .chain([KeyCode::PageDown, KeyCode::PageUp])
            .chain([KeyCode::Up; 40])
        {
            input::handle_key(KeyEvent::new(key, KeyModifiers::NONE), &mut app);
            terminal
                .draw(|f| commit_detail_panel::render(f, &mut app, f.area(), true))
                .unwrap();
            let selected = app
                .engine
                .state
                .commit_detail
                .selected_file
                .as_deref()
                .unwrap();
            let name = selected.rsplit('/').next().unwrap();
            assert!(
                screen(&terminal).contains(name),
                "tree={tree}: {selected} must remain visible after {key:?}"
            );
            assert_eq!(app.engine.active_panel(), AppPanel::Commit);
            if tree {
                assert!(!selected.starts_with("dir02/"));
            }
        }
        // Scrolling the metadata/list by wheel is independent of selection.
        commit_detail_panel::scroll(&mut app, 20);
        terminal
            .draw(|f| commit_detail_panel::render(f, &mut app, f.area(), true))
            .unwrap();
        assert!(!screen(&terminal).contains("file000.rs"));
    }
}

#[test]
fn candidate_wheel_position_survives_render_and_tick_in_both_modes() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (tmp, _repo) = tempdir_repo();
    let _home = HomeGuard::enter(tmp.path());
    let _cwd = CwdGuard::enter(tmp.path());
    for mode in [NavPeekMode::Compact, NavPeekMode::Expanded] {
        let mut app = TuiApp::new(Theme::dark(), None);
        app.engine.state.settings.nav_peek_mode = mode;
        app.engine.state.nav_candidates = Some(NavCandidatesPopup::new(
            (0..40)
                .map(|i| Location {
                    path: Some("file.rs".into()),
                    line: i,
                    byte_range: 0..5,
                    snippet: format!("id{i:03}"),
                    snippet_match_range: 0..5,
                })
                .collect(),
            "file.rs".into(),
            LocationSnapshot {
                surface: LocationSurface::FilePreview,
                path: "file.rs".into(),
                cursor: CursorPosition {
                    line: 0,
                    byte_col: 0,
                },
                scroll: ScrollPosition {
                    vertical: 0,
                    horizontal: 0,
                },
            },
            "id".into(),
            NavCandidateKind::References,
        ));
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|f| nav_candidates_popup::render(f, &mut app, f.area()))
            .unwrap();
        app.tick();
        app.nav_candidates_scroll(20);
        for _ in 0..2 {
            terminal
                .draw(|f| nav_candidates_popup::render(f, &mut app, f.area()))
                .unwrap();
            app.tick();
            let popup = app.engine.nav_candidates().unwrap();
            assert_eq!(popup.selected, 0);
            assert_eq!(popup.scroll, 20);
            assert!(
                screen(&terminal).contains("id020"),
                "{mode:?}: wheel must expose later candidates"
            );
            assert!(!screen(&terminal).contains("id000"));
        }
        app.nav_candidates_move(30);
        terminal
            .draw(|f| nav_candidates_popup::render(f, &mut app, f.area()))
            .unwrap();
        assert!(screen(&terminal).contains("id030"));
        // Viewport shrink still reconciles selection through the host tick.
        terminal.backend_mut().resize(120, 10);
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 120, 10))
            .unwrap();
        terminal
            .draw(|f| nav_candidates_popup::render(f, &mut app, f.area()))
            .unwrap();
        app.tick();
        terminal
            .draw(|f| nav_candidates_popup::render(f, &mut app, f.area()))
            .unwrap();
        assert!(screen(&terminal).contains("id030"));
        app.engine.dispatch(AppCommand::CloseNavCandidates);
    }
}
