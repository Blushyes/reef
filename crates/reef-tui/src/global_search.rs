//! VSCode-style global content search palette (bound to Space-F): ripgrep
//! every file in the workdir (honouring `.gitignore`) for a literal
//! smart-case substring, stream results into a list, and jump to the
//! matching line on Enter.
//!
//! The state machine mirrors `quick_open`'s "active prompt owns input"
//! pattern, but the backing work is heavier so it runs in the task worker
//! thread (`tasks::search_all`) and streams hits back in 50-per chunk. New
//! keystrokes bump `generation` and flip `cancel` so any in-flight worker
//! aborts within the next few files.
//!
//! Post-jump highlight: `accept()` stashes a `PreviewHighlight` on `App`
//! (path + row + byte range) that `ui::preview` overlays once the
//! async preview arrives. See `app::PreviewHighlight` + the tick handler.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::ops::Range;
use std::time::Instant;

use crate::TuiApp as App;
use crate::input::DOUBLE_CLICK_WINDOW;
use crate::keymap::{Command, InputScope, Keymap};
use crate::ui::mouse::ClickAction;
use reef_app::AppCommand;
use reef_app::GLOBAL_SEARCH_MAX_LINE_CHARS as MAX_LINE_CHARS;
#[cfg(test)]
use reef_app::{GlobalSearchState, MatchHit, SearchPanelFocus};

// ─── Entry points ────────────────────────────────────────────────────────────

/// Open the palette. If the active tab has a non-empty text selection
/// (file-preview drag-select on Tab::Files, diff drag-select on Tab::Git
/// /Tab::Graph), seed the query with it — VSCode's "Find with Selection"
/// shortcut. Otherwise keeps the existing `query`/`results` so Esc-peek-
/// and-return doesn't lose state.
pub fn begin(app: &mut App) {
    app.global_search_leader_at = None;
    app.engine.dispatch(AppCommand::OpenGlobalSearch {
        seed: current_text_selection(app),
    });
}

/// Snapshot the user's current text selection as a one-line search seed.
///
/// Sources, in priority order:
/// 1. File preview selection (`preview_selection` × `preview_content`'s
///    text body) — covers Tab::Files and the Tab::Search preview pane.
/// 2. Diff selection (`diff_selection` × `last_diff_hit`) — covers
///    Tab::Git and the Tab::Graph 3-col diff column.
///
/// Multi-line selections collapse to the first non-empty line, then
/// trim leading/trailing whitespace: the global-search prompt is
/// single-line and feeding it raw indentation produces zero matches
/// for anything but the first line of a function.
///
/// Shared with `search::begin_with_selection` so both entry points
/// (`Space+F` in-panel find, `Space+Shift+F` global search) interpret
/// the selection identically.
pub(crate) fn current_text_selection(app: &App) -> Option<String> {
    if let (Some(sel), Some(preview)) = (
        app.preview_selection.as_ref(),
        app.engine.preview_content_ref(),
    ) && !sel.is_empty()
        && preview.is_text()
    {
        let rows = preview.body.display_text_rows();
        let text = crate::ui::selection::collect_selected_text_from_rows(
            rows.iter().map(|row| row.as_ref()),
            rows.len(),
            sel,
        );
        if let Some(line) = first_nonempty_trimmed_line(&text) {
            return Some(line);
        }
    }
    if let (Some(sel), Some(hit)) = (app.diff_selection.as_ref(), app.last_diff_hit.as_ref())
        && !sel.sel.is_empty()
    {
        let text = crate::ui::selection::collect_diff_selected_text(hit, sel);
        if let Some(line) = first_nonempty_trimmed_line(&text) {
            return Some(line);
        }
    }
    None
}

pub(crate) fn first_nonempty_trimmed_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Bracketed-paste arrival while the Space+F overlay is active. Strips
/// newlines and re-runs the search. Called from `input::handle_paste`
/// after the drop-path parser has declined the payload.
pub fn handle_paste_overlay(s: &str, app: &mut App) {
    app.engine.dispatch(AppCommand::PasteGlobalSearchOverlay {
        text: s.to_string(),
        now: Instant::now(),
    });
}

/// Bracketed-paste arrival while a Tab::Search input row is focused.
/// Routes to `query` (FindInput) or `replace_text` (ReplaceInput) based
/// on `focus`; List focus is a no-op (no text input to receive the
/// payload).
pub fn handle_paste_search_tab(s: &str, app: &mut App) {
    app.engine.dispatch(AppCommand::PasteGlobalSearchTab {
        text: s.to_string(),
        now: Instant::now(),
    });
}

/// Commit the selected hit: close the palette, switch to the Files tab,
/// reveal the path, and stash a `PreviewHighlight` so the file preview
/// panel highlights the matching row once it loads async.
pub fn accept(app: &mut App) {
    let Some(hit) = app.engine.selected_global_search_hit() else {
        app.engine.dispatch(AppCommand::CloseGlobalSearch);
        return;
    };

    app.push_location_before_jump();
    app.engine.dispatch(AppCommand::AcceptGlobalSearchHit(hit));
    app.drain_engine_runtime_events();
}

/// Dispatch one key while the palette is active. The caller (input.rs)
/// guarantees exclusivity, as with quick_open.
pub fn handle_key(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Space-leader close: same state machine as the global chord, gated on
    // empty query so a literal space in the search string is allowed.
    match crate::input::leader_decision(
        &key,
        /* allow_arm */ app.engine.global_search_query_is_empty(),
        app.global_search_leader_at,
        Instant::now(),
        crate::input::LEADER_TIMEOUT,
    ) {
        crate::input::LeaderVerdict::Arm => {
            app.global_search_leader_at = Some(Instant::now());
            return;
        }
        crate::input::LeaderVerdict::Fire => {
            // `leader_decision` Fires on any chord target (p/f/h/v).
            // Only Space+Shift+F is OUR own toggle — anything else is
            // a stray Space + chord-letter inside an empty query, and
            // the right thing is to let the literal char fall through
            // to the input-append arm rather than silently swallow it.
            app.global_search_leader_at = None;
            if key.code == KeyCode::Char('F') && !ctrl && !alt {
                app.engine.dispatch(AppCommand::CloseGlobalSearch);
                return;
            }
            // Fall through — non-Shift-F chord lands in the input handler.
        }
        crate::input::LeaderVerdict::Consume => {
            app.global_search_leader_at = None;
            // Fall through — current key still runs below.
        }
        crate::input::LeaderVerdict::None => {}
    }

    // Palette-specific shortcuts that must precede PickerCore:
    // - Alt/Ctrl+Enter pins to Tab::Search (otherwise PickerCore would
    //   treat it as a plain Confirm)
    // - PageUp/PageDown depend on the rendered list height which PickerCore
    //   doesn't see
    let mapped = Keymap::resolve(InputScope::GlobalSearch, &key);
    match mapped {
        Some(Command::PinGlobalSearch) => {
            app.push_location_before_jump();
            app.engine.dispatch(AppCommand::PinGlobalSearchToTab);
            app.drain_engine_runtime_events();
            navigate_to_selected(app);
            return;
        }
        Some(Command::PageUp | Command::PageDown) => {
            let step = app.layout.global_search_last_view_h.max(1) as i32;
            let signed = if mapped == Some(Command::PageUp) {
                -step
            } else {
                step
            };
            app.engine.dispatch(AppCommand::MoveGlobalSearchSelection {
                delta: signed,
                visible_rows: app.layout.global_search_last_view_h as usize,
            });
            return;
        }
        _ => {}
    }

    let input = crate::picker_core::input_for_key(InputScope::GlobalSearch, &key);
    app.engine
        .dispatch(AppCommand::ApplyGlobalSearchPickerInput {
            input,
            now: Instant::now(),
            visible_rows: app.layout.global_search_last_view_h as usize,
        });
    app.drain_engine_runtime_events();
}

/// Dispatch one mouse event while the palette is active.
pub fn handle_mouse(mouse: MouseEvent, app: &mut App) {
    let popup = match app.global_search_popup_area {
        Some(r) => r,
        None => return,
    };
    let inside = mouse.column >= popup.x
        && mouse.column < popup.x + popup.width
        && mouse.row >= popup.y
        && mouse.row < popup.y + popup.height;

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !inside {
                app.global_search_leader_at = None;
                app.engine.dispatch(AppCommand::CloseGlobalSearch);
                app.last_click = None;
                return;
            }

            let now = Instant::now();
            let is_double = matches!(
                app.last_click,
                Some((t, c, r))
                    if c == mouse.column
                        && r == mouse.row
                        && now.duration_since(t) < DOUBLE_CLICK_WINDOW
            );

            if let Some(ClickAction::GlobalSearchSelect(idx)) =
                app.hit_registry.hit_test(mouse.column, mouse.row)
            {
                app.engine.dispatch(AppCommand::SelectGlobalSearchResult {
                    idx,
                    visible_rows: app.layout.global_search_last_view_h as usize,
                });
                if is_double {
                    accept(app);
                    app.last_click = None;
                    return;
                }
            }

            app.last_click = if is_double {
                None
            } else {
                Some((now, mouse.column, mouse.row))
            };
        }
        MouseEventKind::ScrollUp if inside => {
            app.engine.dispatch(AppCommand::MoveGlobalSearchSelection {
                delta: -3,
                visible_rows: app.layout.global_search_last_view_h as usize,
            });
        }
        MouseEventKind::ScrollDown if inside => {
            app.engine.dispatch(AppCommand::MoveGlobalSearchSelection {
                delta: 3,
                visible_rows: app.layout.global_search_last_view_h as usize,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
fn move_selection(state: &mut GlobalSearchState, delta: i32) {
    reef_app::move_global_search_selection(state, delta);
}

/// Public wrapper around `move_selection` for call sites outside the
/// overlay's own handlers (the Search tab's mouse scroll path, the tab
/// panel's keyboard handler). Keeps the private helper small and internal.
///
/// Preview reload is DEBOUNCED — a rapid ↓↓↓↓↓ burst schedules one
/// reload at the end of the burst rather than firing one per keystroke.
/// Click handlers and chunk-arrival syncs skip the debounce and call
/// `navigate_to_selected` directly for immediate feedback.
pub fn move_selection_by(app: &mut App, delta: i32) {
    app.engine.dispatch(AppCommand::MoveGlobalSearchSelection {
        delta,
        visible_rows: app.layout.global_search_last_view_h as usize,
    });
    schedule_preview_sync(app);
}

/// Mark a preview sync as pending; `App::tick` will fire
/// `navigate_to_selected` once `PREVIEW_SYNC_DEBOUNCE` has elapsed. Each
/// call pushes the deadline forward, so the last nav in a burst wins.
pub fn schedule_preview_sync(app: &mut App) {
    app.engine
        .dispatch(AppCommand::ScheduleGlobalSearchPreviewSync {
            now: Instant::now(),
        });
}

/// Force the next tick to re-run the current query — used by the `r`
/// reload key. Resets `last_searched_query` so the equality check in
/// `maybe_kick_global_search` fails; stamps `last_keystroke_at` so the
/// debounce gate fires. Does nothing when the query is empty (there's
/// nothing to reload).
pub fn reload(app: &mut App) {
    if app.engine.global_search_query_is_empty() {
        return;
    }
    app.engine.dispatch(AppCommand::ReloadGlobalSearch {
        now: Instant::now(),
    });
}

/// Sync `preview_highlight` + kick off a preview load for the currently
/// selected result. Used by the Search tab's live-preview behaviour — every
/// selection change updates the right panel. No-op when the list is empty.
///
/// Unlike `accept()`, this does NOT change tabs or reveal the file in the
/// file tree — the user is still in the Search tab browsing results.
///
/// Supersedes any pending debounced sync (`preview_sync_at`): an immediate
/// nav is definitionally fresher than a scheduled one.
pub fn navigate_to_selected(app: &mut App) {
    app.engine
        .dispatch(AppCommand::SyncGlobalSearchPreviewToSelected);
}

// ─── Line-text truncation helpers ────────────────────────────────────────────

/// Truncate `text` to at most [`MAX_LINE_CHARS`] chars at a UTF-8 boundary,
/// returning the truncated copy along with the byte length we ended at.
/// Preserves the invariant that the returned string is valid UTF-8 even if
/// the input had a fragment at the limit.
pub fn truncate_line(text: &str) -> String {
    reef_core::search::truncate_line(text, MAX_LINE_CHARS)
}

/// Clip a byte range to a maximum end offset. If the range falls entirely
/// outside the visible slice, return `None` so the UI knows this particular
/// match is off-screen (we still surface the hit — just without the
/// highlight).
pub fn clip_range(range: Range<usize>, max_end: usize) -> Option<Range<usize>> {
    reef_core::search::clip_range(range, max_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn truncate_line_returns_input_when_short() {
        let s = "short line";
        assert_eq!(truncate_line(s), s);
    }

    #[test]
    fn truncate_line_caps_at_max_chars() {
        let s: String = "a".repeat(MAX_LINE_CHARS + 100);
        let out = truncate_line(&s);
        assert_eq!(out.chars().count(), MAX_LINE_CHARS);
    }

    #[test]
    fn truncate_line_respects_utf8_boundary() {
        // 200 ASCII + 60 CJK chars (each 3 bytes) → 260 chars total, 380
        // bytes. Truncation at 250 chars must land inside the CJK run at a
        // codepoint boundary, and the result must still be valid UTF-8.
        let mut s = "a".repeat(200);
        for _ in 0..60 {
            s.push('你');
        }
        let out = truncate_line(&s);
        assert_eq!(out.chars().count(), MAX_LINE_CHARS);
        // Implicit: `out` is a valid `String`, so UTF-8 invariant holds.
    }

    #[test]
    fn clip_range_in_bounds_is_identity() {
        assert_eq!(clip_range(3..8, 20), Some(3..8));
    }

    #[test]
    fn clip_range_clips_end() {
        assert_eq!(clip_range(3..40, 20), Some(3..20));
    }

    #[test]
    fn clip_range_fully_outside_returns_none() {
        assert_eq!(clip_range(50..60, 20), None);
    }

    #[test]
    fn move_selection_clamps_bounds() {
        let mut s = GlobalSearchState {
            results: vec![dummy_hit("a"), dummy_hit("b"), dummy_hit("c")],
            ..GlobalSearchState::default()
        };
        move_selection(&mut s, 10);
        assert_eq!(s.core.selected_idx, 2);
        move_selection(&mut s, -99);
        assert_eq!(s.core.selected_idx, 0);
    }

    #[test]
    fn focus_defaults_to_list() {
        // Tab::Search enters list mode by default — pressing digit 2 or
        // cycling via Tab lands on the results, not the input. Pinning
        // from the Space+F overlay is the only path that auto-focuses the
        // input, and that's handled in `handle_key` explicitly.
        let s = GlobalSearchState::default();
        assert_eq!(s.focus, SearchPanelFocus::List);
        assert!(!s.input_focused());
    }

    #[test]
    fn cycle_focus_skips_replace_when_closed() {
        let mut s = GlobalSearchState {
            focus: SearchPanelFocus::FindInput,
            ..GlobalSearchState::default()
        };
        s.cycle_focus_forward();
        // replace_open=false → FindInput jumps straight to List.
        assert_eq!(s.focus, SearchPanelFocus::List);
    }

    #[test]
    fn cycle_focus_visits_replace_when_open() {
        let mut s = GlobalSearchState {
            replace_open: true,
            focus: SearchPanelFocus::FindInput,
            ..GlobalSearchState::default()
        };
        s.cycle_focus_forward();
        assert_eq!(s.focus, SearchPanelFocus::ReplaceInput);
        s.cycle_focus_forward();
        assert_eq!(s.focus, SearchPanelFocus::List);
        s.cycle_focus_forward();
        assert_eq!(s.focus, SearchPanelFocus::FindInput);
    }

    #[test]
    fn toggle_match_excluded_round_trip() {
        let mut s = GlobalSearchState {
            results: vec![dummy_hit("a"), dummy_hit("b")],
            ..GlobalSearchState::default()
        };
        assert!(s.is_match_included(0));
        s.toggle_match_excluded(0);
        assert!(!s.is_match_included(0));
        assert!(s.is_match_included(1));
        s.toggle_match_excluded(0);
        assert!(s.is_match_included(0));
    }

    #[test]
    fn mark_query_edited_clears_excluded_set() {
        // Regression for the bug where editing the query (typing a new
        // letter, deleting one, clearing) silently kept the previous
        // search's per-match opt-outs around. A `(path, line)` key
        // shared by the old and new result sets would then be skipped
        // even though the user never opted out of the new one.
        let mut s = GlobalSearchState {
            results: vec![dummy_hit("a")],
            ..GlobalSearchState::default()
        };
        s.toggle_match_excluded(0);
        assert!(!s.excluded.is_empty());
        reef_app::mark_global_search_query_edited_at(&mut s, Instant::now());
        assert!(
            s.excluded.is_empty(),
            "edit-driven query change must reset per-match opt-outs"
        );
    }

    #[test]
    fn included_count_ignores_stale_exclusions() {
        // An entry in `excluded` that no longer corresponds to a current
        // hit (e.g. the user toggled it off, then ran a fresh query that
        // produced a different result set without that line) must not
        // double-count or drag the counter below zero.
        let mut s = GlobalSearchState {
            results: vec![dummy_hit("a"), dummy_hit("b")],
            ..GlobalSearchState::default()
        };
        s.excluded.insert((PathBuf::from("z-stale"), 99));
        assert_eq!(s.included_count(), 2);
        s.toggle_match_excluded(0);
        assert_eq!(s.included_count(), 1);
    }

    #[test]
    fn move_selection_on_empty_is_noop() {
        // `selected = 5` is stale from a prior query that had results; we
        // want move_selection to snap it back to 0 when there's nothing in
        // the list.
        let mut s = GlobalSearchState {
            core: reef_app::PickerState {
                selected_idx: 5,
                ..reef_app::PickerState::default()
            },
            ..GlobalSearchState::default()
        };
        move_selection(&mut s, 1);
        assert_eq!(s.core.selected_idx, 0);
    }

    fn dummy_hit(name: &str) -> MatchHit {
        MatchHit {
            path: PathBuf::from(name),
            display: name.to_string(),
            line: 0,
            line_text: String::new(),
            byte_range: 0..0,
        }
    }

    // ── selection-seed plumbing ──────────────────────────────────────────

    #[test]
    fn first_nonempty_trimmed_line_returns_none_for_blank_input() {
        assert_eq!(first_nonempty_trimmed_line(""), None);
        assert_eq!(first_nonempty_trimmed_line("   \n  \t\n"), None);
    }

    #[test]
    fn first_nonempty_trimmed_line_skips_leading_blanks() {
        assert_eq!(
            first_nonempty_trimmed_line("\n\n   first\nsecond"),
            Some("first".to_string()),
        );
    }

    #[test]
    fn first_nonempty_trimmed_line_strips_indentation_and_trailing_ws() {
        // The selection on a function body grabs the leading indent —
        // feeding "    fn foo()" verbatim into ripgrep would miss every
        // call site that isn't indented identically.
        assert_eq!(
            first_nonempty_trimmed_line("    fn foo()   "),
            Some("fn foo()".to_string()),
        );
    }
}
