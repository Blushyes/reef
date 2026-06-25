//! Keyboard + mouse dispatch. `main.rs` delegates everything below the
//! event-drain loop to `handle_key` and `handle_mouse` here, so the binary
//! entry point stays focused on terminal bootstrap.
//!
//! The one exception is the `show_help` dismiss, which stays inline in
//! `main.rs` because it's simple enough that splitting it out would just
//! add indirection.

use crate::TuiApp as App;
use crate::find_widget;
use crate::global_search;
use crate::i18n::{Msg, t};
use crate::keymap::{Command, InputScope, Keymap, scope_for_app};
use crate::quick_open;
use crate::search;
use crate::settings;
use crate::ui;
use crate::ui::selection::{
    DiffSelection, PreviewSelection, col_to_byte_offset, collect_commit_detail_selected_text,
    collect_diff_selected_text, collect_selected_text_from_rows, word_at_byte,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use reef_app::{AppCommand, AppPanel as Panel, AppTab as Tab, DbNav, NavAnchor, Toast, ViewMode};
use reef_core::diff::{DiffLayout, DiffSide};
use std::time::{Duration, Instant};

pub const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

fn dispatch_clipboard_copy(app: &mut App, text: String) {
    app.engine.dispatch(AppCommand::CopyToClipboard {
        text,
        success: Some(Toast::info(t(Msg::ClipboardCopied))),
        failure: Toast::error(t(Msg::ClipboardCopyFailed)),
    });
}

fn input_modifiers(mods: KeyModifiers) -> reef_app::InputModifiers {
    reef_app::InputModifiers {
        alt: mods.contains(KeyModifiers::ALT),
        ctrl: mods.contains(KeyModifiers::CONTROL),
        shift: mods.contains(KeyModifiers::SHIFT),
    }
}

/// How long a primed Space leader stays valid before being discarded. Long
/// enough for a deliberate "Space, p" chord on a keyboard, short enough
/// that a forgotten leader doesn't steal the next unrelated keypress.
pub const LEADER_TIMEOUT: Duration = Duration::from_millis(800);

/// How long a primed `g` chord stays live before it lapses. Covers the
/// `gg` (scroll-to-top) double-tap and the `gd`/`gr` (goto-definition /
/// find-references) chords. Named so the resolver in `handle_key` and
/// the FocusedPreview bypass share one value instead of repeating the
/// magic number (they had drifted from each other).
pub const G_CHORD_TIMEOUT: Duration = Duration::from_millis(500);

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
    //
    // Lowercase letters arm on the bare key. Uppercase variants accept
    // either no modifier (CapsLock-style) or just SHIFT — without the
    // SHIFT branch most terminals would never deliver a `Space+Shift+F`
    // chord because crossterm reports the SHIFT modifier alongside the
    // uppercase char.
    let is_lower_chord = matches!(
        key.code,
        KeyCode::Char('p') | KeyCode::Char('f') | KeyCode::Char('h') | KeyCode::Char('v')
    ) && key.modifiers.is_empty();
    let is_upper_chord = matches!(
        key.code,
        KeyCode::Char('P') | KeyCode::Char('F') | KeyCode::Char('H') | KeyCode::Char('V')
    ) && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT);
    let is_chord_target = is_lower_chord || is_upper_chord;

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
        // Stale or non-target → Consume clears the leader at the call
        // site, so a single keystroke after timeout resets state cleanly.
        // The destructive-key risk this used to carry in FocusedPreview
        // (Space + idle + stray `d` → discard) is now bounded by the
        // bypass's own timeout check at `handle_key_focused_preview`'s
        // entry — see comment there.
        return LeaderVerdict::Consume;
    }

    LeaderVerdict::None
}

// ─── Keyboard ────────────────────────────────────────────────────────────────

pub fn handle_key(key: KeyEvent, app: &mut App) {
    let scope = scope_for_app(app);

    // SQLite goto-page input — fully owns input while active. Sits at
    // the top of the gate stack because it's an inline prompt that
    // can be invoked from inside the Files tab without crossing any
    // other modal; the user expects "every keystroke goes into the
    // page-number buffer" while it's up.
    if scope == InputScope::DbGoto {
        handle_key_db_goto(key, app);
        return;
    }

    // Hosts picker (Ctrl+O) — fully owns input while active, same contract
    // as the other overlays.
    if scope == InputScope::HostsPicker {
        handle_key_hosts_picker(key, app);
        return;
    }

    // Graph branch picker (`b` on Graph tab) — same exclusive ownership
    // as the other overlays. Sits next to `hosts_picker` because it's a
    // peer overlay (filter input + commit-on-Enter + Esc-cancel).
    if scope == InputScope::GraphBranchPicker {
        handle_key_graph_branch_picker(key, app);
        return;
    }

    // VSCode-style find widget — fully owns input while active. Sits
    // above the global-search palette because the widget is opened by
    // a Space leader chord (`Space+F`) and is the most-recently-opened
    // overlay; routing should land here before any persistent palette.
    if scope == InputScope::FindWidget {
        find_widget::handle_key(key, app);
        return;
    }

    // Global-search palette — fully owns input while active.
    if scope == InputScope::GlobalSearch {
        global_search::handle_key(key, app);
        return;
    }

    // Quick-open palette has the next-highest priority — while active it
    // fully owns input (character append, cursor, Enter/Esc, Space-P close).
    if scope == InputScope::QuickOpen {
        quick_open::handle_key(key, app);
        return;
    }

    // Search mode has priority over everything else — the prompt fully owns
    // input (character append, Backspace, Enter/Esc) while active.
    if scope == InputScope::VimSearch {
        search::handle_key_in_search_mode(key, app);
        return;
    }

    // Inline tree editor (New File / New Folder / Rename): while
    // `tree_edit.active`, every non-Ctrl-C keystroke goes into the
    // editable buffer. Priority-wise this sits above place-mode and
    // the context menu so a stray right-click or drop can't yank the
    // cursor out from under a half-typed filename.
    if scope == InputScope::TreeEdit {
        handle_key_tree_edit(key, app);
        return;
    }

    // Right-click context menu: while visible it owns the keyboard —
    // arrow keys navigate, Enter fires, Esc closes. Any other key
    // closes the menu (VSCode behaviour — keeps the user from
    // accidentally leaving a menu lingering).
    if scope == InputScope::TreeContextMenu {
        handle_key_tree_context_menu(key, app);
        return;
    }

    // Multi-candidate goto-definition popup. Same exclusive-ownership
    // contract as the context menu — Up/Down/k/j move, Enter picks,
    // Esc/q/any-other-key dismiss without picking.
    if scope == InputScope::NavCandidates {
        handle_key_nav_candidates(key, app);
        return;
    }

    // Generic confirm modal owns the keyboard while up. Matches the
    // paste-conflict / context-menu pattern: explicit early-return
    // before any global hotkeys so e.g. `q` can't fall through to
    // "quit" while a "discard changes?" confirm is on screen.
    if scope == InputScope::ConfirmModal {
        handle_key_confirm_modal(key, app);
        return;
    }

    // Paste-conflict prompt: status-bar takeover with R/S/K/A/C +
    // Shift+R/S for "apply to all". Sits above place-mode so a paste
    // landing while place-mode is somehow also armed (defensive — the
    // dispatcher gates them mutually exclusive) doesn't lose the
    // conflict prompt.
    if scope == InputScope::PasteConflict {
        handle_key_paste_conflict(key, app);
        return;
    }

    // Place mode (drag-and-drop destination picker) is mouse-first —
    // most keystrokes are ignored so a stray keypress can't accidentally
    // commit a copy. Exceptions: Esc cancels the mode, and the two
    // terminal-standard quit keys (bare `q` and Ctrl+C) still fire so a
    // user who feels stuck can always bail out of the whole app without
    // Esc'ing first.
    if scope == InputScope::PlaceMode {
        match Keymap::resolve(InputScope::PlaceMode, &key) {
            Some(Command::Close) => app.exit_place_mode(),
            Some(Command::Quit) => app.quit(),
            _ => {}
        }
        return;
    }

    // Intra-tree drag in progress: Esc cancels, Ctrl+C / `q` still
    // quit. Other keys are ignored — the actual move/copy commit
    // happens on `Up(Left)`, not on a keystroke.
    if scope == InputScope::TreeDrag {
        match Keymap::resolve(InputScope::TreeDrag, &key) {
            Some(Command::Close) => app.cancel_tree_drag(),
            Some(Command::Quit) => app.quit(),
            _ => {
                // Live modifier tracking — user might press / release
                // Alt mid-drag to flip move↔copy before releasing the
                // mouse. Crossterm KeyEvent carries the current
                // modifier set on every key event.
                app.update_tree_drag_modifiers(key.modifiers);
            }
        }
        return;
    }

    // Settings owns every key while open. Sits below the modal gates
    // above so a half-typed filename / commit message isn't yanked
    // away, and above the global keymap below so Ctrl+, can flip to
    // Esc-returns semantics inside.
    if scope == InputScope::Settings {
        handle_key_settings(key, app);
        return;
    }

    // FocusedPreview ("纯预览") strips chrome and gives the whole frame
    // to the active panel. Only a tiny key set is meaningful inside:
    // Esc / q exit, Ctrl+C still quits, and a handful of nav keys fall
    // through to the normal dispatcher so scroll/search continue to
    // work on the visible diff/preview.
    if scope == InputScope::FocusedPreview {
        if handle_key_focused_preview(key, app) {
            return;
        }
        // Fallthrough lets navigation/scroll/search keys (↑/↓/PgUp/
        // PgDn/Home/End/←/→/j/k/g/G/n/N/m/f/`/` etc.) keep their
        // normal meaning against the visible panel. The Space-leader
        // chord above already ran for any key reaching here, and
        // overlays would have early-returned earlier in this fn.
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
    let search_input_focused = app.engine.active_tab() == Tab::Search
        && app.engine.active_panel() == Panel::Files
        && app.engine.snapshot().overlays.search_input;
    let commit_input_focused = app.engine.active_tab() == Tab::Git
        && app.engine.active_panel() == Panel::Files
        && app.engine.is_commit_editing();
    let in_input_mode = search_input_focused || commit_input_focused;
    // In Tab::Search list mode + replace_open, bare `Space` is the
    // per-match toggle — disarm the leader chord so a single tap of
    // Space doesn't ambiguously prime a chord and never resolve.
    let search_list_replace_mode = app.engine.active_tab() == Tab::Search
        && app.engine.active_panel() == Panel::Files
        && !app.engine.snapshot().overlays.search_input
        && app.engine.snapshot().search.replace_open;
    let leader_allow_arm = if search_input_focused {
        app.engine.snapshot().search.query.is_empty()
    } else if commit_input_focused {
        app.engine.commit_message_is_empty()
    } else {
        !search_list_replace_mode
    };

    if in_input_mode
        && let Some(command) = Keymap::resolve(scope, &key)
        && dispatch_input_scope_command(command, app)
    {
        return;
    }

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
            app.engine.dispatch(AppCommand::CloseActivePalettes);
            match Keymap::resolve_chord(InputScope::Normal, KeyCode::Char(' '), &key) {
                Some(Command::OpenQuickOpen) => quick_open::begin(app),
                // Space+F = VSCode-style find widget (selection-seeded,
                // floating in the active content panel's upper-right).
                // Space+Shift+F = global ripgrep search (VSCode
                // Cmd+Shift+F). Case differentiation makes these distinct
                // entry points.
                Some(Command::OpenFindWidget) => find_widget::begin_with_selection(app),
                Some(Command::OpenGlobalSearch) => global_search::begin(app),
                Some(Command::OpenGlobalReplace) => {
                    // Open Tab::Search with the replace input pre-expanded
                    // and focused. Mirrors VSCode's Ctrl+Shift+H — the
                    // user gets straight to the replace field. Existing
                    // search state (query, results) is preserved.
                    app.engine.dispatch(AppCommand::OpenGlobalReplaceTab);
                    app.drain_engine_runtime_events();
                }
                // Space+V: 纯预览 toggle — maximise the active tab's
                // content panel (file preview on Files/Search, diff on
                // Git/Graph) from Main; exit back to Main if already
                // focused. Esc inside the mode also exits.
                Some(Command::ToggleFocusedPreview) => app.toggle_focused_preview(),
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

    // vim `gg` / `G` — jump the active preview to top / bottom. Sits
    // between the Space-leader chord and the global keymap so the chord
    // works in both Main and FocusedPreview without each per-tab handler
    // having to reimplement it. Suppressed in:
    //
    // - input modes (search query / commit message / db-goto-page input);
    // - any modal overlay (find widget / global / quick / hosts pickers,
    //   tree edit, search-tab replace mode);
    // - SQLite preview, so bare `g` keeps its goto-page meaning down in
    //   `handle_key_files`.
    //
    // `g` arms the chord; a second `g` within 500 ms fires `to_top` and
    // anything else cancels. `G` is a single-shot `to_bottom` that also
    // clears any pending `g` so an unrelated stutter doesn't strand state.
    let gg_suppressed = in_input_mode
        || app.engine.search().active
        || app.engine.find_widget().active
        || {
            let overlays = app.engine.snapshot().overlays;
            overlays.global_search
                || overlays.quick_open
                || overlays.hosts_picker
                || overlays.graph_branch_picker
        }
        || app.engine.tree_edit_active()
        || app.engine.db_goto_active()
        || app.is_sqlite_preview();
    if !gg_suppressed {
        // Use `contains` + a Ctrl/Alt veto rather than strict equality on
        // `modifiers`, so terminals that surface extra modifier bits
        // alongside Shift (kitty enhanced keyboard, some IME paths) still
        // deliver `G` correctly. Matches the convention in the rest of
        // this file (`if !ctrl` guards on per-tab handlers).
        let no_ctrl_alt = !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        let is_bare_g = key.code == KeyCode::Char('g')
            && no_ctrl_alt
            && !key.modifiers.contains(KeyModifiers::SHIFT);
        let is_shift_g = key.code == KeyCode::Char('G') && no_ctrl_alt;
        if is_shift_g {
            app.clear_g_chord();
            app.scroll_active_preview_to_bottom();
            return;
        }
        if is_bare_g {
            let now = Instant::now();
            if let Some(t0) = app.g_pending_at.take() {
                if now.duration_since(t0) < G_CHORD_TIMEOUT {
                    app.scroll_active_preview_to_top();
                    return;
                }
            }
            app.g_pending_at = Some(now);
            return;
        }
        if let Some(t0) = app.g_pending_at {
            let now = Instant::now();
            let chord_fresh = now.duration_since(t0) < G_CHORD_TIMEOUT;
            // `gd` — goto-definition at the current preview cursor
            // (== `preview_selection.active`). A bare `d` only counts
            // when no input/overlay is active (the `gg_suppressed`
            // gate above ensures that), so the commit textarea and
            // pickers still see literal `d` keystrokes — invariant
            // 1 of references/text-input-stack.md.
            if chord_fresh {
                // When the diff panel owns focus, `gd`/`gr` resolve against
                // the diff (workspace-index, by identifier text); otherwise
                // they run the full preview path (tree-sitter + LSP).
                let in_diff = matches!(app.engine.active_tab(), Tab::Git | Tab::Graph)
                    && app.engine.active_panel() == Panel::Diff;
                // `gd` — goto-definition.
                match Keymap::resolve_chord(InputScope::Normal, KeyCode::Char('g'), &key) {
                    Some(Command::ScrollTop) => {
                        app.clear_g_chord();
                        app.scroll_active_preview_to_top();
                        return;
                    }
                    Some(Command::GotoDefinition) => {
                        app.clear_g_chord();
                        if in_diff {
                            app.goto_definition_in_diff(NavAnchor::Keyboard);
                        } else {
                            app.goto_definition_at_cursor(NavAnchor::Keyboard);
                        }
                        return;
                    }
                    // `gr` — find-references. Opens the candidates popup
                    // with every workspace reference to the symbol under
                    // the cursor.
                    Some(Command::FindReferences) => {
                        app.clear_g_chord();
                        if in_diff {
                            app.find_references_in_diff(NavAnchor::Keyboard);
                        } else {
                            app.find_references_at_cursor(NavAnchor::Keyboard);
                        }
                        return;
                    }
                    _ => {}
                }
            }
            // Anything else (or an expired chord) breaks it.
            app.clear_g_chord();
        }
    }

    if !in_input_mode
        && let Some(command) = Keymap::resolve(InputScope::Normal, &key)
        && dispatch_keymap_command(command, app, search_input_focused)
    {
        return;
    }

    // Esc back-out, two-step. Active search input and modal Esc
    // handlers (place mode, tree-edit, paste-conflict, …) run
    // earlier via top-of-`handle_key` early-returns, so this
    // block only fires for the "no specific modal owns Esc" case.
    //   1. Clear dormant `/` highlights — but only when the
    //      current panel owns them. If the user Tab'd to a
    //      different panel the highlights aren't on screen, so
    //      Esc passes through to that panel's own handler
    //      (commit-box exit, multi-select clear, …) instead of
    //      being silently swallowed.
    //   2. Otherwise, return panel focus to `Panel::Files`. Both
    //      arms use guards so Graph visual-mode exit and other
    //      per-tab Esc semantics still fall through when neither
    //      applies.
    match key.code {
        KeyCode::Esc
            if !app.engine.search().matches.is_empty()
                && search::resolve_target(app) == app.engine.search().target =>
        {
            app.engine.dispatch(AppCommand::ClearVimSearch);
            return;
        }
        KeyCode::Esc if app.engine.active_panel() != Panel::Files => {
            app.set_active_panel(Panel::Files);
            app.engine.dispatch(AppCommand::ClearVimSearch);
            return;
        }
        _ => {}
    }

    match app.engine.active_tab() {
        Tab::Git => handle_key_git(key, app),
        Tab::Files => handle_key_files(key, app),
        Tab::Search => handle_key_search(key, app),
        Tab::Graph => handle_key_graph(key, app),
    }
}

fn dispatch_input_scope_command(command: Command, app: &mut App) -> bool {
    match command {
        Command::Quit => {
            app.quit();
            true
        }
        _ => false,
    }
}

fn dispatch_keymap_command(command: Command, app: &mut App, search_input_focused: bool) -> bool {
    match command {
        Command::Quit => {
            app.quit();
            true
        }
        Command::OpenHelp => {
            app.open_help();
            true
        }
        Command::ToggleSidebar => {
            app.toggle_sidebar();
            true
        }
        Command::OpenSettings => {
            app.open_settings();
            true
        }
        Command::OpenHostsPicker => {
            app.open_hosts_picker();
            true
        }
        Command::LocationBack => {
            app.location_back();
            true
        }
        Command::LocationForward => {
            app.location_forward();
            true
        }
        Command::BeginSearchForward => {
            if app.engine.active_tab() == Tab::Search && app.engine.active_panel() == Panel::Files {
                app.engine.dispatch(AppCommand::FocusGlobalSearchFindInput);
            } else {
                search::begin(app, false);
            }
            true
        }
        Command::BeginSearchBackward => {
            search::begin(app, true);
            true
        }
        Command::NextSearchMatch => {
            if app.engine.search().can_step() && !has_pending_confirm(app) {
                search::step(app, false);
                true
            } else {
                false
            }
        }
        Command::PrevSearchMatch => {
            if app.engine.search().can_step() && !has_pending_confirm(app) {
                search::step(app, true);
                true
            } else {
                false
            }
        }
        Command::SwitchTab(idx) => {
            let tab = if idx == usize::MAX {
                let tabs = Tab::ALL;
                let cur = tabs
                    .iter()
                    .position(|&t| t == app.engine.active_tab())
                    .unwrap_or(0);
                tabs[(cur + 1) % tabs.len()]
            } else {
                let Some(&tab) = Tab::ALL.get(idx) else {
                    return false;
                };
                tab
            };
            if app.engine.active_tab() != tab {
                app.engine.dispatch(AppCommand::ClearVimSearch);
            }
            app.set_active_tab(tab);
            true
        }
        Command::CyclePanelForward => {
            if search_input_focused {
                return false;
            }
            app.cycle_active_panel(false);
            app.engine.dispatch(AppCommand::ClearVimSearch);
            true
        }
        Command::CyclePanelBackward => {
            if search_input_focused {
                return false;
            }
            app.cycle_active_panel(true);
            app.engine.dispatch(AppCommand::ClearVimSearch);
            true
        }
        _ => false,
    }
}

/// `n` / `N` only bind to search navigation when no git-status confirmation
/// banner is up — otherwise `n` keeps its "no, cancel" meaning.
fn has_pending_confirm(app: &App) -> bool {
    app.engine.has_git_confirm_prompt()
}

/// SQLite preview page-jump input. While the DB goto prompt is active,
/// `Some(_)`, this handler fully owns the keyboard.
///
/// Two-phase routing matching every other input in reef:
///   1. Enter parses-and-jumps; Esc cancels.
///   2. Everything else flows into [`input_edit::dispatch_key_filtered`]
///      with a digit-only predicate, so the user gets word-motion,
///      Home/End, Ctrl+U clear, etc. for free while non-digit chars
///      are silently swallowed.
fn handle_key_db_goto(key: KeyEvent, app: &mut App) {
    match Keymap::resolve(InputScope::DbGoto, &key) {
        Some(Command::Confirm) => {
            app.engine.dispatch(AppCommand::ConfirmDbGoto);
        }
        Some(Command::Close) => {
            app.engine.dispatch(AppCommand::CloseDbGoto);
        }
        _ => {
            if let Some(op) = crate::input_edit::op_for_key(&key) {
                app.engine.dispatch(AppCommand::EditDbGoto(op));
            }
        }
    }
}

/// 纯预览模式的早期闸门 —— 拦截退出语义 + 文件 picker 相关按键。
///
/// 返回 `true` 表示已消费,调用方应当 return;返回 `false` 表示让按键继续
/// 走正常分发,这样 ↑↓/PgUp/PgDn/jk 等滚动 + 搜索键仍对全屏的 preview/diff
/// 面板有效。
///
/// picker open 时本闸门完全接管按键 —— ↑↓ 选行,Enter 确认,Esc/o 关闭。
/// 这样 picker 期间用户不会意外滚到 diff 上去。
fn handle_key_focused_preview(key: KeyEvent, app: &mut App) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Picker fully owns the keyboard while open.
    if app.engine.focused_preview_files_open() {
        match key.code {
            KeyCode::Esc => {
                app.close_focused_preview_files();
                return true;
            }
            KeyCode::Char('c') if ctrl => {
                app.quit();
                return true;
            }
            KeyCode::Up | KeyCode::Char('k') if !ctrl => {
                app.move_focused_preview_files_selection(-1);
                return true;
            }
            KeyCode::Down | KeyCode::Char('j') if !ctrl => {
                app.move_focused_preview_files_selection(1);
                return true;
            }
            KeyCode::PageUp | KeyCode::Home => {
                app.engine
                    .dispatch(AppCommand::SetFocusedPreviewFilesSelection(0));
                return true;
            }
            KeyCode::PageDown | KeyCode::End => {
                let len = app.focused_preview_file_entries().len();
                app.engine
                    .dispatch(AppCommand::SetFocusedPreviewFilesSelection(
                        len.saturating_sub(1),
                    ));
                return true;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                app.confirm_focused_preview_files_selection();
                return true;
            }
            KeyCode::Char('o') | KeyCode::Char('O') if !ctrl => {
                app.close_focused_preview_files();
                return true;
            }
            // While picker is up, swallow everything else so accidental
            // keystrokes can't scroll the diff out from under the popup.
            _ => return true,
        }
    }

    // When a Space leader is armed AND still within the chord window,
    // the *next* key must reach `leader_decision` so chord targets
    // (Space+F = FindWidget, Space+V = exit FocusedPreview, etc.) can
    // fire. The whitelist below would otherwise swallow most chord
    // targets — `p`/`P` for QuickOpen aren't in it at all — and a
    // typo'd chord would silently stay armed past the timeout.
    //
    // The timeout check is load-bearing: without it, a stray Space
    // (e.g. the user tapped Space and walked away) would leave
    // `space_leader_at = Some(stale_t)` indefinitely, and every
    // subsequent key — including destructive `s`/`u`/`d` on Git —
    // would bypass the whitelist and reach per-tab dispatch against
    // the invisible status row. With the check, the bypass is bounded
    // to LEADER_TIMEOUT (800 ms) starting from the Space press.
    if app
        .space_leader_at
        .is_some_and(|t| Instant::now().duration_since(t) < LEADER_TIMEOUT)
    {
        return false;
    }

    // Same logic for the `gd` / `gr` chord. `g` arms the chord (it's in
    // the allowlist below and falls through), but the chord *targets*
    // `d` / `r` are deliberately NOT in that allowlist — bare `d`/`r` are
    // destructive on the per-tab handlers (discard / refresh). So when a
    // `g` chord is armed and still fresh, let a bare `d`/`r` fall through
    // to `handle_key`'s chord resolver, where it becomes goto-definition
    // / find-references. Without this, both are unreachable via keyboard
    // in 纯预览 and the chord strands armed. Narrowly scoped to `d`/`r`
    // so no other key gains a destructive fallthrough.
    if app
        .g_pending_at
        .is_some_and(|t| Instant::now().duration_since(t) < G_CHORD_TIMEOUT)
        && !ctrl
        && !key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('r'))
    {
        return false;
    }

    // Explicit handling for FocusedPreview-specific actions.
    match Keymap::resolve(InputScope::FocusedPreview, &key) {
        Some(Command::Close) => {
            app.close_focused_preview();
            return true;
        }
        // Bare q quits reef (same convention as Settings / place mode
        // takeovers). Ctrl+C is the universal quit and is intentionally
        // handled here too so the user can bail without first Esc'ing.
        Some(Command::Quit) => {
            app.quit();
            return true;
        }
        _ => {}
    }

    match key.code {
        // `o` opens the floating file picker — scoped to focused preview
        // only (not a global binding), and only where the picker is
        // actually rendered (Git tab + Graph 3-col). Mirror the
        // render-side `chip_layout_ok` gate so keyboard and visual
        // states agree; otherwise pressing `o` on Graph 2-col would
        // open a picker the renderer refuses to draw and the keyboard
        // would freeze against an invisible selection.
        KeyCode::Char('o') | KeyCode::Char('O')
            if !ctrl && app.space_leader_at.is_none() && app.focused_preview_chip_visible() =>
        {
            app.toggle_focused_preview_files();
            return true;
        }
        // Ctrl+, would otherwise reach the global keymap and call
        // open_settings(), stomping view_mode from FocusedPreview to
        // Settings — and close_settings always returns to Main, so the
        // user would lose the 纯预览 context entirely. Swallow here.
        KeyCode::Char(',') if ctrl => return true,
        // Ctrl+O would open the hosts picker overlay — swallow for the
        // same "don't lose 纯预览 context" reason. The user can Esc
        // first if they really want to swap sessions.
        KeyCode::Char('o') if ctrl => return true,
        // Bare v / V — eat it so tab-specific bindings (Graph visual
        // mode on V, etc.) don't fire against a hidden panel.
        // Exception: when a Space leader is armed we must fall through
        // so the chord can reach `leader_decision` →
        // toggle_focused_preview() and exit.
        KeyCode::Char('v') | KeyCode::Char('V') if !ctrl && app.space_leader_at.is_none() => {
            return true;
        }
        _ => {}
    }

    // Allowlist for falling through to per-tab handlers. The fallthrough
    // path runs `handle_key_git` / `handle_key_files` etc. without any
    // view_mode guard, and those handlers have *destructive* bindings on
    // bare letters: `s/u` stage/unstage, `d` discard, `t/r` toggles,
    // F2/Delete/Backspace rename/trash. None of those make sense while
    // the user is "just looking" at a maximised preview — and the tree/
    // status row they'd act on is invisible, so the action would land on
    // a row the user can't even see.
    //
    // The whitelist below is intentionally narrow: scroll + h-scroll +
    // search + diff layout/mode + SQLite navigation + readline nav. Any
    // key not in here is swallowed (`return true`). Add to this list
    // sparingly — every new entry has to be safe against firing from
    // an invisible tree/status row.
    let bare_allowed = !ctrl
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char(
                    ' '            // Space-leader arm — chord targets fire
                                   // via the leader-armed bypass above.
                    | 'j' | 'k'    // vim vertical
                    | 'h'          // help popup (FocusedPreview is in the
                                   // takeover list now, so the next key
                                   // reaches us, not "dismiss help")
                    | '?' | '/'    // search prompt
                    | 'n' | 'N'    // search step
                    | 'm' | 'f'    // diff layout / diff mode toggles
                    | '[' | ']'    // SQLite preview: prev/next table
                    | 'g' | 'G', // vim top/bottom + SQLite goto-page; the
                                 // chord runs in `handle_key`'s main body,
                                 // SQLite goto is per-tab in handle_key_files.
                )
        );
    // Ctrl-prefixed nav aliases that the per-tab handlers honor and
    // that are safe in FocusedPreview.
    let ctrl_allowed = ctrl
        && matches!(
            key.code,
            KeyCode::Char(
                'p' | 'n'      // readline up/down
                | 'j' | 'k'    // vim-style with ctrl
                | 'b' // sidebar toggle (no-op visually in
                      // FocusedPreview but harmless)
            )
        );

    if bare_allowed || ctrl_allowed {
        false // continue normal dispatch — scroll / search / diff toggles
    } else {
        true // swallow — destructive or modal-opening key
    }
}

/// Settings page key dispatcher. Two modes:
///
/// - **List mode** (default): ↑↓/k/j/Home/End/PageUp/PageDown move the
///   selection cursor; Enter cycles the selected enum / toggles the
///   selected bool, or opens the inline text editor for the
///   `editor.command` row; Esc closes the page.
/// - **Editor-command edit mode** (`app.engine.settings().editor_edit.is_some()`):
///   typing fills the buffer, Enter commits, Esc cancels and reverts.
///
/// Sits below the high-priority modal gates in `handle_key`, so a
/// half-typed commit message / search query won't be yanked away by a
/// stray Settings dispatch — but above the global keymap, so we own
/// every key while the page is open.
fn handle_key_settings(key: KeyEvent, app: &mut App) {
    if app.engine.settings().editor_edit.is_some() {
        handle_key_settings_editor(key, app);
        return;
    }

    match Keymap::resolve(InputScope::Settings, &key) {
        Some(Command::Close) => app.close_settings(),
        Some(Command::Quit) => app.quit(),
        Some(Command::MoveUp) => {
            app.engine.dispatch(AppCommand::MoveSettingsSelection(-1));
        }
        Some(Command::MoveDown) => {
            app.engine.dispatch(AppCommand::MoveSettingsSelection(1));
        }
        Some(Command::ScrollTop) => {
            app.engine.dispatch(AppCommand::SelectSettingsRow(0));
        }
        Some(Command::ScrollBottom) => {
            app.engine.dispatch(AppCommand::SelectSettingsRow(
                crate::settings::SettingItem::ALL.len().saturating_sub(1),
            ));
        }
        Some(Command::Confirm) => {
            let item = app.engine.settings().selected();
            if matches!(item, crate::settings::SettingItem::EditorCommand) {
                app.engine
                    .dispatch(AppCommand::BeginSettingsEditorCommandEdit);
            } else {
                settings::cycle(app, item);
            }
        }
        _ => {}
    }
}

fn handle_key_settings_editor(key: KeyEvent, app: &mut App) {
    match Keymap::resolve(InputScope::Settings, &key) {
        Some(Command::Close) => {
            app.engine
                .dispatch(AppCommand::CancelSettingsEditorCommandEdit);
            return;
        }
        Some(Command::Confirm) => {
            settings::commit_editor_command(app);
            return;
        }
        _ => {}
    }

    if let Some(op) = crate::input_edit::op_for_key(&key) {
        app.engine
            .dispatch(AppCommand::EditSettingsEditorCommand(op));
    }
}

/// Tab::Search key dispatcher. Panel::Files is the search sidebar, which
/// runs in one of three modes tracked by `GlobalSearchState.focus`:
///
/// - **List mode** (default on tab entry): bare keys are either nav
///   (↑↓/j/k/Ctrl+N/P) or they fall back to global shortcuts (h = help,
///   q = quit, etc.). `/` or `i` enters Find input. Enter opens the
///   selected hit in `$EDITOR`. In replace mode, `Space` toggles the
///   current row's checkbox.
/// - **FindInput mode**: typing fills the query; same editing /
///   navigation / accept keys as the overlay. Esc returns to list mode.
/// - **ReplaceInput mode**: typing fills `replace_text`; only reachable
///   when `replace_open` is true. Tab cycles
///   `Find → Replace → List → Find`.
///
/// Panel::Diff is the file preview, same keys as the Files-tab Diff panel.
///
/// Global gates in `handle_key` keep bare-char shortcuts from stealing
/// literal chars while in input mode; see `in_input_mode` there.
fn handle_key_search(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match app.engine.active_panel() {
        Panel::Files => match app.engine.snapshot().search.focus {
            reef_app::SearchPanelFocus::FindInput => {
                handle_key_search_find_input(key, app, ctrl, alt);
            }
            reef_app::SearchPanelFocus::ReplaceInput => {
                handle_key_search_replace_input(key, app, ctrl, alt);
            }
            reef_app::SearchPanelFocus::List => {
                handle_key_search_list_mode(key, app, ctrl);
            }
        },
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
                    app.engine.dispatch(AppCommand::PreviewScroll(-1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.engine.dispatch(AppCommand::PreviewScroll(1));
                }
                KeyCode::PageUp => {
                    app.engine.dispatch(AppCommand::PreviewScroll(-20));
                }
                KeyCode::PageDown => {
                    app.engine.dispatch(AppCommand::PreviewScroll(20));
                }
                KeyCode::Left => {
                    let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                        10
                    } else {
                        1
                    };
                    app.engine
                        .dispatch(AppCommand::PreviewHorizontalScroll(-step));
                }
                KeyCode::Right => {
                    let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                        10
                    } else {
                        1
                    };
                    app.engine
                        .dispatch(AppCommand::PreviewHorizontalScroll(step));
                }
                KeyCode::Home => {
                    app.engine
                        .dispatch(AppCommand::SetPreviewHorizontalScroll(0));
                }
                KeyCode::End => {
                    app.engine
                        .dispatch(AppCommand::SetPreviewHorizontalScroll(usize::MAX));
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
            let step = app.layout.global_search_last_view_h.max(1) as i32;
            global_search::move_selection_by(app, -step);
        }
        KeyCode::PageDown => {
            let step = app.layout.global_search_last_view_h.max(1) as i32;
            global_search::move_selection_by(app, step);
        }

        // ── Horizontal scroll of the results list ──────────────
        // Mirrors the Files-tab Diff panel convention: bare arrows move
        // 1 col, Shift+arrow moves 10, Home/End jump to the extremes.
        // Setting `results_h_scroll` to non-zero disables smart per-row
        // shifting so rows line up at the user's chosen column.
        KeyCode::Left => {
            let step = if shift { 10 } else { 1 };
            app.engine
                .dispatch(AppCommand::ScrollGlobalSearchResultsHorizontal(-step));
        }
        KeyCode::Right => {
            let step = if shift { 10 } else { 1 };
            app.engine
                .dispatch(AppCommand::ScrollGlobalSearchResultsHorizontal(step));
        }
        KeyCode::Home => {
            app.engine
                .dispatch(AppCommand::SetGlobalSearchResultsHorizontalScroll(0));
        }
        KeyCode::End => {
            app.engine
                .dispatch(AppCommand::SetGlobalSearchResultsHorizontalScroll(
                    reef_app::GLOBAL_SEARCH_MAX_H_SCROLL,
                ));
        }

        // ── Mode entry ─────────────────────────────────────────
        // `i` (vim-insert) as a secondary mnemonic. `/` is handled in the
        // global keymap so it also lights up the input from other tabs;
        // dispatching it here too makes the in-tab behaviour obvious.
        KeyCode::Char('i') if key.modifiers.is_empty() => {
            app.engine.dispatch(AppCommand::FocusGlobalSearchFindInput);
        }
        KeyCode::Char('/') if key.modifiers.is_empty() => {
            app.engine.dispatch(AppCommand::FocusGlobalSearchFindInput);
        }

        // ── Reload ─────────────────────────────────────────────
        // Re-run the current query. Mirrors `r` = refresh on Files / Git /
        // Graph tabs. Only available in list mode; in input mode `r` is a
        // literal char for the query.
        KeyCode::Char('r') if key.modifiers.is_empty() => {
            global_search::reload(app);
        }
        // ── Replace mode toggle ────────────────────────────────
        // `R` (Shift+r) flips the chevron — the verbose `Space+H`
        // chord is the primary entry; `R` is a quick in-tab way to
        // toggle without leaving the keyboard.
        KeyCode::Char('R') => {
            app.engine.dispatch(AppCommand::ToggleGlobalSearchReplace);
        }
        // ── Per-match include/exclude (replace mode) ───────────
        // Space-leader is disarmed in this state (see `handle_key`);
        // bare Space toggles the current row's checkbox.
        KeyCode::Char(' ')
            if key.modifiers.is_empty() && app.engine.snapshot().search.replace_open =>
        {
            let idx = app.engine.snapshot().search.selected_idx;
            app.engine
                .dispatch(AppCommand::ToggleGlobalSearchMatchExcluded(idx));
        }

        // ── Focus cycle into the inputs ────────────────────────
        KeyCode::Tab if key.modifiers.is_empty() => {
            app.engine
                .dispatch(AppCommand::CycleGlobalSearchFocusForward);
        }
        KeyCode::BackTab => {
            app.engine
                .dispatch(AppCommand::CycleGlobalSearchFocusBackward);
        }

        // ── Apply replace batch ────────────────────────────────
        // Many terminals collapse Enter and Ctrl/Alt+Enter to the
        // same byte sequence; bind both modifier paths so at least
        // one fires on every host.
        KeyCode::Enter
            if app.engine.snapshot().search.replace_open
                && (ctrl || key.modifiers.contains(KeyModifiers::ALT)) =>
        {
            app.commit_replace_in_files();
        }

        // ── Accept ─────────────────────────────────────────────
        KeyCode::Enter => global_search::accept(app),

        // Esc is a no-op in list mode: nothing to escape from, and we
        // don't want it to close/jump away unexpectedly.
        _ => {}
    }
}

/// Key dispatch for Tab::Search Panel::Files when the FIND input is
/// focused. Same bindings as the Space+F overlay — typing fills the
/// query, Esc exits to list mode, Enter opens the selection. Tab cycles
/// to Replace (when open) or List.
///
/// Two-pass shape mirrors `handle_key_search_replace_input`: app-level
/// keys (Esc/Tab/Enter/list-nav) need `&mut app` and run first with
/// early-return; remaining keys go through the shared
/// `input_edit::dispatch_key` helper which only borrows
/// `&mut query / &mut cursor`. Edit outcomes fire `mark_query_edited`
/// to kick the search-rerun debounce.
fn handle_key_search_find_input(key: KeyEvent, app: &mut App, ctrl: bool, alt: bool) {
    // Pass 1: app-level actions.
    match Keymap::resolve(InputScope::SearchInput, &key) {
        Some(Command::Close) => {
            app.engine.dispatch(AppCommand::FocusGlobalSearchList);
            return;
        }
        Some(Command::Confirm) if app.engine.snapshot().search.replace_open && (ctrl || alt) => {
            app.commit_replace_in_files();
            return;
        }
        Some(Command::Quit) => {
            app.quit();
            return;
        }
        Some(Command::Confirm) => {
            global_search::accept(app);
            return;
        }
        Some(Command::MoveUp) => {
            global_search::move_selection_by(app, -1);
            return;
        }
        Some(Command::MoveDown) => {
            global_search::move_selection_by(app, 1);
            return;
        }
        Some(Command::PageUp) => {
            let step = app.layout.global_search_last_view_h.max(1) as i32;
            global_search::move_selection_by(app, -step);
            return;
        }
        Some(Command::PageDown) => {
            let step = app.layout.global_search_last_view_h.max(1) as i32;
            global_search::move_selection_by(app, step);
            return;
        }
        _ => {}
    }

    match key.code {
        KeyCode::Tab if key.modifiers.is_empty() => {
            app.engine
                .dispatch(AppCommand::CycleGlobalSearchFocusForward);
            return;
        }
        KeyCode::BackTab => {
            app.engine
                .dispatch(AppCommand::CycleGlobalSearchFocusBackward);
            return;
        }
        _ => {}
    }

    if let Some(op) = crate::input_edit::op_for_key(&key) {
        app.engine.dispatch(AppCommand::EditGlobalSearchFindInput {
            op,
            now: Instant::now(),
        });
    }
}

/// Key dispatch for Tab::Search Panel::Files when the REPLACE input is
/// focused. Mirrors the find-input handler but operates on
/// `replace_text`/`replace_cursor` and skips the search-rerun side
/// effect — editing the replacement string never changes the result
/// list. Plain `Enter` commits the replace batch (the user finished
/// typing the replacement); `Esc` returns to list mode.
fn handle_key_search_replace_input(key: KeyEvent, app: &mut App, _ctrl: bool, _alt: bool) {
    // Pass 1: app-level actions.
    match Keymap::resolve(InputScope::SearchInput, &key) {
        Some(Command::Close) => {
            app.engine.dispatch(AppCommand::FocusGlobalSearchList);
            return;
        }
        Some(Command::Confirm) => {
            app.commit_replace_in_files();
            return;
        }
        Some(Command::Quit) => {
            app.quit();
            return;
        }
        Some(Command::MoveUp) => {
            global_search::move_selection_by(app, -1);
            return;
        }
        Some(Command::MoveDown) => {
            global_search::move_selection_by(app, 1);
            return;
        }
        Some(Command::PageUp) => {
            let step = app.layout.global_search_last_view_h.max(1) as i32;
            global_search::move_selection_by(app, -step);
            return;
        }
        Some(Command::PageDown) => {
            let step = app.layout.global_search_last_view_h.max(1) as i32;
            global_search::move_selection_by(app, step);
            return;
        }
        _ => {}
    }

    match key.code {
        KeyCode::Tab if key.modifiers.is_empty() => {
            app.engine
                .dispatch(AppCommand::CycleGlobalSearchFocusForward);
            return;
        }
        KeyCode::BackTab => {
            app.engine
                .dispatch(AppCommand::CycleGlobalSearchFocusBackward);
            return;
        }
        _ => {}
    }

    if let Some(op) = crate::input_edit::op_for_key(&key) {
        app.engine
            .dispatch(AppCommand::EditGlobalSearchReplaceInput(op));
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
        app.set_active_panel(Panel::Files);
        return;
    }
    // Right of the graph split. 3-col Graph splits this further.
    if let Some(diff_start) = graph_diff_column_start(app, total_width) {
        if column >= diff_start {
            app.set_active_panel(Panel::Diff);
        } else {
            app.set_active_panel(Panel::Commit);
        }
    } else {
        app.set_active_panel(Panel::Diff);
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
    match app.engine.active_panel() {
        Panel::Files => {}
        Panel::Commit => commit_detail_panel::scroll(app, delta),
        Panel::Diff => {
            if app.graph_uses_three_col() {
                app.engine
                    .dispatch(AppCommand::ScrollCommitDetailFileDiffVertical(delta));
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
    let in_visual = app.engine.graph_in_visual_mode() && app.engine.active_panel() == Panel::Files;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') if !ctrl => {
            if app.engine.active_panel() == Panel::Files {
                if shift || in_visual {
                    app.engine.dispatch(AppCommand::ExtendGraphSelection(-1));
                } else {
                    git_graph_panel::handle_key(app, "k");
                }
            } else {
                graph_scroll_right_panel(app, -1);
            }
        }
        KeyCode::Down | KeyCode::Char('j') if !ctrl => {
            if app.engine.active_panel() == Panel::Files {
                if shift || in_visual {
                    app.engine.dispatch(AppCommand::ExtendGraphSelection(1));
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
            if app.engine.active_panel() == Panel::Files {
                if shift || in_visual {
                    app.engine.dispatch(AppCommand::ExtendGraphSelection(-1));
                } else {
                    git_graph_panel::handle_key(app, "k");
                }
            } else {
                graph_scroll_right_panel(app, -1);
            }
        }
        KeyCode::Char('n' | 'j') if ctrl => {
            if app.engine.active_panel() == Panel::Files {
                if shift || in_visual {
                    app.engine.dispatch(AppCommand::ExtendGraphSelection(1));
                } else {
                    git_graph_panel::handle_key(app, "j");
                }
            } else {
                graph_scroll_right_panel(app, 1);
            }
        }
        KeyCode::PageUp => {
            if app.engine.active_panel() == Panel::Files {
                if shift || in_visual {
                    app.engine.dispatch(AppCommand::ExtendGraphSelection(-10));
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
            if app.engine.active_panel() == Panel::Files {
                if shift || in_visual {
                    app.engine.dispatch(AppCommand::ExtendGraphSelection(10));
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
        KeyCode::Char('V') if app.engine.active_panel() == Panel::Files => {
            if app.engine.graph_in_visual_mode() {
                app.engine.dispatch(AppCommand::ClearGraphRange);
            } else if app.engine.graph_has_rows() {
                app.engine.dispatch(AppCommand::ResetGraphVisualAnchor);
            }
        }
        // Esc exits visual mode / collapses any range back to single-select.
        // Only consumed when actually armed on the Files panel so higher
        // priority Esc handlers (overlays etc.) aren't shadowed elsewhere.
        KeyCode::Esc
            if app.engine.active_panel() == Panel::Files && app.engine.graph_in_visual_mode() =>
        {
            app.engine.dispatch(AppCommand::ClearGraphRange);
        }
        KeyCode::Char('r') => {
            // `r` on the graph sidebar = force a graph cache refresh
            app.engine.dispatch(AppCommand::RefreshGraphUncached);
        }
        // `b` opens the branch picker overlay (see `graph_branch_picker`).
        // Available from every panel on the Graph tab — the picker is a
        // tab-level navigation, not a sidebar-local action, so the user
        // can switch branches while focus is on the commit pane or the
        // diff column too. (`V`/visual-mode stays gated on Panel::Files
        // because it's specifically a sidebar-row range selection.)
        KeyCode::Char('b') if !ctrl => {
            app.open_graph_branch_picker();
        }
        // m/f/t target the commit-detail panel regardless of focus.
        // `!ctrl` guards so Ctrl+F/M/T (e.g. VSCode muscle-memory Ctrl+F
        // for find) don't silently flip diff layout/mode — the global
        // Ctrl+F binding was removed in favour of Space+F.
        KeyCode::Char('m') if !ctrl => {
            commit_detail_panel::handle_key(app, "m");
        }
        KeyCode::Char('f') if !ctrl => {
            commit_detail_panel::handle_key(app, "f");
        }
        KeyCode::Char('t') if !ctrl => {
            commit_detail_panel::handle_key(app, "t");
        }
        _ => {}
    }
}

/// Route a key event to the commit-box buffer when the Git tab's
/// commit input is focused.
///
/// Two-phase routing matching every other input in reef:
///   1. Commit-box-specific shortcuts (Esc blur, Ctrl+Enter submit;
///      Ctrl+Enter requires `!alt` so a stray Ctrl+Alt+Enter on
///      Linux WMs doesn't silently commit).
///   2. Everything else flows into
///      [`input_edit_multi::dispatch_key_multi`], which extends the
///      shared single-line vocabulary with line-aware Up/Down
///      navigation, line-aware Home/End, and bare-Enter newline
///      insert.
///
/// Returns `true` when the key was actually consumed (Phase 1 hit
/// OR Phase 2 returned `Edited` / `CursorOnly`). Keys the multi-line
/// editor doesn't recognise (PageUp / PageDown / Shift+Arrow / F-keys
/// …) yield `Unhandled` and we return `false` so the outer Git-tab
/// handler can do its own thing with them. Letter chords like
/// `s` / `u` / `d` arrive here as `Char(c)` which the editor DOES
/// handle (as a literal insert), so the draft is safe.
fn handle_key_git_commit(key: KeyEvent, app: &mut App, ctrl: bool, alt: bool) -> bool {
    if app.engine.active_panel() != Panel::Files || !app.engine.is_commit_editing() {
        return false;
    }
    // Phase 1: commit-box-specific shortcuts. ANY Ctrl-modified
    // Enter submits the commit — Shift/Alt extra modifier bits are
    // accepted because terminals disagree about which subset gets
    // forwarded (kitty enhanced-keyboard sends Shift on Enter; some
    // Linux WMs bundle Alt). Without this, Ctrl+Shift+Enter /
    // Ctrl+Alt+Enter would fall through Phase 2 (which only matches
    // `Enter if !ctrl`) as Unhandled — and Enter isn't `Char`, so the
    // Ctrl-letter swallow guard below doesn't catch it — landing on
    // the outer Git handler's `Char('e') | Enter` arm that opens
    // `$EDITOR` on the selected file.
    match Keymap::resolve(InputScope::CommitEditor, &key) {
        Some(Command::Close) => {
            app.engine.dispatch(AppCommand::SetCommitEditing(false));
            return true;
        }
        Some(Command::Confirm) => {
            app.engine.dispatch(AppCommand::RunCommit);
            return true;
        }
        Some(Command::Quit) => {
            app.quit();
            return true;
        }
        _ => {}
    }

    // Phase 2: shared multi-line text-input dispatch. Forward
    // `Unhandled` to the caller as "not consumed" so unknown
    // navigation keys (PageUp / Shift+Arrow / F-keys / …) can flow
    // up to the outer Git-tab handler instead of being silently
    // swallowed mid-edit.
    //
    // EXCEPT: Ctrl+letter and Alt+letter chords are deliberately
    // swallowed even on Unhandled, because the outer Git handler's
    // letter arms (`Char('s')` → stage, `Char('r')` → refresh,
    // `Char('e') | Enter` → open in $EDITOR, etc. at lines ~1603-1645)
    // are NOT modifier-gated and would mis-fire from VSCode muscle
    // memory like Ctrl+S mid-message. The commit box's invariant —
    // documented at the top of `handle_key_git` as "letter chords
    // don't fire mid-message" — depends on this guard.
    let outcome = crate::input_edit_multi::op_for_key(&key)
        .and_then(|op| {
            app.engine
                .dispatch(AppCommand::EditCommitMessage(op))
                .text_edit
        })
        .unwrap_or(crate::input_edit::Outcome::Unhandled);
    if matches!(outcome, crate::input_edit::Outcome::Unhandled)
        && matches!(key.code, KeyCode::Char(_))
        && (ctrl || alt)
    {
        // Swallow any ctrl/alt-modified char chord the editor didn't
        // recognise. Returning true keeps focus on the commit box.
        return true;
    }
    !matches!(outcome, crate::input_edit::Outcome::Unhandled)
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
        KeyCode::Up | KeyCode::Char('k') if !ctrl => match app.engine.active_panel() {
            Panel::Files => app.navigate_files(-1),
            // Git tab has no middle column — Panel::Commit should never
            // be set here, but if it slips through treat it as Diff.
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::DiffScroll(-1));
            }
        },
        KeyCode::Down | KeyCode::Char('j') if !ctrl => match app.engine.active_panel() {
            Panel::Files => app.navigate_files(1),
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::DiffScroll(1));
            }
        },
        // Readline-style nav aliases. Must come BEFORE the bare
        // `Char('n')` / `Char('d')` arms below, which would otherwise
        // route Ctrl+N to the git-status "No" confirm. The bare
        // letters (n/y/d for confirm / discard chord) stay on their
        // own arms because they check `!ctrl` implicitly via being
        // matched only if the Ctrl arm above didn't fire.
        KeyCode::Char('p' | 'k') if ctrl => match app.engine.active_panel() {
            Panel::Files => app.navigate_files(-1),
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::DiffScroll(-1));
            }
        },
        KeyCode::Char('n' | 'j') if ctrl => match app.engine.active_panel() {
            Panel::Files => app.navigate_files(1),
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::DiffScroll(1));
            }
        },
        KeyCode::PageUp => match app.engine.active_panel() {
            Panel::Files => app.navigate_files(-10),
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::DiffScroll(-20));
            }
        },
        KeyCode::PageDown => match app.engine.active_panel() {
            Panel::Files => app.navigate_files(10),
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::DiffScroll(20));
            }
        },
        KeyCode::Left if app.engine.active_panel() == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            // SBS mode: keyboard pans both halves in lockstep — the user
            // has no mouse-column to disambiguate. Mouse scroll keeps the
            // per-side route from `apply_horizontal_scroll`.
            app.engine.dispatch(AppCommand::DiffHorizontalScroll(-step));
        }
        KeyCode::Right if app.engine.active_panel() == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.engine.dispatch(AppCommand::DiffHorizontalScroll(step));
        }
        KeyCode::Home if app.engine.active_panel() == Panel::Diff => {
            app.engine.dispatch(AppCommand::SetDiffHorizontalScroll(0));
        }
        KeyCode::End if app.engine.active_panel() == Panel::Diff => {
            // render 自动钳到实际最大值
            app.engine
                .dispatch(AppCommand::SetDiffHorizontalScroll(usize::MAX));
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
        // `!ctrl` guard on `m`/`f` so Ctrl+F (VSCode muscle memory)
        // doesn't silently flip diff layout/mode now that the global
        // Ctrl+F binding has been removed in favour of Space+F.
        KeyCode::Char('m') if !ctrl => {
            app.toggle_diff_layout();
        }
        KeyCode::Char('f') if !ctrl => {
            app.toggle_diff_mode();
        }
        KeyCode::Char('e') | KeyCode::Enter => {
            // Edit the currently selected changed file. Ignore if nothing's
            // selected (empty status) or the repo's gone. A Deleted-status
            // file will be recreated by the editor if the user writes — same
            // behaviour you'd get running `$EDITOR path/to/deleted` in a shell.
            app.engine.dispatch(AppCommand::RequestEditSelectedGitFile);
        }
        _ => {}
    }
}

fn handle_key_files(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // VS Code-style clipboard / multi-select bindings — only on the
    // tree panel itself. Bypassing the rest of `handle_key_files` for
    // these keys keeps them from colliding with arrow / vim-nav arms
    // below. Sub-handler returns `true` when it consumed the key.
    if app.engine.active_panel() == Panel::Files
        && handle_key_files_clipboard(key, app, ctrl, shift)
    {
        return;
    }

    match key.code {
        KeyCode::Up | KeyCode::Char('k') if !ctrl => match app.engine.active_panel() {
            Panel::Files => {
                app.engine.dispatch(AppCommand::NavigateFileTree(-1));
            }
            // Files tab has no middle column — Panel::Commit should never
            // be set here, but fall back to Diff behaviour defensively.
            Panel::Diff | Panel::Commit => {
                // For Database bodies preview_scroll is "row offset
                // within current_rows" — same field, different
                // semantics. The renderer reads this for either body
                // shape, so a single decrement works for both.
                app.engine.dispatch(AppCommand::PreviewScroll(-1));
            }
        },
        KeyCode::Down | KeyCode::Char('j') if !ctrl => match app.engine.active_panel() {
            Panel::Files => {
                app.engine.dispatch(AppCommand::NavigateFileTree(1));
            }
            Panel::Diff | Panel::Commit => {
                // Same dual-semantics as the Up arm. Render clamps the
                // upper bound against the actual row count, so we
                // don't need to know current_rows.len() here.
                app.engine.dispatch(AppCommand::PreviewScroll(1));
            }
        },
        // Readline-style nav: Ctrl+P/K = up, Ctrl+N/J = down. Mirrors
        // the palette bindings so a Vim+Emacs-era user gets the same
        // keys on any list in the app. Guarded behind `ctrl` (the
        // bare letter guards above check `!ctrl`) so pressing `j`
        // without a modifier still navigates normally.
        KeyCode::Char('p' | 'k') if ctrl => match app.engine.active_panel() {
            Panel::Files => {
                app.engine.dispatch(AppCommand::NavigateFileTree(-1));
            }
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::PreviewScroll(-1));
            }
        },
        KeyCode::Char('n' | 'j') if ctrl => match app.engine.active_panel() {
            Panel::Files => {
                app.engine.dispatch(AppCommand::NavigateFileTree(1));
            }
            Panel::Diff | Panel::Commit => {
                app.engine.dispatch(AppCommand::PreviewScroll(1));
            }
        },
        KeyCode::PageUp => match app.engine.active_panel() {
            Panel::Files => {
                app.engine.dispatch(AppCommand::NavigateFileTree(-10));
            }
            Panel::Diff | Panel::Commit => {
                // SQLite preview hijacks PgUp/PgDn for page-flip;
                // every other body shape keeps the regular scroll
                // semantics so .txt / .png / binary cards aren't
                // affected.
                if app.engine.preview_is_database() {
                    app.db_navigate(DbNav::PrevPage);
                } else {
                    app.engine.dispatch(AppCommand::PreviewScroll(-20));
                }
            }
        },
        KeyCode::PageDown => match app.engine.active_panel() {
            Panel::Files => {
                app.engine.dispatch(AppCommand::NavigateFileTree(10));
            }
            Panel::Diff | Panel::Commit => {
                if app.engine.preview_is_database() {
                    app.db_navigate(DbNav::NextPage);
                } else {
                    app.engine.dispatch(AppCommand::PreviewScroll(20));
                }
            }
        },
        // SQLite preview only — `[` / `]` cycle tables. Bare keys, no
        // modifier guard beyond `!ctrl` (Ctrl+[ is the terminal Esc
        // sequence on most terms; we don't want to swallow it).
        KeyCode::Char('[')
            if !ctrl
                && app.engine.active_panel() == Panel::Diff
                && app.engine.preview_is_database() =>
        {
            app.db_navigate(DbNav::PrevTable);
        }
        KeyCode::Char(']')
            if !ctrl
                && app.engine.active_panel() == Panel::Diff
                && app.engine.preview_is_database() =>
        {
            app.db_navigate(DbNav::NextTable);
        }
        // SQLite preview only — `g` opens the page-jump input. Bare
        // key (no Ctrl) so it doesn't conflict with terminal Ctrl+G
        // (bell). Once the input is active, `handle_key_db_goto` at
        // the top of the dispatcher takes over.
        KeyCode::Char('g')
            if !ctrl
                && app.engine.active_panel() == Panel::Diff
                && app.engine.preview_is_database() =>
        {
            app.engine.dispatch(AppCommand::OpenDbGoto);
        }
        KeyCode::Left if app.engine.active_panel() == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.engine
                .dispatch(AppCommand::PreviewHorizontalScroll(-step));
        }
        KeyCode::Right if app.engine.active_panel() == Panel::Diff => {
            let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                10
            } else {
                1
            };
            app.engine
                .dispatch(AppCommand::PreviewHorizontalScroll(step));
        }
        // `Home`/`End` keep their existing semantics for every body
        // shape (h-scroll to the start / end of the row). Database
        // body's first/last page jumps live on `Ctrl+Home` /
        // `Ctrl+End` instead — overriding bare `Home`/`End` would
        // strand the user with no quick way to reset h_scroll after
        // an accidental drift.
        KeyCode::Home if app.engine.active_panel() == Panel::Diff && !ctrl => {
            app.engine
                .dispatch(AppCommand::SetPreviewHorizontalScroll(0));
        }
        KeyCode::End if app.engine.active_panel() == Panel::Diff && !ctrl => {
            app.engine
                .dispatch(AppCommand::SetPreviewHorizontalScroll(usize::MAX));
        }
        KeyCode::Home
            if ctrl
                && app.engine.active_panel() == Panel::Diff
                && app.engine.preview_is_database() =>
        {
            app.db_navigate(DbNav::FirstPage);
        }
        KeyCode::End
            if ctrl
                && app.engine.active_panel() == Panel::Diff
                && app.engine.preview_is_database() =>
        {
            app.db_navigate(DbNav::LastPage);
        }
        KeyCode::Enter => {
            app.engine
                .dispatch(AppCommand::ActivateSelectedFileTreeEntry);
        }
        KeyCode::Char('r') => {
            app.refresh_file_tree();
        }
        KeyCode::Char('e') => {
            // Explicit alias for "edit selected entry". Unlike Enter, this
            // never expands a directory — on a dir it's a no-op.
            app.engine
                .dispatch(AppCommand::RequestEditSelectedFileTreeEntry);
        }
        KeyCode::F(2) => {
            // F2 = Rename — VSCode's default. Opens the inline rename
            // editor on the selected entry. No-op on an empty tree.
            let idx = app.engine.selected_file_tree_idx();
            if let Some(entry) = app.engine.file_tree_entry(idx) {
                let parent = entry
                    .path
                    .parent()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default();
                app.begin_tree_edit(
                    reef_app::TreeEditMode::Rename,
                    parent,
                    Some(entry.path.clone()),
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
        // Vim-style alias: bare `d` = Move to Trash. Hard delete is
        // reachable via `Shift+Delete` / `Shift+Backspace` (handled
        // above). The capital `D` slot is now reserved for Duplicate
        // — see `handle_key_files_clipboard`. Ctrl / Alt modifiers
        // are rejected so chord bindings like Ctrl+D aren't silently
        // stolen.
        KeyCode::Char('d')
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            prompt_delete_selected(app, /*hard=*/ false);
        }
        _ => {}
    }
}

fn prompt_delete_selected(app: &mut App, hard: bool) {
    let idx = app.engine.selected_file_tree_idx();
    if let Some(entry) = app.engine.file_tree_entry(idx) {
        let Some(abs) = app.engine.file_tree_entry_abs_path(idx) else {
            return;
        };
        app.prompt_tree_delete(abs, entry.is_dir, hard);
    }
}

// ─── Tree modal keyboard helpers ─────────────────────────────────────────────

/// Tree-edit (inline New File / New Folder / Rename) keyboard owner.
/// Drains every keystroke into the buffer until Enter / Esc / Ctrl+C
/// exits — Tab / Up-Down are intentionally ignored so accidental
/// keyboard navigation can't orphan a half-typed filename.
fn handle_key_tree_edit(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // Phase 1: tree-edit-specific shortcuts (Esc/Enter/Ctrl+C).
    // Enter is strict bare-Enter only: Shift+Enter / Alt+Enter /
    // Ctrl+Enter would otherwise commit the rename on a stray
    // modifier press (the commit-box right next door treats
    // Shift+Enter as soft-newline; consistent muscle-memory says
    // Shift+Enter is non-destructive).
    match Keymap::resolve(InputScope::TreeEdit, &key) {
        Some(Command::Close) | Some(Command::Quit) => {
            app.cancel_tree_edit();
            return;
        }
        Some(Command::Confirm) if !ctrl && !alt && !shift => {
            app.commit_tree_edit();
            return;
        }
        _ => {}
    }

    if let Some(op) = crate::input_edit::op_for_key(&key) {
        app.engine.dispatch(AppCommand::EditTreeEditInput(op));
    }
}

/// Keyboard navigation for the right-click context menu popup.
fn handle_key_tree_context_menu(key: KeyEvent, app: &mut App) {
    match Keymap::resolve(InputScope::TreeContextMenu, &key) {
        Some(Command::Close) | Some(Command::Quit) => app.close_tree_context_menu(),
        Some(Command::MoveUp) => {
            app.engine.dispatch(AppCommand::NavigateTreeContextMenu(-1));
        }
        Some(Command::MoveDown) => {
            app.engine.dispatch(AppCommand::NavigateTreeContextMenu(1));
        }
        Some(Command::Confirm) => {
            if let Some(item) = app.engine.tree_context_menu_current() {
                app.dispatch_context_menu_item(item);
            }
        }
        // Any other key closes the menu (VSCode behaviour). Prevents
        // the menu from lingering if the user mis-clicks into it.
        _ => app.close_tree_context_menu(),
    }
}

/// Keyboard handler for the multi-candidate goto-definition popup.
/// Same UX shape as the tree context menu: arrow keys / `j`/`k` move,
/// Enter picks, Esc / `q` / Ctrl+C / any other key dismisses without
/// jumping.
fn handle_key_nav_candidates(key: KeyEvent, app: &mut App) {
    match Keymap::resolve(InputScope::NavCandidates, &key) {
        Some(Command::Close) => app.nav_close_candidates(),
        Some(Command::Quit) => {
            app.nav_close_candidates();
        }
        Some(Command::MoveUp) => app.nav_candidates_move(-1),
        Some(Command::MoveDown) => app.nav_candidates_move(1),
        Some(Command::Confirm) => app.nav_pick_candidate(),
        _ => app.nav_close_candidates(),
    }
}

/// Keyboard handler for the Ctrl+O hosts picker overlay.
///
/// Splits along the picker's input-mode:
/// - **Filter mode**: standard picker dispatch (list nav +
///   Esc/Enter + full editor vocabulary). One bespoke shortcut on top:
///   Ctrl+P switches to path mode.
/// - **Path mode**: no list, just a literal `[user@]host[:path]`
///   buffer. Esc demotes back to filter mode (preserves picker
///   state); Enter commits. Editor keys flow straight through
///   `input_edit::dispatch_key` against `path_buffer` / `path_cursor`.
fn handle_key_hosts_picker(key: KeyEvent, app: &mut App) {
    use reef_app::InputMode;

    match app.engine.hosts_picker_input_mode() {
        InputMode::Search => {
            // Picker-specific shortcut that has to precede PickerCore:
            // Ctrl+P would otherwise be eaten as list-up.
            if Keymap::resolve(InputScope::HostsPicker, &key) == Some(Command::TogglePickerPathMode)
            {
                app.engine.dispatch(AppCommand::EnterHostsPickerPathMode);
                return;
            }
            let input = crate::picker_core::input_for_key(InputScope::HostsPicker, &key);
            app.engine
                .dispatch(AppCommand::ApplyHostsPickerSearchInput(input));
            app.drain_engine_runtime_events();
        }
        InputMode::Path => {
            match Keymap::resolve(InputScope::HostsPicker, &key) {
                Some(Command::Close) => {
                    // Drop back to filter view rather than closing
                    // outright — gives the user a way out of the path
                    // buffer without losing the picker state.
                    app.engine
                        .dispatch(AppCommand::ReturnHostsPickerToSearchMode);
                    return;
                }
                // Symmetric with the path-mode entry: Ctrl+P also
                // exits back to filter mode. Without this, the only
                // way out is Esc, which is a UX asymmetry every
                // tester trips on at least once.
                Some(Command::TogglePickerPathMode) => {
                    app.engine
                        .dispatch(AppCommand::ReturnHostsPickerToSearchMode);
                    return;
                }
                Some(Command::Quit) => {
                    app.close_hosts_picker();
                    app.quit();
                    return;
                }
                // Strict bare Enter — Shift+Enter / Alt+Enter would
                // otherwise commit a half-typed target on a stray
                // modifier press (users muscle-memory Shift+Enter as
                // "newline" even in single-line buffers).
                Some(Command::Confirm) => {
                    app.confirm_hosts_picker();
                    return;
                }
                _ => {}
            }
            // Editor keys against the path buffer. No list = no
            // selected_idx side effect.
            if let Some(op) = crate::input_edit::op_for_key(&key) {
                app.engine
                    .dispatch(AppCommand::EditHostsPickerPathInput(op));
            }
        }
    }
}

/// Keyboard handler for the Graph tab's `b` branch picker.
///
/// All standard picker keys (Esc / Ctrl+C close, Enter commit,
/// Up/Down/Ctrl+J/K/N/P navigate, full text-editing vocabulary) are
/// owned by [`crate::picker_core::dispatch_key`]; we just
/// translate the returned `InputOutcome` into the picker-specific
/// app methods. No bespoke shortcuts (the graph picker has no leader
/// chord or mode-switch key), so the handler stays trivial.
fn handle_key_graph_branch_picker(key: KeyEvent, app: &mut App) {
    let input = crate::picker_core::input_for_key(InputScope::GraphBranchPicker, &key);
    app.engine
        .dispatch(AppCommand::ApplyGraphBranchPickerInput {
            input,
            visible_rows: 0,
        });
    app.drain_engine_runtime_events();
}

/// Keyboard handler for the generic `ConfirmModal`.
///
/// Routing:
/// - A request-specific confirm char (e.g. `Y` for delete) fires primary.
/// - `Esc` / `n` / `N` / `c` / `C` cancels.
/// - `Ctrl+C` keeps its global "force quit" meaning (mirrors paste-conflict).
/// - **`Enter` is intentionally ignored** so a stray return can't fire a
///   destructive primary action. Confirmation must be deliberate (typed
///   shortcut or a click).
/// - Modified chars (`Ctrl+*`, `Alt+*`) are *not* treated as the bare
///   letter — `Ctrl+Y` must not silently fire the destructive primary.
///   Shift is allowed: capital `Y` is the same intent as lowercase `y`
///   and matches via the `confirm_keys` list (which contains both).
/// - Other keys swallowed so they don't fall through to global hotkeys.
fn handle_key_confirm_modal(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if let KeyCode::Char('c') = key.code {
        if ctrl {
            app.quit();
            return;
        }
    }
    // Any ctrl/alt-modified char is not a confirm/cancel shortcut.
    // We still swallow it (no `_ => {}` fall-through to global keys)
    // since the modal owns the keyboard while up.
    if ctrl || alt {
        return;
    }
    match key.code {
        KeyCode::Char(c) => {
            let confirms = app
                .engine
                .confirm_request()
                .is_some_and(|request| ui::confirm_modal::confirm_key_matches(request, c));
            if confirms {
                app.fire_confirm_primary();
            } else if matches!(c, 'n' | 'N' | 'c' | 'C') {
                app.fire_confirm_cancel();
            }
        }
        KeyCode::Esc => app.fire_confirm_cancel(),
        _ => {}
    }
}

/// Status-bar takeover for the paste-conflict prompt.
///
/// Keys (case-insensitive primary letter):
/// - `R` → Replace this item
/// - `S` → Skip this item
/// - `K` → Keep both (rename via `next_copy_name`)
/// - `Shift+R` / `Shift+S` → Replace / Skip *all* remaining
/// - `C` / `Esc` → Cancel the entire batch
/// - other keys: ignored (don't accidentally close the prompt)
fn handle_key_paste_conflict(key: KeyEvent, app: &mut App) {
    use reef_core::file_ops::Resolution;
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => app.cancel_paste_conflict(),
        // Plain `Ctrl+C` keeps its global "force quit" meaning even
        // with the prompt up — same escape hatch as place-mode.
        KeyCode::Char('c') if ctrl => app.quit(),
        // Bare / Shift-only `c|C` cancels the prompt — matches the
        // `[C]ancel` letter advertised in the status-bar hint. Kept
        // below the `ctrl` arm so the global force-quit still wins.
        KeyCode::Char('c' | 'C') => app.cancel_paste_conflict(),
        KeyCode::Char('r' | 'R') => {
            app.resolve_paste_conflict(Resolution::Replace, shift);
        }
        KeyCode::Char('s' | 'S') => {
            app.resolve_paste_conflict(Resolution::Skip, shift);
        }
        KeyCode::Char('k' | 'K') => {
            // KeepBoth needs a fresh basename derived from the
            // destination's existing names. Compute on-demand so the
            // prompt doesn't have to keep a frozen snapshot in sync
            // with concurrent fs activity.
            let new_name = app
                .keep_both_name_for_current_conflict()
                .unwrap_or_else(|| "copy".to_string());
            app.resolve_paste_conflict(Resolution::KeepBoth(new_name), false);
        }
        KeyCode::Char('a' | 'A') => {
            // VS Code's "apply to all" defaults to Replace — the
            // most common reason a user reaches for "all" is "yes,
            // overwrite all my old files with the new ones". Skip
            // is reachable via Shift+S; KeepBoth-all needs per-item
            // renames so it isn't supported as a single key.
            app.resolve_paste_conflict(Resolution::Replace, true);
        }
        // No-op for any other key — keeps the prompt up so the user
        // doesn't accidentally cancel by pressing the wrong letter.
        _ => {}
    }
}

/// VS Code-style clipboard / multi-select bindings on the Files-tab
/// tree panel. Returns `true` when the key was consumed; the caller
/// (`handle_key_files`) routes the rest of the keys through its
/// regular nav / scroll arms when this returns `false`.
fn handle_key_files_clipboard(key: KeyEvent, app: &mut App, ctrl: bool, shift: bool) -> bool {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // ── primary clipboard bindings (vim-style) ────────────────
        KeyCode::Char('y') if !ctrl && !alt && !shift => {
            app.mark_copy(app.effective_action_paths());
            true
        }
        KeyCode::Char('x') if !ctrl && !alt && !shift => {
            app.mark_cut(app.effective_action_paths());
            true
        }
        KeyCode::Char('p') if !ctrl && !alt && !shift => {
            app.paste_into(app.paste_target_dir());
            true
        }
        // ──副绑定: Ctrl+Shift+C/X/V ───────────────────────────────
        // Available on terminals that report Shift+Ctrl letters
        // separately (kitty kbd protocol, iTerm2 / WezTerm with
        // CSI-u). Plain `Ctrl+C` still quits.
        KeyCode::Char('c' | 'C') if ctrl && shift => {
            app.mark_copy(app.effective_action_paths());
            true
        }
        KeyCode::Char('x' | 'X') if ctrl && shift => {
            app.mark_cut(app.effective_action_paths());
            true
        }
        KeyCode::Char('v' | 'V') if ctrl && shift => {
            app.paste_into(app.paste_target_dir());
            true
        }
        // ── Duplicate (capital `D`, no Ctrl/Alt) ──────────────────
        KeyCode::Char('D') if !ctrl && !alt => {
            app.duplicate_selection();
            true
        }
        // ── multi-select ──────────────────────────────────────────
        KeyCode::Char('s') if !ctrl && !alt && !shift => {
            app.engine.dispatch(AppCommand::ToggleCurrentFileSelection);
            true
        }
        KeyCode::Up if shift && !ctrl && !alt => {
            // Move the cursor first, then extend the contiguous
            // range to its new position. The selection's `anchor`
            // (set by `replace_with_single` on a fresh single click,
            // or by the first toggle) is the pivot.
            app.engine
                .dispatch(AppCommand::ExtendFileSelectionAfterTreeNav(-1));
            true
        }
        KeyCode::Down if shift && !ctrl && !alt => {
            app.engine
                .dispatch(AppCommand::ExtendFileSelectionAfterTreeNav(1));
            true
        }
        // Esc clears the selection ONLY if there's something to
        // clear; otherwise fall through so the caller can do its
        // normal Esc handling (currently no-op).
        KeyCode::Esc => {
            let had_selection = !app.engine.file_selection().is_empty();
            app.engine.dispatch(AppCommand::ClearFileSelection);
            had_selection
        }
        _ => false,
    }
}

// ─── Mouse ───────────────────────────────────────────────────────────────────

pub fn handle_mouse<B: Backend>(mouse: MouseEvent, app: &mut App, terminal: &Terminal<B>) {
    // SQLite goto-page input is modal: any mouse click outside the
    // input cancels it (matches popup-style behavior on the web).
    // Without this gate, clicking a pagination chip while the
    // prompt is open would request another page while the prompt
    // stayed visible. UI would say "go to page: 42" while the
    // displayed page had already moved.
    if app.engine.db_goto_active()
        && matches!(
            mouse.kind,
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_)
        )
    {
        app.engine.dispatch(AppCommand::CloseDbGoto);
        return;
    }

    // Palettes fully own mouse input while active (global-search first,
    // then quick-open): clicks must not leak through to hidden panels,
    // and scroll wheels inside the popup should move the selection.
    if app.engine.snapshot().overlays.hosts_picker {
        handle_mouse_hosts_picker(mouse, app);
        return;
    }
    if app.engine.snapshot().overlays.graph_branch_picker {
        handle_mouse_graph_branch_picker(mouse, app);
        return;
    }
    if app.engine.snapshot().overlays.global_search {
        global_search::handle_mouse(mouse, app);
        return;
    }
    if app.engine.snapshot().overlays.quick_open {
        quick_open::handle_mouse(mouse, app);
        return;
    }

    if app.engine.place_mode_active() {
        handle_mouse_place_mode(mouse, app);
        return;
    }

    // Mid-edit click cancels the inline editor before moving the row
    // cursor (mirrors `cancel_tree_edit` on the file tree); otherwise
    // the prompt + footer hint would point at a row the buffer no
    // longer belongs to.
    if app.engine.view_mode() == ViewMode::Settings {
        if matches!(mouse.kind, MouseEventKind::Down(_))
            && app.engine.settings().editor_edit.is_some()
        {
            app.engine
                .dispatch(AppCommand::CancelSettingsEditorCommandEdit);
        }
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row) {
                app.handle_action(action);
            }
        }
        return;
    }

    // Paste-conflict prompt owns input via the keyboard handler. Mouse
    // events must not race with the in-flight batch — clicking the
    // toolbar's `+ File` while the prompt is up would call
    // `fs_mutation_load.begin()`, bumping the generation and silently
    // dropping whichever result arrives first when the prompt
    // resolves. The prompt has no clickable elements of its own, so
    // bailing early is the conservative move; mirrors place_mode's
    // keyboard-only contract.
    if app.engine.paste_conflict_active() {
        return;
    }

    // Generic confirm modal fully owns mouse. Left-clicks dispatch via
    // the hit_registry (Cancel/Primary button zones + a full-screen
    // fallthrough Cancel zone registered in `confirm_modal::render`);
    // every other event is swallowed so scrolls / drags / right-clicks
    // can't leak to the panels underneath while the modal is up.
    //
    // Action filter: the registry from the *previous* frame may still
    // be live if the user clicks before the modal's first render lands
    // (key-then-immediate-mouse). In that window a click could resolve
    // to e.g. `TreeClick(n)`, silently mutating the file tree under
    // the overlay. We restrict dispatch to the two modal-owned
    // variants; any other hit (or no hit) collapses to "cancel" —
    // same semantics as clicking outside the rendered modal.
    if app.engine.confirm_request().is_some() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            match app.hit_registry.hit_test(mouse.column, mouse.row) {
                Some(action @ ui::mouse::ClickAction::ConfirmModalPrimary)
                | Some(action @ ui::mouse::ClickAction::ConfirmModalCancel) => {
                    app.handle_action(action);
                }
                _ => app.fire_confirm_cancel(),
            }
        }
        return;
    }

    // Intra-tree drag in progress: route Drag→hover-update,
    // Up→commit, Right-click→cancel. Scroll wheel falls through so
    // the user can scroll the tree to reach a deep destination
    // mid-drag.
    if app.engine.tree_drag_active() {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                let idx = match app.hit_registry.hit_test(mouse.column, mouse.row) {
                    Some(ui::mouse::ClickAction::TreeClick(i)) => Some(i),
                    _ => None,
                };
                app.update_tree_drag_hover(idx);
                app.update_tree_drag_modifiers(mouse.modifiers);
                return;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                app.commit_tree_drag(mouse.modifiers);
                return;
            }
            MouseEventKind::Down(MouseButton::Right) => {
                app.cancel_tree_drag();
                return;
            }
            _ => {} // scroll, mid-button: pass through
        }
    }

    // Mid-edit mouse-button press → cancel the inline editor. Then let
    // the click fall through to normal handling so clicking another
    // row still selects it, clicking a toolbar button still fires, etc.
    // Keeping the edit active across clicks makes the row UI lie about
    // what a subsequent Enter would commit (`parent_dir` is stale).
    // Move events and scroll wheel pass through untouched so hover /
    // scroll keep working while the user types.
    if app.engine.tree_edit_active() && matches!(mouse.kind, MouseEventKind::Down(_)) {
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
        if app.engine.active_tab() == Tab::Files
            && !app.engine.tree_edit_active()
            && app.engine.confirm_request().is_none()
        {
            // Second right-click while the menu is already open
            // dismisses it (Finder / VSCode behaviour).
            if app.engine.tree_context_menu_active() {
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

    // Find widget mouse intercept: when the user clicks a widget hit
    // zone (close `×`, prev/next arrows, Aa/ab/.* toggles), route
    // straight to `handle_action` BEFORE the preview/diff drag-select
    // fast-paths grab the Down event. Without this, every widget click
    // would silently start a text selection on the panel underneath.
    //
    // Non-widget clicks fall through — the widget is non-modal for
    // mouse, matching VSCode's behavior where clicking outside the
    // find widget doesn't dismiss it but does interact with the editor.
    if app.engine.find_widget().active
        && let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row)
        && matches!(
            action,
            ui::mouse::ClickAction::FindWidgetClose
                | ui::mouse::ClickAction::FindWidgetNext
                | ui::mouse::ClickAction::FindWidgetPrev
                | ui::mouse::ClickAction::FindWidgetToggleCase
                | ui::mouse::ClickAction::FindWidgetToggleWord
                | ui::mouse::ClickAction::FindWidgetToggleRegex
        )
    {
        app.handle_action(action);
        return;
    }

    // FocusedPreview chip / picker mouse intercept — sits in front of
    // the preview/diff drag-select fast-paths because the chip is
    // painted on top of the diff rect (col 0..3, row 0). Without this,
    // a click on the chip falls into `handle_diff_selection`'s
    // point_in_rect gate and starts a text-selection drag instead of
    // toggling the file picker. Mirror of the find-widget intercept
    // a few lines up.
    if app.engine.view_mode() == reef_app::ViewMode::FocusedPreview
        && let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row)
        && matches!(
            action,
            ui::mouse::ClickAction::ToggleFocusedPreviewFiles
                | ui::mouse::ClickAction::PickFocusedPreviewFile(_)
                | ui::mouse::ClickAction::CloseFocusedPreviewFiles
        )
    {
        app.handle_action(action);
        return;
    }

    // Nav candidates popup owns mouse input while open. Scroll wheel
    // moves the visible window; left-clicks on a row or the
    // fallthrough-close zone dispatch via the registry. Both must
    // preempt the preview drag-select fast-path, which only checks
    // `point_in_rect(last_preview_rect, ...)` and would otherwise
    // start a text selection on the pane underneath.
    if app.engine.nav_candidates_active() {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.nav_candidates_scroll(-1);
                return;
            }
            MouseEventKind::ScrollDown => {
                app.nav_candidates_scroll(1);
                return;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(action) = app.hit_registry.hit_test(mouse.column, mouse.row)
                    && matches!(
                        action,
                        ui::mouse::ClickAction::NavCandidateSelect(_)
                            | ui::mouse::ClickAction::NavCandidatesClose
                    )
                {
                    app.handle_action(action);
                    return;
                }
            }
            _ => {}
        }
    }

    // Clicking a rendered Markdown link opens it. Sits before preview
    // drag-selection so links behave like links; non-link text still
    // falls through to selectable reading-view text.
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && let Some(rect) = app.last_preview_rect
        && point_in_rect(rect, mouse.column, mouse.row)
        && let Some(action @ ui::mouse::ClickAction::OpenMarkdownLink(_)) =
            app.hit_registry.hit_test(mouse.column, mouse.row)
    {
        app.handle_action(action);
        return;
    }

    // Ctrl+click on the preview pane → goto-definition. Sits in front
    // of the drag-select fast-path so the click doesn't accidentally
    // start a new text selection.
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && mouse.modifiers.contains(KeyModifiers::CONTROL)
        && let Some(rect) = app.last_preview_rect
        && point_in_rect(rect, mouse.column, mouse.row)
    {
        app.goto_definition_at_cursor(NavAnchor::Mouse {
            col: mouse.column,
            row: mouse.row,
        });
        return;
    }

    // Ctrl+click inside the diff panel → goto-definition, the diff-view
    // analogue of the preview branch above. Gated on `last_diff_rect`
    // (set only when a real diff renders — Git working-tree/staged or
    // Graph commit), so it never competes with the preview branch (whose
    // `last_preview_rect` is None on those tabs). Must precede the diff
    // drag-selection handler so a Ctrl-modified Down resolves a jump
    // instead of starting a selection.
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && mouse.modifiers.contains(KeyModifiers::CONTROL)
        && let Some(rect) = app.last_diff_rect
        && point_in_rect(rect, mouse.column, mouse.row)
    {
        app.goto_definition_in_diff(NavAnchor::Mouse {
            col: mouse.column,
            row: mouse.row,
        });
        return;
    }

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
    if handle_commit_detail_selection(&mouse, app) {
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
                    app.engine.dispatch(AppCommand::SelectGlobalSearchResult {
                        idx,
                        visible_rows: app.layout.global_search_last_view_h as usize,
                    });
                    global_search::accept(app);
                    app.last_click = None;
                    return;
                }
                let effective = action;
                // Shift+Click on a graph row = extend the range, for
                // terminals that actually forward Shift+Click to the app.
                // Most macOS terminals intercept this for text selection;
                // those users should press `V` to enter visual mode and
                // click normally instead — the in-visual-mode click path
                // lives in `git_graph_panel::handle_command`.
                if mouse.modifiers.contains(KeyModifiers::SHIFT)
                    && let ui::mouse::ClickAction::GitCommand { command, args } = &effective
                    && command == "git.selectCommit"
                    && let Some(oid) = args.get("oid").and_then(|v| v.as_str())
                    && let Some(target_idx) = app.engine.graph_find_row_by_oid(oid)
                {
                    let delta = target_idx as i32 - app.engine.graph_selected_idx() as i32;
                    app.extend_graph_selection(delta);
                    app.last_click = if is_double {
                        None
                    } else {
                        Some((now, mouse.column, mouse.row))
                    };
                    return;
                }
                // Files-tab tree multi-select / drag-arm overlay.
                // Modifier handling sits before the standard
                // `handle_action` dispatch:
                //   Shift+Click → extend the selection range
                //   Ctrl+Click  → toggle a single row in/out
                //   plain click → arm a tree-drag press; clear any
                //                 multi-selection that doesn't
                //                 contain the clicked row, otherwise
                //                 leave the selection alone so the
                //                 drag (if it materialises) carries
                //                 the multi.
                if app.engine.active_tab() == Tab::Files
                    && let ui::mouse::ClickAction::TreeClick(idx) = effective
                    && app.engine.file_tree_entry_exists(idx)
                {
                    let shift_held = mouse.modifiers.contains(KeyModifiers::SHIFT);
                    let ctrl_held = mouse.modifiers.contains(KeyModifiers::CONTROL);
                    if shift_held {
                        app.engine
                            .dispatch(AppCommand::ExtendFileSelectionToIndex(idx));
                        app.last_click = if is_double {
                            None
                        } else {
                            Some((now, mouse.column, mouse.row))
                        };
                        return;
                    }
                    if ctrl_held {
                        app.engine
                            .dispatch(AppCommand::ToggleFileSelectionAtIndex(idx));
                        app.last_click = if is_double {
                            None
                        } else {
                            Some((now, mouse.column, mouse.row))
                        };
                        return;
                    }
                    // Plain left-click on a tree row.
                    app.engine.dispatch(AppCommand::ArmFileTreeDragPress {
                        idx,
                        col: mouse.column,
                        row: mouse.row,
                        mods: input_modifiers(mouse.modifiers),
                    });
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
            // A press that never crossed the drag threshold is a
            // plain click — disarm it so the next mouse interaction
            // starts clean.
            if !app.engine.tree_drag_active() && app.engine.tree_drag_press_armed() {
                app.engine.dispatch(AppCommand::CancelTreeDrag);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // Promote a Files-tab tree press to an active drag once
            // the cursor moves past `DRAG_START_THRESHOLD`. Sources
            // are snapshotted at promotion time — a mid-drag
            // selection mutation can't change what's being carried.
            if !app.engine.tree_drag_active()
                && app.engine.tree_drag_press_armed()
                && app
                    .engine
                    .tree_drag_should_start_drag(mouse.column, mouse.row)
            {
                app.begin_tree_drag(mouse.modifiers);
                let idx = match app.hit_registry.hit_test(mouse.column, mouse.row) {
                    Some(ui::mouse::ClickAction::TreeClick(i)) => Some(i),
                    _ => None,
                };
                app.update_tree_drag_hover(idx);
                return;
            }
            let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
            if app.dragging_split {
                if total_width > 0 {
                    let percent = (mouse.column * 100 / total_width).clamp(10, 80);
                    app.engine.dispatch(AppCommand::SetSplitPercent(percent));
                }
            } else if app.dragging_graph_diff_split && total_width > 0 {
                // Boundary is inside the non-graph region. Express the drag
                // position as the diff column's fraction of the remainder,
                // measured from the right edge so "pull left" = grow diff.
                let graph_x = total_width * app.engine.split_percent() / 100;
                let remainder = total_width.saturating_sub(graph_x);
                if remainder > 0 {
                    let from_right = total_width.saturating_sub(mouse.column);
                    let diff_pct = (from_right as u32 * 100 / remainder as u32) as u16;
                    // Floor 20 / ceiling 80 keeps both sub-columns usable
                    // and leaves room for drag to snap back either way.
                    app.engine
                        .dispatch(AppCommand::SetGraphDiffSplitPercent(diff_pct));
                }
            }
        }
        // Shift + 滚轮 = 横向滚动（兼容不发 ScrollLeft/Right 的终端）。
        // 路由在 dispatcher 之外是为了让每个 dispatcher 只负责一条轴。
        MouseEventKind::ScrollUp if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
            dispatch_horizontal_scroll(-1, mouse, app, terminal);
        }
        MouseEventKind::ScrollDown if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
            dispatch_horizontal_scroll(1, mouse, app, terminal);
        }
        MouseEventKind::ScrollUp => dispatch_vertical_scroll(-1, mouse, app, terminal),
        MouseEventKind::ScrollDown => dispatch_vertical_scroll(1, mouse, app, terminal),
        MouseEventKind::ScrollLeft => dispatch_horizontal_scroll(-1, mouse, app, terminal),
        MouseEventKind::ScrollRight => dispatch_horizontal_scroll(1, mouse, app, terminal),
        MouseEventKind::Moved => {
            app.hover_row = Some(mouse.row);
            app.hover_col = Some(mouse.column);

            let has_ctrl = mouse.modifiers.contains(KeyModifiers::CONTROL);
            // UX: a popup opened by Ctrl+click closes the moment the
            // user releases Ctrl. Popups opened by keyboard `gd` are
            // left alone — mouse motion shouldn't dismiss them.
            if !has_ctrl && app.engine.nav_candidates_opened_by_ctrl_click() {
                app.nav_close_candidates();
            }
            // Track the identifier under a Ctrl+hover for the
            // underline-on-hover affordance. Cleared whenever Ctrl
            // isn't held or the cursor leaves the preview pane —
            // matches editor convention.
            if has_ctrl
                && let Some(rect) = app.last_preview_rect
                && point_in_rect(rect, mouse.column, mouse.row)
                && let Some(origin) = app.last_preview_content_origin
                && let Some(cursor) = mouse_to_file_coord(app, mouse.column, mouse.row, origin)
            {
                let id_range = app
                    .engine
                    .preview_content_ref()
                    .and_then(|p| match &p.body {
                        reef_core::preview::PreviewBody::Text(text) => text
                            .parsed
                            .as_ref()
                            .and_then(|parse| reef_core::nav::identifier_range_at(parse, cursor)),
                        _ => None,
                    });
                app.engine
                    .dispatch(AppCommand::SetCtrlHoverTarget(id_range));
            } else {
                app.engine.dispatch(AppCommand::SetCtrlHoverTarget(None));
            }

            // Same affordance for the diff panel. No `FileParse` here, so
            // the hovered identifier is found by `word_at_byte` on the
            // row text (matching `resolve_diff_nav`). Computed in a scope
            // that drops the `last_diff_hit` borrow before assigning.
            let diff_hover = if has_ctrl
                && let Some(rect) = app.last_diff_rect
                && point_in_rect(rect, mouse.column, mouse.row)
            {
                app.last_diff_hit
                    .as_ref()
                    .and_then(|hit| hit.identifier_at(mouse.column, mouse.row))
                    .map(|(row, range, side)| crate::ui::selection::DiffHover { row, range, side })
            } else {
                None
            };
            app.diff_ctrl_hover = diff_hover;
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
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(rect) = app.last_preview_rect else {
                return false;
            };
            if !point_in_rect(rect, mouse.column, mouse.row) {
                return false;
            }
            // Focus follows the click — this handler returns `true` and
            // short-circuits the main dispatcher's `focus_panel_under_cursor`
            // call, so we have to promote the panel ourselves. Mirror of
            // the same line in `handle_diff_selection`.
            app.set_active_panel(Panel::Diff);

            let Some((file_line, byte_offset)) =
                mouse_to_preview_coord(app, mouse.column, mouse.row)
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

            let preview_line = preview_display_line(app, file_line).unwrap_or("");
            let line_len = preview_line.len();

            let sel = match click_count {
                2 => {
                    // Double-click → select word
                    let word = word_at_byte(preview_line, byte_offset);
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
            // Seed the drag-autoscroll tracker so the first tick after
            // Down can fire if the user clicks at the very edge.
            app.last_drag_mouse = Some((mouse.column, mouse.row));
            app.preview_autoscroll_at = None;
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let dragging = app.preview_selection.is_some_and(|s| s.dragging);
            if !dragging {
                return false;
            }
            // Refresh the autoscroll tracker even if origin is missing —
            // the tick will guard on origin separately and we still want
            // the latest mouse position for the next valid frame.
            app.last_drag_mouse = Some((mouse.column, mouse.row));
            if let Some(pos) = mouse_to_preview_coord(app, mouse.column, mouse.row) {
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
            // Drag ended — stop the autoscroll loop.
            app.last_drag_mouse = None;
            app.preview_autoscroll_at = None;
            let sel_snapshot = *sel;
            if !sel_snapshot.is_empty() {
                if let Some(preview) = app.engine.preview_content_ref() {
                    // Only text bodies have selectable lines — image/binary
                    // previews have no `lines` vector.
                    if preview.is_text() {
                        let text = collect_preview_selected_text(preview, &sel_snapshot);
                        if !text.is_empty() {
                            dispatch_clipboard_copy(app, text);
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
            let row_text = hit.rows[row_idx].text_for(side).to_string();

            // Focus follows the click — otherwise the user is stuck
            // scrolling the panel they came from (common in Graph 3-col:
            // start on Panel::Commit, click into diff, expect arrows to
            // pan the diff). Mirror of `focus_panel_under_cursor` but
            // local — the main-dispatcher version never runs because
            // this handler returns early on Down.
            app.set_active_panel(Panel::Diff);

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
            app.last_drag_mouse = Some((mouse.column, mouse.row));
            app.diff_autoscroll_at = None;
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let dragging = app.diff_selection.is_some_and(|s| s.sel.dragging);
            if !dragging {
                return false;
            }
            app.last_drag_mouse = Some((mouse.column, mouse.row));
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
            app.last_drag_mouse = None;
            app.diff_autoscroll_at = None;
            let snap = *sel;
            if !snap.sel.is_empty() {
                if let Some(hit) = app.last_diff_hit.as_ref() {
                    let text = collect_diff_selected_text(hit, &snap);
                    if !text.is_empty() {
                        dispatch_clipboard_copy(app, text);
                    }
                }
            }
            true
        }
        _ => false,
    }
}

fn handle_commit_detail_selection(mouse: &MouseEvent, app: &mut App) -> bool {
    let Some(rect) = app.last_commit_detail_rect else {
        return false;
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !point_in_rect(rect, mouse.column, mouse.row) {
                return false;
            }
            let Some(hit) = app.last_commit_detail_hit.as_ref() else {
                return false;
            };
            let Some((row_idx, byte_offset)) = hit.selectable_coord_for(mouse.column, mouse.row)
            else {
                return false;
            };
            let Some(row_text) = hit.text_for_row(row_idx) else {
                return false;
            };
            let row_text = row_text.to_string();

            if app.graph_uses_three_col() {
                app.set_active_panel(Panel::Commit);
            } else {
                app.set_active_panel(Panel::Diff);
            }

            let now = Instant::now();
            let click_count = if let Some((t, c, r, n)) = app.commit_detail_click_state {
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
            app.commit_detail_click_state = Some((now, mouse.column, mouse.row, click_count));

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
            app.commit_detail_selection = Some(sel);
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let dragging = app
                .commit_detail_selection
                .is_some_and(|selection| selection.dragging);
            if !dragging {
                return false;
            }
            let Some(hit) = app.last_commit_detail_hit.as_ref() else {
                return true;
            };
            if let Some(pos) = hit.selectable_coord_for_clamped(mouse.column, mouse.row)
                && let Some(selection) = app.commit_detail_selection.as_mut()
            {
                selection.active = pos;
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(selection) = app.commit_detail_selection.as_mut() else {
                return false;
            };
            if !selection.dragging {
                return false;
            }
            selection.dragging = false;
            let snap = *selection;
            if !snap.is_empty()
                && let Some(hit) = app.last_commit_detail_hit.as_ref()
            {
                let text = collect_commit_detail_selected_text(hit, &snap);
                if !text.is_empty() {
                    dispatch_clipboard_copy(app, text);
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
        (DiffLayout::Unified, _) => col,
        (DiffLayout::SideBySide, DiffSide::Unified) => col,
        (DiffLayout::SideBySide, DiffSide::SbsLeft) => {
            // Right edge of the left half is just before `right_start_x`
            // (which is the divider column). `saturating_sub(1)` keeps us
            // on the left half's last content column.
            col.min(hit.right_start_x.saturating_sub(1))
        }
        (DiffLayout::SideBySide, DiffSide::SbsRight) => col.max(hit.right_start_x),
    }
}

fn point_in_rect(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

fn preview_display_line(app: &App, row: usize) -> Option<&str> {
    let preview = app.engine.preview_content_ref()?;
    match &preview.body {
        reef_core::preview::PreviewBody::Markdown(markdown) => markdown.text_for_row(row),
        reef_core::preview::PreviewBody::Text(text) => text.lines.get(row).map(String::as_str),
        _ => None,
    }
}

fn collect_preview_selected_text(
    preview: &reef_core::preview::PreviewDocument,
    sel: &PreviewSelection,
) -> String {
    let rows = preview.body.display_text_rows();
    collect_selected_text_from_rows(rows.iter().map(|row| row.as_ref()), rows.len(), sel)
}

/// Translate a terminal `(column, row)` hit into `(file_line_index,
/// byte_offset_in_line)` using the cached content-area origin + current
/// scroll state. Returns `None` when the preview is empty / unloaded.
///
/// Columns left of the gutter collapse to byte 0; rows above the content area
/// collapse to file line `preview_scroll` (first visible line). Rows past the
/// last line clamp to the last line's terminator (so "drag past end" selects
/// through the final line cleanly).
pub(crate) fn mouse_to_file_coord(
    app: &App,
    col: u16,
    row: u16,
    origin: (u16, u16, u16),
) -> Option<(usize, usize)> {
    let preview = app.engine.preview_content_ref()?;
    let lines = match &preview.body {
        reef_core::preview::PreviewBody::Text(text) => &text.lines,
        // Image / binary cards don't have per-line content, so
        // drag-select is a no-op over them.
        _ => return None,
    };
    if lines.is_empty() {
        return None;
    }
    let (content_x, content_y, _) = origin;

    let visible_row = row.saturating_sub(content_y) as usize;
    let file_line = (app.engine.preview_scroll() + visible_row).min(lines.len() - 1);
    let line = &lines[file_line];

    let visible_col = (col.saturating_sub(content_x) as usize) + app.engine.preview_h_scroll();
    let byte_offset = col_to_byte_offset(line, visible_col);

    Some((file_line, byte_offset))
}

fn mouse_to_preview_coord(app: &App, col: u16, row: u16) -> Option<(usize, usize)> {
    let preview = app.engine.preview_content_ref()?;
    match &preview.body {
        reef_core::preview::PreviewBody::Markdown(markdown) => {
            if markdown.text_rows.is_empty() {
                return None;
            }
            let (content_x, content_y) = app.last_markdown_content_origin?;
            let visible_row = row.saturating_sub(content_y) as usize;
            let line_idx =
                (app.engine.preview_scroll() + visible_row).min(markdown.text_rows.len() - 1);
            let visible_col =
                (col.saturating_sub(content_x) as usize) + app.engine.preview_h_scroll();
            let byte_offset = col_to_byte_offset(&markdown.text_rows[line_idx], visible_col);
            Some((line_idx, byte_offset))
        }
        reef_core::preview::PreviewBody::Text(_) => {
            let origin = app.last_preview_content_origin?;
            mouse_to_file_coord(app, col, row, origin)
        }
        _ => None,
    }
}

fn preview_content_top(app: &App) -> Option<u16> {
    let preview = app.engine.preview_content_ref()?;
    match &preview.body {
        reef_core::preview::PreviewBody::Markdown(_) => {
            app.last_markdown_content_origin.map(|(_, y)| y)
        }
        reef_core::preview::PreviewBody::Text(_) => {
            app.last_preview_content_origin.map(|(_, y, _)| y)
        }
        _ => None,
    }
}

// ─── Drag-select auto-scroll ────────────────────────────────────────────────
//
// VSCode-style: while the left button is held and the cursor leaves the
// preview / diff viewport vertically, scroll the view toward the cursor and
// extend the selection along with it. Terminals don't deliver Drag events
// when the mouse is idle, so we replay the last known mouse position from
// `App::tick` and let the scroll edge "pull" the selection.
//
// Step size scales gently with distance — at the boundary one line per
// step, farther out up to four — so a hovering "just outside" reads as
// smooth and a deliberate "drag way past the panel" feels deliberate.
// A short throttle keeps a 60Hz tick loop from running away.

/// Number of lines to scroll per step, signed (negative = up). Zero when
/// the cursor is inside `[content_top, content_bottom)`.
fn autoscroll_step(mouse_y: u16, content_top: u16, content_bottom: u16) -> i32 {
    if mouse_y < content_top {
        let dist = (content_top - mouse_y) as i32;
        -((1 + dist / 3).min(4))
    } else if mouse_y >= content_bottom {
        let dist = (mouse_y - content_bottom + 1) as i32;
        (1 + dist / 3).min(4)
    } else {
        0
    }
}

/// Min wall-clock spacing between autoscroll steps. Distance is how far
/// the cursor sits past the nearest viewport edge — close = slow, far =
/// fast. The 15ms floor keeps a sustained "drag way off" capped near
/// 60-some lines/sec even when the tick loop runs at full 60Hz, so the
/// viewport doesn't slingshot.
fn autoscroll_interval(distance: u16) -> Duration {
    let denom = 1 + (distance as u64) / 2;
    let ms = (90 / denom).max(15);
    Duration::from_millis(ms)
}

/// `true` when a tick at `distance` from the viewport edge should be
/// throttled out — the previous step is still inside its cool-off
/// window. `None` last_at always passes (first step after Down).
fn autoscroll_throttled(last_at: Option<Instant>, distance: u16, now: Instant) -> bool {
    last_at.is_some_and(|at| now.duration_since(at) < autoscroll_interval(distance))
}

/// Distance (in terminal rows) between `mouse_y` and the viewport edge it
/// has crossed. 0 when inside the viewport. Used to scale step size and
/// throttle.
fn autoscroll_distance(mouse_y: u16, content_top: u16, content_bottom: u16) -> u16 {
    if mouse_y < content_top {
        content_top - mouse_y
    } else if mouse_y >= content_bottom {
        mouse_y - content_bottom + 1
    } else {
        0
    }
}

/// Run autoscroll for whichever panel currently owns a dragging selection.
/// Invoked from `App::tick`. No-op when no drag is active or when the
/// frozen mouse position sits inside the viewport.
pub fn tick_drag_autoscroll(app: &mut App) {
    tick_preview_drag_autoscroll(app);
    tick_diff_drag_autoscroll(app);
}

fn tick_preview_drag_autoscroll(app: &mut App) {
    let dragging = app.preview_selection.is_some_and(|s| s.dragging);
    if !dragging {
        return;
    }
    let Some((mx, my)) = app.last_drag_mouse else {
        return;
    };
    let view_h = app.layout.last_preview_view_h;
    if view_h == 0 {
        return;
    }
    let Some(content_top) = preview_content_top(app) else {
        return;
    };
    let content_bottom = content_top.saturating_add(view_h);
    let step = autoscroll_step(my, content_top, content_bottom);
    if step == 0 {
        return;
    }

    // Throttle: skip if the previous step is too recent for the current
    // distance. First step after Down/edge entry passes immediately
    // (preview_autoscroll_at cleared on Down / on Up).
    let now = Instant::now();
    let dist = autoscroll_distance(my, content_top, content_bottom);
    if autoscroll_throttled(app.preview_autoscroll_at, dist, now) {
        return;
    }

    // Clamp the scroll target to the line count, accounting for the
    // viewport height (you can't scroll the last line off the top).
    let Some(preview) = app.engine.preview_content_ref() else {
        return;
    };
    let line_count = match &preview.body {
        reef_core::preview::PreviewBody::Markdown(markdown) => markdown.text_rows.len(),
        reef_core::preview::PreviewBody::Text(text) => text.lines.len(),
        _ => return,
    };
    let max_scroll = line_count.saturating_sub(view_h as usize);
    let new_scroll = if step < 0 {
        app.engine
            .preview_scroll()
            .saturating_sub(step.unsigned_abs() as usize)
    } else {
        (app.engine.preview_scroll() + step as usize).min(max_scroll)
    };
    if new_scroll == app.engine.preview_scroll() {
        return;
    }
    app.engine
        .dispatch(AppCommand::SetPreviewVerticalScroll(new_scroll));
    app.preview_autoscroll_at = Some(now);

    // Re-translate the frozen mouse against the new scroll — this is what
    // makes the selection extend with the viewport as it scrolls.
    if let Some(coord) = mouse_to_preview_coord(app, mx, my) {
        if let Some(s) = app.preview_selection.as_mut() {
            s.active = coord;
        }
    }
}

fn tick_diff_drag_autoscroll(app: &mut App) {
    let dsel = match app.diff_selection {
        Some(d) if d.sel.dragging => d,
        _ => return,
    };
    let Some((mx, my)) = app.last_drag_mouse else {
        return;
    };
    let view_h = app.layout.last_diff_view_h;
    if view_h == 0 {
        return;
    }
    // Snapshot what we need from the cached DiffHit before we take a
    // mut borrow on the scroll field below.
    let Some((content_top, rows_len)) = app
        .last_diff_hit
        .as_ref()
        .map(|h| (h.content_y, h.rows.len()))
    else {
        return;
    };
    if rows_len == 0 {
        return;
    }
    let content_bottom = content_top.saturating_add(view_h);
    let step = autoscroll_step(my, content_top, content_bottom);
    if step == 0 {
        return;
    }

    let now = Instant::now();
    let dist = autoscroll_distance(my, content_top, content_bottom);
    if autoscroll_throttled(app.diff_autoscroll_at, dist, now) {
        return;
    }

    // Which scroll field backs the active diff panel — Git tab and Graph
    // tab keep their own offsets, mirroring `dispatch_vertical_scroll`.
    let Some(current_scroll) = app.engine.active_diff_vertical_scroll() else {
        return;
    };
    let max_scroll = rows_len.saturating_sub(view_h as usize);
    let new_scroll = if step < 0 {
        current_scroll.saturating_sub(step.unsigned_abs() as usize)
    } else {
        (current_scroll + step as usize).min(max_scroll)
    };
    if new_scroll == current_scroll {
        return;
    }
    app.engine
        .dispatch(AppCommand::SetActiveDiffVerticalScroll(new_scroll));
    app.diff_autoscroll_at = Some(now);

    // Sync the cached hit's scroll snapshot in place so `coord_for` picks
    // up the new value this tick — otherwise selection.active lags by a
    // frame and looks like a stutter against the smooth scroll.
    if let Some(hit) = app.last_diff_hit.as_mut() {
        hit.scroll = new_scroll;
    }

    if let Some(hit) = app.last_diff_hit.as_ref() {
        let clamped_col = clamp_col_to_side(hit, mx, dsel.side);
        if let Some(pos) = hit.coord_for(clamped_col, my, dsel.side) {
            if let Some(s) = app.diff_selection.as_mut() {
                s.sel.active = pos;
            }
        }
    }
}

/// Bidirectional axis-lock window. After a scroll dispatch on one
/// axis, events on the orthogonal axis are dropped for this duration
/// so trackpad noise during a primary swipe can't drift the view
/// sideways/upward against the user's intent. Renewed on every event
/// in the locked direction, so a continuous swipe holds the lock for
/// its entire duration; an intentional axis change just needs a
/// brief pause longer than this window.
const AXIS_LOCK_WINDOW: Duration = Duration::from_millis(120);

/// Time gap after which a streak counter resets to zero. Trackpad
/// scrolling fires ~60-90 events per second during a sustained
/// swipe (~12-16 ms inter-event), so 150 ms is comfortably long
/// enough that the streak survives the entire swipe but short
/// enough that an isolated noise event decays before the user's
/// intended swipe on the orthogonal axis even starts.
const AXIS_LOCK_STREAK_GAP: Duration = Duration::from_millis(150);

/// Number of consecutive same-axis events required before the lock
/// arms against the orthogonal axis. `2` means a single stray
/// trackpad-noise event never gates anything out — the user has to
/// be actually swiping on an axis (≥ 2 events in quick succession)
/// before its lock takes effect.
const AXIS_LOCK_STREAK_THRESHOLD: u32 = 2;

/// Per-axis scroll lock state — timestamp of the last event plus
/// the consecutive-event streak counter. Encapsulates the streak
/// reset / arm-after-N-events logic so the input dispatcher can
/// just call `observe()` on the firing axis and `locked()` on the
/// orthogonal one. The time-injection variants (`observe_at` /
/// `locked_at`) exist solely so the unit tests can drive synthetic
/// time without sleeping; production paths use the wall-clock
/// `observe()` / `locked()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct AxisLock {
    last_at: Option<Instant>,
    streak: u32,
}

impl AxisLock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self) {
        self.observe_at(Instant::now());
    }

    pub fn locked(&self) -> bool {
        self.locked_at(Instant::now())
    }

    fn observe_at(&mut self, now: Instant) {
        let stale = self
            .last_at
            .map(|at| now.duration_since(at) > AXIS_LOCK_STREAK_GAP)
            .unwrap_or(true);
        self.streak = if stale {
            1
        } else {
            self.streak.saturating_add(1)
        };
        self.last_at = Some(now);
    }

    fn locked_at(&self, now: Instant) -> bool {
        match self.last_at {
            Some(at) => {
                now.duration_since(at) < AXIS_LOCK_WINDOW
                    && self.streak >= AXIS_LOCK_STREAK_THRESHOLD
            }
            None => false,
        }
    }
}

/// Threshold separating "trackpad cadence" from "wheel cadence". macOS
/// trackpads deliver scroll events every ~12-16 ms during a swipe;
/// detent mouse wheels deliver them every ~100 ms or sparser. 60 ms
/// gives ample margin both ways without misclassifying decelerating
/// trackpad fade-outs.
const TRACKPAD_INTERVAL_THRESHOLD: Duration = Duration::from_millis(60);

/// Lines advanced per event at wheel cadence.
const SPARSE_STEP: usize = 3;

/// Base lines per event when scrolling at "trackpad cadence". `1`
/// keeps a gentle trackpad swipe roughly in line with native macOS
/// scroll throughput (~70 events/s × 1 line ≈ same lines/s as the
/// browser/`less`).
const DENSE_STEP_BASE: usize = 1;

/// After this many consecutive dense events, the per-event step
/// bumps from 1→2. At ~12-16 ms cadence, 8 events ≈ 100 ms of
/// sustained swipe — the user has clearly committed to a long
/// scroll, so we mildly accelerate.
const DENSE_ACCEL_AT_2: u32 = 8;

/// After this many consecutive dense events, the per-event step
/// caps at 3 (matches sparse). At ~15 ms cadence that's ~360 ms of
/// sustained trackpad swipe — long-haul navigation.
const DENSE_ACCEL_AT_3: u32 = 24;

/// Step-size pacer for wheel/trackpad scroll. Reads inter-event
/// intervals to distinguish dense trackpad streams (step = 1, with
/// mild acceleration on sustained swipes) from sparse wheel notches
/// (step = 3). Returns *magnitude only*; the dispatcher applies the
/// sign based on ScrollUp vs ScrollDown.
///
/// Two instances live on `App` — one for the vertical axis, one for
/// horizontal — so simultaneous V+H scrolling doesn't cross-pollute
/// streak state.
///
/// Like [`AxisLock`], the `step_at(now)` variant exists for unit
/// tests with injected time; production paths use [`Self::step`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrollPacer {
    last_at: Option<Instant>,
    dense_streak: u32,
}

impl ScrollPacer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn step(&mut self) -> usize {
        self.step_at(Instant::now())
    }

    fn step_at(&mut self, now: Instant) -> usize {
        let dense = self
            .last_at
            .map(|at| now.duration_since(at) < TRACKPAD_INTERVAL_THRESHOLD)
            .unwrap_or(false);
        self.last_at = Some(now);

        if dense {
            self.dense_streak = self.dense_streak.saturating_add(1);
            if self.dense_streak >= DENSE_ACCEL_AT_3 {
                3
            } else if self.dense_streak >= DENSE_ACCEL_AT_2 {
                2
            } else {
                DENSE_STEP_BASE
            }
        } else {
            self.dense_streak = 0;
            SPARSE_STEP
        }
    }
}

/// Apply a horizontal-scroll delta (in display columns) to whichever panel
/// the cursor sits over. Routed from Shift+wheel, trackpad ScrollLeft/Right,
/// and bare ← / → keys. Tab::Search is the only tab whose LEFT panel also
/// h-scrolls (the results list) — other tabs' left panels are tree/list
/// widgets with no long horizontal content.
fn apply_horizontal_scroll(app: &mut App, column: u16, total_width: u16, delta: i32) {
    app.engine
        .dispatch(reef_app::AppCommand::HorizontalScrollAtColumn {
            column,
            total_width,
            delta,
        });
}

/// Handle a vertical wheel/trackpad scroll event. `sign` is -1 for
/// `ScrollUp`, +1 for `ScrollDown`. Shift + wheel is routed to the
/// horizontal dispatcher upstream — this function only sees the
/// bare vertical axis.
fn dispatch_vertical_scroll<B: Backend>(
    sign: i32,
    mouse: MouseEvent,
    app: &mut App,
    terminal: &Terminal<B>,
) {
    let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
    // Drop bare vertical events that arrive during the tail of an
    // in-progress horizontal swipe (trackpad noise on the orthogonal
    // axis). Streak-gated so a single horizontal noise event doesn't
    // lock vertical out.
    if app.horizontal_scroll_lock.locked() {
        return;
    }
    app.vertical_scroll_lock.observe();
    let step_i = sign * (app.vertical_scroll_pacer.step() as i32);

    // FocusedPreview collapses the layout to a single content column —
    // the sidebar / commit middle column are hidden, so the usual
    // `is_left = column < graph_sidebar_width` heuristic would mis-route
    // wheel scrolls in the leftmost ~30 cols to the hidden tree/status.
    // Route directly to the panel that 纯预览 actually renders.
    if app.engine.view_mode() == reef_app::ViewMode::FocusedPreview {
        match app.engine.active_tab() {
            Tab::Files | Tab::Search => {
                app.engine.dispatch(AppCommand::PreviewScroll(step_i));
            }
            Tab::Git => {
                app.engine.dispatch(AppCommand::DiffScroll(step_i));
            }
            Tab::Graph => {
                if app.graph_uses_three_col() {
                    app.engine
                        .dispatch(AppCommand::ScrollCommitDetailFileDiffVertical(step_i));
                } else {
                    ui::commit_detail_panel::scroll(app, step_i);
                }
            }
        }
        return;
    }

    // Use the shared clamp + sidebar-hidden short-circuit so wheel
    // routing lines up with hit-testing. With sidebar hidden
    // `graph_sidebar_width` returns 0 and `is_left` never fires.
    let split_x = app.graph_sidebar_width(total_width);
    let is_left = mouse.column < split_x;
    match app.engine.active_tab() {
        Tab::Git => {
            if is_left {
                ui::git_status_panel::scroll(app, step_i);
            } else {
                app.engine.dispatch(AppCommand::DiffScroll(step_i));
            }
        }
        Tab::Files => {
            if is_left {
                app.engine.dispatch(AppCommand::ScrollFileTree(step_i));
            } else {
                app.engine.dispatch(AppCommand::PreviewScroll(step_i));
            }
        }
        Tab::Graph => {
            if is_left {
                ui::git_graph_panel::scroll(app, step_i);
            } else if let Some(diff_start) = graph_diff_column_start(app, total_width)
                && mouse.column >= diff_start
            {
                // 3-col diff column — scroll only the diff viewport
                // so commit metadata under the cursor's path stays put.
                app.engine
                    .dispatch(AppCommand::ScrollCommitDetailFileDiffVertical(step_i));
            } else {
                ui::commit_detail_panel::scroll(app, step_i);
            }
        }
        Tab::Search => {
            if is_left {
                // Left column IS the result list — move selection
                // rather than mutating a scroll offset.
                global_search::move_selection_by(app, step_i);
            } else {
                app.engine.dispatch(AppCommand::PreviewScroll(step_i));
            }
        }
    }
}

/// Handle a horizontal wheel/trackpad scroll event. `sign` is -1
/// for `ScrollLeft`, +1 for `ScrollRight`. Trackpad noise on the
/// orthogonal axis during an active vertical swipe is dropped via
/// the same axis-lock as the vertical path.
fn dispatch_horizontal_scroll<B: Backend>(
    sign: i32,
    mouse: MouseEvent,
    app: &mut App,
    terminal: &Terminal<B>,
) {
    // Axis lock: drop horizontal events that arrive during an
    // active vertical swipe. Streak-gated so a single vertical
    // noise event from a trackpad doesn't lock horizontal out.
    if app.vertical_scroll_lock.locked() {
        return;
    }
    let total_width = terminal.size().map(|s| s.width).unwrap_or(80);
    let step = app.horizontal_scroll_pacer.step() as i32;
    apply_horizontal_scroll(app, mouse.column, total_width, sign * step);
    app.horizontal_scroll_lock.observe();
}

// ─── Hosts picker overlay ────────────────────────────────────────────────────

/// Mouse dispatch for the Ctrl+O hosts picker. Clicks inside the popup
/// select a row (and double-click commits); clicks outside dismiss the
/// overlay, matching the quick-open / global-search click-away behaviour.
fn handle_mouse_hosts_picker(mouse: MouseEvent, app: &mut App) {
    let popup = match app.hosts_picker_popup_area {
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
                app.engine.dispatch(AppCommand::SelectHostsPickerRow(idx));
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
            let step = app.vertical_scroll_pacer.step() as i32;
            app.engine
                .dispatch(AppCommand::MoveHostsPickerSelection(-step));
        }
        MouseEventKind::ScrollDown if inside => {
            let step = app.vertical_scroll_pacer.step() as i32;
            app.engine
                .dispatch(AppCommand::MoveHostsPickerSelection(step));
        }
        _ => {}
    }
}

/// Mouse dispatch for the Graph tab branch picker. Same shape as
/// `handle_mouse_hosts_picker`: click inside selects (double-click
/// commits), click outside dismisses, scroll moves the selection.
fn handle_mouse_graph_branch_picker(mouse: MouseEvent, app: &mut App) {
    let popup = match app.graph_branch_picker_popup_area {
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
                app.close_graph_branch_picker();
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
            if let Some(ui::mouse::ClickAction::GraphBranchPickerSelect(idx)) =
                app.hit_registry.hit_test(mouse.column, mouse.row)
            {
                app.engine.dispatch(AppCommand::SelectGraphBranchPickerRow {
                    idx,
                    visible_rows: 0,
                });
                if is_double {
                    app.confirm_graph_branch_picker();
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
            let step = app.vertical_scroll_pacer.step() as i32;
            app.engine
                .dispatch(AppCommand::MoveGraphBranchPickerSelection {
                    delta: -step,
                    visible_rows: 0,
                });
        }
        MouseEventKind::ScrollDown if inside => {
            let step = app.vertical_scroll_pacer.step() as i32;
            app.engine
                .dispatch(AppCommand::MoveGraphBranchPickerSelection {
                    delta: step,
                    visible_rows: 0,
                });
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
            let step = app.vertical_scroll_pacer.step();
            app.engine
                .dispatch(AppCommand::ScrollFileTree(-(step as i32)));
        }
        MouseEventKind::ScrollDown => {
            let step = app.vertical_scroll_pacer.step();
            app.engine.dispatch(AppCommand::ScrollFileTree(step as i32));
        }
        _ => {}
    }
}

// ─── Bracketed paste dispatch ────────────────────────────────────────────────

/// Entry point for `Event::Paste(s)` from the main loop.
///
/// Routing priority mirrors the gate stack in [`handle_key`]: paste
/// targets the same buffer that a keystroke at this moment would type
/// into. The unification was driven by Space+F find widget pastes
/// silently dropping; once one overlay needed paste support, lining
/// the rest of the gate stack up was free.
///
/// 1. **Drop targets first.** If the payload parses as one or more
///    existing absolute paths, enter place mode — Finder drops land
///    here regardless of which overlay happens to be focused.
/// 2. **Otherwise route to whichever input owns the keyboard right
///    now.** Each branch mirrors its `handle_key_*` counterpart's
///    gate, in the same order, so paste behaviour and typing behaviour
///    can't drift apart.
/// 3. **Fallthrough drops silently.** A paste landing on plain tab
///    navigation has no sensible target.
pub fn handle_paste(s: String, app: &mut App) {
    let paths = parse_dropped_paths(&s);
    if !paths.is_empty() {
        app.enter_place_mode(paths);
        return;
    }

    // Modal gates that own the keyboard but have NO text input of
    // their own (tree_context_menu / confirm_modal / paste_conflict /
    // place_mode / tree_drag). `handle_key` early-returns under each;
    // mirror that by swallowing the paste, otherwise it would fall
    // through to the sticky trailing branches (Tab::Git commit
    // textarea, Tab::Search input) and silently mutate a buffer the
    // user can't see behind the modal.
    if app.engine.tree_context_menu_active()
        || app.engine.confirm_request().is_some()
        || app.engine.paste_conflict_active()
        || app.engine.place_mode_active()
        || app.engine.tree_drag_active()
    {
        return;
    }

    // Mirror `handle_key`'s gate stack so paste lands wherever a
    // keystroke at this moment would type. Order matters — db_goto
    // sits above every overlay because it's an inline prompt that
    // owns input even while another overlay is technically open.
    if app.engine.db_goto_active() {
        app.engine.dispatch(AppCommand::PasteDbGoto(s));
    } else if app.engine.snapshot().overlays.hosts_picker {
        app.engine.dispatch(AppCommand::PasteHostsPicker(s));
    } else if app.engine.snapshot().overlays.graph_branch_picker {
        app.engine.dispatch(AppCommand::PasteGraphBranchPicker(s));
    } else if app.engine.find_widget().active {
        find_widget::handle_paste(&s, app);
    } else if app.engine.snapshot().overlays.global_search {
        global_search::handle_paste_overlay(&s, app);
    } else if app.engine.snapshot().overlays.quick_open {
        quick_open::handle_paste(&s, app);
    } else if app.engine.search().active {
        search::handle_paste(&s, app);
    } else if app.engine.tree_edit_active() {
        // Per-char predicate: reject path separators, NUL, control
        // chars. Mirrors `handle_key_tree_edit`'s phase-2 filter so
        // the buffer stays perpetually-valid across both typing and
        // pasting. The error banner only clears when at least one
        // char actually landed — a fully-rejected paste (e.g. all
        // `/`s) must not silently wipe a validation message the
        // user hasn't addressed.
        app.engine.dispatch(AppCommand::PasteTreeEdit(s));
    } else if app.engine.view_mode() == ViewMode::Settings {
        // Settings owns every key when open. `editor_edit` is the
        // only text input inside; everything else (nav, toggles)
        // has no paste target. Unconditional early-return mirrors
        // `handle_key_settings` so a paste while the menu is up
        // can't fall through to the commit-textarea / search-input
        // trailing branches.
        if app.engine.settings().editor_edit.is_some() {
            app.engine
                .dispatch(AppCommand::PasteSettingsEditorCommand(s));
        }
    } else if app.engine.active_tab() == Tab::Search
        && app.engine.active_panel() == Panel::Files
        && app.engine.snapshot().overlays.search_input
    {
        global_search::handle_paste_search_tab(&s, app);
    } else if app.engine.active_tab() == Tab::Git
        && app.engine.active_panel() == Panel::Files
        && app.engine.is_commit_editing()
    {
        // Multi-line textarea: keep `\n` (the textarea is multi-line)
        // but strip `\r` so CRLF clipboard payloads don't embed
        // carriage returns into the commit message. Without this,
        // Windows users pasting a multi-line draft on a terminal that
        // disagrees with bracketed-paste land with `^M` characters in
        // `git log` and tripped commit-lint hooks.
        app.engine.dispatch(AppCommand::PasteCommitMessage(s));
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

/// Anchor every time-injection test on a single base instant so
/// durations line up with module constants. `Instant` has no public
/// `ZERO`, so the test helper caches one `Instant::now()` and adds
/// offsets — used by both `axis_lock_tests` and `scroll_pacer_tests`.
#[cfg(test)]
fn at(ms: u64) -> Instant {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    base + Duration::from_millis(ms)
}

#[cfg(test)]
mod axis_lock_tests {
    use super::*;

    #[test]
    fn empty_lock_is_unlocked() {
        let lock = AxisLock::new();
        assert!(!lock.locked_at(at(0)));
    }

    #[test]
    fn single_event_does_not_arm_lock() {
        // Threshold is 2 — a single isolated trackpad-noise event
        // must never gate out the orthogonal axis. This is the
        // core property the streak refactor was introduced for.
        let mut lock = AxisLock::new();
        lock.observe_at(at(0));
        assert_eq!(lock.streak, 1);
        assert!(!lock.locked_at(at(0)));
        assert!(!lock.locked_at(at(50)));
    }

    #[test]
    fn two_events_within_gap_arm_lock() {
        let mut lock = AxisLock::new();
        lock.observe_at(at(0));
        lock.observe_at(at(50)); // 50 ms < 150 ms gap
        assert_eq!(lock.streak, 2);
        assert!(lock.locked_at(at(50)));
    }

    #[test]
    fn streak_resets_after_gap() {
        // An event arriving after AXIS_LOCK_STREAK_GAP (150 ms)
        // restarts the streak from 1 — even prior accumulation
        // doesn't leak forward, so a stale `last_at` from minutes
        // ago can't accidentally pre-arm the lock on a fresh swipe.
        let mut lock = AxisLock::new();
        lock.observe_at(at(0));
        lock.observe_at(at(50));
        assert_eq!(lock.streak, 2);
        // 50 + 200 = 250 ms gap — well past the 150 ms reset.
        lock.observe_at(at(250));
        assert_eq!(lock.streak, 1);
        assert!(!lock.locked_at(at(250)));
    }

    #[test]
    fn lock_decays_after_window() {
        // After arming, the lock holds for AXIS_LOCK_WINDOW (120 ms)
        // past the most recent event. Beyond that, even with a high
        // streak count, the lock is considered released.
        let mut lock = AxisLock::new();
        lock.observe_at(at(0));
        lock.observe_at(at(50));
        assert!(lock.locked_at(at(50)));
        // Still inside the 120 ms window (110 ms since last event).
        assert!(lock.locked_at(at(160)));
        // Past the 120 ms window (130 ms since last event) — released.
        assert!(!lock.locked_at(at(180)));
    }

    #[test]
    fn sustained_swipe_holds_lock_continuously() {
        // A real swipe fires events every ~12-16 ms; the lock must
        // stay armed for the entire swipe via per-event renewal,
        // not decay between them. Simulate 20 events at 15 ms apart.
        let mut lock = AxisLock::new();
        for i in 0..20 {
            lock.observe_at(at(i * 15));
        }
        // After 19 events (offset 0..285 ms), lock should still be
        // armed because each event renewed `last_at`.
        assert!(lock.locked_at(at(285)));
        // And after the swipe ends, the lock decays within
        // AXIS_LOCK_WINDOW after the final event (285 + 120 = 405).
        assert!(!lock.locked_at(at(410)));
    }
}

#[cfg(test)]
mod autoscroll_tests {
    use super::*;

    // ── autoscroll_step ─────────────────────────────────────────────────

    #[test]
    fn step_zero_inside_viewport() {
        // Viewport rows [10, 30). Mouse comfortably inside.
        assert_eq!(autoscroll_step(15, 10, 30), 0);
        assert_eq!(autoscroll_step(10, 10, 30), 0); // top edge inclusive
        assert_eq!(autoscroll_step(29, 10, 30), 0); // bottom edge inclusive
    }

    #[test]
    fn step_one_just_above_top() {
        // 1 row above the top → minimum step of -1.
        assert_eq!(autoscroll_step(9, 10, 30), -1);
    }

    #[test]
    fn step_one_just_below_bottom() {
        // content_bottom is exclusive, so row 30 itself is "1 outside".
        assert_eq!(autoscroll_step(30, 10, 30), 1);
    }

    #[test]
    fn step_caps_at_four_far_above() {
        // Very far above the viewport — distance grows but step caps at 4.
        assert_eq!(autoscroll_step(0, 100, 130), -4);
    }

    #[test]
    fn step_caps_at_four_far_below() {
        // distance = my - bottom + 1 = 200 - 30 + 1 = 171, step = (1 + 57).min(4) = 4
        assert_eq!(autoscroll_step(200, 10, 30), 4);
    }

    #[test]
    fn step_scales_gently_near_edge() {
        // dist 3 (mouse_y = top-3): step = -(1 + 3/3).min(4) = -2
        assert_eq!(autoscroll_step(7, 10, 30), -2);
        // dist 6: step = -(1 + 6/3).min(4) = -3
        assert_eq!(autoscroll_step(4, 10, 30), -3);
        // dist 9: step = -(1 + 9/3).min(4) = -4 (cap)
        assert_eq!(autoscroll_step(1, 10, 30), -4);
    }

    // ── autoscroll_distance ─────────────────────────────────────────────

    #[test]
    fn distance_zero_when_inside() {
        assert_eq!(autoscroll_distance(15, 10, 30), 0);
        assert_eq!(autoscroll_distance(10, 10, 30), 0);
        assert_eq!(autoscroll_distance(29, 10, 30), 0);
    }

    #[test]
    fn distance_above_top_counts_rows_up() {
        assert_eq!(autoscroll_distance(9, 10, 30), 1);
        assert_eq!(autoscroll_distance(0, 10, 30), 10);
    }

    #[test]
    fn distance_below_bottom_counts_rows_down() {
        // content_bottom = 30 exclusive: row 30 is "1 below".
        assert_eq!(autoscroll_distance(30, 10, 30), 1);
        assert_eq!(autoscroll_distance(40, 10, 30), 11);
    }

    // ── autoscroll_interval ─────────────────────────────────────────────

    #[test]
    fn interval_at_boundary_is_around_90ms() {
        // distance 1 → denom = 1 + 0 = 1 → 90 ms.
        assert_eq!(autoscroll_interval(1), Duration::from_millis(90));
    }

    #[test]
    fn interval_shrinks_with_distance() {
        // distance grows → interval shrinks (faster scroll).
        let a = autoscroll_interval(1);
        let b = autoscroll_interval(5);
        let c = autoscroll_interval(20);
        assert!(b < a);
        assert!(c < b);
    }

    #[test]
    fn interval_floors_at_15ms() {
        // Way past the viewport — must not run away faster than 15ms/step.
        assert_eq!(autoscroll_interval(500), Duration::from_millis(15));
        assert_eq!(autoscroll_interval(u16::MAX), Duration::from_millis(15));
    }
}

#[cfg(test)]
mod scroll_pacer_tests {
    use super::*;

    /// Sub-threshold inter-event interval used to simulate trackpad
    /// cadence — must stay well below `TRACKPAD_INTERVAL_THRESHOLD`.
    const DENSE_MS: u64 = 15;

    #[test]
    fn first_event_returns_sparse_step() {
        let mut p = ScrollPacer::new();
        assert_eq!(p.step_at(at(0)), SPARSE_STEP);
    }

    #[test]
    fn dense_cadence_returns_base_step() {
        let mut p = ScrollPacer::new();
        p.step_at(at(0));
        assert_eq!(p.step_at(at(30)), DENSE_STEP_BASE);
    }

    #[test]
    fn sparse_cadence_after_pause_returns_sparse() {
        let mut p = ScrollPacer::new();
        p.step_at(at(0));
        p.step_at(at(30));
        assert_eq!(p.step_at(at(230)), SPARSE_STEP);
    }

    #[test]
    fn dense_streak_accelerates_to_2() {
        // Streak counts dense events. Event 0 is sparse (no prior
        // interval) so streak stays at 0; events 1..=7 grow it to 7
        // (still base); event 8 hits DENSE_ACCEL_AT_2 and bumps to 2.
        let mut p = ScrollPacer::new();
        p.step_at(at(0));
        for i in 1..DENSE_ACCEL_AT_2 {
            let s = p.step_at(at(i as u64 * DENSE_MS));
            assert_eq!(s, DENSE_STEP_BASE, "event {i} should still be base");
        }
        let s = p.step_at(at(DENSE_ACCEL_AT_2 as u64 * DENSE_MS));
        assert_eq!(s, 2);
    }

    #[test]
    fn dense_streak_caps_at_3() {
        let mut p = ScrollPacer::new();
        p.step_at(at(0));
        let mut last = 0;
        for i in 1..=100 {
            last = p.step_at(at(i * DENSE_MS));
        }
        assert_eq!(last, 3);
    }

    #[test]
    fn sparse_event_resets_dense_streak() {
        let mut p = ScrollPacer::new();
        p.step_at(at(0));
        for i in 1..=30 {
            p.step_at(at(i * DENSE_MS));
        }
        // A >threshold gap should drop streak back to 0 — without it,
        // the next dense event would carry over the accelerated step.
        let sparse = p.step_at(at(30 * DENSE_MS + 200));
        assert_eq!(sparse, SPARSE_STEP);
        let dense = p.step_at(at(30 * DENSE_MS + 200 + DENSE_MS));
        assert_eq!(dense, DENSE_STEP_BASE);
    }

    #[test]
    fn accelerates_to_3_at_exact_boundary() {
        // The accel curve uses `>=`, so streak = DENSE_ACCEL_AT_3 is
        // exactly where step bumps to 3. Locks in the boundary so a
        // future refactor to `>` would be caught here.
        let mut p = ScrollPacer::new();
        p.step_at(at(0));
        for i in 1..DENSE_ACCEL_AT_3 {
            p.step_at(at(i as u64 * DENSE_MS));
        }
        let s = p.step_at(at(DENSE_ACCEL_AT_3 as u64 * DENSE_MS));
        assert_eq!(s, 3);
    }

    #[test]
    fn interval_exactly_at_threshold_is_sparse() {
        // Comparison is strict `<`, so 60 ms exactly = sparse — guards
        // against a future refactor flipping to `<=`.
        let mut p = ScrollPacer::new();
        p.step_at(at(0));
        assert_eq!(p.step_at(at(60)), SPARSE_STEP);
    }
}

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
    fn shift_uppercase_f_after_arm_fires_for_global_search() {
        // Most terminals report SHIFT alongside the uppercase char. The
        // upper-case chord branch must accept SHIFT, otherwise
        // `Space+Shift+F` (the global-search chord) is unreachable on
        // those terminals.
        let now = Instant::now();
        let shift_f = ke(KeyCode::Char('F'), KeyModifiers::SHIFT);
        let v = leader_decision(&shift_f, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Fire);
    }

    #[test]
    fn g_is_not_a_chord_target_and_consumes_leader() {
        // `g` / `G` used to step the find widget; the chord was removed.
        // Make sure no future commit re-adds it without updating intent —
        // and make sure today's behavior is "consume the leader, drop the
        // key" rather than "fire something stale".
        let now = Instant::now();
        let g = ke(KeyCode::Char('g'), KeyModifiers::empty());
        let v = leader_decision(&g, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Consume);

        let shift_g = ke(KeyCode::Char('G'), KeyModifiers::SHIFT);
        let v = leader_decision(&shift_g, true, Some(now), now, LEADER_TIMEOUT);
        assert_eq!(v, LeaderVerdict::Consume);
    }

    #[test]
    fn ctrl_f_is_not_a_chord_target() {
        // Only bare `f` / `F` is the FindWidget chord target — Ctrl+F has no
        // global binding (Find is reached exclusively via Space+F), so when
        // the leader is armed pressing Ctrl+F just consumes the leader.
        // NOTE: this test only covers `leader_decision`'s view. The
        // integration-level guarantee that "Ctrl+F doesn't toggle diff
        // mode / layout in Git/Graph tabs" is asserted by the
        // `ctrl_f_in_main_mode_*` tests in tests/focused_preview_scroll.rs.
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
