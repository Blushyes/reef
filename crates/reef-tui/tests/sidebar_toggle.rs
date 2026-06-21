//! Toggle-sidebar behaviour. Verifies the flag flips, width math
//! collapses to 0 on hide, and focus demotes off the hidden sidebar
//! panel so keyboard nav doesn't aim at a column no one renders.
//! Also pins the tab-bar toggle button's hit-registry registration
//! so a refactor of `render_tab_bar` can't silently mis-place it.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use reef::app::{App, Panel};
use reef::ui::mouse::ClickAction;
use reef::ui::theme::Theme;
use reef::ui::{self, SIDEBAR_TOGGLE_GLYPH_HIDDEN, SIDEBAR_TOGGLE_GLYPH_VISIBLE};
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::{CwdGuard, force_en_lang};

static CWD_LOCK: Mutex<()> = Mutex::new(());

/// Render a single frame at the given size and return both the buffer
/// (for cell inspection) and the hit-registry mutations the render
/// path produced (which now lives on `app` as a side effect).
fn render_once(app: &mut App, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| ui::render(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

/// Scan the tab-bar row (y=1) for the first cell whose glyph matches
/// `needle`. Used to locate the toggle button column without hardcoding
/// a position that depends on tab-label widths.
fn find_tab_bar_glyph(buf: &Buffer, needle: &str) -> Option<u16> {
    (0..buf.area().width).find(|&x| buf.cell((x, 1)).map(|c| c.symbol()) == Some(needle))
}

#[test]
fn sidebar_visible_by_default() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let app = App::new(Theme::dark(), None);
    assert!(app.sidebar_visible);
    assert!(app.graph_sidebar_width(200) > 0);
}

#[test]
fn toggle_hides_sidebar_and_zeroes_width() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.toggle_sidebar();
    assert!(!app.sidebar_visible);
    assert_eq!(app.graph_sidebar_width(200), 0);
}

#[test]
fn toggle_restores_sidebar() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.toggle_sidebar();
    app.toggle_sidebar();
    assert!(app.sidebar_visible);
    assert!(app.graph_sidebar_width(200) > 0);
}

#[test]
fn hiding_demotes_files_focus_to_diff() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.active_panel = Panel::Files;
    app.toggle_sidebar();
    assert_eq!(app.active_panel, Panel::Diff);
}

#[test]
fn hiding_preserves_non_files_focus() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.active_panel = Panel::Diff;
    app.toggle_sidebar();
    assert_eq!(app.active_panel, Panel::Diff);
}

#[test]
fn hiding_cancels_in_flight_drags() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.dragging_split = true;
    app.dragging_graph_diff_split = true;
    app.toggle_sidebar();
    assert!(!app.dragging_split);
    assert!(!app.dragging_graph_diff_split);
}

#[test]
fn normalize_panel_catches_stranded_files_focus() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    // Direct field write bypasses `toggle_sidebar`'s focus demotion —
    // `normalize_active_panel` must catch it on the next render.
    let mut app = App::new(Theme::dark(), None);
    app.sidebar_visible = false;
    app.active_panel = Panel::Files;
    app.normalize_active_panel();
    assert_eq!(app.active_panel, Panel::Diff);
}

#[test]
fn first_hide_pushes_toast_subsequent_dont() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    let before = app.toasts.len();
    app.toggle_sidebar(); // hide → expect one new toast
    assert_eq!(app.toasts.len(), before + 1);
    assert!(app.toasts.last().unwrap().message.contains("Ctrl+B"));

    app.toggle_sidebar(); // show
    app.toggle_sidebar(); // hide again
    // Hint flag stays set for the session; second hide must stay quiet.
    assert_eq!(app.toasts.len(), before + 1);
}

#[test]
fn render_registers_toggle_button_at_glyph_column_when_visible() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    let buf = render_once(&mut app, 80, 20);

    let x = find_tab_bar_glyph(&buf, SIDEBAR_TOGGLE_GLYPH_VISIBLE)
        .expect("collapse button must render in tab bar when sidebar visible");
    assert!(
        matches!(
            app.hit_registry.hit_test(x, 1),
            Some(ClickAction::ToggleSidebar)
        ),
        "hit-test on the rendered glyph column must dispatch ToggleSidebar",
    );
    assert!(find_tab_bar_glyph(&buf, SIDEBAR_TOGGLE_GLYPH_HIDDEN).is_none());
}

#[test]
fn render_registers_toggle_button_at_glyph_column_when_hidden() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    force_en_lang();
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());

    let mut app = App::new(Theme::dark(), None);
    app.toggle_sidebar();
    let buf = render_once(&mut app, 80, 20);

    let x = find_tab_bar_glyph(&buf, SIDEBAR_TOGGLE_GLYPH_HIDDEN)
        .expect("expand button must render in tab bar when sidebar hidden");
    assert!(matches!(
        app.hit_registry.hit_test(x, 1),
        Some(ClickAction::ToggleSidebar)
    ));
    assert!(find_tab_bar_glyph(&buf, SIDEBAR_TOGGLE_GLYPH_VISIBLE).is_none());
}
