//! End-to-end test for the Phase 1 intra-file navigation: `gd` finds
//! the in-file definition, single-candidate path jumps + pushes the
//! back-stack, multi-candidate path pops the popup, popup pick commits
//! the jump, `nav_back` restores the pre-jump scroll. Closes the
//! `enter_focused_preview_with_file`-shaped pathway against regression
//! by exercising it through the App API rather than mocking pieces.

use reef::app::nav::NavAnchor;
use reef::app::{App, Panel, Tab};
use reef::ui::selection::PreviewSelection;
use reef::ui::theme::Theme;
use reef_core::nav::{NavLang, parse_file_if_supported};
use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent, TextPreview};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use test_support::CwdGuard;

// Several tests mutate process cwd via `CwdGuard`. Serialize them so
// they don't race — same pattern as `find_widget_seed.rs`.
static CWD_LOCK: Mutex<()> = Mutex::new(());

fn fresh_app() -> (App, TempDir, CwdGuard) {
    let tmp = TempDir::new().unwrap();
    let g = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);
    app.active_tab = Tab::Files;
    app.active_panel = Panel::Diff;
    // Give the preview a non-zero viewport so center_scroll has
    // something to work with — otherwise the jump lands at scroll=0
    // regardless of target row, which masks the back-stack restore.
    app.last_preview_view_h = 20;
    (app, tmp, g)
}

fn install_rust_preview(app: &mut App, path: &str, src: &str) {
    let bytes: Arc<[u8]> = Arc::from(src.as_bytes().to_vec().into_boxed_slice());
    let parsed = parse_file_if_supported(NavLang::Rust, bytes).map(Arc::new);
    app.preview_content = Some(PreviewContent {
        path: path.to_string(),
        body: PreviewBody::Text(TextPreview {
            lines: src.lines().map(|s| s.to_string()).collect(),
            highlighted: None,
            parsed,
        }),
    });
}

fn set_keyboard_cursor(app: &mut App, line: usize, byte_col: usize) {
    let mut sel = PreviewSelection::new((line, byte_col));
    sel.active = (line, byte_col);
    sel.dragging = false;
    app.preview_selection = Some(sel);
}

/// Locate the n-th occurrence of `needle` and return `(line, byte_col)`
/// — same helper as the nav unit tests.
fn cursor_at_nth(src: &str, needle: &str, n: usize) -> (usize, usize) {
    let mut start = 0usize;
    let mut idx = None;
    for _ in 0..=n {
        let next = src[start..].find(needle).expect("needle present");
        idx = Some(start + next);
        start += next + needle.len();
    }
    let idx = idx.unwrap();
    let prefix = &src[..idx];
    let row = prefix.bytes().filter(|b| *b == b'\n').count();
    let line_start = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
    (row, idx - line_start)
}

#[test]
fn intra_file_single_candidate_jumps_and_pushes_back_stack() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "fn helper() -> i32 { 42 }\n\n\n\n\n\n\n\n\nfn main() { let _ = helper(); }\n";
    install_rust_preview(&mut app, "scratch.rs", src);
    // Cursor on `helper` in the call site (the second occurrence).
    let cursor = cursor_at_nth(src, "helper", 1);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);

    // Capture pre-jump scroll. center_scroll defaults to 0 since the
    // target is line 0 and view_h is 20 — both cases would yield 0 —
    // so set a deliberately non-zero pre-scroll to make the back-stack
    // restore observable.
    app.preview_scroll = 7;

    app.goto_definition_at_cursor(NavAnchor::Keyboard);

    let hl = app
        .preview_highlight
        .as_ref()
        .expect("highlight set after single-candidate jump");
    assert_eq!(hl.row, 0, "jumped to the fn definition on line 0");
    assert_eq!(app.location_history.len(), 1, "back-stack got an entry");
    assert_eq!(
        app.location_history.back_items()[0].scroll.vertical,
        7,
        "back-stack captured the pre-jump scroll"
    );
    assert!(
        app.nav_candidates.is_none(),
        "single-candidate path should NOT open the popup"
    );

    // Now nav_back — should restore preview_scroll to 7.
    app.nav_back();
    assert_eq!(app.preview_scroll, 7);
    assert!(
        app.location_history.is_empty(),
        "back-stack drained after one Ctrl-o"
    );
    assert_eq!(
        app.location_history.forward_len(),
        1,
        "forward stack got the post-jump state"
    );
}

/// A jump launched from a diff records a `GitDiff`/`GraphDiff` origin, so
/// Ctrl-o returns the user to that diff (tab + panel + scroll), not the
#[test]
fn intra_file_multi_candidate_opens_popup() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "\
struct A;
struct B;
impl A { fn run(&self) {} }
impl B { fn run(&self) {} }
fn main() { let a = A; a.run(); }
";
    install_rust_preview(&mut app, "scratch.rs", src);
    let cursor = cursor_at_nth(src, "run", 2);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);

    app.goto_definition_at_cursor(NavAnchor::Keyboard);

    let popup = app
        .nav_candidates
        .as_ref()
        .expect("popup opens for multi-candidate match");
    assert!(
        popup.candidates.len() >= 2,
        "popup contains the impl candidates"
    );
    assert_eq!(popup.selected, 0, "popup defaults to row 0");
    assert!(
        app.location_history.is_empty(),
        "back-stack NOT pushed yet — pick commits, not open"
    );
    assert!(
        app.preview_highlight.is_none(),
        "no jump yet — popup is shown first"
    );

    // Picking commits the navigation.
    app.nav_pick_candidate();
    assert!(app.nav_candidates.is_none(), "popup closes after pick");
    assert_eq!(
        app.location_history.len(),
        1,
        "back-stack received origin entry after pick"
    );
    assert!(app.preview_highlight.is_some(), "jump committed after pick");
}

#[test]
fn popup_close_without_pick_does_not_mutate_history() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "\
impl A { fn run(&self) {} }
impl B { fn run(&self) {} }
fn main() { let a = A; a.run(); }
";
    install_rust_preview(&mut app, "scratch.rs", src);
    let cursor = cursor_at_nth(src, "run", 2);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);

    app.goto_definition_at_cursor(NavAnchor::Keyboard);
    assert!(app.nav_candidates.is_some());

    app.nav_close_candidates();
    assert!(app.nav_candidates.is_none());
    assert!(
        app.location_history.is_empty(),
        "closing the popup must not leak a back-stack entry"
    );
}

#[test]
fn goto_definition_on_unknown_extension_is_noop() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    // `.txt` has no parser → parsed = None even if we install a
    // text body, so gd silently does nothing.
    app.preview_content = Some(PreviewContent {
        path: "scratch.txt".to_string(),
        body: PreviewBody::Text(TextPreview {
            lines: vec!["fn helper() {}".to_string()],
            highlighted: None,
            parsed: None,
        }),
    });
    set_keyboard_cursor(&mut app, 0, 3);
    app.goto_definition_at_cursor(NavAnchor::Keyboard);
    assert!(app.location_history.is_empty());
    assert!(app.nav_candidates.is_none());
    assert!(app.preview_highlight.is_none());
}

#[test]
fn clicking_on_definition_with_no_refs_does_not_open_empty_popup() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "fn helper() -> i32 { 42 }\nfn main() { let _ = helper(); helper(); }\n";
    install_rust_preview(&mut app, "scratch.rs", src);
    // Click on `helper` in the DEFINITION (first occurrence). The
    // skip-self filter hides the only intra-file def, so candidates=0
    // and goto falls through to find-references.
    let cursor = cursor_at_nth(src, "helper", 0);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);

    app.goto_definition_at_cursor(NavAnchor::Keyboard);

    // No workspace index is built in this synthetic test, so refs is
    // empty. The NEW contract: an empty refs result shows a toast and
    // does NOT open an empty (invisible, keyboard-capturing) popup.
    assert!(
        app.nav_candidates.is_none(),
        "an empty references result must not open a popup"
    );
}

#[test]
fn persistent_highlight_does_not_fade() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "fn helper() {}\nfn main() { helper(); }\n";
    install_rust_preview(&mut app, "scratch.rs", src);

    // Global-search-style persistent highlight — must survive the fade
    // timer (regression: a fade on the shared slot yanked the search
    // locator band away while the user was still reading the result).
    app.set_preview_highlight_persistent(std::path::PathBuf::from("scratch.rs"), 1, 0..3);
    assert!(app.preview_highlight.is_some());

    // Even if a (defensive) stamp were present and ancient, a
    // non-fading highlight must not be cleared.
    for _ in 0..3 {
        app.advance_preview_highlight_fade();
    }
    assert!(
        app.preview_highlight.is_some(),
        "persistent (global-search) highlight must never auto-fade"
    );
}

#[test]
fn preview_highlight_fades_after_ttl_via_tick() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "fn helper() {}\nfn main() { helper(); }\n";
    install_rust_preview(&mut app, "scratch.rs", src);
    let cursor = cursor_at_nth(src, "helper", 1);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);

    app.goto_definition_at_cursor(NavAnchor::Keyboard);
    assert!(
        app.preview_highlight.is_some(),
        "highlight set right after jump"
    );

    // The jump is intra-file (scratch.rs is the loaded preview), so the
    // first tick transitions Pending → Counting. Then back-date the
    // `since` past the TTL to force expiry on the next tick.
    app.advance_preview_highlight_fade();
    let ttl_plus = App::PREVIEW_HIGHLIGHT_TTL + std::time::Duration::from_millis(100);
    let earlier = std::time::Instant::now()
        .checked_sub(ttl_plus)
        .expect("clock is monotonic and well past UNIX_EPOCH");
    // Back-date the in-highlight fade state past the TTL.
    app.preview_highlight
        .as_mut()
        .expect("highlight present")
        .fade = reef::app::HighlightFade::Counting { since: earlier };

    app.advance_preview_highlight_fade();

    assert!(
        app.preview_highlight.is_none(),
        "highlight should have faded by TTL+slack"
    );
}

#[test]
fn candidates_popup_scrolls_to_keep_selection_visible() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    // 20 same-named methods across impl blocks → 20 candidates,
    // well past MAX_VISIBLE_ROWS (8).
    let mut src = String::new();
    for i in 0..20 {
        src.push_str(&format!("impl S{i} {{ fn run(&self) {{}} }}\n"));
    }
    src.push_str("fn main() { s.run(); }\n");
    install_rust_preview(&mut app, "scratch.rs", &src);
    let cursor = cursor_at_nth(&src, "run", 20); // the call site
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);

    app.goto_definition_at_cursor(NavAnchor::Keyboard);
    let popup = app.nav_candidates.as_ref().expect("popup for 20 defs");
    assert!(popup.candidates.len() >= 8, "many candidates");
    assert_eq!(popup.selected, 0);
    assert_eq!(popup.scroll, 0, "starts un-scrolled");

    // Walk selection down past the visible window; scroll must follow.
    for _ in 0..10 {
        app.nav_candidates_move(1);
    }
    let popup = app.nav_candidates.as_ref().unwrap();
    assert_eq!(popup.selected, 10);
    assert!(
        popup.selected >= popup.scroll
            && popup.selected < popup.scroll + reef::app::nav::NavCandidatesPopup::MAX_VISIBLE_ROWS,
        "selection {} must be within window [{}, {}+{})",
        popup.selected,
        popup.scroll,
        popup.scroll,
        reef::app::nav::NavCandidatesPopup::MAX_VISIBLE_ROWS
    );
}

#[test]
fn candidates_popup_wheel_scrolls_without_moving_selection() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let mut src = String::new();
    for i in 0..20 {
        src.push_str(&format!("impl S{i} {{ fn run(&self) {{}} }}\n"));
    }
    src.push_str("fn main() { s.run(); }\n");
    install_rust_preview(&mut app, "scratch.rs", &src);
    let cursor = cursor_at_nth(&src, "run", 20);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);
    app.goto_definition_at_cursor(NavAnchor::Keyboard);

    let before_sel = app.nav_candidates.as_ref().unwrap().selected;
    app.nav_candidates_scroll(3);
    let popup = app.nav_candidates.as_ref().unwrap();
    assert_eq!(popup.scroll, 3, "wheel moved the window");
    assert_eq!(
        popup.selected, before_sel,
        "wheel scroll must not move the highlighted row"
    );
}

#[test]
fn nav_back_and_forward_round_trip() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    let src = "fn helper() -> i32 { 42 }\n\n\n\n\n\nfn main() { let _ = helper(); }\n";
    install_rust_preview(&mut app, "scratch.rs", src);
    let cursor = cursor_at_nth(src, "helper", 1);
    set_keyboard_cursor(&mut app, cursor.0, cursor.1);
    app.preview_scroll = 5;

    app.goto_definition_at_cursor(NavAnchor::Keyboard);
    assert_eq!(app.location_history.len(), 1);

    app.nav_back();
    assert_eq!(app.preview_scroll, 5);
    assert_eq!(app.location_history.forward_len(), 1);

    app.nav_forward();
    assert_eq!(
        app.location_history.len(),
        1,
        "back-stack got the symmetric push"
    );
    assert!(
        app.location_history.forward_is_empty(),
        "forward stack consumed by nav_forward"
    );
}
