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
use crate::clipboard;
use crate::global_search;
use crate::i18n::{Msg, t};
use crate::quick_open;
use crate::search;
use crate::ui;
use crate::ui::selection::{
    DiffSelection, DiffSide, PreviewSelection, col_to_byte_offset, collect_diff_selected_text,
    collect_selected_text, word_at_byte,
};
use crate::ui::toast::Toast;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::Rect;
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
    // Hosts picker (Ctrl+O) — fully owns input while active, same contract
    // as the other overlays.
    if app.hosts_picker.active {
        handle_key_hosts_picker(key, app);
        return;
    }

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

    // Inline tree editor (New File / New Folder / Rename): while
    // `tree_edit.active`, every non-Ctrl-C keystroke goes into the
    // editable buffer. Priority-wise this sits above place-mode and
    // the context menu so a stray right-click or drop can't yank the
    // cursor out from under a half-typed filename.
    if app.tree_edit.active {
        handle_key_tree_edit(key, app);
        return;
    }

    // Right-click context menu: while visible it owns the keyboard —
    // arrow keys navigate, Enter fires, Esc closes. Any other key
    // closes the menu (VSCode behaviour — keeps the user from
    // accidentally leaving a menu lingering).
    if app.tree_context_menu.active {
        handle_key_tree_context_menu(key, app);
        return;
    }

    // Delete confirmation status-bar prompt: Y confirms, N / Esc
    // cancels. Every other key is ignored so the user can't
    // accidentally trigger something else while the confirm is up.
    if app.tree_delete_confirm.is_some() {
        handle_key_tree_delete_confirm(key, app);
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
    // Exception: when a text input is focused — the Tab::Search query or
    // the Tab::Git commit box — bare Space is a literal character the user
    // is typing. We gate arming off so "foo bar" / "fix: the thing" just
    // types. An empty buffer is fine to arm anyway — there's no char to
    // accidentally swallow yet.
    let search_input_focused = app.active_tab == Tab::Search
        && app.active_panel == Panel::Files
        && app.global_search.tab_input_focused;
    let commit_input_focused = app.active_tab == Tab::Git
        && app.active_panel == Panel::Files
        && app.git_status.commit_editing;
    let in_input_mode = search_input_focused || commit_input_focused;
    let leader_allow_arm = if search_input_focused {
        app.global_search.query.is_empty()
    } else if commit_input_focused {
        app.git_status.commit_message.is_empty()
    } else {
        true
    };
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
        // Ctrl+O opens the hosts picker (session switcher). Shares the
        // overlay priority with quick-open / global-search: any key
        // handled there returns early before reaching this match.
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_hosts_picker();
            return;
        }
        // Ctrl+B toggles the left sidebar — VSCode muscle memory. Sits in
        // the always-on block so it works regardless of which tab or panel
        // owns focus; overlays (quick-open, global-search, hosts picker)
        // return earlier so they're unaffected.
        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.toggle_sidebar();
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
            // Graph tab 3-col cycles Files → Commit → Diff → Files so
            // Shift+Tab reaches the new middle column. Other tabs (and the
            // Graph 2-col fallback) keep the two-way toggle.
            app.active_panel = if app.graph_uses_three_col() {
                match app.active_panel {
                    Panel::Files => Panel::Commit,
                    Panel::Commit => Panel::Diff,
                    Panel::Diff => Panel::Files,
                }
            } else {
                match app.active_panel {
                    Panel::Files | Panel::Commit => Panel::Diff,
                    Panel::Diff => Panel::Files,
                }
            };
            app.search.clear();
            return;
        }
        _ => {}
    }

    // Bare-character global shortcuts — suppressed whenever a text input
    // is focused (Tab::Search query or Tab::Git commit box) so they don't
    // steal literal keystrokes mid-typing. Otherwise `h` = help, `q` = quit,
    // `/` = in-panel search, `n`/`N` = step matches, `1`-`9` = jump tab.
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
            // Require NO Control modifier so Ctrl+N stays available as
            // a per-tab "down" nav alias rather than stepping the
            // vim-style search. Bare N (Shift+n) keeps its step-back
            // meaning.
            KeyCode::Char('n')
                if app.search.can_step()
                    && !has_pending_confirm(app)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                search::step(app, false);
                return;
            }
            KeyCode::Char('N')
                if app.search.can_step()
                    && !has_pending_confirm(app)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
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
        // Search tab has no middle column. `normalize_active_panel`
        // demotes Commit elsewhere, but guard here in case a key lands
        // mid-transition.
        Panel::Commit => {}
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

/// Set `active_panel` based on which column the cursor hit. For tabs with
/// only two columns this is a Files/Diff toggle; Graph 3-col adds a
/// Commit variant for the middle column. Called on every Down(Left) so
/// the user's subsequent arrow keys go to whatever they just clicked —
/// matching the VSCode focus-follows-click behaviour, and avoiding the
/// surprise where the scroll keys "aim" at a different column than the
/// mouse just poked.
fn focus_panel_under_cursor(app: &mut App, column: u16, total_width: u16) {
    let graph_x = app.graph_sidebar_width(total_width);
    if column < graph_x {
        app.active_panel = Panel::Files;
        return;
    }
    // Right of the graph split. 3-col Graph splits this further.
    if let Some(diff_start) = graph_diff_column_start(app, total_width) {
        if column >= diff_start {
            app.active_panel = Panel::Diff;
        } else {
            app.active_panel = Panel::Commit;
        }
    } else {
        app.active_panel = Panel::Diff;
    }
}

/// Screen column where the Graph 3-col diff column starts. Returns `None`
/// when the Graph tab isn't in 3-col mode so callers can fall through to
/// the 2-col routing. Shares `App::graph_three_col_widths` with `ui::render`
/// — the two paths can't drift apart.
fn graph_diff_column_start(app: &App, total_width: u16) -> Option<u16> {
    if !app.graph_uses_three_col() {
        return None;
    }
    let (_, _, diff_w) = app.graph_three_col_widths(total_width);
    Some(total_width.saturating_sub(diff_w))
}

/// Route a vertical-scroll delta to whichever Graph-tab panel currently
/// owns focus. Panel::Files (the graph sidebar) is handled by the caller
/// — its delta is tied to visual-mode extend vs graph navigation and
/// doesn't reduce to a plain scroll. Panel::Commit always scrolls the
/// commit-detail row list (metadata + files). Panel::Diff scrolls the
/// standalone diff column in 3-col mode, or the whole commit-detail
/// panel in 2-col fallback (where the diff is rendered inline).
fn graph_scroll_right_panel(app: &mut App, delta: i32) {
    use ui::commit_detail_panel;
    match app.active_panel {
        Panel::Files => {}
        Panel::Commit => commit_detail_panel::scroll(app, delta),
        Panel::Diff => {
            if app.graph_uses_three_col() {
                let s = &mut app.commit_detail.file_diff_scroll;
                *s = if delta < 0 {
                    s.saturating_sub((-delta) as usize)
                } else {
                    s.saturating_add(delta as usize)
                };
            } else {
                commit_detail_panel::scroll(app, delta);
            }
        }
    }
}

fn handle_key_graph(key: KeyEvent, app: &mut App) {
    use ui::{commit_detail_panel, git_graph_panel};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // While in visual mode every direction key extends (no Shift needed —
    // works in terminals that intercept Shift+Click / Shift+Arrow for text
    // selection), a mouse click on a commit moves the endpoint, and `V` /
    // `Esc` exits. This is the primary path; Shift+Arrow below is kept as
    // a convenience for terminals that *do* forward the modifier.
    let in_visual = app.git_graph.in_visual_mode() && app.active_panel == Panel::Files;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if !ctrl => {
            if app.active_panel == Panel::Files {
                if shift || in_visual {
                    app.extend_graph_selection(-1);
                } else {
                    git_graph_panel::handle_key(app, "k");
                }
            } else {
                graph_scroll_right_panel(app, -1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') if !ctrl => {
            if app.active_panel == Panel::Files {
                if shift || in_visual {
                    app.extend_graph_selection(1);
                } else {
                    git_graph_panel::handle_key(app, "j");
                }
            } else {
                graph_scroll_right_panel(app, 1);
            }
        }
        // Readline-style nav aliases (parallel to what palettes and
        // Files/Git tabs bind).
        KeyCode::Char('p' | 'k') if ctrl => {
            if app.active_panel == Panel::Files {
                if shift || in_visual {
                    app.extend_graph_selection(-1);
                } else {
                    git_graph_panel::handle_key(app, "k");
                }
            } else {
                graph_scroll_right_panel(app, -1);
            }
        }
        KeyCode::Char('n' | 'j') if ctrl => {
            if app.active_panel == Panel::Files {
                if shift || in_visual {
                    app.extend_graph_selection(1);
                } else {
                    git_graph_panel::handle_key(app, "j");
                }
            } else {
                graph_scroll_right_panel(app, 1);
            }
        }
        KeyCode::PageUp => {
            if app.active_panel == Panel::Files {
                if shift || in_visual {
                    app.extend_graph_selection(-10);
                } else {
                    for _ in 0..10 {
                        git_graph_panel::handle_key(app, "k");
                    }
                }
            } else {
                graph_scroll_right_panel(app, -20);
            }
        }
        KeyCode::PageDown => {
            if app.active_panel == Panel::Files {
                if shift || in_visual {
                    app.extend_graph_selection(10);
                } else {
                    for _ in 0..10 {
                        git_graph_panel::handle_key(app, "j");
                    }
                }
            } else {
                graph_scroll_right_panel(app, 20);
            }
        }
        // `V` (uppercase = Shift+v) toggles visual mode. Entering: anchor
        // collapses onto the cursor (is_range() stays false until the user
        // actually extends), so the status bar can distinguish "armed but
        // empty" from an active range if it wants to.
        KeyCode::Char('V') if app.active_panel == Panel::Files => {
            if app.git_graph.in_visual_mode() {
                app.clear_graph_range();
            } else if !app.git_graph.rows.is_empty() {
                app.git_graph.selection_anchor = Some(app.git_graph.selected_idx);
            }
        }
        // Esc exits visual mode / collapses any range back to single-select.
        // Only consumed when actually armed on the Files panel so higher
        // priority Esc handlers (overlays etc.) aren't shadowed elsewhere.
        KeyCode::Esc if app.active_panel == Panel::Files && app.git_graph.in_visual_mode() => {
            app.clear_graph_range();
        }
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

/// Route a key event to the commit-box buffer when the Git tab's
/// commit input is focused. Mirrors the text-input contract used by
/// `handle_key_search_input_mode`: typing fills the draft, standard
/// nav / edit keys move the cursor, Esc blurs, Ctrl+Enter submits,
/// bare Enter inserts a newline (subject + body pattern). Returns
/// `true` when the key was consumed.
fn handle_key_git_commit(key: KeyEvent, app: &mut App, ctrl: bool, alt: bool) -> bool {
    use crate::app::Panel;
    if app.active_panel != Panel::Files || !app.git_status.commit_editing {
        return false;
    }
    match key.code {
        KeyCode::Esc => {
            app.git_status.commit_editing = false;
            true
        }
        KeyCode::Enter if ctrl => {
            app.run_commit();
            true
        }
        KeyCode::Enter => {
            crate::input_edit::insert_char(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
                '\n',
            );
            true
        }
        KeyCode::Backspace if alt || ctrl => {
            crate::input_edit::delete_word_backward(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Backspace => {
            crate::input_edit::backspace(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Delete if alt || ctrl => {
            crate::input_edit::delete_word_forward(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Delete => {
            crate::input_edit::delete_char_forward(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Left if alt || ctrl => {
            crate::input_edit::move_cursor_word_backward(
                &app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Right if alt || ctrl => {
            crate::input_edit::move_cursor_word_forward(
                &app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Left => {
            crate::input_edit::move_cursor(
                &app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
                -1,
            );
            true
        }
        KeyCode::Right => {
            crate::input_edit::move_cursor(
                &app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
                1,
            );
            true
        }
        KeyCode::Home => {
            app.git_status.commit_cursor = 0;
            true
        }
        KeyCode::End => {
            app.git_status.commit_cursor = app.git_status.commit_message.len();
            true
        }
        KeyCode::Char('u') if ctrl => {
            crate::input_edit::clear(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Char('w') if ctrl => {
            crate::input_edit::delete_word_backward(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
            );
            true
        }
        KeyCode::Char('a') if ctrl => {
            app.git_status.commit_cursor = 0;
            true
        }
        KeyCode::Char('e') if ctrl => {
            app.git_status.commit_cursor = app.git_status.commit_message.len();
            true
        }
        KeyCode::Char(c) if !ctrl => {
            crate::input_edit::insert_char(
                &mut app.git_status.commit_message,
                &mut app.git_status.commit_cursor,
                c,
            );
            true
        }
        _ => false,
    }
}

fn handle_key_git(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // Commit-box input mode owns the keyboard while focused so the
    // letter chords below (s/u/d/…) don't fire mid-message.
    if handle_key_git_commit(key, app, ctrl, alt) {
        return;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if !ctrl => match app.active_panel {
            Panel::Files => app.navigate_files(-1),
            // Git tab has no middle column — Panel::Commit should never
            // be set here, but if it slips through treat it as Diff.
            Panel::Diff | Panel::Commit => {
                app.diff_scroll = app.diff_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') if !ctrl => match app.active_panel {
            Panel::Files => app.navigate_files(1),
            Panel::Diff | Panel::Commit => {
                app.diff_scroll += 1;
            }
        },
        // Readline-style nav aliases. Must come BEFORE the bare
        // `Char('n')` / `Char('d')` arms below, which would otherwise
        // route Ctrl+N to the git-status "No" confirm. The bare
        // letters (n/y/d for confirm / discard chord) stay on their
        // own arms because they check `!ctrl` implicitly via being
        // matched only if the Ctrl arm above didn't fire.
        KeyCode::Char('p' | 'k') if ctrl => match app.active_panel {
            Panel::Files => app.navigate_files(-1),
            Panel::Diff | Panel::Commit => {
                app.diff_scroll = app.diff_scroll.saturating_sub(1);
            }
        },
        KeyCode::Char('n' | 'j') if ctrl => match app.active_panel {
            Panel::Files => app.navigate_files(1),
            Panel::Diff | Panel::Commit => {
                app.diff_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => app.navigate_files(-10),
            Panel::Diff | Panel::Commit => {
                app.diff_scroll = app.diff_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => app.navigate_files(10),
            Panel::Diff | Panel::Commit => {
                app.diff_scroll += 20;
            }
        },
        KeyCode::Left if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            // SBS mode: keyboard pans both halves in lockstep — the user
            // has no mouse-column to disambiguate. Mouse scroll keeps the
            // per-side route from `apply_horizontal_scroll`.
            app.diff_h_scroll = app.diff_h_scroll.saturating_sub(step);
            app.sbs_left_h_scroll = app.sbs_left_h_scroll.saturating_sub(step);
            app.sbs_right_h_scroll = app.sbs_right_h_scroll.saturating_sub(step);
        }
        KeyCode::Right if app.active_panel == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.diff_h_scroll = app.diff_h_scroll.saturating_add(step);
            app.sbs_left_h_scroll = app.sbs_left_h_scroll.saturating_add(step);
            app.sbs_right_h_scroll = app.sbs_right_h_scroll.saturating_add(step);
        }
        KeyCode::Home if app.active_panel == Panel::Diff => {
            app.diff_h_scroll = 0;
            app.sbs_left_h_scroll = 0;
            app.sbs_right_h_scroll = 0;
        }
        KeyCode::End if app.active_panel == Panel::Diff => {
            // render 自动钳到实际最大值
            app.diff_h_scroll = usize::MAX;
            app.sbs_left_h_scroll = usize::MAX;
            app.sbs_right_h_scroll = usize::MAX;
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
            if let Some(sel) = &app.selected_file {
                let workdir = app.backend.workdir_path();
                app.pending_edit = Some(workdir.join(&sel.path));
            }
        }
        _ => {}
    }
}

fn handle_key_files(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if !ctrl => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-1);
                app.load_preview();
            }
            // Files tab has no middle column — Panel::Commit should never
            // be set here, but fall back to Diff behaviour defensively.
            Panel::Diff | Panel::Commit => {
                app.preview_scroll = app.preview_scroll.saturating_sub(1);
            }
        },
        KeyCode::Down | KeyCode::Char('j') if !ctrl => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(1);
                app.load_preview();
            }
            Panel::Diff | Panel::Commit => {
                app.preview_scroll += 1;
            }
        },
        // Readline-style nav: Ctrl+P/K = up, Ctrl+N/J = down. Mirrors
        // the palette bindings so a Vim+Emacs-era user gets the same
        // keys on any list in the app. Guarded behind `ctrl` (the
        // bare letter guards above check `!ctrl`) so pressing `j`
        // without a modifier still navigates normally.
        KeyCode::Char('p' | 'k') if ctrl => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-1);
                app.load_preview();
            }
            Panel::Diff | Panel::Commit => {
                app.preview_scroll = app.preview_scroll.saturating_sub(1);
            }
        },
        KeyCode::Char('n' | 'j') if ctrl => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(1);
                app.load_preview();
            }
            Panel::Diff | Panel::Commit => {
                app.preview_scroll += 1;
            }
        },
        KeyCode::PageUp => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(-10);
                app.load_preview();
            }
            Panel::Diff | Panel::Commit => {
                app.preview_scroll = app.preview_scroll.saturating_sub(20);
            }
        },
        KeyCode::PageDown => match app.active_panel {
            Panel::Files => {
                app.file_tree.navigate(10);
                app.load_preview();
            }
            Panel::Diff | Panel::Commit => {
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
        KeyCode::F(2) => {
            // F2 = Rename — VSCode's default. Opens the inline rename
            // editor on the selected entry. No-op on an empty tree.
            let idx = app.file_tree.selected;
            if let Some(entry) = app.file_tree.entries.get(idx).cloned() {
                let abs = app.file_tree.root.join(&entry.path);
                let parent = abs
                    .parent()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| app.file_tree.root.clone());
                app.begin_tree_edit(
                    crate::tree_edit::TreeEditMode::Rename,
                    parent,
                    Some(abs),
                    Some(idx),
                );
            }
        }
        KeyCode::Delete | KeyCode::Backspace => {
            // Delete / Cmd+Backspace — default is "Move to Trash"
            // (safer, reversible). Shift modifier escalates to the
            // hard-delete path. Backspace aliases Delete so macOS
            // users (who don't have a real Delete key on most
            // keyboards) get the same action.
            let hard = key.modifiers.contains(KeyModifiers::SHIFT);
            prompt_delete_selected(app, hard);
        }
        // Vim-style alias: `d` = Move to Trash, `D` (Shift+d) = hard
        // delete. Parallels `dd` in Vim semantics (delete the current
        // line/selection) — the tree has no motion to compose with, so
        // the single-key form stands in for the chord. Scoped to the
        // Files tab so Git-tab's `d → y` discard chord stays unambiguous.
        // Ctrl / Alt modifiers are rejected so chord bindings like
        // Ctrl+D aren't silently stolen.
        KeyCode::Char(c)
            if matches!(c, 'd' | 'D')
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            let hard = c == 'D' || key.modifiers.contains(KeyModifiers::SHIFT);
            prompt_delete_selected(app, hard);
        }
        _ => {}
    }
}

fn prompt_delete_selected(app: &mut App, hard: bool) {
    let idx = app.file_tree.selected;
    if let Some(entry) = app.file_tree.entries.get(idx).cloned() {
        let abs = app.file_tree.root.join(&entry.path);
        app.prompt_tree_delete(abs, entry.is_dir, hard);
    }
}

// ─── Tree modal keyboard helpers ─────────────────────────────────────────────

/// Tree-edit (inline New File / New Folder / Rename) keyboard owner.
/// Drains every keystroke into the buffer until Enter / Esc / Ctrl+C
/// exits — Tab / Up-Down are intentionally ignored so accidental
/// keyboard navigation can't orphan a half-typed filename.
fn handle_key_tree_edit(key: KeyEvent, app: &mut App) {
    // Any keystroke clears a lingering validation banner — the user
    // is typing, which means they're trying to fix the issue.
    if app.tree_edit.error.is_some() {
        app.tree_edit.error = None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => app.cancel_tree_edit(),
        KeyCode::Char('c') if ctrl => app.cancel_tree_edit(),
        KeyCode::Enter => app.commit_tree_edit(),
        KeyCode::Backspace => {
            if ctrl || key.modifiers.contains(KeyModifiers::ALT) {
                crate::input_edit::delete_word_backward(
                    &mut app.tree_edit.buffer,
                    &mut app.tree_edit.cursor,
                );
            } else {
                crate::input_edit::backspace(&mut app.tree_edit.buffer, &mut app.tree_edit.cursor);
            }
        }
        KeyCode::Char('w') if ctrl => {
            crate::input_edit::delete_word_backward(
                &mut app.tree_edit.buffer,
                &mut app.tree_edit.cursor,
            );
        }
        KeyCode::Char('u') if ctrl => {
            app.tree_edit.buffer.clear();
            app.tree_edit.cursor = 0;
        }
        KeyCode::Left => {
            crate::input_edit::move_cursor(&app.tree_edit.buffer, &mut app.tree_edit.cursor, -1);
        }
        KeyCode::Right => {
            crate::input_edit::move_cursor(&app.tree_edit.buffer, &mut app.tree_edit.cursor, 1);
        }
        KeyCode::Home => {
            app.tree_edit.cursor = 0;
        }
        KeyCode::End => {
            app.tree_edit.cursor = app.tree_edit.buffer.len();
        }
        KeyCode::Char(c) if !ctrl => {
            crate::input_edit::insert_char(&mut app.tree_edit.buffer, &mut app.tree_edit.cursor, c);
        }
        _ => {}
    }
}

/// Keyboard navigation for the right-click context menu popup.
fn handle_key_tree_context_menu(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => app.close_tree_context_menu(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.close_tree_context_menu();
        }
        KeyCode::Up | KeyCode::Char('k') => app.tree_context_menu.navigate(-1),
        KeyCode::Down | KeyCode::Char('j') => app.tree_context_menu.navigate(1),
        KeyCode::Enter => {
            if let Some(item) = app.tree_context_menu.current() {
                app.dispatch_context_menu_item(item);
            }
        }
        // Any other key closes the menu (VSCode behaviour). Prevents
        // the menu from lingering if the user mis-clicks into it.
        _ => app.close_tree_context_menu(),
    }
}

/// Keyboard handler for the Ctrl+O hosts picker overlay. Mirrors the
/// quick-open contract: Esc / Ctrl+C close, Enter commits, arrows
/// navigate, printable chars append to the active input (filter or
/// path). Ctrl+P toggles between the two input modes so the user can
/// swap from "filter my config" to "type a target literally".
fn handle_key_hosts_picker(key: KeyEvent, app: &mut App) {
    use crate::hosts_picker::InputMode;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            if app.hosts_picker.input_mode == InputMode::Path {
                // Esc in path mode drops back to the filter view rather
                // than closing outright — gives the user a way out of
                // the path buffer without losing the picker state.
                app.hosts_picker.input_mode = InputMode::Search;
                app.hosts_picker.path_buffer.clear();
            } else {
                app.close_hosts_picker();
            }
        }
        KeyCode::Char('c') if ctrl => {
            app.close_hosts_picker();
            app.should_quit = true;
        }
        KeyCode::Char('p') if ctrl => {
            app.hosts_picker.enter_path_mode();
        }
        KeyCode::Enter => app.confirm_hosts_picker(),
        KeyCode::Up => app.hosts_picker.move_selection(-1),
        KeyCode::Down => app.hosts_picker.move_selection(1),
        KeyCode::Char('k') if ctrl => app.hosts_picker.move_selection(-1),
        KeyCode::Char('j') if ctrl => app.hosts_picker.move_selection(1),
        KeyCode::Backspace => match app.hosts_picker.input_mode {
            InputMode::Search => {
                app.hosts_picker.filter.pop();
                app.hosts_picker.selected_idx = 0;
            }
            InputMode::Path => {
                app.hosts_picker.path_buffer.pop();
            }
        },
        KeyCode::Char(c) if !ctrl => match app.hosts_picker.input_mode {
            InputMode::Search => {
                app.hosts_picker.filter.push(c);
                app.hosts_picker.selected_idx = 0;
            }
            InputMode::Path => {
                app.hosts_picker.path_buffer.push(c);
            }
        },
        _ => {}
    }
}

/// Y/Esc handler for the status-bar delete confirm.
fn handle_key_tree_delete_confirm(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => app.confirm_tree_delete(),
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('c') => {
            app.cancel_tree_delete()
        }
        _ => {}
    }
}

// ─── Mouse ───────────────────────────────────────────────────────────────────

pub fn handle_mouse<B: Backend>(mouse: MouseEvent, app: &mut App, terminal: &Terminal<B>) {
    // Palettes fully own mouse input while active (global-search first,
    // then quick-open): clicks must not leak through to hidden panels,
    // and scroll wheels inside the popup should move the selection.
    if app.hosts_picker.active {
        handle_mouse_hosts_picker(mouse, app);
        return;
    }
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

    // Mid-edit mouse-button press → cancel the inline editor. Then let
    // the click fall through to normal handling so clicking another
    // row still selects it, clicking a toolbar button still fires, etc.
    // Keeping the edit active across clicks makes the row UI lie about
    // what a subsequent Enter would commit (`parent_dir` is stale).
    // Move events and scroll wheel pass through untouched so hover /
    // scroll keep working while the user types.
    if app.tree_edit.active && matches!(mouse.kind, MouseEventKind::Down(_)) {
        app.cancel_tree_edit();
    }

    // Right-click on the Files tab's tree panel → open context menu.
    // Gated on the hit_test result, not just `active_tab`: right-click
    // on the preview panel, on the toolbar row, or on an empty area
    // outside the tree must NOT open the menu. `TreeClick(idx)` means
    // a row was hit; `TreeClearSelection` means the click landed in
    // the empty space below rows (root-flavoured menu). Every other
    // hit (toolbar buttons, preview content, no-op areas) bails out.
    if let MouseEventKind::Down(MouseButton::Right) = mouse.kind {
        if app.active_tab == Tab::Files
            && !app.tree_edit.active
            && app.tree_delete_confirm.is_none()
        {
            // Second right-click while the menu is already open
            // dismisses it (Finder / VSCode behaviour).
            if app.tree_context_menu.active {
                app.close_tree_context_menu();
                return;
            }
            let opens_menu = match app.hit_registry.hit_test(mouse.column, mouse.row) {
                Some(ui::mouse::ClickAction::TreeClick(idx)) => Some(Some(idx)),
                Some(ui::mouse::ClickAction::TreeClearSelection) => Some(None),
                _ => None,
            };
            if let Some(target) = opens_menu {
                app.open_tree_context_menu(target, (mouse.column, mouse.row));
            }
            return;
        }
    }

    // Clicks while the context menu is open: left-click outside the
    // menu closes it; hit_registry routing to `TreeContextMenuItem`
    // happens through the normal path below.
    // (The fallthrough-close region is registered by the menu renderer
    // underneath the menu panel, so it goes through handle_action.)

    // Preview drag-selection fast-path. Owns Down/Drag/Up(Left) when the
    // gesture starts inside the preview panel. Scroll wheel, right-click,
    // and Down outside the panel fall through to the normal match below.
    //
    // Once a drag has begun, subsequent Drag and Up events are captured
    // unconditionally (even if the cursor leaves the panel) — otherwise
    // selection would silently drop whenever the user pulls past the edge.
    if handle_preview_selection(&mouse, app) {
        return;
    }
    // Diff-panel selection gets the same priority as preview: Down inside
    // the cached diff rect owns the drag through Up, even if the cursor
    // later leaves the panel. Wheel / right-click / Down outside fall
    // through below.
    if handle_diff_selection(&mouse, app) {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            // Click-to-focus: land the active panel on whichever column
            // the click landed in. VSCode-style — previously you had to
            // Shift+Tab into a column before its arrows responded.
            if let Ok(size) = terminal.size() {
                focus_panel_under_cursor(app, mouse.column, size.width);
            }

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
                // Shift+Click on a graph row = extend the range, for
                // terminals that actually forward Shift+Click to the app.
                // Most macOS terminals intercept this for text selection;
                // those users should press `V` to enter visual mode and
                // click normally instead — the in-visual-mode click path
                // lives in `git_graph_panel::handle_command`.
                if mouse.modifiers.contains(KeyModifiers::SHIFT)
                    && let ui::mouse::ClickAction::GitCommand { command, args, .. } = &effective
                    && command == "git.selectCommit"
                    && let Some(oid) = args.get("oid").and_then(|v| v.as_str())
                    && let Some(target_idx) = app.git_graph.find_row_by_oid(oid)
                {
                    let delta = target_idx as i32 - app.git_graph.selected_idx as i32;
                    app.extend_graph_selection(delta);
                    app.last_click = if is_double {
                        None
                    } else {
                        Some((now, mouse.column, mouse.row))
                    };
                    return;
                }
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
            app.dragging_graph_diff_split = false;
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            if app.dragging_split {
                if total_width > 0 {
                    let percent = (mouse.column * 100 / total_width).clamp(10, 80);
                    app.split_percent = percent;
                }
            } else if app.dragging_graph_diff_split && total_width > 0 {
                // Boundary is inside the non-graph region. Express the drag
                // position as the diff column's fraction of the remainder,
                // measured from the right edge so "pull left" = grow diff.
                let graph_x = total_width * app.split_percent / 100;
                let remainder = total_width.saturating_sub(graph_x);
                if remainder > 0 {
                    let from_right = total_width.saturating_sub(mouse.column);
                    let diff_pct = (from_right as u32 * 100 / remainder as u32) as u16;
                    // Floor 20 / ceiling 80 keeps both sub-columns usable
                    // and leaves room for drag to snap back either way.
                    app.graph_diff_split_percent = diff_pct.clamp(20, 80);
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
            // Use the shared clamp + sidebar-hidden short-circuit so wheel
            // routing lines up with hit-testing. With sidebar hidden
            // `graph_sidebar_width` returns 0 and `is_left` never fires.
            let split_x = app.graph_sidebar_width(total_width);
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
                    } else if let Some(diff_start) = graph_diff_column_start(app, total_width)
                        && mouse.column >= diff_start
                    {
                        // 3-col diff column — scroll only the diff viewport
                        // so commit metadata under the cursor's path stays put.
                        app.commit_detail.file_diff_scroll =
                            app.commit_detail.file_diff_scroll.saturating_sub(3);
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
            let split_x = app.graph_sidebar_width(total_width);
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
                    } else if let Some(diff_start) = graph_diff_column_start(app, total_width)
                        && mouse.column >= diff_start
                    {
                        app.commit_detail.file_diff_scroll += 3;
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

/// Drive in-panel text selection on the preview panel. Returns `true` when
/// the mouse event was consumed so the caller stops further dispatch.
///
/// Click levels (same position within `DOUBLE_CLICK_WINDOW`):
/// - 1× (single drag) → anchor-to-cursor drag selection
/// - 2× (double-click) → select word under cursor
/// - 3× (triple-click) → select entire line
///
/// For levels 2 and 3 the initial selection is committed immediately on
/// `Down`, but `dragging = true` so the `Up` handler still triggers the
/// clipboard copy. A `Drag` after a double/triple click extends the active
/// endpoint normally (VS Code-style word-range extension).
fn handle_preview_selection(mouse: &MouseEvent, app: &mut App) -> bool {
    let content_origin = app.last_preview_content_origin;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(rect) = app.last_preview_rect else {
                return false;
            };
            if !point_in_rect(rect, mouse.column, mouse.row) {
                return false;
            }
            let Some(origin) = content_origin else {
                return false;
            };
            let Some((file_line, byte_offset)) =
                mouse_to_file_coord(app, mouse.column, mouse.row, origin)
            else {
                return false;
            };

            // Advance (or reset) the click counter.
            let now = Instant::now();
            let click_count = if let Some((t, c, r, n)) = app.preview_click_state {
                if c == mouse.column
                    && r == mouse.row
                    && now.duration_since(t) < DOUBLE_CLICK_WINDOW
                {
                    (n + 1).min(3)
                } else {
                    1
                }
            } else {
                1
            };
            app.preview_click_state = Some((now, mouse.column, mouse.row, click_count));

            let preview_lines: &[String] = app
                .preview_content
                .as_ref()
                .and_then(|p| match &p.body {
                    crate::file_tree::PreviewBody::Text { lines, .. } => Some(lines.as_slice()),
                    _ => None,
                })
                .unwrap_or_default();
            let line_len = preview_lines.get(file_line).map(|l| l.len()).unwrap_or(0);

            let sel = match click_count {
                2 => {
                    // Double-click → select word
                    let word = preview_lines
                        .get(file_line)
                        .map(|l| word_at_byte(l, byte_offset))
                        .unwrap_or(byte_offset..byte_offset);
                    PreviewSelection {
                        anchor: (file_line, word.start),
                        active: (file_line, word.end),
                        dragging: true,
                    }
                }
                3 => {
                    // Triple-click → select entire line
                    PreviewSelection {
                        anchor: (file_line, 0),
                        active: (file_line, line_len),
                        dragging: true,
                    }
                }
                _ => PreviewSelection::new((file_line, byte_offset)),
            };
            app.preview_selection = Some(sel);
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let dragging = app.preview_selection.is_some_and(|s| s.dragging);
            if !dragging {
                return false;
            }
            let Some(origin) = content_origin else {
                return true; // swallow even without update — the drag is ours
            };
            if let Some(pos) = mouse_to_file_coord(app, mouse.column, mouse.row, origin) {
                if let Some(s) = app.preview_selection.as_mut() {
                    s.active = pos;
                }
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(sel) = app.preview_selection.as_mut() else {
                return false;
            };
            if !sel.dragging {
                return false;
            }
            sel.dragging = false;
            let sel_snapshot = *sel;
            if !sel_snapshot.is_empty() {
                if let Some(preview) = app.preview_content.as_ref() {
                    // Only text bodies have selectable lines — image/binary
                    // previews have no `lines` vector.
                    if let crate::file_tree::PreviewBody::Text { lines, .. } = &preview.body {
                        let text = collect_selected_text(lines, &sel_snapshot);
                        if !text.is_empty() {
                            match clipboard::copy_to_clipboard(&text) {
                                Ok(()) => app.toasts.push(Toast::info(t(Msg::ClipboardCopied))),
                                Err(_) => {
                                    app.toasts.push(Toast::error(t(Msg::ClipboardCopyFailed)))
                                }
                            }
                        }
                    }
                }
            }
            true
        }
        _ => false,
    }
}

/// Drive in-panel text selection on the diff panel. Returns `true` when
/// the mouse event was consumed. Mirrors `handle_preview_selection` shape —
/// click levels, anchor-drag-extend-on-Drag, copy-on-Up — but works on the
/// flattened display-row list in `DiffHit` instead of file lines.
///
/// SBS side lock: the Down gesture picks a side (left/right of the divider,
/// or `Unified` in unified layout) and stores it with the selection. A
/// subsequent Drag clamps the cursor column into that side's content area
/// before translating to byte offsets, so crossing the divider extends
/// vertically along the anchored side instead of flipping. Matches VSCode's
/// diff editor.
fn handle_diff_selection(mouse: &MouseEvent, app: &mut App) -> bool {
    let Some(rect) = app.last_diff_rect else {
        return false;
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !point_in_rect(rect, mouse.column, mouse.row) {
                return false;
            }
            let Some(hit) = app.last_diff_hit.as_ref() else {
                return false;
            };
            if hit.rows.is_empty() {
                return false;
            }
            let side = hit.side_for_column(mouse.column);
            let Some((row_idx, byte_offset)) = hit.coord_for(mouse.column, mouse.row, side) else {
                return false;
            };

            // Focus follows the click — otherwise the user is stuck
            // scrolling the panel they came from (common in Graph 3-col:
            // start on Panel::Commit, click into diff, expect arrows to
            // pan the diff). Mirror of `focus_panel_under_cursor` but
            // local — the main-dispatcher version never runs because
            // this handler returns early on Down.
            app.active_panel = Panel::Diff;

            // Advance (or reset) the click counter — same 400 ms window as
            // the preview panel so users get consistent double/triple-click
            // timing across both surfaces.
            let now = Instant::now();
            let click_count = if let Some((t, c, r, n)) = app.diff_click_state {
                if c == mouse.column
                    && r == mouse.row
                    && now.duration_since(t) < DOUBLE_CLICK_WINDOW
                {
                    (n + 1).min(3)
                } else {
                    1
                }
            } else {
                1
            };
            app.diff_click_state = Some((now, mouse.column, mouse.row, click_count));

            let row_text = hit.rows[row_idx].text_for(side).to_string();
            let sel = match click_count {
                2 => {
                    let word = word_at_byte(&row_text, byte_offset);
                    PreviewSelection {
                        anchor: (row_idx, word.start),
                        active: (row_idx, word.end),
                        dragging: true,
                    }
                }
                3 => PreviewSelection {
                    anchor: (row_idx, 0),
                    active: (row_idx, row_text.len()),
                    dragging: true,
                },
                _ => PreviewSelection::new((row_idx, byte_offset)),
            };
            app.diff_selection = Some(DiffSelection { sel, side });
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let dragging = app.diff_selection.is_some_and(|s| s.sel.dragging);
            if !dragging {
                return false;
            }
            let Some(hit) = app.last_diff_hit.as_ref() else {
                return true;
            };
            let side = app.diff_selection.unwrap().side;
            // Clamp the cursor column back into the anchor's side before
            // translating — this is the SBS side-lock. In Unified the
            // clamp is a no-op (no divider).
            let clamped_col = clamp_col_to_side(hit, mouse.column, side);
            if let Some(pos) = hit.coord_for(clamped_col, mouse.row, side) {
                if let Some(s) = app.diff_selection.as_mut() {
                    s.sel.active = pos;
                }
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(sel) = app.diff_selection.as_mut() else {
                return false;
            };
            if !sel.sel.dragging {
                return false;
            }
            sel.sel.dragging = false;
            let snap = *sel;
            if !snap.sel.is_empty() {
                if let Some(hit) = app.last_diff_hit.as_ref() {
                    let text = collect_diff_selected_text(hit, &snap);
                    if !text.is_empty() {
                        match clipboard::copy_to_clipboard(&text) {
                            Ok(()) => app.toasts.push(Toast::info(t(Msg::ClipboardCopied))),
                            Err(_) => app.toasts.push(Toast::error(t(Msg::ClipboardCopyFailed))),
                        }
                    }
                }
            }
            true
        }
        _ => false,
    }
}

/// Clamp `col` into the content range of the given SBS side so drag-
/// through-divider doesn't bleed the selection onto the other half.
/// In Unified layout there's nothing to clamp and `col` passes through.
fn clamp_col_to_side(hit: &crate::ui::selection::DiffHit, col: u16, side: DiffSide) -> u16 {
    match (hit.layout, side) {
        (crate::app::DiffLayout::Unified, _) => col,
        (crate::app::DiffLayout::SideBySide, DiffSide::Unified) => col,
        (crate::app::DiffLayout::SideBySide, DiffSide::SbsLeft) => {
            // Right edge of the left half is just before `right_start_x`
            // (which is the divider column). `saturating_sub(1)` keeps us
            // on the left half's last content column.
            col.min(hit.right_start_x.saturating_sub(1))
        }
        (crate::app::DiffLayout::SideBySide, DiffSide::SbsRight) => col.max(hit.right_start_x),
    }
}

fn point_in_rect(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Translate a terminal `(column, row)` hit into `(file_line_index,
/// byte_offset_in_line)` using the cached content-area origin + current
/// scroll state. Returns `None` when the preview is empty / unloaded.
///
/// Columns left of the gutter collapse to byte 0; rows above the content area
/// collapse to file line `preview_scroll` (first visible line). Rows past the
/// last line clamp to the last line's terminator (so "drag past end" selects
/// through the final line cleanly).
fn mouse_to_file_coord(
    app: &App,
    col: u16,
    row: u16,
    origin: (u16, u16, u16),
) -> Option<(usize, usize)> {
    let preview = app.preview_content.as_ref()?;
    let lines = match &preview.body {
        crate::file_tree::PreviewBody::Text { lines, .. } => lines,
        // Image / binary cards don't have per-line content, so
        // drag-select is a no-op over them.
        _ => return None,
    };
    if lines.is_empty() {
        return None;
    }
    let (content_x, content_y, _) = origin;

    let visible_row = row.saturating_sub(content_y) as usize;
    let file_line = (app.preview_scroll + visible_row).min(lines.len() - 1);
    let line = &lines[file_line];

    let visible_col = (col.saturating_sub(content_x) as usize) + app.preview_h_scroll;
    let byte_offset = col_to_byte_offset(line, visible_col);

    Some((file_line, byte_offset))
}

/// `true` when a cursor at `column` lands on the left half of an SBS panel
/// spanning `[panel_start, panel_start + panel_w)`. Shared between the Git
/// and Graph SBS routing so they can't drift apart.
fn sbs_cursor_on_left(panel_start: u16, panel_w: u16, column: u16) -> bool {
    let panel_mid = panel_start.saturating_add(panel_w / 2);
    column < panel_mid
}

/// Apply a horizontal-scroll delta (in display columns) to whichever panel
/// the cursor sits over. Routed from Shift+wheel, trackpad ScrollLeft/Right,
/// and bare ← / → keys. Tab::Search is the only tab whose LEFT panel also
/// h-scrolls (the results list) — other tabs' left panels are tree/list
/// widgets with no long horizontal content.
fn apply_horizontal_scroll(app: &mut App, column: u16, total_width: u16, delta: i32) {
    // Use the shared sidebar clamp so this matches `ui::render` even when
    // `split_percent` lives near its edges on narrow terminals.
    let split_x = app.graph_sidebar_width(total_width);
    let is_left = column < split_x;

    let target: Option<&mut usize> = match (app.active_tab, is_left) {
        (Tab::Search, true) => Some(&mut app.global_search.results_h_scroll),
        (_, true) => None,
        (Tab::Files, false) => Some(&mut app.preview_h_scroll),
        (Tab::Git, false) => match app.diff_layout {
            // Unified: one h_scroll applies to the whole content column.
            crate::app::DiffLayout::Unified => Some(&mut app.diff_h_scroll),
            // SBS: each half scrolls independently (old vs new version
            // line widths often diverge — rename, large rewrite). Route
            // to whichever side the cursor sits over.
            crate::app::DiffLayout::SideBySide => {
                let panel_w = total_width.saturating_sub(split_x);
                if sbs_cursor_on_left(split_x, panel_w, column) {
                    Some(&mut app.sbs_left_h_scroll)
                } else {
                    Some(&mut app.sbs_right_h_scroll)
                }
            }
        },
        (Tab::Search, false) => Some(&mut app.preview_h_scroll),
        (Tab::Graph, false) => {
            // In 3-col mode the right portion is [commit | diff]; figure
            // out which column the cursor sits over so h_scroll targets
            // the right triad.
            let diff_start = graph_diff_column_start(app, total_width).unwrap_or(total_width);
            let in_diff_column = diff_start < total_width && column >= diff_start;
            match app.commit_detail.diff_layout {
                crate::app::DiffLayout::Unified => {
                    if in_diff_column {
                        Some(&mut app.commit_detail.file_diff_h_scroll)
                    } else {
                        Some(&mut app.commit_detail.diff_h_scroll)
                    }
                }
                crate::app::DiffLayout::SideBySide => {
                    let (panel_start, panel_w, left_h, right_h) = if in_diff_column {
                        let panel_w = total_width.saturating_sub(diff_start);
                        (
                            diff_start,
                            panel_w,
                            &mut app.commit_detail.file_diff_sbs_left_h_scroll,
                            &mut app.commit_detail.file_diff_sbs_right_h_scroll,
                        )
                    } else {
                        let panel_w = diff_start.saturating_sub(split_x);
                        (
                            split_x,
                            panel_w,
                            &mut app.commit_detail.sbs_left_h_scroll,
                            &mut app.commit_detail.sbs_right_h_scroll,
                        )
                    };
                    if sbs_cursor_on_left(panel_start, panel_w, column) {
                        Some(left_h)
                    } else {
                        Some(right_h)
                    }
                }
            }
        }
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

// ─── Hosts picker overlay ────────────────────────────────────────────────────

/// Mouse dispatch for the Ctrl+O hosts picker. Clicks inside the popup
/// select a row (and double-click commits); clicks outside dismiss the
/// overlay, matching the quick-open / global-search click-away behaviour.
fn handle_mouse_hosts_picker(mouse: MouseEvent, app: &mut App) {
    let popup = match app.hosts_picker.last_popup_area {
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
                app.close_hosts_picker();
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
            if let Some(ui::mouse::ClickAction::HostsPickerSelect(idx)) =
                app.hit_registry.hit_test(mouse.column, mouse.row)
            {
                app.hosts_picker.selected_idx = idx;
                if is_double {
                    app.confirm_hosts_picker();
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
            app.hosts_picker.move_selection(-3);
        }
        MouseEventKind::ScrollDown if inside => {
            app.hosts_picker.move_selection(3);
        }
        _ => {}
    }
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
/// framing varies — and multi-file drops use *different separators* per
/// terminal:
///
/// - iTerm2: each path single-quote wrapped, **space-separated** on a
///   single line. Single paths may still arrive per-line.
/// - Ghostty / WezTerm / Alacritty / Kitty: raw paths with `\ ` escaping
///   spaces, **space-separated** (no quotes).
/// - Terminal.app: `\ ` escaped, space-separated.
/// - GNOME Terminal / older: `file:///…` URIs, typically newline-separated.
///
/// So we do two-level splitting: first by newline (which `file://` URIs
/// and GNOME-style drops rely on), then shell-tokenize each line so a
/// line like `'/a/b.txt' '/c/d.txt'` or `/a/b /c/d` yields two tokens.
///
/// Every candidate must be an absolute path (drops from Finder always
/// are) that `exists()` on disk. Relative paths and non-existent strings
/// are rejected outright — a user pasting the word `settings.json` into
/// the quick-open palette must NOT trip place mode, and the
/// absolute-path requirement is what makes that reliable.
///
/// Returns an empty vector when the payload is "not a drop"; callers
/// use that as the signal to forward the paste to the focused text input.
pub fn parse_dropped_paths(s: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for token in shell_tokenize(line) {
            let Some(p) = normalize_token(&token) else {
                continue;
            };
            if p.is_absolute() && p.exists() {
                out.push(p);
            }
        }
    }
    out
}

/// Shell-style tokenize: split on unquoted whitespace, respecting matched
/// single/double quote regions and backslash escapes. Keeps multi-file
/// drops like `'/a/b' '/c/d'` or `/a/b /c\ d` as separate tokens while
/// leaving `'hello world.txt'` (quoted intra-path space) as one.
fn shell_tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if c.is_whitespace() && !in_single && !in_double {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(c);
    }
    if escaped {
        // Dangling backslash at EOL — keep it literal rather than dropping.
        cur.push('\\');
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Convert an already-unquoted, already-unescaped token into a path.
/// Only the `file://` URI scheme needs handling here (quotes and
/// backslash escapes are consumed by `shell_tokenize`).
fn normalize_token(raw: &str) -> Option<std::path::PathBuf> {
    if raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_prefix("file://") {
        let path_part = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = url_decode(path_part);
        if decoded.is_empty() {
            return None;
        }
        return Some(std::path::PathBuf::from(decoded));
    }
    Some(std::path::PathBuf::from(raw))
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

    #[test]
    fn multi_file_iterm2_single_quoted_space_separated() {
        // iTerm2 default: `'/path/a.txt' '/path/b.txt'` on one line.
        let tmp = TempDir::new().unwrap();
        let a = touch(tmp.path(), "a.txt");
        let b = touch(tmp.path(), "b.txt");
        let paste = format!("'{}' '{}'", a.to_string_lossy(), b.to_string_lossy());
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn multi_file_ghostty_backslash_escaped_space_separated() {
        // Ghostty / WezTerm / Terminal.app: `/path/a /path/b` with `\ ` for
        // embedded spaces.
        let tmp = TempDir::new().unwrap();
        let a = touch(tmp.path(), "a b.txt");
        let c = touch(tmp.path(), "c.txt");
        let paste = format!(
            "{} {}",
            a.to_string_lossy().replace(' ', r"\ "),
            c.to_string_lossy()
        );
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![a, c]);
    }

    #[test]
    fn multi_file_mixed_newline_and_space() {
        // Tolerate payloads that mix the two separators.
        let tmp = TempDir::new().unwrap();
        let a = touch(tmp.path(), "a.txt");
        let b = touch(tmp.path(), "b.txt");
        let c = touch(tmp.path(), "c.txt");
        let paste = format!(
            "{} {}\n{}",
            a.to_string_lossy(),
            b.to_string_lossy(),
            c.to_string_lossy()
        );
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![a, b, c]);
    }

    #[test]
    fn quoted_path_with_intra_space_stays_single_token() {
        // `'hello world.txt'` must remain one path, not two.
        let tmp = TempDir::new().unwrap();
        let file = touch(tmp.path(), "hello world.txt");
        let paste = format!("'{}'", file.to_string_lossy());
        let got = parse_dropped_paths(&paste);
        assert_eq!(got, vec![file]);
    }
}
