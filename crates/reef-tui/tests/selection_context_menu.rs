use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use reef::TuiApp as App;
use reef::input;
use reef::selection_context_menu::{SelectionContextMenuItem, SelectionContextTarget};
use reef::ui;
use reef::ui::mouse::ClickAction;
use reef::ui::selection::{DiffHit, DiffSelection, PreviewSelection};
use reef::ui::theme::Theme;
use reef_app::{AppEffect, AppPanel as Panel, AppTab as Tab};
use reef_core::diff::{DiffLayout, DiffRowText, DiffSide};
use reef_core::preview::{PreviewBody, PreviewDocument, TextPreview};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use test_support::CwdGuard;

static CWD_LOCK: Mutex<()> = Mutex::new(());

fn fresh_app() -> (App, TempDir, CwdGuard) {
    let tmp = TempDir::new().unwrap();
    let guard = CwdGuard::enter(tmp.path());
    let mut app = App::new(Theme::dark(), None);
    app.engine.state.active_tab = Tab::Files;
    app.engine.state.active_panel = Panel::Diff;
    app.engine.state.preview_content = Some(
        PreviewDocument {
            path: "scratch.txt".to_owned(),
            resolved_path: None,
            local_path: None,
            bytes_on_disk: 16,
            mime: Some("text/plain".to_owned()),
            body: PreviewBody::Text(TextPreview {
                lines: vec!["alpha beta".to_owned(), "gamma".to_owned()],
                source: None,
                highlighted: None,
                parsed: None,
            }),
        }
        .into(),
    );
    app.last_preview_rect = Some(Rect::new(20, 2, 60, 20));
    (app, tmp, guard)
}

fn install_diff_hit(app: &mut App) {
    app.engine.state.active_tab = Tab::Git;
    app.last_preview_rect = None;
    app.last_diff_rect = Some(Rect::new(20, 2, 60, 20));
    app.last_diff_hit = Some(DiffHit {
        layout: DiffLayout::Unified,
        content_y: 2,
        content_x_unified: 20,
        content_x_left: 20,
        content_x_right: 50,
        right_start_x: 50,
        scroll: 0,
        h_scroll: 0,
        sbs_left_h_scroll: 0,
        sbs_right_h_scroll: 0,
        rows: Arc::new(vec![
            DiffRowText::Unified(Arc::from("alpha beta")),
            DiffRowText::Unified(Arc::from("gamma")),
        ]),
    });
}

fn right_click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Right),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn right_click_inside_text_preview_opens_context_menu() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    let terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

    input::handle_mouse(right_click(32, 8), &mut app, &terminal);

    assert_eq!(
        app.selection_context_menu.target(),
        Some(SelectionContextTarget::Preview)
    );
    assert_eq!(app.selection_context_menu.anchor(), (32, 8));
}

#[test]
fn right_click_inside_diff_opens_context_menu_for_hit_side() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    install_diff_hit(&mut app);
    let terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

    input::handle_mouse(right_click(32, 8), &mut app, &terminal);

    assert_eq!(
        app.selection_context_menu.target(),
        Some(SelectionContextTarget::Diff(DiffSide::Unified))
    );
}

#[test]
fn right_click_inside_side_by_side_diff_targets_clicked_half() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    install_diff_hit(&mut app);
    let hit = app.last_diff_hit.as_mut().expect("diff hit is installed");
    hit.layout = DiffLayout::SideBySide;
    hit.rows = Arc::new(vec![DiffRowText::Sbs {
        left: Arc::from("old"),
        right: Arc::from("new"),
    }]);
    let terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

    input::handle_mouse(right_click(60, 8), &mut app, &terminal);

    assert_eq!(
        app.selection_context_menu.target(),
        Some(SelectionContextTarget::Diff(DiffSide::SbsRight))
    );
}

#[test]
fn copy_item_emits_selected_preview_text() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    app.preview_selection = Some(PreviewSelection {
        anchor: (0, 0),
        active: (0, 5),
        dragging: false,
    });
    app.open_selection_context_menu(SelectionContextTarget::Preview, (32, 8));

    app.dispatch_selection_context_menu_item(SelectionContextMenuItem::Copy);

    let effects = app.engine.drain_effects();
    assert!(matches!(
        effects.as_slice(),
        [AppEffect::CopyToClipboard { text, .. }] if text == "alpha"
    ));
}

#[test]
fn copy_item_emits_selected_diff_text() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    install_diff_hit(&mut app);
    app.diff_selection = Some(DiffSelection {
        sel: PreviewSelection {
            anchor: (0, 0),
            active: (0, 5),
            dragging: false,
        },
        side: DiffSide::Unified,
    });
    app.open_selection_context_menu(SelectionContextTarget::Diff(DiffSide::Unified), (32, 8));

    app.dispatch_selection_context_menu_item(SelectionContextMenuItem::Copy);

    let effects = app.engine.drain_effects();
    assert!(matches!(
        effects.as_slice(),
        [AppEffect::CopyToClipboard { text, .. }] if text == "alpha"
    ));
}

#[test]
fn select_all_item_selects_every_preview_row() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    app.open_selection_context_menu(SelectionContextTarget::Preview, (32, 8));

    app.dispatch_selection_context_menu_item(SelectionContextMenuItem::SelectAll);

    let selection = app
        .preview_selection
        .expect("select all creates a selection");
    assert_eq!(
        (selection.anchor, selection.active, selection.dragging),
        ((0, 0), (1, 5), false)
    );
}

#[test]
fn select_all_item_selects_every_diff_row() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    install_diff_hit(&mut app);
    app.open_selection_context_menu(SelectionContextTarget::Diff(DiffSide::Unified), (32, 8));

    app.dispatch_selection_context_menu_item(SelectionContextMenuItem::SelectAll);

    let selection = app.diff_selection.expect("select all creates a selection");
    assert_eq!(
        (
            selection.sel.anchor,
            selection.sel.active,
            selection.sel.dragging,
            selection.side,
        ),
        ((0, 0), (1, 5), false, DiffSide::Unified)
    );
}

#[test]
fn rendered_menu_registers_copy_and_select_all_rows() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _tmp, _guard) = fresh_app();
    app.open_selection_context_menu(SelectionContextTarget::Preview, (32, 8));
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::render(frame, &mut app)).unwrap();

    let mut copy = false;
    let mut select_all = false;
    for row in 0..24 {
        for column in 0..100 {
            match app.hit_registry.hit_test(column, row) {
                Some(ClickAction::SelectionContextMenuItem(SelectionContextMenuItem::Copy)) => {
                    copy = true;
                }
                Some(ClickAction::SelectionContextMenuItem(
                    SelectionContextMenuItem::SelectAll,
                )) => {
                    select_all = true;
                }
                _ => {}
            }
        }
    }
    assert!(copy && select_all);
}
