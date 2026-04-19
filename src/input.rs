//! Keyboard + mouse dispatch. `main.rs` delegates everything below the
//! event-drain loop to `handle_key` and `handle_mouse` here, so the binary
//! entry point stays focused on terminal bootstrap.
//!
//! The one exception is the `v` (select mode toggle) and `show_help`
//! dismiss — those stay inline in `main.rs` because the first needs
//! `execute!(terminal.backend_mut(), ...)` to flip crossterm's mouse
//! capture mode, and both are simple enough that splitting them out would
//! just add indirection.

use crate::app::{App, Panel, Tab};
use crate::global_search;
use crate::quick_open;
use crate::search;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use std::time::{Duration, Instant};

pub const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// How long a primed Space leader stays valid before being discarded. Long
/// enough for a deliberate "Space, p" chord on a keyboard, short enough
/// that a forgotten leader doesn't steal the next unrelated keypress.
pub const LEADER_TIMEOUT: Duration = Duration::from_millis(800);

/// One-keystroke verdict for the Space-leader chord.
///
/// The chord lives in two places (global toggle and palette-side close), so
/// the decision is pulled out of both call sites into a pure function that
/// both drive off the same state machine.
#[derive(Debug, PartialEq, Eq)]
pub enum LeaderVerdict {
    /// Arm the leader now — caller writes `Some(Instant::now())` into its
    /// state slot and returns without further dispatch.
    Arm,
    /// Fire the chord — caller triggers the target action (open / close)
    /// and clears the leader slot.
    Fire,
    /// Leader was armed but this keystroke isn't the chord target. Caller
    /// clears the leader slot and falls through to normal key dispatch so
    /// the key still does whatever it would have done without the leader.
    Consume,
    /// No leader interaction; dispatch the key normally.
    None,
}

/// Pure leader-decision state machine. Returns what the caller should do
/// about `key` given whether arming is permitted in this context
/// (`allow_arm`) and whether a leader is already pending (`leader_at`).
///
/// Rules in one paragraph: when nothing is armed, a bare Space with
/// `allow_arm` asks to arm. When a leader is armed and `key` is a bare `p`
/// or `P` within `timeout`, fire. When a leader is armed and the user
/// presses Space again with `allow_arm`, re-arm (double-Space is more
/// usefully "reset the chord" than "lose it"). Any other key with a primed
/// leader consumes the leader — the user changed their mind.
pub fn leader_decision(
    key: &KeyEvent,
    allow_arm: bool,
    leader_at: Option<Instant>,
    now: Instant,
    timeout: Duration,
) -> LeaderVerdict {
    let is_bare_space = key.code == KeyCode::Char(' ') && key.modifiers.is_empty();
    // Any chord target character fires the pending leader. The caller
    // inspects `key.code` at Fire time to decide which palette to open,
    // so the verdict itself stays a unit variant.
    let is_chord_target = matches!(
        key.code,
        KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Char('f') | KeyCode::Char('F')
    ) && key.modifiers.is_empty();

    // Fresh-arm path: no leader pending, space pressed, context allows.
    if allow_arm && leader_at.is_none() && is_bare_space {
        return LeaderVerdict::Arm;
    }

    // A leader is pending — resolve it.
    if let Some(t) = leader_at {
        let within = now.duration_since(t) < timeout;
        if within && is_chord_target {
            return LeaderVerdict::Fire;
        }
        // Re-arm on a second Space so the user can stutter their leader.
        if allow_arm && is_bare_space {
            return LeaderVerdict::Arm;
        }
        return LeaderVerdict::Consume;
    }

    LeaderVerdict::None
}

// ─── Keyboard ────────────────────────────────────────────────────────────────

pub fn handle_key(key: KeyEvent, app: &mut App) {
    // Global-search palette — fully owns input while active.
    if app.global_search.active {
        global_search::handle_key(key, app);
        return;
    }

    // Quick-open palette has the next-highest priority — while active it
    // fully owns input (character append, cursor, Enter/Esc, Space-P close).
    if app.quick_open.active {
        quick_open::handle_key(key, app);
        return;
    }

    // Search mode has priority over everything else — the prompt fully owns
    // input (character append, Backspace, Enter/Esc) while active.
    if app.search.active {
        search::handle_key_in_search_mode(key, app);
        return;
    }

    // Place mode (drag-and-drop destination picker) is mouse-first —
    // most keystrokes are ignored so a stray keypress can't accidentally
    // commit a copy. Exceptions: Esc cancels the mode, and the two
    // terminal-standard quit keys (bare `q` and Ctrl+C) still fire so a
    // user who feels stuck can always bail out of the whole app without
    // Esc'ing first.
    if app.place_mode.active {
        match key.code {
            KeyCode::Esc => app.exit_place_mode(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.should_quit = true;
            }
            KeyCode::Char('q') if key.modifiers.is_empty() => {
                app.should_quit = true;
            }
            _ => {}
        }
        return;
    }

    // Space-leader chord: bare Space primes, bare `p` opens the quick-open
    // palette, bare `f` opens the global-search palette. Bare Space has no
    // other global meaning, so the chord doesn't collide with any existing
    // binding. Context: we're already past the palette / search / place
    // gates, so the leader is only in play during normal tab navigation.
    //
    // Exception: when the Tab::Search search input is focused (modal
    // `tab_input_focused`), bare Space is a literal separator in a
    // multi-word query. We gate arming off so "foo bar" just types. An
    // empty query is fine to arm anyway — there's no char to accidentally
    // swallow yet.
    let in_input_mode = app.active_tab == Tab::Search
        && app.active_panel == Panel::Files
        && app.global_search.tab_input_focused;
    let leader_allow_arm = !in_input_mode || app.global_search.query.is_empty();
    match leader_decision(
        &key,
        leader_allow_arm,
        app.space_leader_at,
        Instant::now(),
        LEADER_TIMEOUT,
    ) {
        LeaderVerdict::Arm => {
            app.space_leader_at = Some(Instant::now());
            return;
        }
        LeaderVerdict::Fire => {
            app.space_leader_at = None;
            // Only one palette at a time — opening either implicitly closes
            // the other. `begin()` then activates the chosen one.
            app.quick_open.active = false;
            app.global_search.active = false;
            match key.code {
                KeyCode::Char('p') | KeyCode::Char('P') => quick_open::begin(app),
                KeyCode::Char('f') | KeyCode::Char('F') => global_search::begin(app),
                _ => {}
            }
            return;
        }
        LeaderVerdict::Consume => {
            app.space_leader_at = None;
            // Fall through — the current key still gets its normal meaning.
        }
        LeaderVerdict::None => {}
    }

    // Always-available global keys — even when the user is typing in a
    // search input, these need to remain usable as the escape hatch.
    // `Ctrl+C` quits, `Tab` / `BackTab` move between tabs and panels so the
    // user can get out of any focus state.
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
            return;
        }
        KeyCode::Tab => {
            let tabs = Tab::ALL;
            let cur = tabs.iter().position(|&t| t == app.active_tab).unwrap_or(0);
            app.set_active_tab(tabs[(cur + 1) % tabs.len()]);
            app.search.clear();
            return;
        }
        KeyCode::BackTab => {
            app.active_panel = match app.active_panel {
                Panel::Files => Panel::Diff,
                Panel::Diff => Panel::Files,
            };
            app.search.clear();
            return;
        }
        _ => {}
    }

    // Bare-character global shortcuts — stolen from the query when the
    // Tab::Search input is focused. Otherwise `h` = help, `q` = quit,
    // `/` = in-panel search, `n`/`N` = step matches, `1`-`9` = jump tab —
    // all of which the user is likely to want as literal chars inside a
    // search query.
    //
    // `/` has a context-aware override: in Tab::Search list mode it focuses
    // the search input instead of firing in-panel search. The in-panel
    // search's `resolve_target` is None for that (tab, panel) combo anyway,
    // so the former behaviour was a dead-end key — wiring it to mode-entry
    // makes the keybinding earn its slot.
    if !in_input_mode {
        match key.code {
            KeyCode::Char('q') => {
                app.should_quit = true;
                return;
            }
            KeyCode::Char(c) if matches!(c, '1'..='9') => {
                let idx = (c as u8 - b'1') as usize;
                if let Some(&tab) = Tab::ALL.get(idx) {
                    if app.active_tab != tab {
                        app.search.clear();
                    }
                    app.set_active_tab(tab);
                }
                return;
            }
            KeyCode::Char('h') => {
                app.show_help = true;
                return;
            }
            KeyCode::Char('/') => {
                if app.active_tab == Tab::Search && app.active_panel == Panel::Files {
                    app.global_search.tab_input_focused = true;
                } else {
                    search::begin(app, false);
                }
                return;
            }
            KeyCode::Char('?') => {
                search::begin(app, true);
                return;
            }
            KeyCode::Char('n') if app.search.can_step() && !has_pending_confirm(app) => {
                search::step(app, false);
                return;
            }
            KeyCode::Char('N') if app.search.can_step() && !has_pending_confirm(app) => {
                search::step(app, true);
                return;
            }
            _ => {}
        }
    }

    match app.active_tab {
        Tab::Git => handle_key_git(key, app),
        Tab::Files => handle_key_files(key, app),
        Tab::Search => handle_key_search(key, app),
        Tab::Graph => handle_key_graph(key, app),
    }
}

/// `n` / `N` only bind to search navigation when no git-status confirmation
/// banner is up — otherwise `n` keeps its "no, cancel" meaning.
fn has_pending_confirm(app: &App) -> bool {
    app.git_status.confirm_discard.is_some()
        || app.git_status.confirm_push
        || app.git_status.confirm_force_push
}

/// Tab::Search key dispatcher. Panel::Files is the search sidebar, which
/// runs in one of two modes tracked by `GlobalSearchState.tab_input_focused`:
///
/// - **List mode** (default on tab entry): bare keys are either nav
///   (↑↓/j/k/Ctrl+N/P) or they fall back to global shortcuts (h = help,
///   q = quit, etc.). `/` or `i` enters input mode. Enter opens the
///   selected hit in `$EDITOR`.
/// - **Input mode**: typing fills the query; same editing / navigation /
///   accept keys as the overlay. Esc returns to list mode.
///
/// Panel::Diff is the file preview, same keys as the Files-tab Diff panel.
///
/// Global gates in `handle_key` keep bare-char shortcuts from stealing
/// literal chars while in input mode; see `in_input_mode` there.
fn handle_key_search(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match app.active_panel {
        Panel::Files => {
            if app.global_search.tab_input_focused {
                handle_key_search_input_mode(key, app, ctrl, alt);
            } else {
                handle_key_search_list_mode(key, app, ctrl);
            }
        }
        Panel::Diff => {
            // Preview panel on the right — same scrolling keys as the
            // Files-tab Diff panel. `/` is handled at the global level
            // (search::begin) and works here via resolve_target.
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    app.preview_scroll = app.preview_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.preview_scroll += 1;
                }
                KeyCode::PageUp => {
                    app.preview_scroll = app.preview_scroll.saturating_sub(20);
                }
                KeyCode::PageDown => {
                    app.preview_scroll += 20;
                }
                KeyCode::Left => {
                    let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                        10
                    } else {
                        1
                    };
                    app.preview_h_scroll = app.preview_h_scroll.saturating_sub(step);
                }
                KeyCode::Right => {
                    let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                        10
                    } else {
                        1
                    };
                    app.preview_h_scroll = app.preview_h_scroll.saturating_add(step);
                }
                KeyCode::Home => {
                    app.preview_h_scroll = 0;
                }
                KeyCode::End => {
                    app.preview_h_scroll = usize::MAX;
                }
                KeyCode::Enter => {
                    global_search::accept(app);
                }
                _ => {}
            }
        }
    }
}

/// Key dispatch for Tab::Search Panel::Files when input is NOT focused
/// (list mode). The user is browsing existing results — bare chars fall
/// back to global shortcuts via the gate in `handle_key`, so here we only
/// bind nav + mode-entry + accept + h-scroll.
fn handle_key_search_list_mode(key: KeyEvent, app: &mut App, ctrl: bool) {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        // ── Vertical navigation ────────────────────────────────
        KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
            global_search::move_selection_by(app, -1);
        }
        KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
            global_search::move_selection_by(app, 1);
        }
        KeyCode::Char('p') if ctrl => global_search::move_selection_by(app, -1),
        KeyCode::Char('k') if ctrl => global_search::move_selection_by(app, -1),
        KeyCode::Char('n') if ctrl => global_search::move_selection_by(app, 1),
        KeyCode::Char('j') if ctrl => global_search::move_selection_by(app, 1),
        KeyCode::PageUp => {
            let step = app.global_search.last_view_h.max(1) as i32;
            global_search::move_selection_by(app, -step);
        }
        KeyCode::PageDown => {
            let step = app.global_search.last_view_h.max(1) as i32;
            global_search::move_selection_by(app, step);
        }

        // ── Horizontal scroll of the results list ──────────────
        // Mirrors the Files-tab Diff panel convention: bare arrows move
        // 1 col, Shift+arrow moves 10, Home/End jump to the extremes.
        // Setting `results_h_scroll` to non-zero disables smart per-row
        // shifting so rows line up at the user's chosen column.
        KeyCode::Left => {
            let step = if shift { 10 } else { 1 };
            app.global_search.results_h_scroll =
                app.global_search.results_h_scroll.saturating_sub(step);
        }
        KeyCode::Right => {
            let step = if shift { 10 } else { 1 };
            app.global_search.results_h_scroll = app
                .global_search
                .results_h_scroll
                .saturating_add(step)
                .min(crate::global_search::MAX_H_SCROLL);
        }
        KeyCode::Home => {
            app.global_search.results_h_scroll = 0;
        }
        KeyCode::End => {
            app.global_search.results_h_scroll = crate::global_search::MAX_H_SCROLL;
        }

        // ── Mode entry ─────────────────────────────────────────
        // `i` (vim-insert) as a secondary mnemonic. `/` is handled in the
        // global keymap so it also lights up the input from other tabs;
        // dispatching it here too makes the in-tab behaviour obvious.
        KeyCode::Char('i') if key.modifiers.is_empty() => {
            app.global_search.tab_input_focused = true;
        }
        KeyCode::Char('/') if key.modifiers.is_empty() => {
            app.global_search.tab_input_focused = true;
        }

        // ── Reload ─────────────────────────────────────────────
        // Re-run the current query. Mirrors `r` = refresh on Files / Git /
        // Graph tabs. Only available in list mode; in input mode `r` is a
        // literal char for the query.
        KeyCode::Char('r') if key.modifiers.is_empty() => {
            global_search::reload(app);
        }

        // ── Accept ─────────────────────────────────────────────
        KeyCode::Enter => global_search::accept(app),

        // Esc is a no-op in list mode: nothing to escape from, and we
        // don't want it to close/jump away unexpectedly.
        _ => {}
    }
}

/// Key dispatch for Tab::Search Panel::Files when input IS focused
/// (input mode). Same bindings as the Space+F overlay — typing fills the
/// query, Esc exits to list mode, Enter opens the selection.
fn handle_key_search_input_mode(key: KeyEvent, app: &mut App, ctrl: bool, alt: bool) {
    match key.code {
        // ── Exit to list mode ──────────────────────────────────
        KeyCode::Esc => {
            app.global_search.tab_input_focused = false;
        }

        // ── List navigation (works alongside typing) ───────────
        KeyCode::Up => global_search::move_selection_by(app, -1),
        KeyCode::Down => global_search::move_selection_by(app, 1),
        KeyCode::Char('p') if ctrl => global_search::move_selection_by(app, -1),
        KeyCode::Char('k') if ctrl => global_search::move_selection_by(app, -1),
        KeyCode::Char('n') if ctrl => global_search::move_selection_by(app, 1),
        KeyCode::Char('j') if ctrl => global_search::move_selection_by(app, 1),
        KeyCode::PageUp => {
            let step = app.global_search.last_view_h.max(1) as i32;
            global_search::move_selection_by(app, -step);
        }
        KeyCode::PageDown => {
            let step = app.global_search.last_view_h.max(1) as i32;
            global_search::move_selection_by(app, step);
        }

        // ── Cursor movement inside the query ───────────────────
        // Alt/Ctrl + arrow = jump by word (readline / Option+Arrow /
        // Ctrl+Arrow convention). Must come before plain-arrow arms so
        // modifier combos win.
        KeyCode::Left if alt || ctrl => {
            crate::input_edit::move_cursor_word_backward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Right if alt || ctrl => {
            crate::input_edit::move_cursor_word_forward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Left => {
            crate::input_edit::move_cursor(
                &app.global_search.query,
                &mut app.global_search.cursor,
                -1,
            );
        }
        KeyCode::Right => {
            crate::input_edit::move_cursor(
                &app.global_search.query,
                &mut app.global_search.cursor,
                1,
            );
        }
        KeyCode::Home => {
            app.global_search.cursor = 0;
        }
        KeyCode::End => {
            app.global_search.cursor = app.global_search.query.len();
        }

        // ── Forward-delete ─────────────────────────────────────
        // Symmetric with Backspace: plain Delete kills a char,
        // Alt/Ctrl+Delete kills a word. Both re-run the search.
        KeyCode::Delete if alt || ctrl => {
            crate::input_edit::delete_word_forward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            global_search::mark_query_edited(&mut app.global_search);
        }
        KeyCode::Delete => {
            crate::input_edit::delete_char_forward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            global_search::mark_query_edited(&mut app.global_search);
        }

        // ── Readline aliases ────────────────────────────────────
        // Reliable fallback for terminals that don't forward Alt/Ctrl+Arrow.
        // Bash-readline muscle memory: Alt+b/f/d for word motion, Ctrl+A/E
        // for line start/end.
        KeyCode::Char('b') if alt => {
            crate::input_edit::move_cursor_word_backward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Char('f') if alt => {
            crate::input_edit::move_cursor_word_forward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Char('d') if alt => {
            crate::input_edit::delete_word_forward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            global_search::mark_query_edited(&mut app.global_search);
        }
        KeyCode::Char('a') if ctrl => {
            app.global_search.cursor = 0;
        }
        KeyCode::Char('e') if ctrl => {
            app.global_search.cursor = app.global_search.query.len();
        }

        // ── Accept ─────────────────────────────────────────────
        KeyCode::Enter => global_search::accept(app),

        // ── Edit query ─────────────────────────────────────────
        KeyCode::Backspace if alt || ctrl => {
            crate::input_edit::delete_word_backward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            global_search::mark_query_edited(&mut app.global_search);
        }
        KeyCode::Char('w') if ctrl => {
            crate::input_edit::delete_word_backward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            global_search::mark_query_edited(&mut app.global_search);
        }
        KeyCode::Char('u') if ctrl => {
            crate::input_edit::clear(&mut app.global_search.query, &mut app.global_search.cursor);
            global_search::mark_query_edited(&mut app.global_search);
        }
        KeyCode::Backspace => {
            crate::input_edit::backspace(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            global_search::mark_query_edited(&mut app.global_search);
        }

        // Any other Ctrl-combo is a no-op so Ctrl+A etc. don't leak as
        // literal chars into the query.
        KeyCode::Char(c) if !ctrl => {
            crate::input_edit::insert_char(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
                c,
            );
            global_search::mark_query_edited(&mut app.global_search);
        }
        _ => {}
    }
}

fn handle_key_graph(key: KeyEvent, app: &mut App) {
    use ui::{commit_detail_panel, git_graph_panel};
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => {
                git_graph_panel::handle_key(app, "k");
            }
            Panel::Diff => commit_detail_panel::scroll(app, -1),
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => {
                git_graph_panel::handle_key(app, "j");
            }
            Panel::Diff => commit_detail_panel::scroll(app, 1),
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                for _ in 0..10 {
                    git_graph_panel::handle_key(app, "k");
                }
            }
            Panel::Diff => commit_detail_panel::scroll(app, -20),
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                for _ in 0..10 {
                    git_graph_panel::handle_key(app, "j");
                }
            }
            Panel::Diff => commit_detail_panel::scroll(app, 20),
        },
        KeyCode::Char('r') => {
            // `r` on the graph sidebar = force a graph cache refresh
            app.git_graph.cache_key = None;
            app.refresh_graph();
        }
        // m/f/t target the commit-detail panel regardless of focus.
        KeyCode::Char('m') => {
            commit_detail_panel::handle_key(app, "m");
        }
        KeyCode::Char('f') => {
            commit_detail_panel::handle_key(app, "f");
        }
        KeyCode::Char('t') => {
            commit_detail_panel::handle_key(app, "t");
        }
        _ => {}
    }
}

fn handle_key_git(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => app.navigate_files(-1),
            Panel::Diff => {
                app.diff_scroll = app.diff_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => app.navigate_files(1),
            Panel::Diff => {
                app.diff_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => app.navigate_files(-10),
            Panel::Diff => {
                app.diff_scroll = app.diff_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => app.navigate_files(10),
            Panel::Diff => {
                app.diff_scroll += 20;
            }
        },
        KeyCode::Left if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.diff_h_scroll = app.diff_h_scroll.saturating_sub(step);
        }
        KeyCode::Right if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.diff_h_scroll = app.diff_h_scroll.saturating_add(step);
        }
        KeyCode::Home if app.active_panel == Panel::Diff => {
            app.diff_h_scroll = 0;
        }
        KeyCode::End if app.active_panel == Panel::Diff => {
            app.diff_h_scroll = usize::MAX; // render 自动钳到实际最大值
        }
        KeyCode::Char('s') => {
            ui::git_status_panel::handle_key(app, "s");
        }
        KeyCode::Char('u') => {
            ui::git_status_panel::handle_key(app, "u");
        }
        KeyCode::Char('d') => {
            ui::git_status_panel::handle_key(app, "d");
        }
        KeyCode::Char('y') => {
            ui::git_status_panel::handle_key(app, "y");
        }
        KeyCode::Char('n') => {
            ui::git_status_panel::handle_key(app, "n");
        }
        KeyCode::Esc => {
            ui::git_status_panel::handle_key(app, "Escape");
        }
        KeyCode::Char('r') => {
            ui::git_status_panel::handle_key(app, "r");
        }
        KeyCode::Char('t') => {
            ui::git_status_panel::handle_key(app, "t");
        }
        KeyCode::Char('m') => {
            app.toggle_diff_layout();
        }
        KeyCode::Char('f') => {
            app.toggle_diff_mode();
        }
        KeyCode::Char('e') | KeyCode::Enter => {
            // Edit the currently selected changed file. Ignore if nothing's
            // selected (empty status) or the repo's gone. A Deleted-status
            // file will be recreated by the editor if the user writes — same
            // behaviour you'd get running `$EDITOR path/to/deleted` in a shell.
            if let (Some(repo), Some(sel)) = (&app.repo, &app.selected_file) {
                if let Some(workdir) = repo.workdir_path() {
                    app.pending_edit = Some(workdir.join(&sel.path));
                }
            }
        }
        _ => {}
    }
}

fn handle_key_files(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-1);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll = app.preview_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(1);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-10);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll = app.preview_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(10);
                app.load_preview();
            }
            Panel::Diff => {
                app.preview_scroll += 20;
            }
        },
        KeyCode::Left if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.preview_h_scroll = app.preview_h_scroll.saturating_sub(step);
        }
        KeyCode::Right if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.preview_h_scroll = app.preview_h_scroll.saturating_add(step);
        }
        KeyCode::Home if app.active_panel == Panel::Diff => {
            app.preview_h_scroll = 0;
        }
        KeyCode::End if app.active_panel == Panel::Diff => {
            app.preview_h_scroll = usize::MAX;
        }
        KeyCode::Enter => {
            let idx = app.file_tree.selected;
            if let Some(entry) = app.file_tree.entries.get(idx) {
                if entry.is_dir {
                    app.file_tree.toggle_expand(idx);
                    app.refresh_file_tree_with_target(app.file_tree.selected_path());
                } else {
                    // File: hand off to the main loop, which owns the
                    // terminal and can suspend it around `$EDITOR`.
                    app.pending_edit = Some(app.file_tree.root.join(&entry.path));
                }
            }
        }
        KeyCode::Char('r') => {
            app.refresh_file_tree();
        }
        KeyCode::Char('e') => {
            // Explicit alias for "edit selected entry". Unlike Enter, this
            // never expands a directory — on a dir it's a no-op.
            let idx = app.file_tree.selected;
            if let Some(entry) = app.file_tree.entries.get(idx) {
                if !entry.is_dir {
                    app.pending_edit = Some(app.file_tree.root.join(&entry.path));
                }
            }
        }
        _ => {}
    }
}

// ─── Mouse ───────────────────────────────────────────────────────────────────

pub fn handle_mouse<B: Backend>(mouse: MouseEvent, app: &mut App, terminal: &Terminal<B>) {
    // Palettes fully own mouse input while active (global-search first,
    // then quick-open): clicks must not leak through to hidden panels,
    // and scroll wheels inside the popup should move the selection.
    if app.global_search.active {
        global_search::handle_mouse(mouse, app);
        return;
    }
    if app.quick_open.active {
        quick_open::handle_mouse(mouse, app);
        return;
    }

    if app.place_mode.active {
        handle_mouse_place_mode(mouse, app);
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let now = Instant::now();
            let is_double = matches!(
                app.last_click,
                Some((t, c, r))
                    if c == mouse.column
                        && r == mouse.row
                        && now.duration_since(t) < DOUBLE_CLICK_WINDOW
            );

            if let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row) {
                // Double-click on a search result row commits the hit:
                // switch to Files tab, reveal the file, and load its preview
                // with the matched line highlighted — same as the overlay's
                // Enter. Single-click falls through to handle_action for
                // "select + live preview" without leaving the Search tab.
                // Handled here rather than in `handle_action` because the
                // is_double signal isn't threaded through App methods.
                if is_double && let ui::mouse::ClickAction::GlobalSearchSelect(idx) = action {
                    app.global_search.selected = idx;
                    global_search::accept(app);
                    app.last_click = None;
                    return;
                }
                // On double-click: if the region carries a dbl action, swap to it
                // and run through handle_action so host-side side effects fire.
                let effective = if is_double {
                    if let ui::mouse::ClickAction::GitCommand {
                        dbl_command: Some(ref cmd),
                        ref dbl_args,
                        ..
                    } = action
                    {
                        ui::mouse::ClickAction::GitCommand {
                            command: cmd.clone(),
                            args: dbl_args.clone().unwrap_or(serde_json::Value::Null),
                            dbl_command: None,
                            dbl_args: None,
                        }
                    } else {
                        action
                    }
                } else {
                    action
                };
                app.handle_action(effective);
            }

            // Reset tracking on every genuine second click so triple-clicks don't chain.
            app.last_click = if is_double {
                None
            } else {
                Some((now, mouse.column, mouse.row))
            };
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.dragging_split = false;
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.dragging_split {
                let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
                if total_width > 0 {
                    let percent = (mouse.column * 100 / total_width).clamp(10, 80);
                    app.split_percent = percent;
                }
            }
        }
        MouseEventKind::ScrollUp => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            // Shift + 滚轮 = 横向滚动（兼容不发 ScrollLeft/Right 的终端）
            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                apply_horizontal_scroll(app, mouse.column, total_width, -3);
                return;
            }
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left {
                        ui::git_status_panel::scroll(app, -3);
                    } else {
                        app.diff_scroll = app.diff_scroll.saturating_sub(3);
                    }
                }
                Tab::Files => {
                    if is_left {
                        app.tree_scroll = app.tree_scroll.saturating_sub(3);
                    } else {
                        app.preview_scroll = app.preview_scroll.saturating_sub(3);
                    }
                }
                Tab::Graph => {
                    if is_left {
                        ui::git_graph_panel::scroll(app, -3);
                    } else {
                        ui::commit_detail_panel::scroll(app, -3);
                    }
                }
                Tab::Search => {
                    if is_left {
                        // Scroll the results list by moving selection — the
                        // left column IS the result list, not a separate
                        // scroll surface.
                        global_search::move_selection_by(app, -3);
                    } else {
                        app.preview_scroll = app.preview_scroll.saturating_sub(3);
                    }
                }
            }
        }
        MouseEventKind::ScrollDown => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                apply_horizontal_scroll(app, mouse.column, total_width, 3);
                return;
            }
            let split_x = total_width * app.split_percent / 100;
            let is_left = mouse.column < split_x;
            match app.active_tab {
                Tab::Git => {
                    if is_left {
                        ui::git_status_panel::scroll(app, 3);
                    } else {
                        app.diff_scroll += 3;
                    }
                }
                Tab::Files => {
                    if is_left {
                        app.tree_scroll += 3;
                    } else {
                        app.preview_scroll += 3;
                    }
                }
                Tab::Graph => {
                    if is_left {
                        ui::git_graph_panel::scroll(app, 3);
                    } else {
                        ui::commit_detail_panel::scroll(app, 3);
                    }
                }
                Tab::Search => {
                    if is_left {
                        global_search::move_selection_by(app, 3);
                    } else {
                        app.preview_scroll += 3;
                    }
                }
            }
        }
        MouseEventKind::ScrollLeft => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            apply_horizontal_scroll(app, mouse.column, total_width, -3);
        }
        MouseEventKind::ScrollRight => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            apply_horizontal_scroll(app, mouse.column, total_width, 3);
        }
        MouseEventKind::Moved => {
            app.hover_row = Some(mouse.row);
            app.hover_col = Some(mouse.column);
        }
        _ => {}
    }
}

/// Apply a horizontal-scroll delta (in display columns) to whichever panel
/// the cursor sits over. Routed from Shift+wheel, trackpad ScrollLeft/Right,
/// and bare ← / → keys. Tab::Search is the only tab whose LEFT panel also
/// h-scrolls (the results list) — other tabs' left panels are tree/list
/// widgets with no long horizontal content.
///
/// Graph tab right side (commit detail) isn't wired yet — rows already
/// truncate to width there; add `CommitDetailState.diff_h_scroll` if
/// long-line viewing becomes a real need.
fn apply_horizontal_scroll(app: &mut App, column: u16, total_width: u16, delta: i32) {
    let split_x = total_width * app.split_percent / 100;
    let is_left = column < split_x;

    let target: Option<&mut usize> = match (app.active_tab, is_left) {
        (Tab::Search, true) => Some(&mut app.global_search.results_h_scroll),
        (_, true) => None,
        (Tab::Files, false) => Some(&mut app.preview_h_scroll),
        (Tab::Git, false) => Some(&mut app.diff_h_scroll),
        (Tab::Search, false) => Some(&mut app.preview_h_scroll),
        (Tab::Graph, false) => None,
    };
    let Some(target) = target else {
        return;
    };
    *target = if delta < 0 {
        target.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        target.saturating_add(delta as usize)
    };
}

// ─── Place mode (drag-and-drop destination picker) ───────────────────────────

fn handle_mouse_place_mode(mouse: MouseEvent, app: &mut App) {
    match mouse.kind {
        MouseEventKind::Moved => {
            app.hover_row = Some(mouse.row);
            app.hover_col = Some(mouse.column);
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // Only PlaceMode* click actions are meaningful here. Any other
            // hit (e.g. a file row that we no longer register, or the tab
            // bar) is treated as "clicked outside the droppable area" and
            // cancels the modal.
            match app.hit_registry.hit_test(mouse.column, mouse.row) {
                Some(ui::mouse::ClickAction::PlaceModeFolder(idx)) => {
                    app.handle_action(ui::mouse::ClickAction::PlaceModeFolder(idx));
                }
                Some(ui::mouse::ClickAction::PlaceModeRoot) => {
                    app.handle_action(ui::mouse::ClickAction::PlaceModeRoot);
                }
                _ => {
                    app.exit_place_mode();
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            app.exit_place_mode();
        }
        MouseEventKind::ScrollUp => {
            app.tree_scroll = app.tree_scroll.saturating_sub(3);
        }
        MouseEventKind::ScrollDown => {
            app.tree_scroll = app.tree_scroll.saturating_add(3);
        }
        _ => {}
    }
}

// ─── Bracketed paste dispatch ────────────────────────────────────────────────

/// Entry point for `Event::Paste(s)` from the main loop. Priorities:
///
/// 1. If the payload parses as one or more existing absolute paths, enter
///    drag-and-drop place mode with those sources.
/// 2. Otherwise, if an input field has focus (quick-open palette, search
///    prompt), forward the payload as typed text.
/// 3. Otherwise drop silently — a paste landing on plain tab navigation
///    has no sensible target.
pub fn handle_paste(s: String, app: &mut App) {
    let paths = parse_dropped_paths(&s);
    if !paths.is_empty() {
        app.enter_place_mode(paths);
        return;
    }
    if app.quick_open.active {
        quick_open::handle_paste(&s, app);
    } else if app.search.active {
        search::handle_paste(&s, app);
    }
    // No focused input; intentionally dropped. A stray paste into the
    // global keymap has no defined meaning, and we don't want to
    // accidentally trigger an action.
}

/// Extract filesystem paths from a bracketed-paste payload.
///
/// Terminals normalise file drops into paste content, but the exact
/// framing varies:
///
/// - iTerm2: single-quote wrapped, `\ ` for spaces, one path per line.
/// - Ghostty / WezTerm / Alacritty / Kitty: raw path with `\ ` escapes.
/// - GNOME Terminal / older: `file:///…` URI with `%xx` URL-encoding.
///
/// We tolerate all of the above, then demand that every candidate be an
/// absolute path (drops from Finder always are) that `exists()` on disk.
/// Relative paths and non-existent strings are rejected outright — a
/// user pasting the word `settings.json` into the quick-open palette
/// must NOT trip place mode, and the absolute-path requirement is what
/// makes that reliable.
///
/// Returns an empty vector when the payload is "not a drop"; callers
/// use that as the signal to forward the paste to the focused text input.
pub fn parse_dropped_paths(s: &str) -> Vec<std::path::PathBuf> {
    let trimmed = s.trim_matches(|c: char| c == '\n' || c == '\r');
    if trimmed.is_empty() {
        return Vec::new();
    }
    // Newline is the common multi-file separator across every terminal we
    // support. Single-segment → single path (even if it contains spaces);
    // multi-segment → one path per line.
    let mut out = Vec::new();
    for segment in trimmed.split('\n') {
        let seg = segment.trim().trim_end_matches('\r');
        if seg.is_empty() {
            continue;
        }
        let Some(p) = normalize_segment(seg) else {
            continue;
        };
        if p.is_absolute() && p.exists() {
            out.push(p);
        }
    }
    out
}

/// Strip one layer of framing from a single dropped-path segment:
/// matched outer quotes → removed; `file://` scheme → stripped + URL
/// decoded; backslash escapes → unescaped. Returns `None` if the segment
/// ends up empty or can't be interpreted as a path.
fn normalize_segment(raw: &str) -> Option<std::path::PathBuf> {
    let mut s = raw.to_string();

    // Outer quote pair — some terminals (iTerm2 by default) wrap drag
    // payloads in single quotes; paste-from-clipboard in others may use
    // double quotes. Only strip if both ends match.
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if (first == '\'' && last == '\'') || (first == '"' && last == '"') {
            s = s[1..s.len() - 1].to_string();
        }
    }

    // `file://` URI → filesystem path + URL decode. We accept both
    // `file:///Users/...` (triple-slash, most common) and `file://localhost/...`
    // (hostname-qualified, older macOS) by stripping the authority.
    if let Some(rest) = s.strip_prefix("file://") {
        let path_part = rest.strip_prefix("localhost").unwrap_or(rest).to_string();
        s = url_decode(&path_part);
    }

    s = unescape_backslashes(&s);

    if s.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from(s))
}

/// Minimal `%xx` percent-decoder, enough for common file URIs. Invalid
/// escapes are left as-is (we never fail the whole parse over a stray `%`).
fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push((hi * 16 + lo) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Unescape shell-style backslash sequences commonly used by terminals
/// when injecting dropped paths: `\ ` → ` `, `\\` → `\`, `\t` → tab, etc.
/// A trailing unmatched backslash is passed through verbatim.
fn unescape_backslashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod leader_tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn ke(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    fn space() -> KeyEvent {
        ke(KeyCode::Char(' '), KeyModifiers::empty())
    }

    fn lower_p() -> KeyEvent {
        ke(KeyCode::Char('p'), KeyModifiers::empty())
    }

    fn upper_p() -> KeyEvent {
        ke(KeyCode::Char('P'), KeyModifiers::empty())
    }

    #[test]
    fn space_with_no_leader_arms() {
        let now = Instant::now();
        let v = leader_decision(&space(), true, None, now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Arm);
    }

    #[test]
    fn space_when_arm_not_allowed_does_not_arm() {
        // Palette has non-empty query → bare Space is a literal char.
        let now = Instant::now();
        let v = leader_decision(&space(), false, None, now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::None);
    }

    #[test]
    fn p_after_arm_within_window_fires() {
        let now = Instant::now();
        let v = leader_decision(&lower_p(), true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Fire);
    }

    #[test]
    fn uppercase_p_after_arm_also_fires() {
        // Accept both cases so CapsLock or Shift doesn't defeat the chord.
        let now = Instant::now();
        let v = leader_decision(&upper_p(), true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Fire);
    }

    #[test]
    fn f_after_arm_fires_for_global_search() {
        // Space+F is the global-search chord. The verdict stays a unit
        // `Fire`; the caller disambiguates via `key.code`.
        let now = Instant::now();
        let f = ke(KeyCode::Char('f'), KeyModifiers::empty());
        let v = leader_decision(&f, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Fire);
    }

    #[test]
    fn uppercase_f_after_arm_also_fires() {
        let now = Instant::now();
        let f = ke(KeyCode::Char('F'), KeyModifiers::empty());
        let v = leader_decision(&f, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Fire);
    }

    #[test]
    fn ctrl_f_does_not_fire_even_when_armed() {
        // Keep Ctrl+F free for future panel-level search bindings; only bare
        // `f` is the chord target.
        let now = Instant::now();
        let ctrl_f = ke(KeyCode::Char('f'), KeyModifiers::CONTROL);
        let v = leader_decision(&ctrl_f, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Consume);
    }

    #[test]
    fn p_after_window_expired_consumes() {
        let armed = Instant::now();
        let now = armed + LEADER_TIMEOUT + Duration::from_millis(50);
        let v = leader_decision(&lower_p(), true, Some(armed), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Consume);
    }

    #[test]
    fn p_with_ctrl_does_not_fire_even_when_armed() {
        // Ctrl+P is bound to "prev candidate" inside the palette; the
        // chord must not accidentally swallow it.
        let now = Instant::now();
        let ctrl_p = ke(KeyCode::Char('p'), KeyModifiers::CONTROL);
        let v = leader_decision(&ctrl_p, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Consume);
    }

    #[test]
    fn second_space_rearms_the_leader() {
        // Double-Space is more usefully "reset the chord" than "lose it"
        // — otherwise Space+Space+P wouldn't open the palette.
        let first = Instant::now();
        let second = first + Duration::from_millis(100);
        let v = leader_decision(&space(), true, Some(first), second, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Arm);
    }

    #[test]
    fn non_chord_key_after_arm_consumes() {
        let now = Instant::now();
        let j = ke(KeyCode::Char('j'), KeyModifiers::empty());
        let v = leader_decision(&j, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Consume);
    }

    #[test]
    fn no_leader_and_non_space_is_passthrough() {
        let now = Instant::now();
        let j = ke(KeyCode::Char('j'), KeyModifiers::empty());
        let v = leader_decision(&j, true, None, now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::None);
    }

    #[test]
    fn shift_space_does_not_arm() {
        let now = Instant::now();
        let shift_space = ke(KeyCode::Char(' '), KeyModifiers::SHIFT);
        let v = leader_decision(&shift_space, true, None, now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::None);
    }
}

#[cfg(test)]
mod paste_parser_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        fs::write(&p, "").unwrap();
        p
    }

    #[test]
    fn non_path_text_is_not_a_drop() {
        // User pastes a regular word into an input — must not activate
        // place mode. The absolute-path requirement is what makes this
        // robust.
        assert!(parse_dropped_paths("settings.json").is_empty());
        assert!(parse_dropped_paths("let x = 1;").is_empty());
        assert!(parse_dropped_paths("").is_empty());
    }

    #[test]
    fn relative_paths_rejected_even_if_they_exist() {
        // Even if `src/main.rs` exists from the cwd, a relative path is
        // never what a drop would produce — reject to avoid false
        // positives on pasted code snippets.
        assert!(parse_dropped_paths("src/main.rs").is_empty());
    }

    #[test]
    fn plain_absolute_path_is_accepted() {
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "a.txt");
        let paste = file.to_string_lossy().to_string();
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![file]);
    }

    #[test]
    fn file_uri_is_decoded_and_accepted() {
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "hello world.txt");
        let uri = format!("file://{}", file.to_string_lossy().replace(' ', "%20"));
        let got = parse_dropped_paths(&uri);
        assert_eq!(got, vec![file]);
    }

    #[test]
    fn single_quoted_iterm2_style() {
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "quoted.txt");
        let paste = format!("'{}'", file.to_string_lossy());
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![file]);
    }

    #[test]
    fn backslash_escaped_spaces() {
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "name with space.txt");
        let escaped = file.to_string_lossy().replace(' ', r"\ ").to_string();
        let got = parse_dropped_paths(&escaped);
        assert_eq!(got, vec![file]);
    }

    #[test]
    fn multi_file_newline_separated() {
        let tmp = TempDir::new().unwrap();
        let a = touch(tmp.path(), "a.txt");
        let b = touch(tmp.path(), "b.txt");
        let paste = format!("{}\n{}", a.to_string_lossy(), b.to_string_lossy());
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn trailing_newline_tolerated() {
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "a.txt");
        let paste = format!("{}\n", file.to_string_lossy());
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![file]);
    }

    #[test]
    fn non_existent_absolute_path_rejected() {
        // Absolute and looks like a path, but doesn't exist on disk.
        // A drop would only ever hand us a real file, so reject to
        // avoid pasting arbitrary fake paths into place mode.
        let got = parse_dropped_paths("/this/does/not/exist/abc.xyz");
        assert!(got.is_empty());
    }

    #[test]
    fn mixed_valid_and_invalid_keeps_only_valid() {
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "ok.txt");
        let paste = format!("{}\n/nope/nope/nope", file.to_string_lossy());
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![file]);
    }

    #[test]
    fn file_localhost_prefix_stripped() {
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "loc.txt");
        let paste = format!("file://localhost{}", file.to_string_lossy());
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![file]);
    }
}
