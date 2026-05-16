//! SBS diff targeting: `find_widget::begin_with_selection` must pick
//! `DiffSbsLeft` / `DiffSbsRight` based on the user's drag-selection
//! side, and run the match only against that side's content. No
//! selection in SBS layout defaults to the right (new code) side.

use reef::app::{App, DiffLayout, HighlightedDiff, Panel, Tab};
use reef::find_widget;
use reef::find_widget::FindTarget;
use reef::git::{DiffContent, DiffHunk, DiffLine, LineTag};
use reef::ui::selection::{DiffSelection, DiffSide, PreviewSelection};
use reef::ui::theme::Theme;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn fresh_app() -> (App, TempDir, CwdGuard) {
    let tmp = TempDir::new().unwrap();
    let g = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);
    app.active_tab = Tab::Git;
    app.active_panel = Panel::Diff;
    app.diff_layout = DiffLayout::SideBySide;
    (app, tmp, g)
}

/// A diff whose paired `Removed`/`Added` lines stay on the same row in
/// SBS mode, with the two sides carrying different needle text — so a
/// left-vs-right targeted search can distinguish them.
fn install_paired_diff(app: &mut App) {
    let hunk = DiffHunk {
        header: "@@ -1,3 +1,3 @@".to_string(),
        lines: vec![
            DiffLine {
                tag: LineTag::Context,
                content: "ctx 1".to_string(),
                old_lineno: Some(1),
                new_lineno: Some(1),
            },
            DiffLine {
                tag: LineTag::Removed,
                content: "old needle here".to_string(),
                old_lineno: Some(2),
                new_lineno: None,
            },
            DiffLine {
                tag: LineTag::Added,
                content: "new needle there".to_string(),
                old_lineno: None,
                new_lineno: Some(2),
            },
            DiffLine {
                tag: LineTag::Context,
                content: "ctx 3".to_string(),
                old_lineno: Some(3),
                new_lineno: Some(3),
            },
        ],
    };
    app.diff_content = Some(HighlightedDiff {
        diff: DiffContent {
            file_path: "scratch.rs".to_string(),
            hunks: vec![hunk],
        },
        highlighted: None,
    });
}

fn diff_selection(side: DiffSide) -> DiffSelection {
    // Anchor / active byte offsets are irrelevant for the
    // selection-side decision; `current_text_selection` reads the
    // text via the cached `DiffHit`, but `begin_with_selection`
    // tolerates a missing `DiffHit` — the selection seed just falls
    // back to empty in that case.
    let mut sel = PreviewSelection::new((0, 0));
    sel.active = (0, 0);
    sel.dragging = false;
    DiffSelection { sel, side }
}

#[test]
fn left_side_selection_targets_sbs_left() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_paired_diff(&mut app);
    app.diff_selection = Some(diff_selection(DiffSide::SbsLeft));

    find_widget::begin_with_selection(&mut app);

    assert_eq!(app.find_widget.target, Some(FindTarget::DiffSbsLeft));
}

#[test]
fn right_side_selection_targets_sbs_right() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_paired_diff(&mut app);
    app.diff_selection = Some(diff_selection(DiffSide::SbsRight));

    find_widget::begin_with_selection(&mut app);

    assert_eq!(app.find_widget.target, Some(FindTarget::DiffSbsRight));
}

#[test]
fn no_selection_defaults_to_sbs_right() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_paired_diff(&mut app);
    app.diff_selection = None;

    find_widget::begin_with_selection(&mut app);

    assert_eq!(app.find_widget.target, Some(FindTarget::DiffSbsRight));
}

#[test]
fn unified_layout_targets_diff_unified() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    app.diff_layout = DiffLayout::Unified;
    install_paired_diff(&mut app);

    find_widget::begin_with_selection(&mut app);

    assert_eq!(app.find_widget.target, Some(FindTarget::DiffUnified));
}

#[test]
fn left_search_finds_old_text_not_new_text() {
    // Targeted at SbsLeft, the query "old" should match the Removed
    // line; "new" (only present on the Added side) should produce
    // zero matches.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_paired_diff(&mut app);
    app.diff_selection = Some(diff_selection(DiffSide::SbsLeft));

    find_widget::begin_with_selection(&mut app);
    // No selection text to seed with — type manually via handle_key.
    // Simpler: poke `app.find_widget.query` directly and recompute via
    // the public re-match entry point.
    app.find_widget.query = "old".to_string();
    app.find_widget.cursor = 3;
    find_widget::recompute(&mut app);
    assert!(
        !app.find_widget.matches.is_empty(),
        "'old' is on the left side and should match"
    );

    app.find_widget.query = "new".to_string();
    app.find_widget.cursor = 3;
    find_widget::recompute(&mut app);
    assert!(
        app.find_widget.matches.is_empty(),
        "'new' lives only on the right side; SbsLeft target shouldn't see it"
    );
}

#[test]
fn right_search_finds_new_text_not_old_text() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_paired_diff(&mut app);
    app.diff_selection = Some(diff_selection(DiffSide::SbsRight));

    find_widget::begin_with_selection(&mut app);
    app.find_widget.query = "new".to_string();
    app.find_widget.cursor = 3;
    find_widget::recompute(&mut app);
    assert!(!app.find_widget.matches.is_empty());

    app.find_widget.query = "old".to_string();
    app.find_widget.cursor = 3;
    find_widget::recompute(&mut app);
    assert!(app.find_widget.matches.is_empty());
}
