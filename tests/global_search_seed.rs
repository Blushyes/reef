//! `global_search::begin` selection-seed contract: opening the Space+F
//! palette while text is selected pre-fills the query with the first
//! non-empty trimmed line of the selection (VSCode "Find with
//! Selection"). Without a selection, the existing `query` survives so
//! Esc-peek-and-return is non-destructive.

use reef::app::App;
use reef::global_search;
use reef::ui::selection::PreviewSelection;
use reef::ui::theme::Theme;
use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent, TextPreview};
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::TempDir;
use test_support::CwdGuard;

// `App::new` cd's into the workdir + reads $HOME prefs; the rest of the
// suite uses the same lock to keep parallel tests from racing on those
// process-globals.
static CWD_LOCK: Mutex<()> = Mutex::new(());

fn fresh_app() -> (App, TempDir, CwdGuard) {
    let tmp = TempDir::new().unwrap();
    let g = CwdGuard::enter(tmp.path());
    let app = App::new(Theme::dark(), None);
    (app, tmp, g)
}

fn install_text_preview(app: &mut App, lines: &[&str]) {
    app.preview_content = Some(PreviewContent {
        path: "scratch.txt".to_string(),
        body: PreviewBody::Text(TextPreview {
            lines: lines.iter().map(|s| s.to_string()).collect(),
            highlighted: None,
            parsed: None,
        }),
    });
}

fn install_markdown_preview(app: &mut App, source: &str) {
    let markdown = reef_core::markdown::build_markdown_preview("README.md", source, true)
        .expect("markdown preview");
    app.preview_content = Some(PreviewContent {
        path: "README.md".to_string(),
        body: PreviewBody::Markdown(markdown),
    });
}

fn select_byte_range(app: &mut App, start: (usize, usize), end: (usize, usize)) {
    let mut sel = PreviewSelection::new(start);
    sel.active = end;
    sel.dragging = false;
    app.preview_selection = Some(sel);
}

#[test]
fn begin_without_selection_preserves_existing_query() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    app.global_search.core.filter = "previous".to_string();
    app.global_search.core.cursor = "previous".len();

    global_search::begin(&mut app);

    assert!(app.global_search.core.active);
    assert_eq!(app.global_search.core.filter, "previous");
    assert_eq!(app.global_search.core.cursor, "previous".len());
}

#[test]
fn begin_seeds_query_from_preview_selection() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(&mut app, &["fn foo() {", "    bar();", "}"]);
    // Select "    bar();" — the helper must trim the leading indent.
    select_byte_range(&mut app, (1, 0), (1, "    bar();".len()));

    global_search::begin(&mut app);

    assert_eq!(app.global_search.core.filter, "bar();");
    assert_eq!(app.global_search.core.cursor, "bar();".len());
    assert!(app.global_search.core.active);
}

#[test]
fn begin_seeds_first_nonempty_line_when_selection_spans_multiple_rows() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_text_preview(&mut app, &["", "  hello", "world"]);
    // Selection: blank row 0 fully, all of row 1, all of row 2.
    select_byte_range(&mut app, (0, 0), (2, "world".len()));

    global_search::begin(&mut app);

    assert_eq!(
        app.global_search.core.filter, "hello",
        "leading blank row must be skipped, indent must be trimmed",
    );
}

#[test]
fn begin_with_seed_equal_to_existing_query_keeps_excluded_set() {
    // The user has a search active with a per-match opt-out; they then
    // re-trigger Space+F with the same text selected. Re-arming the
    // search would clear `excluded` and re-show the deselected hit.
    // Guard: `begin()` skips `mark_query_edited` when the seed matches.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    app.global_search.core.filter = "bar".to_string();
    app.global_search
        .excluded
        .insert((PathBuf::from("a.rs"), 1));
    install_text_preview(&mut app, &["bar"]);
    select_byte_range(&mut app, (0, 0), (0, 3));

    global_search::begin(&mut app);

    assert_eq!(app.global_search.core.filter, "bar");
    assert!(
        !app.global_search.excluded.is_empty(),
        "no-op seed must not wipe per-match opt-outs",
    );
}

#[test]
fn begin_with_empty_selection_keeps_existing_query() {
    // A `PreviewSelection` exists but is collapsed (anchor == active).
    // That's the "click but don't drag" state — there's no text to
    // seed with, so the existing query stays.
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    app.global_search.core.filter = "kept".to_string();
    install_text_preview(&mut app, &["foo bar"]);
    select_byte_range(&mut app, (0, 2), (0, 2));

    global_search::begin(&mut app);

    assert_eq!(app.global_search.core.filter, "kept");
}

#[test]
fn begin_seeds_from_markdown_rendered_rows() {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _tmp, _g) = fresh_app();
    install_markdown_preview(&mut app, "| Name | Count |\n|---|---:|\n| reef | 12 |\n");
    let rendered = match &app.preview_content.as_ref().unwrap().body {
        PreviewBody::Markdown(markdown) => markdown.text_rows[1].clone(),
        _ => panic!("markdown preview expected"),
    };
    assert_eq!(rendered, "┃ Name ┃ Count ┃");
    select_byte_range(&mut app, (1, 0), (1, rendered.len()));

    global_search::begin(&mut app);

    assert_eq!(app.global_search.core.filter, rendered);
}
