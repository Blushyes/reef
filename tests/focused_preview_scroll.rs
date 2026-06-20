//! Scroll routing in `ViewMode::FocusedPreview`.
//!
//! 纯预览态把 active_panel 强制设到内容列, 所有 ↑↓/PgUp/PgDn/←/→ 都应落
//! 在对应的滚动字段上。这组测试逐 tab 验证, 防止以后改 `enter_focused_preview`
//! 或 per-tab 键位路由时把这个路径打断。

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use reef::app::{App, Tab, ViewMode};
use reef::input;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn key_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn files_tab_focused_preview_scrolls_preview_vertically() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x\n".repeat(200)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    assert_eq!(app.active_tab, Tab::Files);
    app.enter_focused_preview();
    assert_eq!(app.view_mode, ViewMode::FocusedPreview);

    let before = app.preview_scroll;
    input::handle_key(key(KeyCode::Down), &mut app);
    assert_eq!(
        app.preview_scroll,
        before + 1,
        "↓ in Files focused preview should pan preview_scroll"
    );
    input::handle_key(key(KeyCode::Down), &mut app);
    input::handle_key(key(KeyCode::Down), &mut app);
    assert_eq!(app.preview_scroll, before + 3);
    input::handle_key(key(KeyCode::Up), &mut app);
    assert_eq!(app.preview_scroll, before + 2);

    // PageDown step is +20 in handle_key_files.
    input::handle_key(key(KeyCode::PageDown), &mut app);
    assert_eq!(app.preview_scroll, before + 22);
}

#[test]
fn files_tab_focused_preview_scrolls_preview_horizontally() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "wide".repeat(200)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();

    let before = app.preview_h_scroll;
    input::handle_key(key(KeyCode::Right), &mut app);
    assert_eq!(
        app.preview_h_scroll,
        before + 1,
        "→ should pan preview_h_scroll horizontally"
    );
    input::handle_key(key_with(KeyCode::Right, KeyModifiers::SHIFT), &mut app);
    // Shift+→ steps by 10 in handle_key_files.
    assert_eq!(app.preview_h_scroll, before + 11);
    input::handle_key(key(KeyCode::Left), &mut app);
    assert_eq!(app.preview_h_scroll, before + 10);
}

#[test]
fn git_tab_focused_preview_scrolls_diff_vertically_and_horizontally() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.enter_focused_preview();
    assert_eq!(app.view_mode, ViewMode::FocusedPreview);

    let vert_before = app.diff_scroll;
    input::handle_key(key(KeyCode::Down), &mut app);
    assert_eq!(app.diff_scroll, vert_before + 1);
    input::handle_key(key(KeyCode::PageDown), &mut app);
    // PageDown step in handle_key_git is +20.
    assert_eq!(app.diff_scroll, vert_before + 21);
    input::handle_key(key(KeyCode::Up), &mut app);
    assert_eq!(app.diff_scroll, vert_before + 20);

    let h_before = app.diff_h_scroll;
    input::handle_key(key(KeyCode::Right), &mut app);
    assert_eq!(app.diff_h_scroll, h_before + 1);
    input::handle_key(key_with(KeyCode::Right, KeyModifiers::SHIFT), &mut app);
    assert_eq!(app.diff_h_scroll, h_before + 11);
    input::handle_key(key(KeyCode::Home), &mut app);
    assert_eq!(app.diff_h_scroll, 0);
}

fn wheel(col: u16, row: u16, kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn git_focused_preview_wheel_in_left_columns_scrolls_diff_not_hidden_sidebar() {
    // Regression: dispatch_vertical_scroll routes wheel events by
    // `column < graph_sidebar_width`. In 纯预览 the sidebar isn't
    // rendered, so a wheel in cols 0..30 (which would normally hit the
    // git status panel) must still scroll the diff that fills the
    // whole frame instead. The bug looked like "scrolling doesn't work"
    // because the wheel was silently moving an invisible status row
    // cursor.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.enter_focused_preview();

    let backend = TestBackend::new(100, 24);
    let terminal = Terminal::new(backend).unwrap();

    let before = app.diff_scroll;
    // Column 5 sits well inside what would have been the sidebar.
    input::handle_mouse(
        wheel(5, 10, MouseEventKind::ScrollDown),
        &mut app,
        &terminal,
    );
    assert!(
        app.diff_scroll > before,
        "wheel in former-sidebar columns must scroll diff in 纯预览"
    );
    let after_down = app.diff_scroll;
    input::handle_mouse(wheel(5, 10, MouseEventKind::ScrollUp), &mut app, &terminal);
    assert!(app.diff_scroll < after_down);
}

#[test]
fn files_focused_preview_wheel_scrolls_preview_at_any_column() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x\n".repeat(200)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();

    let backend = TestBackend::new(100, 24);
    let terminal = Terminal::new(backend).unwrap();

    // Same column the (now-hidden) file tree would have lived in.
    let before = app.preview_scroll;
    input::handle_mouse(wheel(3, 8, MouseEventKind::ScrollDown), &mut app, &terminal);
    assert!(app.preview_scroll > before);
}

#[test]
fn focused_preview_horizontal_wheel_routes_to_preview_or_diff() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("wide.txt"), "x".repeat(5000)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();

    let backend = TestBackend::new(100, 24);
    let terminal = Terminal::new(backend).unwrap();

    let before = app.preview_h_scroll;
    input::handle_mouse(
        wheel(2, 5, MouseEventKind::ScrollRight),
        &mut app,
        &terminal,
    );
    assert!(
        app.preview_h_scroll > before,
        "horizontal wheel must move preview_h_scroll even in former-sidebar columns"
    );

    // Switch to Git tab + focused preview and confirm horizontal routes
    // to diff_h_scroll instead.
    app.close_focused_preview();
    app.set_active_tab(Tab::Git);
    app.enter_focused_preview();
    let dh_before = app.diff_h_scroll;
    input::handle_mouse(
        wheel(4, 6, MouseEventKind::ScrollRight),
        &mut app,
        &terminal,
    );
    assert!(app.diff_h_scroll > dh_before);
}

/// The wash must reach the path's *last* cell — diff_panel pads its
/// inner area by 1 column on the left, so a naïve `width = path_w`
/// stops one column short of where the filename actually ends.
/// Regression for: "highlight covers `/ui/mod.r` but not the trailing
/// `s`".
#[test]
fn hover_wash_reaches_last_char_of_filename() {
    use reef::ui;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    // 10-char filename so we can pin the "last cell" cleanly.
    let name = "abcdefghij";
    std::fs::write(tmp.path().join(name), "hi\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.unstaged_files.push(reef_core::git::FileEntry {
        path: name.to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    app.diff_content = Some(reef::app::HighlightedDiff::new(
        reef_core::diff::DiffContent {
            path: name.to_string(),
            hunks: Vec::new(),
        },
        None,
    ));
    app.enter_focused_preview();
    // Drive picker-open so the wash uses the accent (easier to spot than
    // hover_bg) and we're sure the path is up.
    app.focused_preview_files_open = true;

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    // diff_panel pads left by 1, so path is at cols 1..=10. The wash
    // must cover col 10 (the trailing 'j') and stop at col 11 (the
    // first space of the tag tail).
    let accent = Theme::dark().accent;
    let last_path_bg = terminal.backend().buffer()[(10, 0)].bg;
    let first_tag_bg = terminal.backend().buffer()[(11, 0)].bg;
    assert_eq!(
        last_path_bg, accent,
        "wash must reach the path's last cell (col 10 = trailing 'j')"
    );
    assert_ne!(
        first_tag_bg, accent,
        "wash must stop before the tag tail (col 11)"
    );
}

/// Hover on the chip+file-path span should wash that span only. The
/// right-hand tag (`[unified][compact] m/f toggle`) stays unwashed —
/// it's chrome, not part of the picker affordance.
#[test]
fn hovering_file_path_washes_chip_and_path_only() {
    use ratatui::style::Color;
    use reef::ui;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let long_name = "long_filename_for_hover_test.txt"; // 32 cols
    std::fs::write(tmp.path().join(long_name), "hi\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.unstaged_files.push(reef_core::git::FileEntry {
        path: long_name.to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    // Seed diff_content so `focused_preview_interactive_width` can read
    // the path (matches what diff_panel draws in the header).
    app.diff_content = Some(reef::app::HighlightedDiff::new(
        reef_core::diff::DiffContent {
            path: long_name.to_string(),
            hunks: Vec::new(),
        },
        None,
    ));
    app.enter_focused_preview();

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // Baseline: no hover position.
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    let baseline_in_path = terminal.backend().buffer()[(15, 0)].bg;
    let baseline_in_tag = terminal.backend().buffer()[(90, 0)].bg;

    // Hover sitting on col=15 — well inside the long filename text.
    app.hover_row = Some(0);
    app.hover_col = Some(15);
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    let path_bg = terminal.backend().buffer()[(15, 0)].bg;
    let tag_bg = terminal.backend().buffer()[(90, 0)].bg;
    let hover_bg = Theme::dark().hover_bg;

    assert_eq!(
        path_bg, hover_bg,
        "hover should wash cells inside the path (col 15)"
    );
    assert_ne!(
        baseline_in_path, hover_bg,
        "baseline at col 15 must differ from hovered state"
    );
    assert_eq!(
        tag_bg, baseline_in_tag,
        "cells inside the tag tail (col 90) must NOT pick up the hover wash"
    );

    // Chip cells share the same span so they should also wash.
    let chip_bg = terminal.backend().buffer()[(1, 0)].bg;
    assert_ne!(chip_bg, Color::Reset);
}

/// Clicking inside the file-path span toggles the picker.
#[test]
fn clicking_inside_file_path_toggles_picker() {
    use reef::ui;
    use reef::ui::mouse::ClickAction;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let long_name = "long_filename_for_click_test.txt";
    std::fs::write(tmp.path().join(long_name), "hi\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.unstaged_files.push(reef_core::git::FileEntry {
        path: long_name.to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    app.diff_content = Some(reef::app::HighlightedDiff::new(
        reef_core::diff::DiffContent {
            path: long_name.to_string(),
            hunks: Vec::new(),
        },
        None,
    ));
    app.enter_focused_preview();

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    // Col 15 sits inside the filename text.
    let action = app.hit_registry.hit_test(15, 0);
    assert!(
        matches!(action, Some(ClickAction::ToggleFocusedPreviewFiles)),
        "file-path cell should toggle picker; got {action:?}"
    );

    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 15,
        row: 0,
        modifiers: KeyModifiers::NONE,
    };
    input::handle_mouse(click, &mut app, &terminal);
    assert!(app.focused_preview_files_open);
}

/// Clicking on the tag tail (`[unified][compact] m/f toggle`) does
/// NOT toggle the picker — the highlight + click zone are deliberately
/// scoped to chip + path. Regression test for the "framed area only"
/// scoping requested after the initial full-row implementation.
#[test]
fn clicking_tag_tail_does_not_toggle_picker() {
    use reef::ui;
    use reef::ui::mouse::ClickAction;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    std::fs::write(tmp.path().join("a.txt"), "hi\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.unstaged_files.push(reef_core::git::FileEntry {
        path: "a.txt".to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    app.diff_content = Some(reef::app::HighlightedDiff::new(
        reef_core::diff::DiffContent {
            path: "a.txt".to_string(),
            hunks: Vec::new(),
        },
        None,
    ));
    app.enter_focused_preview();

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    // Col 80 is past "a.txt" (which is 5 chars), well inside the tag tail.
    // hit_test must NOT return ToggleFocusedPreviewFiles there.
    let action = app.hit_registry.hit_test(80, 0);
    assert!(
        !matches!(action, Some(ClickAction::ToggleFocusedPreviewFiles)),
        "tag-tail cells must not be claimed by the picker chip; got {action:?}"
    );
}

/// Click on the ☰ chip in the upper-left of a Git-tab focused preview
/// must reach `ToggleFocusedPreviewFiles` — not get hijacked by the
/// diff-panel drag-select fast path. Regression for the original
/// implementation where `handle_diff_selection` saw the chip's column
/// inside `last_diff_rect` and ate the click.
#[test]
fn chip_click_in_git_focused_preview_toggles_picker_not_diff_drag() {
    use reef::ui;
    use reef::ui::mouse::ClickAction;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Chip is only painted when `backend.has_repo()` is true, so we
    // need a real git repo here — a bare tempdir won't trigger it.
    let (tmp, _repo) = test_support::tempdir_repo();
    std::fs::write(tmp.path().join("a.txt"), "hi\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    // Fake an unstaged entry so the picker can open.
    app.unstaged_files.push(reef_core::git::FileEntry {
        path: "a.txt".to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    app.enter_focused_preview();

    // Render once so the chip's hit zone (and last_diff_rect) lands in
    // the registry.
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    // Chip sits at body.y row 0, columns 0-2 (" ☰ "). Click the glyph
    // cell — column 1 is the cell most likely to be inside last_diff_rect
    // (diff_panel pads by 1 column, so col 0 is outside but col 1 is in).
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 1,
        row: 0,
        modifiers: KeyModifiers::NONE,
    };
    // Sanity: the registered hit zone really is the chip action, so a
    // failure of `handle_mouse` to fire it points at routing not rendering.
    let action = app.hit_registry.hit_test(1, 0);
    assert!(
        matches!(action, Some(ClickAction::ToggleFocusedPreviewFiles)),
        "chip hit zone missing at (1,0); got {action:?}"
    );

    assert!(!app.focused_preview_files_open);
    let entries = app.focused_preview_file_entries();
    assert!(
        !entries.is_empty(),
        "test fixture must provide at least one changed file or open_focused_preview_files no-ops"
    );

    input::handle_mouse(click, &mut app, &terminal);
    assert!(
        app.focused_preview_files_open,
        "click on chip must open the picker — not start a diff drag"
    );
}

/// Graph 3-col happy path: entering focused preview on Graph,
/// opening the picker, and picking a row must load that file's diff
/// via `load_commit_file_diff` — i.e. the `GraphCommit` source maps
/// to the right backend call.
#[test]
fn graph_focused_preview_picks_commit_file_via_picker() {
    use reef::ui;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Graph);
    // Stub commit_detail.detail with two changed files so
    // `focused_preview_file_entries` returns a non-empty list.
    app.commit_detail.detail = Some(reef_core::git::CommitDetail {
        info: reef_core::git::CommitInfo {
            oid: "deadbeef".to_string(),
            short_oid: "dead".to_string(),
            parents: Vec::new(),
            author_name: "Tester".to_string(),
            author_email: "t@example.com".to_string(),
            time: 0,
            subject: "test".to_string(),
        },
        message: "test".to_string(),
        committer_name: "Tester".to_string(),
        committer_time: 0,
        files: vec![
            reef_core::git::FileEntry {
                path: "src/app.rs".to_string(),
                status: reef_core::git::FileStatus::Modified,
                additions: 0,
                deletions: 0,
            },
            reef_core::git::FileEntry {
                path: "src/input.rs".to_string(),
                status: reef_core::git::FileStatus::Modified,
                additions: 0,
                deletions: 0,
            },
        ],
    });
    app.enter_focused_preview();

    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    let entries = app.focused_preview_file_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].path, "src/app.rs");
    assert_eq!(
        entries[0].source,
        reef::app::FocusedPreviewFileSource::GraphCommit
    );

    // Open picker + pick row 1 (src/input.rs).
    app.open_focused_preview_files();
    app.pick_focused_preview_file(1);
    // Without a real backend / git repo content, the load is fire-and-
    // forget; what we *can* assert is that the picker closed and the
    // selection cursor moved to the picked index.
    assert!(!app.focused_preview_files_open);
    assert_eq!(app.focused_preview_files_selected, 1);
}

/// Enter inside the picker confirms — for Git tab, this means
/// `select_file` runs and `selected_file.path` matches the row.
#[test]
fn picker_enter_confirm_switches_diff_target() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.unstaged_files = vec![
        reef_core::git::FileEntry {
            path: "a.txt".to_string(),
            status: reef_core::git::FileStatus::Modified,
            additions: 0,
            deletions: 0,
        },
        reef_core::git::FileEntry {
            path: "b.txt".to_string(),
            status: reef_core::git::FileStatus::Modified,
            additions: 0,
            deletions: 0,
        },
    ];
    app.enter_focused_preview();
    app.open_focused_preview_files();

    // Picker open, cursor at index 0 (a.txt — sort is lexical). Press
    // ↓ then Enter — should select b.txt and close the picker.
    input::handle_key(key(KeyCode::Down), &mut app);
    assert_eq!(app.focused_preview_files_selected, 1);
    input::handle_key(key(KeyCode::Enter), &mut app);

    assert!(!app.focused_preview_files_open);
    let sel = app
        .selected_file
        .as_ref()
        .expect("Enter should set selected_file");
    assert_eq!(sel.path, "b.txt");
    assert!(!sel.is_staged, "row came from unstaged_files");
}

/// Selection wraps around at the boundary — pressing ↑ on row 0
/// lands on the last row (and ↓ from last lands on 0). Vim-friendly
/// + matches what `rem_euclid` was supposed to do.
#[test]
fn picker_selection_wraps_at_boundaries() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.unstaged_files = (0..3)
        .map(|i| reef_core::git::FileEntry {
            path: format!("f{i}.txt"),
            status: reef_core::git::FileStatus::Modified,
            additions: 0,
            deletions: 0,
        })
        .collect();
    app.enter_focused_preview();
    app.open_focused_preview_files();
    assert_eq!(app.focused_preview_files_selected, 0);

    // Up from 0 → wraps to last (2).
    input::handle_key(key(KeyCode::Up), &mut app);
    assert_eq!(app.focused_preview_files_selected, 2);

    // Down from last → wraps to 0.
    input::handle_key(key(KeyCode::Down), &mut app);
    assert_eq!(app.focused_preview_files_selected, 0);
}

/// Graph 2-col layout deliberately doesn't show the chip: the
/// commit_detail_panel renders a different header (commit metadata
/// plus inline file tree) where the chip's path-width math doesn't
/// apply. Regression guard so this stays intentional.
#[test]
fn graph_two_col_does_not_register_chip_hit_zone() {
    use reef::ui;
    use reef::ui::mouse::ClickAction;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Graph);
    app.enter_focused_preview();

    // GRAPH_THREE_COL_MIN_WIDTH is 120 in the source; render at 80 so
    // graph_uses_three_col() returns false → 2-col layout, no chip.
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    let action = app.hit_registry.hit_test(1, 0);
    assert!(
        !matches!(action, Some(ClickAction::ToggleFocusedPreviewFiles)),
        "Graph 2-col must not register chip hit zone; got {action:?}"
    );
}

/// Regression for Fix #1: bare 'd' in Git tab FocusedPreview must NOT
/// trigger discard-changes against the (invisible) selected file.
/// Previously the gate `_ => false` let destructive keys fall through
/// to handle_key_git's unguarded chord arms.
#[test]
fn focused_preview_swallows_destructive_keys_on_git_tab() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.unstaged_files.push(reef_core::git::FileEntry {
        path: "a.txt".to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    // Pretend the user has a.txt selected — the discard chord would
    // target it if we let `d` fall through.
    app.selected_file = Some(reef::app::SelectedFile {
        path: "a.txt".to_string(),
        is_staged: false,
    });
    app.enter_focused_preview();

    // No confirm modal should be up to begin with.
    assert!(app.confirm_modal.is_none());

    // Bare 'd' on Git tab outside FocusedPreview opens the discard
    // confirmation. Inside FocusedPreview it must be swallowed.
    input::handle_key(key(KeyCode::Char('d')), &mut app);
    assert!(
        app.confirm_modal.is_none(),
        "bare 'd' in FocusedPreview must not open the discard prompt"
    );
    assert!(
        app.git_status.confirm_discard.is_none(),
        "bare 'd' must not arm the discard-changes chord either"
    );

    // 's' (stage) and 'u' (unstage) likewise.
    let unstaged_before = app.unstaged_files.len();
    let staged_before = app.staged_files.len();
    input::handle_key(key(KeyCode::Char('s')), &mut app);
    input::handle_key(key(KeyCode::Char('u')), &mut app);
    assert_eq!(app.unstaged_files.len(), unstaged_before);
    assert_eq!(app.staged_files.len(), staged_before);
}

/// Regression for Fix #5: when the same path is in both staged and
/// unstaged, opening the picker while viewing the UNSTAGED diff must
/// snap to the unstaged row, not silently pick the staged duplicate.
#[test]
fn picker_snaps_to_unstaged_when_viewing_unstaged_diff_of_dup_path() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    // Same path in both lists (committed + further edited).
    app.staged_files.push(reef_core::git::FileEntry {
        path: "a.txt".to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    app.unstaged_files.push(reef_core::git::FileEntry {
        path: "a.txt".to_string(),
        status: reef_core::git::FileStatus::Modified,
        additions: 0,
        deletions: 0,
    });
    // Currently viewing the UNSTAGED diff of a.txt.
    app.selected_file = Some(reef::app::SelectedFile {
        path: "a.txt".to_string(),
        is_staged: false,
    });
    app.enter_focused_preview();
    app.open_focused_preview_files();

    let entries = app.focused_preview_file_entries();
    assert_eq!(entries.len(), 2);
    let selected = &entries[app.focused_preview_files_selected];
    assert_eq!(selected.path, "a.txt");
    assert_eq!(
        selected.source,
        reef::app::FocusedPreviewFileSource::GitUnstaged,
        "picker must snap to the unstaged duplicate when viewing unstaged diff"
    );
}

/// Regression for Fix #6: pressing Space then `v` while quick_open is
/// open with an empty query must NOT close the palette. (Before the
/// fix, leader_decision Fired on `v` and quick_open closed.)
#[test]
fn quick_open_keeps_v_keystroke_after_leader() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    reef::quick_open::begin(&mut app);
    assert!(app.quick_open.core.active);
    assert!(app.quick_open.core.filter.is_empty());

    // Space arms the leader (allowed because query is empty).
    input::handle_key(key(KeyCode::Char(' ')), &mut app);
    assert!(app.quick_open.space_leader_at.is_some());

    // 'v' is now a chord target — must NOT close the palette. The
    // leader is cleared and `v` falls through to the char-append arm.
    input::handle_key(key(KeyCode::Char('v')), &mut app);
    assert!(
        app.quick_open.core.active,
        "Space+V must not close quick_open — it's not the palette's own chord"
    );
    assert!(app.quick_open.space_leader_at.is_none());
    assert_eq!(
        app.quick_open.core.filter, "v",
        "the 'v' keystroke should land in the query buffer"
    );

    // Sanity: Space+P (the actual quick_open chord) DOES close.
    input::handle_key(key(KeyCode::Char(' ')), &mut app);
    input::handle_key(key(KeyCode::Char('p')), &mut app);
    // After Space+P the palette closes — but only when query is empty
    // again, which it isn't (we typed 'v'). With 'v' in query, the
    // leader isn't armed by Space, so Space appends to query.
    // Reset for a clean P-toggle check:
    app.quick_open.core.filter.clear();
    app.quick_open.core.cursor = 0;
    input::handle_key(key(KeyCode::Char(' ')), &mut app);
    input::handle_key(key(KeyCode::Char('p')), &mut app);
    assert!(
        !app.quick_open.core.active,
        "Space+P with empty query should close quick_open"
    );
}

/// Regression for Fix #3: pressing `o` on Graph 2-col (terminal width
/// below GRAPH_THREE_COL_MIN_WIDTH) must not open a picker that the
/// renderer refuses to draw.
#[test]
fn graph_two_col_o_keypress_does_not_open_invisible_picker() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Graph);
    // Set last_total_width below the 3-col threshold so
    // graph_uses_three_col() returns false.
    app.last_total_width = 80;
    app.enter_focused_preview();

    assert!(!app.focused_preview_chip_visible());
    assert!(!app.focused_preview_files_open);
    input::handle_key(key(KeyCode::Char('o')), &mut app);
    assert!(
        !app.focused_preview_files_open,
        "'o' on Graph 2-col must not open the picker (which wouldn't render)"
    );
}

#[test]
fn picker_open_swallows_scroll_keys_so_diff_stays_put() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.enter_focused_preview();

    // Picker open with at least one row so it gets armed; the row list
    // comes from staged+unstaged, which is empty in this fixture, so
    // simulate it being open via the explicit setter.
    app.focused_preview_files_open = true;

    let before = app.diff_scroll;
    input::handle_key(key(KeyCode::Down), &mut app);
    assert_eq!(
        app.diff_scroll, before,
        "diff should not scroll while picker has the keyboard"
    );

    // Esc closes picker without exiting focused preview.
    input::handle_key(key(KeyCode::Esc), &mut app);
    assert!(!app.focused_preview_files_open);
    assert_eq!(app.view_mode, ViewMode::FocusedPreview);
}

// ─── `/` 底部输入框 / Ctrl+F noop / Space+F / vim gg-G ──────────────────

#[test]
fn focused_preview_slash_renders_prompt_at_bottom() {
    // Regression target: pressing `/` in FocusedPreview used to flip
    // `search.active=true` but the prompt row was never painted because
    // FocusedPreview replaces the normal status bar. focused_preview_panel
    // now mirrors render_status_bar's priority for the bottom row.
    use reef::ui;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "abc\ndef\nghi\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();

    input::handle_key(key(KeyCode::Char('/')), &mut app);
    assert!(app.search.active, "`/` must arm vim search");

    let backend = TestBackend::new(40, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    // hint_row is the last row (y = height - 1).
    let bottom_first = terminal.backend().buffer()[(0, 9)].symbol().to_string();
    assert_eq!(
        bottom_first, "/",
        "FocusedPreview bottom row should show the search prompt `/`"
    );
}

#[test]
fn focused_preview_ctrl_f_is_noop() {
    // Ctrl+F was removed in favour of Space+F (FindWidget). Pressing it
    // in FocusedPreview must not arm vim search and must not open the
    // find widget — it's a fully unbound key now.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "abc\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();
    let view_before = app.view_mode;

    input::handle_key(
        key_with(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &mut app,
    );
    assert!(!app.search.active, "Ctrl+F must not arm vim search");
    assert!(
        !app.find_widget.active,
        "Ctrl+F must not open the FindWidget"
    );
    assert_eq!(app.view_mode, view_before, "view_mode must be unchanged");
}

#[test]
fn focused_preview_space_f_opens_find_widget() {
    // Space+F is the only path into the FindWidget overlay. Make sure it
    // still works from FocusedPreview — the Space-leader gate runs after
    // handle_key_focused_preview's fallthrough.
    use reef::ui;
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "abc\n".repeat(40)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();

    // Drive a frame so the preview body publishes `last_preview_rect`;
    // without that the find widget anchors to None and silently no-op
    // renders, which would defeat the visibility check below.
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();

    input::handle_key(key(KeyCode::Char(' ')), &mut app);
    assert!(
        app.space_leader_at.is_some(),
        "Space must arm the leader chord"
    );
    input::handle_key(key(KeyCode::Char('f')), &mut app);
    assert!(
        app.find_widget.active,
        "Space+F must open the FindWidget overlay"
    );

    // Render again and assert the widget actually painted itself.
    // Without focused_preview_panel calling find_widget_panel::render,
    // `last_widget_rect` stays `None` even though `find_widget.active`
    // is true — the overlay would be invisible to the user.
    terminal.draw(|f| ui::render(f, &mut app)).unwrap();
    assert!(
        app.find_widget.last_widget_rect.is_some(),
        "FindWidget overlay must paint a rect in FocusedPreview, not just \
         flip the active flag"
    );
}

#[test]
fn gg_scrolls_files_focused_preview_to_top() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x\n".repeat(200)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();
    app.preview_scroll = 80;

    input::handle_key(key(KeyCode::Char('g')), &mut app);
    assert!(app.g_pending_at.is_some(), "first `g` arms the chord");
    assert_eq!(app.preview_scroll, 80, "first `g` must not move yet");

    input::handle_key(key(KeyCode::Char('g')), &mut app);
    assert_eq!(app.preview_scroll, 0, "second `g` jumps to top");
    assert!(
        app.g_pending_at.is_none(),
        "fired chord clears the pending slot"
    );
}

#[test]
fn gg_scrolls_git_diff_focused_preview_to_top() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.enter_focused_preview();
    app.diff_scroll = 42;

    input::handle_key(key(KeyCode::Char('g')), &mut app);
    input::handle_key(key(KeyCode::Char('g')), &mut app);
    assert_eq!(app.diff_scroll, 0);
}

#[test]
fn capital_g_scrolls_files_focused_preview_to_bottom() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x\n".repeat(200)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();
    app.preview_scroll = 5;

    input::handle_key(key_with(KeyCode::Char('G'), KeyModifiers::SHIFT), &mut app);
    assert_eq!(
        app.preview_scroll,
        usize::MAX,
        "G sets preview_scroll to the saturation sentinel; render clamps"
    );
}

#[test]
fn gg_timeout_does_not_trigger() {
    use std::time::{Duration, Instant};
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x\n".repeat(200)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();
    app.preview_scroll = 50;

    input::handle_key(key(KeyCode::Char('g')), &mut app);
    // Backdate the pending arm past the 500ms window.
    app.g_pending_at = Some(Instant::now() - Duration::from_millis(800));
    input::handle_key(key(KeyCode::Char('g')), &mut app);
    assert_eq!(
        app.preview_scroll, 50,
        "stale `g` must not fire scroll-to-top"
    );
    assert!(
        app.g_pending_at.is_some(),
        "expired chord re-arms instead of firing"
    );
}

#[test]
fn gg_suppressed_inside_slash_search() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "x\n".repeat(200)).unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.enter_focused_preview();
    app.preview_scroll = 25;

    input::handle_key(key(KeyCode::Char('/')), &mut app);
    assert!(app.search.active);
    input::handle_key(key(KeyCode::Char('g')), &mut app);
    input::handle_key(key(KeyCode::Char('g')), &mut app);
    assert_eq!(
        app.preview_scroll, 25,
        "while `/` owns input, `gg` must feed the query instead of scrolling"
    );
    assert_eq!(
        app.search.query, "gg",
        "the two `g` keystrokes should land in the search buffer"
    );
}

// ─── Ctrl+F regression guard for Main mode ──────────────────────────────

#[test]
fn ctrl_f_in_main_mode_git_tab_does_not_toggle_diff_mode() {
    // Removing the global Ctrl+F binding could leak the keystroke into
    // `handle_key_git`'s bare-`f` arm (which toggles diff_mode). Guard
    // against that regression by sending Ctrl+F in Main mode + Tab::Git
    // and asserting nothing observable changes.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);

    let layout_before = app.diff_layout;
    let mode_before = app.diff_mode;
    input::handle_key(
        key_with(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &mut app,
    );
    assert_eq!(
        app.diff_layout, layout_before,
        "Ctrl+F must not call toggle_diff_layout in Git tab"
    );
    assert_eq!(
        app.diff_mode, mode_before,
        "Ctrl+F must not call toggle_diff_mode in Git tab"
    );
    assert!(!app.search.active);
    assert!(!app.find_widget.active);
}

#[test]
fn ctrl_f_in_main_mode_graph_tab_does_not_reach_commit_detail() {
    // Same guard for the Graph tab's bare-`f` arm at handle_key_graph's
    // commit_detail dispatch. The commit_detail diff_mode is the
    // observable side effect.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Graph);

    let cd_mode_before = app.commit_detail.diff_mode;
    let cd_layout_before = app.commit_detail.diff_layout;
    input::handle_key(
        key_with(KeyCode::Char('f'), KeyModifiers::CONTROL),
        &mut app,
    );
    assert_eq!(
        app.commit_detail.diff_mode, cd_mode_before,
        "Ctrl+F must not flip commit_detail diff_mode in Graph tab"
    );
    assert_eq!(
        app.commit_detail.diff_layout, cd_layout_before,
        "Ctrl+F must not flip commit_detail diff_layout in Graph tab"
    );
}

#[test]
fn space_f_force_focuses_diff_panel_from_file_tree() {
    // Regression for the Ctrl+F deletion: the removed global arm used
    // to set `active_panel = Diff` before calling search::begin, so
    // pressing find from the file tree would always land on the diff.
    // The replacement (Space+F → find_widget) needs the same force-
    // focus, otherwise Space+F is a silent no-op when focus is on the
    // file tree (the default state).
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "abc\n").unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    assert_eq!(app.active_panel, reef::app::Panel::Files);

    input::handle_key(key(KeyCode::Char(' ')), &mut app);
    input::handle_key(key(KeyCode::Char('f')), &mut app);

    assert!(
        app.find_widget.active,
        "Space+F must open the FindWidget even with file-tree focus"
    );
    assert_eq!(
        app.active_panel,
        reef::app::Panel::Diff,
        "Space+F must force-focus the Diff panel before opening"
    );
}

// ─── Stale-leader / chord-state hygiene ─────────────────────────────────

#[test]
fn stale_space_leader_does_not_bypass_focused_preview_whitelist() {
    // The FocusedPreview leader-armed bypass must check LEADER_TIMEOUT,
    // otherwise a Space pressed minutes ago would keep the bypass on
    // forever and let destructive Git keys (`d` for discard, etc.)
    // reach per-tab dispatch against an invisible status row.
    use std::time::Duration as StdDuration;
    use std::time::Instant as StdInstant;

    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.set_active_tab(Tab::Git);
    app.enter_focused_preview();

    // Stale leader from "minutes ago".
    app.space_leader_at = Some(StdInstant::now() - StdDuration::from_secs(60));

    // `d` is destructive in handle_key_git (calls git_status_panel `d`
    // → discard). It's NOT in the FocusedPreview whitelist, so with the
    // bypass's timeout check correctly recognising the leader as stale,
    // the whitelist swallows the keystroke as if no leader were armed.
    let layout_before = app.diff_layout;
    let mode_before = app.diff_mode;
    let stage_count_before = (app.staged_files.len(), app.unstaged_files.len());
    input::handle_key(key(KeyCode::Char('d')), &mut app);
    assert_eq!(app.diff_layout, layout_before);
    assert_eq!(app.diff_mode, mode_before);
    assert_eq!(
        (app.staged_files.len(), app.unstaged_files.len()),
        stage_count_before,
        "stale leader must not let `d` reach git_status_panel discard"
    );
}

#[test]
fn g_pending_clears_on_tab_switch() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (tmp, _repo) = test_support::tempdir_repo();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    input::handle_key(key(KeyCode::Char('g')), &mut app);
    assert!(app.g_pending_at.is_some());

    app.set_active_tab(Tab::Git);
    assert!(
        app.g_pending_at.is_none(),
        "set_active_tab must drop in-flight chord state so a stray `g` \
         after a mouse-driven tab switch doesn't surprise-scroll the new tab"
    );
}
