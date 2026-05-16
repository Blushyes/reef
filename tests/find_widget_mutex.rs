//! Find widget and vim-style `/` (`SearchState`) are independent state
//! machines that must not coexist with active matches — otherwise the
//! diff/preview renderer would have to decide between two highlight
//! colors per row. `find_widget::begin_with_selection` clears
//! `app.search` on open; conversely, opening `/` is expected to clear
//! the widget (caller responsibility — exercised manually for now).

use reef::app::{App, Panel, Tab};
use reef::file_tree::{PreviewBody, PreviewContent};
use reef::find_widget;
use reef::search::{MatchLoc, SearchState, SearchTarget};
use reef::ui::theme::Theme;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn fresh_app() -> (App, TempDir, CwdGuard) {
    let tmp = TempDir::new().unwrap();
    let g = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);
    app.active_tab = Tab::Files;
    app.active_panel = Panel::Diff;
    app.preview_content = Some(PreviewContent {
        file_path: "scratch.txt".to_string(),
        body: PreviewBody::Text {
            lines: vec!["foo bar foo".to_string(), "bar foo".to_string()],
            highlighted: None,
        },
    });
    (app, tmp, g)
}

#[test]
fn opening_widget_clears_legacy_search() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();

    // Park the legacy `/` in a dormant-but-non-empty state — the
    // "user committed a search, then went back to navigating" shape.
    app.search = SearchState {
        active: false,
        backwards: false,
        query: "foo".to_string(),
        cursor: 3,
        target: Some(SearchTarget::FilePreview),
        matches: vec![MatchLoc {
            row: 0,
            byte_range: 0..3,
        }],
        current: Some(0),
        snapshot: None,
        wrap_msg: None,
    };
    assert_eq!(app.search.target, Some(SearchTarget::FilePreview));

    // Opening the widget must wipe `app.search` so its highlights stop
    // painting.
    find_widget::begin_with_selection(&mut app);

    assert!(app.find_widget.active);
    assert!(
        app.search.target.is_none(),
        "widget open should clear legacy `/` state"
    );
    assert!(app.search.matches.is_empty());
}

#[test]
fn widget_close_does_not_resurrect_legacy_search() {
    // Reverse direction is just a sanity check: closing the widget
    // restores its own snapshot but should NOT re-arm `app.search`.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();

    find_widget::begin_with_selection(&mut app);
    assert!(app.find_widget.active);
    find_widget::close(&mut app);

    assert!(!app.find_widget.active);
    assert!(app.search.target.is_none());
}
