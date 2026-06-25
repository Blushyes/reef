//! VSCode-style quick-open palette (bound to Space-P): fuzzy search every
//! file in the workdir (honouring `.gitignore`) and jump to it in the Files
//! tab preview.
//!
//! The state machine mirrors `search.rs`'s "active prompt owns input" pattern:
//! while `active` is true, `input::handle_key` delegates all key events here
//! and the overlay renders on top of the normal UI. Unlike `search.rs` this
//! never mutates backing panel scroll/selection until the user confirms with
//! Enter — so Esc cleanly drops back to exactly what was on screen.
//!
//! Index & MRU:
//! - Index is (re)built lazily when the palette opens. `reef-app` schedules
//!   the backend walk on a worker, then caches the UTF-32 encoding nucleo
//!   needs so typing stays hot on large repos.
//! - MRU is an ordered dedup of recently accepted paths, capped at 50 and
//!   persisted via the same flat-kv prefs file the rest of the app uses. We
//!   sanitise `\t` / `\n` in paths on write to keep the file parseable; those
//!   characters in real-world paths are exotic enough to be worth the edge.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

use crate::TuiApp as App;
use crate::input::DOUBLE_CLICK_WINDOW;
use crate::keymap::{Command, InputScope, Keymap};
use crate::prefs;
use crate::ui::mouse::ClickAction;
use reef_app::AppCommand;
use reef_app::QuickOpenState;
use reef_core::quick_open::{MRU_MAX, MRU_PREF_KEY};

/// Build state from persisted prefs (loads the MRU). Index stays empty
/// and stale — it'll be walked on the first `begin()` call so startup
/// time isn't spent on an index the user may never open.
pub fn from_prefs() -> QuickOpenState {
    QuickOpenState {
        mru: load_mru_from_prefs(),
        ..QuickOpenState::default()
    }
}

// ─── Entry points ────────────────────────────────────────────────────────────

/// Open the palette. Rebuilds the index if stale (first open, or fs-watcher
/// saw changes since the last build). Preserves `query` across close/reopen
/// so the user can Esc to peek at something and come back.
pub fn begin(app: &mut App) {
    app.quick_open_leader_at = None;
    app.engine.dispatch(AppCommand::OpenQuickOpen);
}

/// Commit the current selection: update MRU, close the palette, and jump
/// the Files tab to the chosen file with a fresh preview loaded.
pub fn accept(app: &mut App) {
    if !app.engine.quick_open_has_selection() {
        app.engine.dispatch(AppCommand::CloseQuickOpen);
        return;
    }
    app.push_location_before_jump();
    app.engine
        .dispatch(AppCommand::AcceptQuickOpenSelection { mru_cap: MRU_MAX });
    app.drain_engine_runtime_events();
}

/// Dispatch one key while the palette is active. The caller (input.rs)
/// guarantees exclusivity — no other handler sees these keys.
///
/// Binding map:
/// - `Esc`                                 close palette (keeps query for re-open)
/// - `Ctrl+C`                              close + quit app
/// - `Enter`                               accept selected
/// - `Backspace`                           delete char
/// - `Alt+Backspace` / `Ctrl+Backspace` / `Ctrl+W`  delete previous word
/// - `Ctrl+U`                              clear the whole query
/// - `Up` / `Ctrl+P` / `Ctrl+K`            previous candidate
/// - `Down` / `Ctrl+N` / `Ctrl+J`          next candidate
/// - `PageUp` / `PageDown`                 page by viewport height
/// - `Left` / `Right` / `Home` / `End`     edit-cursor movement
///
/// Historic note: an earlier revision made `Ctrl+P` close the palette
/// (toggle-on, toggle-off). That conflicted with users' expectation that
/// `Ctrl+P` navigates inside the palette (VSCode / readline / vim parity),
/// so `Ctrl+P` now only means "previous candidate" — Esc is the sole
/// keyboard close.
pub fn handle_key(key: KeyEvent, app: &mut App) {
    // Space-leader close: mirrors the global open chord. Only armed while
    // `query.is_empty()` so once the user starts typing a path that might
    // legitimately contain a space (or a `p`), the chord shuts off and
    // characters go straight into the query.
    match crate::input::leader_decision(
        &key,
        /* allow_arm */ app.engine.quick_open_query_is_empty(),
        app.quick_open_leader_at,
        Instant::now(),
        crate::input::LEADER_TIMEOUT,
    ) {
        crate::input::LeaderVerdict::Arm => {
            app.quick_open_leader_at = Some(Instant::now());
            return;
        }
        crate::input::LeaderVerdict::Fire => {
            // `leader_decision` Fires on *any* chord target (p/f/h/v).
            // But quick-open's chord identity is Space+P — only that
            // pair should toggle the palette closed. For other chord
            // letters (`v` from FocusedPreview, `h` from help, etc.)
            // the user pressed Space-then-letter while the palette
            // happened to be empty; we don't want to swallow it. Treat
            // those as Consume so the literal char still appends to the
            // query below.
            app.quick_open_leader_at = None;
            if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) {
                app.engine.dispatch(AppCommand::CloseQuickOpen);
                return;
            }
            // Fall through — non-P chord lands in the char-append arm.
        }
        crate::input::LeaderVerdict::Consume => {
            app.quick_open_leader_at = None;
            // Fall through — the current key still runs below.
        }
        crate::input::LeaderVerdict::None => {}
    }

    // PageUp/PageDown depend on the rendered viewport height
    // (`last_view_h`), which PickerCore doesn't know about — handle
    // them inline before delegating the rest to the shared core.
    let mapped = Keymap::resolve(InputScope::QuickOpen, &key);
    if matches!(mapped, Some(Command::PageUp | Command::PageDown)) {
        let step = app.layout.quick_open_last_view_h.max(1) as i32;
        let signed = if mapped == Some(Command::PageUp) {
            -step
        } else {
            step
        };
        app.engine.dispatch(AppCommand::MoveQuickOpenSelection {
            delta: signed,
            visible_rows: app.layout.quick_open_last_view_h as usize,
        });
        return;
    }

    let input = crate::picker_core::input_for_key(InputScope::QuickOpen, &key);
    app.engine.dispatch(AppCommand::ApplyQuickOpenPickerInput {
        input,
        visible_rows: app.layout.quick_open_last_view_h as usize,
    });
    app.drain_engine_runtime_events();
}

/// Bracketed-paste arrival while the palette is active. Stripping newlines
/// keeps a multi-line paste from breaking the single-line query model; CRs
/// from Windows clipboards get the same treatment. Tabs stay as literal
/// characters — users searching for odd filenames can type them on purpose
/// and we don't want to drop that signal.
///
/// Called from `input::handle_paste` after the drop-path parser has already
/// ruled out the payload as a file drop.
pub fn handle_paste(s: &str, app: &mut App) {
    app.engine
        .dispatch(AppCommand::PasteQuickOpen(s.to_string()));
}

/// Dispatch one mouse event while the palette is active. The caller
/// (input.rs) routes all events here instead of the normal mouse pipeline,
/// so the underlying panels can't receive clicks through the overlay.
///
/// Semantics:
/// - Left click outside the popup area → close palette
/// - Left click on a row → select that candidate
/// - Double left-click on a row → select + accept (open file)
/// - Scroll wheel inside popup → move selection (3 rows per tick)
/// - Everything else (drag, right-click, move) → ignored
pub fn handle_mouse(mouse: MouseEvent, app: &mut App) {
    let popup = match app.quick_open_popup_area {
        Some(r) => r,
        // No popup rendered yet — swallow the event without side-effects so
        // a spurious click during the first tick can't dismiss the palette.
        None => return,
    };
    let inside = mouse.column >= popup.x
        && mouse.column < popup.x + popup.width
        && mouse.row >= popup.y
        && mouse.row < popup.y + popup.height;

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !inside {
                // Click-away dismisses the palette while keeping the
                // pre-palette state intact.
                app.quick_open_leader_at = None;
                app.engine.dispatch(AppCommand::CloseQuickOpen);
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

            // hit_test walks the registry in reverse registration order, and
            // the palette registers its rows AFTER the underlying panels, so
            // the palette's zones always win on overlap — meaning a click on
            // the popup never leaks through to a TreeClick / GitCommand
            // behind it.
            if let Some(ClickAction::QuickOpenSelect(idx)) =
                app.hit_registry.hit_test(mouse.column, mouse.row)
            {
                app.engine.dispatch(AppCommand::SelectQuickOpenMatch {
                    idx,
                    visible_rows: app.layout.quick_open_last_view_h as usize,
                });
                if is_double {
                    accept(app);
                    app.last_click = None;
                    return;
                }
            }

            // Track click for the next frame's double-click check.
            app.last_click = if is_double {
                None
            } else {
                Some((now, mouse.column, mouse.row))
            };
        }
        MouseEventKind::ScrollUp if inside => {
            app.engine.dispatch(AppCommand::MoveQuickOpenSelection {
                delta: -3,
                visible_rows: app.layout.quick_open_last_view_h as usize,
            });
        }
        MouseEventKind::ScrollDown if inside => {
            app.engine.dispatch(AppCommand::MoveQuickOpenSelection {
                delta: 3,
                visible_rows: app.layout.quick_open_last_view_h as usize,
            });
        }
        _ => {}
    }
}

// ─── Filtering ───────────────────────────────────────────────────────────────

/// Recompute `matches` from the current `query` and `index`. When `query` is
/// empty we surface MRU first (alive paths only) and then the rest of the
/// index in alphabetical order, so an empty palette is still useful. When
/// `query` is non-empty we delegate to nucleo and sort by score desc, with
/// shorter paths as a tiebreaker (keeps basename hits above deep-path hits).
pub fn filter(state: &mut QuickOpenState) {
    reef_app::filter_quick_open(state);
}

/// Mark the index as stale. Called from `App::tick` when fs-watcher fires —
/// cheaper than re-walking the tree on every event, and if the user never
/// opens the palette we never pay the walk cost at all.
pub fn mark_stale(state: &mut QuickOpenState) {
    reef_app::mark_quick_open_stale(state);
}

// ─── MRU persistence ─────────────────────────────────────────────────────────

fn load_mru_from_prefs() -> VecDeque<PathBuf> {
    let Some(raw) = prefs::get(MRU_PREF_KEY) else {
        return VecDeque::new();
    };
    reef_core::quick_open::decode_mru(&raw)
}

// ─── Input helpers ───────────────────────────────────────────────────────────
//
// Text-editing primitives (insert/backspace/delete_word_backward/clear/
// move_cursor) live in `crate::input_edit` and are shared with
// `crate::global_search`.

#[cfg(test)]
fn move_selection(state: &mut QuickOpenState, delta: i32) {
    reef_app::move_quick_open_selection(state, delta);
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_state(paths: &[&str]) -> QuickOpenState {
        let index = reef_core::quick_open::build_candidates(paths.iter().map(|p| p.to_string()));
        QuickOpenState {
            index,
            index_stale: false,
            ..QuickOpenState::default()
        }
    }

    #[test]
    fn empty_query_lists_all_with_mru_first() {
        let mut s = mk_state(&["a/x.rs", "b/y.rs", "c/z.rs"]);
        s.mru.push_back(PathBuf::from("c/z.rs"));
        s.mru.push_back(PathBuf::from("a/x.rs"));
        filter(&mut s);
        assert_eq!(s.matches.len(), 3);
        // MRU order first, then the rest
        assert_eq!(s.index[s.matches[0].idx].display, "c/z.rs");
        assert_eq!(s.index[s.matches[1].idx].display, "a/x.rs");
        assert_eq!(s.index[s.matches[2].idx].display, "b/y.rs");
    }

    #[test]
    fn empty_query_skips_mru_entries_that_no_longer_exist() {
        let mut s = mk_state(&["a/x.rs", "b/y.rs"]);
        s.mru.push_back(PathBuf::from("ghost.rs"));
        s.mru.push_back(PathBuf::from("a/x.rs"));
        filter(&mut s);
        assert_eq!(s.matches.len(), 2);
        assert_eq!(s.index[s.matches[0].idx].display, "a/x.rs");
        assert_eq!(s.index[s.matches[1].idx].display, "b/y.rs");
    }

    #[test]
    fn subsequence_match_hits_camelcase() {
        let mut s = mk_state(&["src/ui/file_tree_panel.rs", "src/app.rs", "README.md"]);
        s.core.filter = "uiftp".to_string();
        s.core.cursor = s.core.filter.len();
        filter(&mut s);
        assert!(!s.matches.is_empty());
        // The file_tree_panel path must rank first — it's the only one
        // containing the subsequence.
        assert_eq!(
            s.index[s.matches[0].idx].display,
            "src/ui/file_tree_panel.rs"
        );
    }

    #[test]
    fn shorter_path_wins_on_score_tie() {
        let mut s = mk_state(&["deep/nested/foo.rs", "foo.rs"]);
        s.core.filter = "foo".to_string();
        s.core.cursor = s.core.filter.len();
        filter(&mut s);
        assert_eq!(s.index[s.matches[0].idx].display, "foo.rs");
    }

    #[test]
    fn non_match_is_excluded_when_query_nonempty() {
        let mut s = mk_state(&["alpha.rs", "beta.rs"]);
        s.core.filter = "zzz".to_string();
        s.core.cursor = s.core.filter.len();
        filter(&mut s);
        assert!(s.matches.is_empty());
    }

    #[test]
    fn move_selection_clamps() {
        let mut s = mk_state(&["a.rs", "b.rs", "c.rs"]);
        filter(&mut s);
        move_selection(&mut s, 10);
        assert_eq!(s.core.selected_idx, 2);
        move_selection(&mut s, -99);
        assert_eq!(s.core.selected_idx, 0);
    }
}
