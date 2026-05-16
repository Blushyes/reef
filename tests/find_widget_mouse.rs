//! Mouse hit-zone routing for the find widget. Renders a frame so the
//! `find_widget_panel` registers its button rects, then queries
//! `hit_registry` at the widget's content row to confirm each control
//! resolves to the right `ClickAction`. Finally drives `handle_action`
//! to verify the state transitions match expectations.
//!
//! Skips `input::handle_mouse` itself — that wrapper is exercised by
//! manual QA. This test covers the registration + dispatch layers,
//! which is where the actual logic lives.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use reef::app::{App, Panel, Tab};
use reef::file_tree::{PreviewBody, PreviewContent};
use reef::find_widget;
use reef::ui;
use reef::ui::mouse::ClickAction;
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
    app.active_tab = Tab::Files;
    app.active_panel = Panel::Diff;
    app.preview_content = Some(PreviewContent {
        file_path: "scratch.txt".to_string(),
        body: PreviewBody::Text {
            lines: vec![
                "foo bar baz".to_string(),
                "    bar();".to_string(),
                "    bar();".to_string(),
            ],
            highlighted: None,
        },
    });
    (app, tmp, g)
}

/// Render once into a `TestBackend` so `find_widget_panel` populates
/// `app.hit_registry` and `app.find_widget.last_widget_rect`.
fn render_once(app: &mut App) {
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
}

/// Walk the widget's content row left-to-right, returning the first
/// column whose hit zone resolves to a `ClickAction` matching `target`
/// by enum discriminant. Avoids hardcoding the widget's internal x-
/// layout in the test.
fn find_button_col(app: &App, target: &ClickAction) -> Option<u16> {
    let rect = app.find_widget.last_widget_rect?;
    let content_row = rect.y + 1;
    for col in rect.x..rect.x + rect.width {
        if let Some(action) = app.hit_registry.hit_test(col, content_row)
            && std::mem::discriminant(&action) == std::mem::discriminant(target)
        {
            return Some(col);
        }
    }
    None
}

fn select(app: &mut App, line: usize, start: usize, end: usize) {
    let mut sel = PreviewSelection::new((line, start));
    sel.active = (line, end);
    sel.dragging = false;
    app.preview_selection = Some(sel);
}

#[test]
fn close_button_dispatches_to_widget_close() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    select(&mut app, 0, 0, 3); // "foo"
    find_widget::begin_with_selection(&mut app);
    assert!(app.find_widget.active);
    render_once(&mut app);

    let col = find_button_col(&app, &ClickAction::FindWidgetClose)
        .expect("close button hit zone must be registered");
    let row = app.find_widget.last_widget_rect.unwrap().y + 1;
    let action = app.hit_registry.hit_test(col, row).unwrap();
    app.handle_action(action);

    assert!(!app.find_widget.active);
}

#[test]
fn next_button_advances_current_match() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    select(&mut app, 1, 4, 7); // "bar"
    find_widget::begin_with_selection(&mut app);
    assert_eq!(app.find_widget.current, Some(0));
    render_once(&mut app);

    let col = find_button_col(&app, &ClickAction::FindWidgetNext).unwrap();
    let row = app.find_widget.last_widget_rect.unwrap().y + 1;
    let action = app.hit_registry.hit_test(col, row).unwrap();
    app.handle_action(action);

    assert_eq!(app.find_widget.current, Some(1));
}

#[test]
fn prev_button_steps_backward() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    select(&mut app, 1, 4, 7);
    find_widget::begin_with_selection(&mut app);
    render_once(&mut app);
    // Step forward twice via API so prev has somewhere to go.
    find_widget::step(&mut app, false);
    find_widget::step(&mut app, false);
    assert_eq!(app.find_widget.current, Some(2));

    let col = find_button_col(&app, &ClickAction::FindWidgetPrev).unwrap();
    let row = app.find_widget.last_widget_rect.unwrap().y + 1;
    let action = app.hit_registry.hit_test(col, row).unwrap();
    app.handle_action(action);

    assert_eq!(app.find_widget.current, Some(1));
}

#[test]
fn match_case_toggle_flips_and_rematches() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    // Use a query that proves case-sensitivity matters: "Bar" vs "bar".
    app.preview_content = Some(PreviewContent {
        file_path: "scratch.txt".to_string(),
        body: PreviewBody::Text {
            lines: vec!["Bar bar BAR".to_string()],
            highlighted: None,
        },
    });
    select(&mut app, 0, 0, 3); // "Bar"
    find_widget::begin_with_selection(&mut app);
    // match_case starts off → insensitive → 3 hits
    assert_eq!(app.find_widget.matches.len(), 3);
    render_once(&mut app);

    let col = find_button_col(&app, &ClickAction::FindWidgetToggleCase).unwrap();
    let row = app.find_widget.last_widget_rect.unwrap().y + 1;
    let action = app.hit_registry.hit_test(col, row).unwrap();
    app.handle_action(action);

    assert!(app.find_widget.match_case);
    // Case-sensitive "Bar" → only the literal "Bar" matches.
    assert_eq!(app.find_widget.matches.len(), 1);
}

#[test]
fn whole_word_toggle_flips_and_filters_matches() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    app.preview_content = Some(PreviewContent {
        file_path: "scratch.txt".to_string(),
        body: PreviewBody::Text {
            lines: vec!["foo food foobar foo!".to_string()],
            highlighted: None,
        },
    });
    select(&mut app, 0, 0, 3);
    find_widget::begin_with_selection(&mut app);
    // Without whole_word: "foo" appears in food, foobar, foo, foo —
    // 4 substring hits.
    assert_eq!(app.find_widget.matches.len(), 4);
    render_once(&mut app);

    let col = find_button_col(&app, &ClickAction::FindWidgetToggleWord).unwrap();
    let row = app.find_widget.last_widget_rect.unwrap().y + 1;
    let action = app.hit_registry.hit_test(col, row).unwrap();
    app.handle_action(action);

    assert!(app.find_widget.whole_word);
    // With whole_word: only standalone "foo" and "foo!" match. food
    // and foobar are filtered out.
    assert_eq!(app.find_widget.matches.len(), 2);
}

#[test]
fn regex_toggle_reinterprets_query() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    app.preview_content = Some(PreviewContent {
        file_path: "scratch.txt".to_string(),
        body: PreviewBody::Text {
            lines: vec!["abc 12 d345 ef".to_string()],
            highlighted: None,
        },
    });
    // Seed with the literal "\d+" — without regex it matches no chars.
    find_widget::begin_with_selection(&mut app);
    app.find_widget.query = r"\d+".to_string();
    app.find_widget.cursor = 3;
    find_widget::recompute(&mut app);
    assert!(app.find_widget.matches.is_empty());
    render_once(&mut app);

    let col = find_button_col(&app, &ClickAction::FindWidgetToggleRegex).unwrap();
    let row = app.find_widget.last_widget_rect.unwrap().y + 1;
    let action = app.hit_registry.hit_test(col, row).unwrap();
    app.handle_action(action);

    assert!(app.find_widget.regex);
    // `\d+` as a regex now matches "12" and "345".
    assert_eq!(app.find_widget.matches.len(), 2);
}
