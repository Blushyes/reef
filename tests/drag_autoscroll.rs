//! Integration tests for `tick_drag_autoscroll` — the tick-driven loop
//! that scrolls the preview / diff viewport (and extends the active
//! selection) when the user drags past the viewport vertically.
//!
//! The pure helpers (step / distance / interval) have their own unit
//! tests in `src/input.rs`. These tests cover the orchestration: gate
//! conditions, scroll-field routing, clamp-at-bounds, throttle, and the
//! `selection.active` refresh that makes the selection appear to "follow"
//! the autoscroll.

use reef::app::{App, DiffLayout, Tab};
use reef::file_tree::{PreviewBody, PreviewContent};
use reef::input::tick_drag_autoscroll;
use reef::ui::selection::{DiffHit, DiffRowText, DiffSelection, DiffSide, PreviewSelection};
use reef::ui::theme::Theme;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use test_support::CwdGuard;

// `App::new` walks the cwd to find a repo. Tests that build an App must
// own the process cwd for the duration of construction.
static CWD_LOCK: Mutex<()> = Mutex::new(());

fn fresh_app() -> (App, TempDir) {
    let tmp = TempDir::new().unwrap();
    let _g = CwdGuard::enter(tmp.path());
    let app = App::new(Theme::dark(), None);
    drop(_g);
    (app, tmp)
}

fn text_preview(line_count: usize) -> PreviewContent {
    PreviewContent {
        file_path: "test.txt".into(),
        body: PreviewBody::Text {
            lines: (0..line_count).map(|i| format!("line {}", i)).collect(),
            highlighted: None,
        },
    }
}

// ── preview ──────────────────────────────────────────────────────────────

#[test]
fn preview_scrolls_up_and_extends_active_when_mouse_above_viewport() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    // Viewport: content_y = 5, view_h = 20 → content rows 5..25.
    // Mouse at row 2 (3 cells above the top). Scroll currently at 30.
    app.preview_content = Some(text_preview(200));
    app.preview_scroll = 30;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 2));
    app.preview_selection = Some(PreviewSelection {
        anchor: (50, 0),
        active: (30, 0),
        dragging: true,
    });

    tick_drag_autoscroll(&mut app);

    // dist=3, step=-(1+1)=-2 → scroll moves from 30 to 28.
    assert_eq!(app.preview_scroll, 28, "scroll up by step magnitude");

    // selection.active follows the scroll — at the new scroll, the
    // visible row 0 (where the clamped mouse Y lands) maps to file line
    // = new_scroll = 28.
    let sel = app.preview_selection.expect("selection retained");
    assert_eq!(sel.active.0, 28, "active row follows scroll up");
    assert!(sel.dragging, "still dragging after autoscroll step");

    assert!(
        app.preview_autoscroll_at.is_some(),
        "throttle stamp set after a scroll step"
    );
}

#[test]
fn preview_scroll_down_clamps_at_max_scroll() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    // 30 lines, view_h 20 → max_scroll = 10. Already at scroll=9, view
    // mouse far below content_bottom (=25): should clamp at 10, not
    // overshoot.
    app.preview_content = Some(text_preview(30));
    app.preview_scroll = 9;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 200));
    app.preview_selection = Some(PreviewSelection {
        anchor: (0, 0),
        active: (10, 0),
        dragging: true,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(app.preview_scroll, 10, "clamped at max_scroll");
}

#[test]
fn preview_already_at_top_stays_put_with_no_throttle_stamp() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.preview_content = Some(text_preview(200));
    app.preview_scroll = 0;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 0)); // way above the viewport
    app.preview_selection = Some(PreviewSelection {
        anchor: (5, 0),
        active: (0, 0),
        dragging: true,
    });
    app.preview_autoscroll_at = None;

    tick_drag_autoscroll(&mut app);

    assert_eq!(app.preview_scroll, 0, "no scroll possible at top");
    // No-op path → throttle stamp must NOT be set (else the next genuine
    // step after a clamp would be delayed for no reason).
    assert!(
        app.preview_autoscroll_at.is_none(),
        "throttle untouched when scroll didn't move"
    );
}

#[test]
fn preview_throttle_rejects_second_call_inside_interval() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.preview_content = Some(text_preview(200));
    app.preview_scroll = 30;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 4)); // 1 above content_top=5, dist=1
    app.preview_selection = Some(PreviewSelection {
        anchor: (50, 0),
        active: (30, 0),
        dragging: true,
    });

    // First call: dist=1 step=-1 → scroll 29, stamp now.
    tick_drag_autoscroll(&mut app);
    assert_eq!(app.preview_scroll, 29);
    let stamp_after_first = app.preview_autoscroll_at;
    assert!(stamp_after_first.is_some());

    // Immediate second call should be throttled out — at distance 1 the
    // interval is 90ms, and we're firing back-to-back synchronously.
    tick_drag_autoscroll(&mut app);
    assert_eq!(
        app.preview_scroll, 29,
        "second call inside throttle is no-op"
    );
    assert_eq!(
        app.preview_autoscroll_at, stamp_after_first,
        "stamp unchanged"
    );
}

#[test]
fn preview_inside_viewport_does_nothing() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.preview_content = Some(text_preview(200));
    app.preview_scroll = 30;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 15)); // squarely inside content area
    app.preview_selection = Some(PreviewSelection {
        anchor: (50, 0),
        active: (40, 0),
        dragging: true,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(
        app.preview_scroll, 30,
        "no scroll when mouse inside viewport"
    );
    assert!(app.preview_autoscroll_at.is_none());
}

#[test]
fn preview_non_text_body_aborts() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    // Construct a Binary body (an image picker isn't available in this
    // test env, so use Binary which doesn't need image decoding).
    app.preview_content = Some(PreviewContent {
        file_path: "blob.bin".into(),
        body: PreviewBody::Binary(reef::file_tree::BinaryInfo {
            bytes_on_disk: 42,
            mime: None,
            reason: reef::file_tree::BinaryReason::NullBytes,
            meta_line: "x".into(),
        }),
    });
    app.preview_scroll = 30;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 0));
    app.preview_selection = Some(PreviewSelection {
        anchor: (50, 0),
        active: (30, 0),
        dragging: true,
    });

    tick_drag_autoscroll(&mut app);

    // Non-text bodies can't be selected; autoscroll must not pretend
    // otherwise (would scroll past the cached line index and panic).
    assert_eq!(app.preview_scroll, 30);
}

#[test]
fn preview_no_drag_no_op() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.preview_content = Some(text_preview(200));
    app.preview_scroll = 30;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 0));
    // Selection present but NOT dragging — autoscroll must stay quiet
    // (this is the "selection still rendered after Up" state).
    app.preview_selection = Some(PreviewSelection {
        anchor: (50, 0),
        active: (30, 0),
        dragging: false,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(app.preview_scroll, 30);
}

// ── diff ─────────────────────────────────────────────────────────────────

fn unified_hit(rows: usize, scroll: usize) -> DiffHit {
    DiffHit {
        layout: DiffLayout::Unified,
        content_y: 5,
        content_x_unified: 0,
        content_x_left: 0,
        content_x_right: 0,
        right_start_x: 0,
        scroll,
        h_scroll: 0,
        sbs_left_h_scroll: 0,
        sbs_right_h_scroll: 0,
        rows: (0..rows)
            .map(|i| DiffRowText::Unified(format!("row {}", i)))
            .collect(),
    }
}

#[test]
fn diff_git_tab_scrolls_app_diff_scroll() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.active_tab = Tab::Git;
    app.diff_scroll = 30;
    app.commit_detail.file_diff_scroll = 999; // bystander, must not move
    app.last_diff_view_h = 20;
    app.last_diff_hit = Some(unified_hit(200, 30));
    app.last_drag_mouse = Some((0, 2));
    app.diff_selection = Some(DiffSelection {
        sel: PreviewSelection {
            anchor: (50, 0),
            active: (30, 0),
            dragging: true,
        },
        side: DiffSide::Unified,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(app.diff_scroll, 28, "scroll up by step=-2");
    assert_eq!(
        app.commit_detail.file_diff_scroll, 999,
        "graph diff scroll untouched on Git tab"
    );
    // Cached hit.scroll synced in place so the next coord_for is accurate.
    let hit = app.last_diff_hit.as_ref().unwrap();
    assert_eq!(
        hit.scroll, 28,
        "DiffHit scroll synced to new app.diff_scroll"
    );

    let sel = app.diff_selection.unwrap();
    assert_eq!(sel.sel.active.0, 28, "active row follows scroll up");
    assert!(app.diff_autoscroll_at.is_some());
}

#[test]
fn diff_graph_tab_routes_to_commit_detail_field() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.active_tab = Tab::Graph;
    app.diff_scroll = 999; // bystander
    app.commit_detail.file_diff_scroll = 30;
    app.last_diff_view_h = 20;
    app.last_diff_hit = Some(unified_hit(200, 30));
    app.last_drag_mouse = Some((0, 2));
    app.diff_selection = Some(DiffSelection {
        sel: PreviewSelection {
            anchor: (50, 0),
            active: (30, 0),
            dragging: true,
        },
        side: DiffSide::Unified,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(
        app.commit_detail.file_diff_scroll, 28,
        "graph 3-col diff scroll moved"
    );
    assert_eq!(
        app.diff_scroll, 999,
        "git-tab scroll untouched on Graph tab"
    );
}

#[test]
fn diff_files_tab_with_stuck_selection_is_safe_noop() {
    // Defensive: if a diff selection somehow survives a tab switch to
    // Files (last_diff_rect/hit may also linger from the last render),
    // autoscroll must not blow up — it just has no scroll field to
    // mutate.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.active_tab = Tab::Files;
    app.diff_scroll = 30;
    app.commit_detail.file_diff_scroll = 30;
    app.last_diff_view_h = 20;
    app.last_diff_hit = Some(unified_hit(200, 30));
    app.last_drag_mouse = Some((0, 2));
    app.diff_selection = Some(DiffSelection {
        sel: PreviewSelection {
            anchor: (50, 0),
            active: (30, 0),
            dragging: true,
        },
        side: DiffSide::Unified,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(app.diff_scroll, 30);
    assert_eq!(app.commit_detail.file_diff_scroll, 30);
}

#[test]
fn diff_throttle_is_independent_of_preview_throttle() {
    // The split into preview_autoscroll_at / diff_autoscroll_at exists so
    // a preview drag step never delays a diff drag step (and vice
    // versa). Verify by pre-stamping the preview throttle to "now" and
    // confirming the diff side still fires.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.preview_autoscroll_at = Some(Instant::now());
    app.diff_autoscroll_at = None;

    app.active_tab = Tab::Git;
    app.diff_scroll = 30;
    app.last_diff_view_h = 20;
    app.last_diff_hit = Some(unified_hit(200, 30));
    app.last_drag_mouse = Some((0, 2));
    app.diff_selection = Some(DiffSelection {
        sel: PreviewSelection {
            anchor: (50, 0),
            active: (30, 0),
            dragging: true,
        },
        side: DiffSide::Unified,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(
        app.diff_scroll, 28,
        "diff autoscroll runs even though preview throttle is hot"
    );
}

#[test]
fn diff_no_drag_no_op() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.active_tab = Tab::Git;
    app.diff_scroll = 30;
    app.last_diff_view_h = 20;
    app.last_diff_hit = Some(unified_hit(200, 30));
    app.last_drag_mouse = Some((0, 0));
    app.diff_selection = Some(DiffSelection {
        sel: PreviewSelection {
            anchor: (50, 0),
            active: (30, 0),
            dragging: false, // not dragging
        },
        side: DiffSide::Unified,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(app.diff_scroll, 30);
}

#[test]
fn diff_empty_rows_no_op() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.active_tab = Tab::Git;
    app.diff_scroll = 0;
    app.last_diff_view_h = 20;
    app.last_diff_hit = Some(unified_hit(0, 0)); // empty diff
    app.last_drag_mouse = Some((0, 0));
    app.diff_selection = Some(DiffSelection {
        sel: PreviewSelection {
            anchor: (0, 0),
            active: (0, 0),
            dragging: true,
        },
        side: DiffSide::Unified,
    });

    tick_drag_autoscroll(&mut app);

    assert_eq!(app.diff_scroll, 0);
}

// ── elapsed throttle ─────────────────────────────────────────────────────

#[test]
fn preview_throttle_releases_after_interval_elapses() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp) = fresh_app();

    app.preview_content = Some(text_preview(200));
    app.preview_scroll = 30;
    app.last_preview_view_h = 20;
    app.last_preview_content_origin = Some((0, 5, 0));
    app.last_drag_mouse = Some((0, 4)); // dist=1, interval=90ms
    app.preview_selection = Some(PreviewSelection {
        anchor: (50, 0),
        active: (30, 0),
        dragging: true,
    });

    // First step → scrolls + stamps.
    tick_drag_autoscroll(&mut app);
    assert_eq!(app.preview_scroll, 29);
    // Manually backdate the stamp past the interval so we don't have to
    // sleep in a unit test.
    app.preview_autoscroll_at = Some(Instant::now() - Duration::from_millis(200));

    // Second step now should pass the throttle.
    tick_drag_autoscroll(&mut app);
    assert_eq!(
        app.preview_scroll, 28,
        "second step fires after interval elapses"
    );
}
