//! `find_widget::begin_with_selection` contract on the file preview:
//! pressing `Space+F` while text is selected pops the floating widget,
//! seeds the query with the first non-empty trimmed line of the
//! selection, runs the search, and leaves the prompt focused so the
//! user can refine the query or just navigate with `Space+G`.

use reef::app::{App, Panel, Tab};
use reef::file_tree::{PreviewBody, PreviewContent};
use reef::find_widget;
use reef::find_widget::FindTarget;
use reef::ui::selection::PreviewSelection;
use reef::ui::theme::Theme;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn fresh_app() -> (App, TempDir, CwdGuard) {
    let tmp = TempDir::new().unwrap();
    let g = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);
    // Preview pane is hosted under `Panel::Diff` in the Files tab.
    app.active_tab = Tab::Files;
    app.active_panel = Panel::Diff;
    (app, tmp, g)
}

fn install_text_preview(app: &mut App, lines: &[&str]) {
    app.preview_content = Some(PreviewContent {
        file_path: "scratch.txt".to_string(),
        body: PreviewBody::Text {
            lines: lines.iter().map(|s| s.to_string()).collect(),
            highlighted: None,
        },
    });
}

fn select_byte_range(app: &mut App, start: (usize, usize), end: (usize, usize)) {
    let mut sel = PreviewSelection::new(start);
    sel.active = end;
    sel.dragging = false;
    app.preview_selection = Some(sel);
}

#[test]
fn seeds_query_and_runs_match_from_preview_selection() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(
        &mut app,
        &["fn foo() { bar(); }", "    bar();", "    bar();"],
    );
    select_byte_range(&mut app, (1, 4), (1, 7));

    find_widget::begin_with_selection(&mut app);

    assert!(app.find_widget.active);
    assert_eq!(app.find_widget.query, "bar");
    assert_eq!(app.find_widget.cursor, "bar".len());
    assert_eq!(app.find_widget.target, Some(FindTarget::FilePreview));
    assert_eq!(app.find_widget.matches.len(), 3);
    assert_eq!(app.find_widget.current, Some(0));
}

#[test]
fn trims_indentation_when_selection_grabs_full_line() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(&mut app, &["fn foo() {", "    bar();", "}"]);
    select_byte_range(&mut app, (1, 0), (1, "    bar();".len()));

    find_widget::begin_with_selection(&mut app);

    assert_eq!(app.find_widget.query, "bar();");
    assert!(!app.find_widget.matches.is_empty());
}

#[test]
fn no_selection_opens_empty_widget() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(&mut app, &["hello world"]);

    find_widget::begin_with_selection(&mut app);

    assert!(app.find_widget.active);
    assert!(app.find_widget.query.is_empty());
    assert_eq!(app.find_widget.target, Some(FindTarget::FilePreview));
    assert!(app.find_widget.matches.is_empty());
}

#[test]
fn collapsed_selection_falls_back_to_empty_widget() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(&mut app, &["hello world"]);
    select_byte_range(&mut app, (0, 5), (0, 5));

    find_widget::begin_with_selection(&mut app);

    assert!(app.find_widget.active);
    assert!(app.find_widget.query.is_empty());
}

#[test]
fn step_after_seed_advances_current() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(&mut app, &["bar", "bar", "bar"]);
    select_byte_range(&mut app, (0, 0), (0, 3));

    find_widget::begin_with_selection(&mut app);
    assert_eq!(app.find_widget.current, Some(0));

    find_widget::step(&mut app, /*reverse=*/ false);
    assert_eq!(app.find_widget.current, Some(1));
    find_widget::step(&mut app, /*reverse=*/ false);
    assert_eq!(app.find_widget.current, Some(2));
    // Wraps back to start.
    find_widget::step(&mut app, /*reverse=*/ false);
    assert_eq!(app.find_widget.current, Some(0));
    // Reverse wraps to last.
    find_widget::step(&mut app, /*reverse=*/ true);
    assert_eq!(app.find_widget.current, Some(2));
}

#[test]
fn close_restores_pre_find_scroll_and_clears_state() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(
        &mut app,
        &[
            "line 0", "line 1", "line 2", "bar 3", "line 4", "line 5", "line 6", "line 7",
            "line 8", "line 9",
        ],
    );
    app.preview_scroll = 0;
    app.last_preview_view_h = 4;
    select_byte_range(&mut app, (3, 0), (3, 3));

    find_widget::begin_with_selection(&mut app);
    // Match landed on row 3 — center-scroll pushes preview_scroll
    // away from 0.
    assert!(app.find_widget.active);

    find_widget::close(&mut app);
    assert!(!app.find_widget.active);
    assert_eq!(app.find_widget.query, "");
    assert_eq!(app.preview_scroll, 0, "snapshot should restore scroll");
}

#[test]
fn tab_switch_closes_widget_and_restores_scroll() {
    // `App::set_active_tab` calls `find_widget::close` so a widget
    // anchored on one tab's panel doesn't bleed into another. Verifies
    // both halves: `active` flips off and pre-find scroll is restored.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(
        &mut app,
        &[
            "line 0", "line 1", "line 2", "bar 3", "line 4", "line 5", "line 6", "line 7",
            "line 8", "line 9",
        ],
    );
    app.preview_scroll = 0;
    app.last_preview_view_h = 4;
    select_byte_range(&mut app, (3, 0), (3, 3));

    find_widget::begin_with_selection(&mut app);
    assert!(app.find_widget.active);

    // Switch away — should auto-close.
    app.set_active_tab(Tab::Git);

    assert!(!app.find_widget.active);
    assert_eq!(app.find_widget.query, "");
    assert_eq!(app.find_widget.target, None);
    assert_eq!(
        app.preview_scroll, 0,
        "tab switch must restore pre-find scroll via the same snapshot path as Esc",
    );
}

#[test]
fn step_after_close_is_a_noop_not_a_panic() {
    // Calling `step` once the widget has been closed (matches drained)
    // should silently no-op — the close-then-step ordering can happen
    // when a chord fires between widget Esc and the next frame.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(&mut app, &["bar", "bar"]);
    select_byte_range(&mut app, (0, 0), (0, 3));

    find_widget::begin_with_selection(&mut app);
    find_widget::close(&mut app);
    // Must not panic; current stays None.
    find_widget::step(&mut app, false);
    find_widget::step(&mut app, true);
    assert_eq!(app.find_widget.current, None);
    assert!(app.find_widget.matches.is_empty());
}

#[test]
fn no_target_panel_is_a_noop() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    // Tab::Search left panel has no in-panel find target.
    app.active_tab = Tab::Search;
    app.active_panel = Panel::Files;

    find_widget::begin_with_selection(&mut app);

    assert!(!app.find_widget.active);
    assert!(app.find_widget.query.is_empty());
    assert_eq!(app.find_widget.target, None);
}
