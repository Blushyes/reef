//! VSCode-style "Find" widget for the file-preview and diff panels.
//!
//! Independent of the vim-style `/` (`crate::search`). Opened via `Space+F`,
//! anchored to the upper-right of the active content panel, supports
//! side-by-side diff (targets one half based on selection side), and
//! exposes Match Case / Whole Word / Regex toggles.
//!
//! Mutual exclusion: opening the widget force-clears vim search so there's
//! exactly one source of match highlights at any time. Opening `/` is
//! expected to call `find_widget::close` to do the reverse.

use crate::TuiApp as App;
use crate::input;
use crate::keymap::{Command, InputScope, Keymap};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use reef_app::{AppCommand, FindTarget, FindWidgetToggle, SearchViewport};
use reef_core::diff::{DiffRowText, DiffSide};
use std::time::Instant;

// ─── Public API ──────────────────────────────────────────────────────────────

/// Open the widget on the active content panel, seeding the query from
/// the current text selection (file-preview drag-select or diff
/// drag-select). VSCode `Cmd+F` semantics: prompt stays active, cursor
/// at query end, matches highlighted, scroll jumped to first hit.
///
/// No-op when the active panel has no in-panel find target (e.g. focus
/// is on the Search tab's left list).
pub fn begin_with_selection(app: &mut App) {
    use reef_app::{AppPanel as Panel, AppTab as Tab};
    // VSCode-style "find in this file" — if focus is currently on a
    // list panel (file tree, git status, commit graph) that has no
    // searchable preview target, force-focus onto the content column
    // so Space+F lands on the diff/preview the user sees, mirroring
    // the behaviour the removed Ctrl+F binding used to give.
    // Without this, pressing Space+F at app startup (default focus is
    // `Panel::Files`) silently no-ops because resolve_target returns
    // None for `(Files, Files)` / `(Git, Files)` etc.
    //
    // Exception: `(Tab::Search, Panel::Files)` is the search input
    // row, not a list — pulling focus away from it would lose the
    // user's half-typed query. Leave it alone; the user can Tab into
    // the preview panel themselves if they want find-in-file there.
    let on_search_input =
        app.engine.active_tab() == Tab::Search && app.engine.active_panel() == Panel::Files;
    if !on_search_input && resolve_target(app).is_none() && app.engine.active_panel() != Panel::Diff
    {
        app.set_active_panel(Panel::Diff);
    }
    let Some(target) = resolve_target(app) else {
        return;
    };
    let query = crate::global_search::current_text_selection(app).unwrap_or_default();
    app.engine
        .dispatch(AppCommand::BeginFindWidget { target, query });
    app.drain_engine_runtime_events();
}

/// Close the widget, restoring pre-find scroll. Idempotent — safe to call
/// when widget is already inactive.
pub fn close(app: &mut App) {
    app.engine.dispatch(AppCommand::CloseFindWidget {
        dark: app.theme.is_dark,
    });
    app.find_widget_ui = Default::default();
}

/// Step to next (`reverse=false`) or previous (`reverse=true`) match.
/// `Space+G` / `Space+Shift+G` route here. Works regardless of whether
/// the widget is currently `active` — as long as `matches` is non-empty
/// and the target is set, navigation is meaningful.
pub fn step(app: &mut App, reverse: bool) {
    app.engine.dispatch(AppCommand::StepFindWidget {
        reverse,
        viewport: viewport(app),
    });
}

/// Owned-input dispatch while the find widget is active. Mirrors the
/// pattern in `search::handle_key_in_search_mode` — every key landing
/// here either edits one of the inputs, navigates matches, toggles an
/// option, or closes the widget.
pub fn handle_key(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // In-widget leader chord: a second `Space+F` toggles the widget off
    // when the query is empty (palette-style isolation).
    let allow_arm = app.engine.find_widget().query.is_empty();
    match input::leader_decision(
        &key,
        allow_arm,
        app.find_widget_ui.space_leader_at,
        Instant::now(),
        input::LEADER_TIMEOUT,
    ) {
        input::LeaderVerdict::Arm => {
            app.find_widget_ui.space_leader_at = Some(Instant::now());
            return;
        }
        input::LeaderVerdict::Fire => {
            // `leader_decision` Fires on any chord target (p/f/h/v). The
            // widget's identity is Space+F — only that closes. For
            // other chord letters we used to `return` here, which
            // silently dropped the user's keystroke (`v` after Space
            // typed into an empty query just vanished). Treat them as
            // Consume instead so the literal char falls through to the
            // input-dispatch arm and appends to the query buffer.
            app.find_widget_ui.space_leader_at = None;
            if key.code == KeyCode::Char('f') && !ctrl && !alt {
                close(app);
                return;
            }
            // Fall through — non-F chord lands in the input handler.
        }
        input::LeaderVerdict::Consume => {
            app.find_widget_ui.space_leader_at = None;
        }
        input::LeaderVerdict::None => {}
    }

    match Keymap::resolve(InputScope::FindWidget, &key) {
        Some(Command::Close) | Some(Command::Quit) => {
            close(app);
            return;
        }
        Some(Command::Confirm) => {
            // Enter = next match (Shift+Enter = previous), VSCode-style.
            if !app.engine.find_widget().matches.is_empty() {
                step(app, /*reverse=*/ shift);
            }
            return;
        }
        Some(Command::MoveDown) => {
            step(app, /*reverse=*/ false);
            return;
        }
        Some(Command::MoveUp) => {
            step(app, /*reverse=*/ true);
            return;
        }
        // Alt+C / Alt+W / Alt+R are the three toggles. Must match
        // before `input_edit::dispatch_key`, which binds Alt+B / Alt+F /
        // Alt+D and would otherwise eat them as word-motion / delete.
        Some(Command::ToggleCase) => {
            toggle_option(app, FindWidgetToggle::MatchCase);
            return;
        }
        Some(Command::ToggleWholeWord) => {
            toggle_option(app, FindWidgetToggle::WholeWord);
            return;
        }
        Some(Command::ToggleRegex) => {
            toggle_option(app, FindWidgetToggle::Regex);
            return;
        }
        _ => {}
    }

    if let Some(op) = crate::input_edit::op_for_key(&key) {
        app.engine.dispatch(AppCommand::EditFindWidgetInput(op));
        app.drain_engine_runtime_events();
    }
}

/// Bracketed-paste arrival while the find widget is active. Same shape
/// as `search::handle_paste` / `quick_open::handle_paste`: fold the
/// payload in as typed chars (dropping CR/LF — the widget query is
/// single-line) and re-derive matches when at least one char actually
/// landed. Called from `input::handle_paste` after the drop-path parser
/// has declined the payload.
pub fn handle_paste(s: &str, app: &mut App) {
    app.engine
        .dispatch(AppCommand::PasteFindWidgetInput(s.to_string()));
    app.drain_engine_runtime_events();
}

pub fn toggle_option(app: &mut App, option: FindWidgetToggle) {
    if app.engine.find_widget().active {
        app.engine
            .dispatch(AppCommand::ToggleFindWidgetOption(option));
        app.drain_engine_runtime_events();
    }
}

// ─── Internals ───────────────────────────────────────────────────────────────

fn resolve_target(app: &App) -> Option<FindTarget> {
    use reef_app::{AppPanel as Panel, AppTab as Tab};

    match (app.engine.active_tab(), app.engine.active_panel()) {
        (Tab::Files, Panel::Diff) | (Tab::Search, Panel::Diff) => Some(FindTarget::FilePreview),
        (Tab::Git, Panel::Diff) => Some(reef_app::diff_target_from_layout(
            app.engine.diff_layout(),
            app.diff_selection.map(|s| s.side),
            /*graph=*/ false,
        )),
        (Tab::Graph, Panel::Diff) => {
            if app.graph_uses_three_col() {
                Some(reef_app::diff_target_from_layout(
                    app.engine.diff_layout(),
                    app.diff_selection.map(|s| s.side),
                    /*graph=*/ true,
                ))
            } else {
                // 2-col Graph view inlines the diff into the Commit
                // detail panel; no in-panel widget target there yet.
                None
            }
        }
        _ => None,
    }
}

fn collect_rows(app: &App, target: FindTarget) -> Vec<String> {
    match target {
        FindTarget::FilePreview => match app.engine.preview_content_ref().map(|p| &p.body) {
            Some(body) => body
                .display_text_rows()
                .into_iter()
                .map(|row| row.into_owned())
                .collect(),
            _ => Vec::new(),
        },
        FindTarget::DiffUnified => match app.engine.diff_content() {
            Some(d) => row_texts_to_strings(&d.display.unified_row_texts),
            None => Vec::new(),
        },
        FindTarget::GraphDiffUnified => match app.engine.commit_detail().file_diff.as_ref() {
            Some(d) => row_texts_to_strings(&d.display.unified_row_texts),
            None => Vec::new(),
        },
        FindTarget::DiffSbsLeft => match app.engine.diff_content() {
            Some(d) => sbs_side_strings(&d.display.sbs_row_texts, DiffSide::SbsLeft),
            None => Vec::new(),
        },
        FindTarget::DiffSbsRight => match app.engine.diff_content() {
            Some(d) => sbs_side_strings(&d.display.sbs_row_texts, DiffSide::SbsRight),
            None => Vec::new(),
        },
        FindTarget::GraphDiffSbsLeft => match app.engine.commit_detail().file_diff.as_ref() {
            Some(d) => sbs_side_strings(&d.display.sbs_row_texts, DiffSide::SbsLeft),
            None => Vec::new(),
        },
        FindTarget::GraphDiffSbsRight => match app.engine.commit_detail().file_diff.as_ref() {
            Some(d) => sbs_side_strings(&d.display.sbs_row_texts, DiffSide::SbsRight),
            None => Vec::new(),
        },
    }
}

fn row_texts_to_strings(rows: &[DiffRowText]) -> Vec<String> {
    rows.iter()
        .map(|r| match r {
            DiffRowText::Separator => String::new(),
            DiffRowText::Header(s) => s.to_string(),
            DiffRowText::Unified(s) => s.to_string(),
            DiffRowText::Sbs { .. } => String::new(),
        })
        .collect()
}

fn sbs_side_strings(rows: &[DiffRowText], side: DiffSide) -> Vec<String> {
    rows.iter()
        .map(|r| match r {
            DiffRowText::Separator => String::new(),
            DiffRowText::Header(h) => h.to_string(),
            DiffRowText::Unified(s) => s.to_string(),
            DiffRowText::Sbs { left, right } => match side {
                DiffSide::SbsLeft => left.to_string(),
                DiffSide::SbsRight => right.to_string(),
                DiffSide::Unified => String::new(),
            },
        })
        .collect()
}

/// Recompute matches for the current query + toggles. Called from:
///   - `handle_key` after every keystroke that may have edited the query
///   - `App::handle_action` when a mouse-driven toggle flips
///   - Integration tests that poke `query` directly and need the match
///     set rebuilt to reflect the change
pub fn recompute(app: &mut App) {
    let Some(target) = app.engine.find_widget().target else {
        return;
    };
    let rows = collect_rows(app, target);
    app.engine.dispatch(AppCommand::RecomputeFindWidget {
        rows,
        dark: app.theme.is_dark,
        viewport: viewport(app),
    });
}

fn viewport(app: &App) -> SearchViewport {
    SearchViewport {
        preview_view_h: app.layout.last_preview_view_h as usize,
        diff_view_h: app.layout.last_diff_view_h as usize,
        commit_detail_view_h: app.layout.last_commit_detail_view_h as usize,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use reef_core::search::{SearchOptions, find_all_with_options};

    fn opts(case_sensitive: bool, whole_word: bool, regex: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive,
            whole_word,
            regex,
        }
    }

    #[test]
    fn regex_case_sensitivity_obeys_toggle() {
        let r_i = find_all_with_options("Foo foo FOO", r"foo", &opts(false, false, true)).unwrap();
        let r_s = find_all_with_options("Foo foo FOO", r"foo", &opts(true, false, true)).unwrap();
        assert_eq!(r_i, vec![0..3, 4..7, 8..11]);
        assert_eq!(r_s, vec![4..7]);
    }

    #[test]
    fn empty_needle_returns_no_matches() {
        let r = find_all_with_options("anything", "", &opts(false, false, false)).unwrap();
        assert!(r.is_empty());
        let r = find_all_with_options("anything", "", &opts(false, false, true)).unwrap();
        assert!(r.is_empty());
    }
}
