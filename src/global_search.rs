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
//! (path + row + byte range) that `file_preview_panel` overlays once the
//! async preview arrives. See `app::PreviewHighlight` + the tick handler.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crate::app::{App, PreviewHighlight, Tab};
use crate::input::DOUBLE_CLICK_WINDOW;
use crate::input_edit;
use crate::ui::mouse::ClickAction;

/// Hard cap on result count. VSCode uses 20k but virtualises its tree; our
/// flat TUI list becomes unhelpful long before that, so we stop scanning
/// and surface a "refine query" hint.
pub const MAX_RESULTS: usize = 1000;

/// Per-line preview text is truncated to this many chars (at a UTF-8
/// boundary). Matches VSCode's `previewText` limit — long enough to see
/// context, short enough that minified bundles don't wreck the list.
pub const MAX_LINE_CHARS: usize = 250;

/// Cap on `results_h_scroll` so `End` has something concrete to jump to
/// and `Right` doesn't drift unbounded. Equals [`MAX_LINE_CHARS`] because
/// there's never anything to scroll past that.
pub const MAX_H_SCROLL: usize = MAX_LINE_CHARS;

/// Debounce window from the last keystroke to the kickoff of a new search.
/// 300ms matches VSCode's default `search.searchOnTypeDebouncePeriod`.
pub const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// Three-state focus for the Tab::Search left panel. The middle variant
/// (`ReplaceInput`) is only reachable when `replace_open` is true. See
/// `GlobalSearchState::focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPanelFocus {
    FindInput,
    ReplaceInput,
    List,
}

/// One search hit flattened for the UI. `line_text` is already truncated to
/// [`MAX_LINE_CHARS`] chars, and `byte_range` has been clipped to fall
/// within that truncated text so renderers can slice freely.
#[derive(Debug, Clone)]
pub struct MatchHit {
    pub path: PathBuf,
    /// Workdir-relative display string, pre-computed to avoid re-stringifying
    /// on every render.
    pub display: String,
    /// 0-indexed line number.
    pub line: usize,
    pub line_text: String,
    pub byte_range: Range<usize>,
}

pub struct GlobalSearchState {
    pub active: bool,
    pub query: String,
    /// Byte offset into `query`. Always on a char boundary.
    pub cursor: usize,
    pub selected: usize,
    pub scroll: usize,
    /// Results are sorted by path so same-file hits are adjacent in the list
    /// (mirrors VSCode's data-layer grouping without a full tree UI).
    pub results: Vec<MatchHit>,
    /// True once the worker reports hitting `MAX_RESULTS`; UI surfaces
    /// "1000+ (refine query)" in the footer.
    pub truncated: bool,

    /// Shared with the worker thread. Flipping to `true` lets an in-flight
    /// searcher bail on the next file boundary (worker polls it every few
    /// files). A fresh `Arc` is allocated per search so the old thread's
    /// observation of `true` doesn't affect the new one. Kept on this
    /// struct rather than folded into `AsyncState` because cancellation
    /// isn't part of the one-shot AsyncState contract — AsyncState handles
    /// the generation/loading half, this handles cooperative abort.
    pub cancel: Arc<AtomicBool>,
    /// Timestamp of the most recent edit to `query`. `None` means "no
    /// pending search" — set by edit helpers, cleared when `tick()` fires
    /// the next search.
    pub last_keystroke_at: Option<Instant>,
    /// Query that actually ran (or is running). Tick compares against
    /// `query` to avoid re-firing a search for an unchanged input.
    pub last_searched_query: String,

    pub last_view_h: u16,
    pub last_popup_area: Option<Rect>,

    /// Palette-side Space-leader slot. Re-uses `input::leader_decision` so
    /// Space-F inside the palette (when `query` is empty) closes it, same
    /// pattern as `quick_open::space_leader_at`. Only armed while the query
    /// is empty so "foo" containing a space works as a literal char.
    pub space_leader_at: Option<Instant>,

    /// Where keyboard input lands inside the persistent Tab::Search view.
    /// Three states because replace mode adds a second input row:
    ///
    /// - `FindInput` — typing edits `query` (search pattern).
    /// - `ReplaceInput` — typing edits `replace_text`. Only reachable
    ///   when `replace_open` is true.
    /// - `List` — typing is dispatched to per-tab list-mode shortcuts
    ///   (`↑↓`, `Enter`, `Space` to toggle a checkbox in replace mode,
    ///   etc.). Bare chars become tab-cycle / search-step shortcuts via
    ///   the global handler in `input.rs`.
    ///
    /// Transitions: `/` or `i` in list mode → `FindInput`; `Esc` in input
    /// mode → `List`; `Tab` in input mode cycles
    /// `FindInput → ReplaceInput → List → FindInput` (skipping
    /// `ReplaceInput` when `replace_open` is false); overlay pin
    /// (Alt/Ctrl+Enter) → `FindInput`. Preserved across tab-switches so
    /// bouncing out to Files and back keeps you where you were.
    ///
    /// Unused by the overlay — the overlay is always implicitly focused
    /// on its single input row (= `FindInput`-like behaviour).
    pub focus: SearchPanelFocus,

    /// VSCode-style "Replace in Files" toggle. When false, Tab::Search
    /// behaves exactly as before (single input, no checkboxes). When
    /// true, a second `replace_text` input row appears below the find
    /// input, each result row gets a checkbox, and `Ctrl/Alt+Enter`
    /// commits the replace via `FilesTask::ReplaceInFiles`. Toggled by
    /// the chevron click, the Space+H leader chord, or bare `r` in
    /// list mode.
    pub replace_open: bool,
    /// Replacement string. Empty allowed (= delete the matched span).
    pub replace_text: String,
    /// Byte offset of the caret in `replace_text`. Always on a char
    /// boundary; same invariant as `cursor` for `query`.
    pub replace_cursor: usize,
    /// Per-match opt-out set, keyed by `(path, line)`. A `(p, l)` pair
    /// in this set means "do NOT replace this hit when Apply runs."
    /// Default behaviour is to include every streamed match — toggling
    /// a checkbox off inserts here; toggling back on removes. Cleared
    /// only on user-initiated `mark_query_edited` (a fresh search), so
    /// fs-watcher-driven re-runs preserve the user's per-row choices.
    pub excluded: HashSet<(PathBuf, usize)>,
    /// `Some((files_done, files_total))` while a replace batch is in
    /// flight, fed by `WorkerResult::ReplaceProgress` events. Footer
    /// renders this as `Replacing N/M…`. `None` between batches.
    ///
    /// In-flight state itself lives on `App.replace_load: AsyncState`
    /// (the same generation gate every other async path uses); the
    /// footer reads `app.replace_load.loading`. Keep the progress
    /// counter here because it's UI presentation state and the
    /// `AsyncState` contract doesn't carry payloads.
    pub replace_progress: Option<(usize, usize)>,

    /// Uniform horizontal scroll offset applied to every result row's
    /// line-text column. When `0`, the renderer falls back to the
    /// per-row smart shift (match stays in view); any non-zero value
    /// disables smart mode and all rows are rendered from this column,
    /// so the user's scroll is predictable across the list.
    ///
    /// Reset to 0 on query change (alongside `selected` / `scroll`) so
    /// fresh results land in smart-view rather than some stale offset.
    pub results_h_scroll: usize,

    /// When `Some(t)`, a preview-sync is pending and should fire at time
    /// `t`. Written by `schedule_preview_sync` (called from keyboard nav
    /// handlers) and drained by `App::tick`. Debounces the
    /// `load_preview_for_path` calls so holding ↓ across 50 rows doesn't
    /// pile up 50 supersedable jobs on the preview worker thread.
    ///
    /// Explicit actions (click, overlay pin, chunk-arrival sync) skip the
    /// debounce and call `navigate_to_selected` directly — those are
    /// user-initiated commits, not incremental browsing.
    pub preview_sync_at: Option<Instant>,
}

impl Default for GlobalSearchState {
    fn default() -> Self {
        Self {
            active: false,
            query: String::new(),
            cursor: 0,
            selected: 0,
            scroll: 0,
            results: Vec::new(),
            truncated: false,
            cancel: Arc::new(AtomicBool::new(false)),
            last_keystroke_at: None,
            last_searched_query: String::new(),
            last_view_h: 0,
            last_popup_area: None,
            space_leader_at: None,
            focus: SearchPanelFocus::List,
            replace_open: false,
            replace_text: String::new(),
            replace_cursor: 0,
            excluded: HashSet::new(),
            replace_progress: None,
            results_h_scroll: 0,
            preview_sync_at: None,
        }
    }
}

impl GlobalSearchState {
    /// True iff focus is on either input row. The Tab::Search input
    /// gating in `input.rs` and the empty-state hint logic in the panel
    /// renderer key off this — neither cares which of the two inputs
    /// has focus.
    pub fn input_focused(&self) -> bool {
        matches!(
            self.focus,
            SearchPanelFocus::FindInput | SearchPanelFocus::ReplaceInput
        )
    }

    /// `true` iff the streamed match at `idx` is currently included in
    /// the replace batch. New matches default to included; the user
    /// flips them off via the checkbox. Out-of-range indices answer
    /// `false` defensively (avoids a panic if the results vector
    /// shrinks under us between render and apply).
    pub fn is_match_included(&self, idx: usize) -> bool {
        let Some(hit) = self.results.get(idx) else {
            return false;
        };
        !self.excluded.contains(&(hit.path.clone(), hit.line))
    }

    /// Toggle inclusion of the match at `idx`. No-op for out-of-range
    /// indices.
    pub fn toggle_match_excluded(&mut self, idx: usize) {
        let Some(hit) = self.results.get(idx).cloned() else {
            return;
        };
        let key = (hit.path.clone(), hit.line);
        if !self.excluded.remove(&key) {
            self.excluded.insert(key);
        }
    }

    /// Number of currently-included matches (= results.len() minus
    /// `excluded` entries that still match a current hit). Used by the
    /// footer's "N to replace" counter and by the apply gate.
    ///
    /// Single-pass over `results` with `O(1)` hash probes into
    /// `excluded`; stale exclusions (entries whose key has no current
    /// hit) contribute zero to the count automatically. Beats the
    /// naive `excluded.iter().filter(any-match-in-results)` which is
    /// `O(results × excluded)` per render.
    pub fn included_count(&self) -> usize {
        if self.excluded.is_empty() {
            return self.results.len();
        }
        self.results
            .iter()
            .filter(|h| !self.excluded.contains(&(h.path.clone(), h.line)))
            .count()
    }

    /// Cycle keyboard focus forward: `FindInput → ReplaceInput → List →
    /// FindInput`. Skips `ReplaceInput` when `replace_open` is false so
    /// the cycle still works in plain search mode. Bound to bare `Tab`
    /// inside Tab::Search input modes; the global Ctrl+Tab tab-cycle
    /// handler runs earlier so this never overrides it.
    pub fn cycle_focus_forward(&mut self) {
        self.focus = match (self.focus, self.replace_open) {
            (SearchPanelFocus::FindInput, true) => SearchPanelFocus::ReplaceInput,
            (SearchPanelFocus::FindInput, false) => SearchPanelFocus::List,
            (SearchPanelFocus::ReplaceInput, _) => SearchPanelFocus::List,
            (SearchPanelFocus::List, _) => SearchPanelFocus::FindInput,
        };
    }

    /// Reverse of `cycle_focus_forward`. Bound to BackTab.
    pub fn cycle_focus_backward(&mut self) {
        self.focus = match (self.focus, self.replace_open) {
            (SearchPanelFocus::FindInput, true) => SearchPanelFocus::List,
            (SearchPanelFocus::FindInput, false) => SearchPanelFocus::List,
            (SearchPanelFocus::ReplaceInput, _) => SearchPanelFocus::FindInput,
            (SearchPanelFocus::List, true) => SearchPanelFocus::ReplaceInput,
            (SearchPanelFocus::List, false) => SearchPanelFocus::FindInput,
        };
    }
}

/// Milliseconds to wait after a keyboard nav action before actually loading
/// the preview. Short enough to feel responsive, long enough that holding
/// an arrow key doesn't spam the preview worker with 30+ superseded jobs
/// per second.
pub const PREVIEW_SYNC_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(100);

// ─── Entry points ────────────────────────────────────────────────────────────

/// Open the palette. Keeps existing `query`/`results` so Esc-peek-and-return
/// doesn't lose state.
pub fn begin(app: &mut App) {
    app.global_search.active = true;
    app.global_search.cursor = app.global_search.query.len();
    // Fresh leader slot — a stale timestamp from a prior session would make
    // the first Space-after-open surprisingly close the palette.
    app.global_search.space_leader_at = None;
}

/// Commit the selected hit: close the palette, switch to the Files tab,
/// reveal the path, and stash a `PreviewHighlight` so the file preview
/// panel highlights the matching row once it loads async.
pub fn accept(app: &mut App) {
    let Some(hit) = app
        .global_search
        .results
        .get(app.global_search.selected)
        .cloned()
    else {
        app.global_search.active = false;
        return;
    };

    // Defense-in-depth: the worker's snapshot may have disappeared by now.
    // Just drop the missing entry and leave the palette open — the user can
    // re-search.
    let full = app.file_tree.root.join(&hit.path);
    if !full.exists() {
        let root = app.file_tree.root.clone();
        app.global_search
            .results
            .retain(|h| root.join(&h.path).exists());
        let len = app.global_search.results.len();
        if app.global_search.selected >= len {
            app.global_search.selected = len.saturating_sub(1);
        }
        return;
    }

    app.global_search.active = false;
    app.set_active_tab(Tab::Files);
    app.file_tree.reveal(&hit.path);
    app.refresh_file_tree_with_target(Some(hit.path.clone()));
    app.preview_highlight = Some(PreviewHighlight {
        path: hit.path.clone(),
        row: hit.line,
        byte_range: hit.byte_range.clone(),
    });
    app.load_preview_for_path(hit.path);
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
        /* allow_arm */ app.global_search.query.is_empty(),
        app.global_search.space_leader_at,
        Instant::now(),
        crate::input::LEADER_TIMEOUT,
    ) {
        crate::input::LeaderVerdict::Arm => {
            app.global_search.space_leader_at = Some(Instant::now());
            return;
        }
        crate::input::LeaderVerdict::Fire => {
            app.global_search.space_leader_at = None;
            // Only close when the follow-up matches OUR chord target (`f`/`F`).
            // Other chord targets (`p`/`P`) fall through to Consume so Space
            // inside the palette stays a literal char in those cases.
            if matches!(key.code, KeyCode::Char('f' | 'F')) && !ctrl && !alt {
                app.global_search.active = false;
            }
            return;
        }
        crate::input::LeaderVerdict::Consume => {
            app.global_search.space_leader_at = None;
            // Fall through — current key still runs below.
        }
        crate::input::LeaderVerdict::None => {}
    }

    match key.code {
        KeyCode::Esc => {
            app.global_search.active = false;
        }
        KeyCode::Char('c') if ctrl => {
            app.global_search.active = false;
            app.should_quit = true;
        }
        // Pin to Tab::Search — close the overlay, switch to the persistent
        // search view. Query/results are preserved on `app.global_search`.
        // Alt and Ctrl both bound because terminal emulators disagree on
        // which modifier they forward for Shift/Ctrl+Enter: Alt+Enter is
        // the most portable. VSCode's analogue is "Open Results in Editor"
        // (workbench.action.search.openInEditor) which is also a two-handed
        // hand-off of the same state.
        KeyCode::Enter if alt || ctrl => {
            app.global_search.active = false;
            // Land in input mode — the user was typing inside the overlay,
            // so keeping the input focused is the no-surprise hand-off.
            // From the other entry paths (digit key, Tab cycle) the tab
            // starts in list mode; see `handle_key_search`.
            app.global_search.focus = SearchPanelFocus::FindInput;
            app.set_active_tab(Tab::Search);
            // Sync preview_highlight to the current selection so the tab's
            // right panel lights up without waiting for another keystroke.
            navigate_to_selected(app);
        }
        KeyCode::Enter => accept(app),

        // ── Deletion ─────────────────────────────────────────────
        KeyCode::Backspace if alt || ctrl => {
            input_edit::delete_word_backward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            mark_query_edited(&mut app.global_search);
        }
        KeyCode::Char('w') if ctrl => {
            input_edit::delete_word_backward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            mark_query_edited(&mut app.global_search);
        }
        KeyCode::Char('u') if ctrl => {
            input_edit::clear(&mut app.global_search.query, &mut app.global_search.cursor);
            mark_query_edited(&mut app.global_search);
        }
        KeyCode::Backspace => {
            input_edit::backspace(&mut app.global_search.query, &mut app.global_search.cursor);
            mark_query_edited(&mut app.global_search);
        }

        // ── List navigation ──────────────────────────────────────
        KeyCode::Up => move_selection(&mut app.global_search, -1),
        KeyCode::Char('p') if ctrl => move_selection(&mut app.global_search, -1),
        KeyCode::Char('k') if ctrl => move_selection(&mut app.global_search, -1),
        KeyCode::Down => move_selection(&mut app.global_search, 1),
        KeyCode::Char('n') if ctrl => move_selection(&mut app.global_search, 1),
        KeyCode::Char('j') if ctrl => move_selection(&mut app.global_search, 1),
        KeyCode::PageUp => {
            let step = app.global_search.last_view_h.max(1) as i32;
            move_selection(&mut app.global_search, -step);
        }
        KeyCode::PageDown => {
            let step = app.global_search.last_view_h.max(1) as i32;
            move_selection(&mut app.global_search, step);
        }

        // ── Cursor movement inside the query ────────────────────
        // Alt/Ctrl + arrow = jump by word, matching readline (Meta+B/F),
        // macOS Option+Arrow, and Windows/Linux Ctrl+Arrow conventions.
        // Must come before the bare-arrow arms so modifier combos win.
        KeyCode::Left if alt || ctrl => {
            input_edit::move_cursor_word_backward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Right if alt || ctrl => {
            input_edit::move_cursor_word_forward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Left => {
            input_edit::move_cursor(&app.global_search.query, &mut app.global_search.cursor, -1);
        }
        KeyCode::Right => {
            input_edit::move_cursor(&app.global_search.query, &mut app.global_search.cursor, 1);
        }
        KeyCode::Home => {
            app.global_search.cursor = 0;
        }
        KeyCode::End => {
            app.global_search.cursor = app.global_search.query.len();
        }

        // ── Forward-delete ──────────────────────────────────────
        // Symmetric with Backspace: plain Delete kills one char, Alt/Ctrl
        // +Delete kills a word. Both re-run the search via mark_query_edited.
        KeyCode::Delete if alt || ctrl => {
            input_edit::delete_word_forward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            mark_query_edited(&mut app.global_search);
        }
        KeyCode::Delete => {
            input_edit::delete_char_forward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            mark_query_edited(&mut app.global_search);
        }

        // ── Readline aliases ────────────────────────────────────
        // Reliable fallback for terminals that don't pass Alt/Ctrl+Arrow
        // through cleanly (the kitty kbd protocol in main.rs helps, but
        // isn't universally supported). `Alt+b/f/d` and `Ctrl+A/E` are
        // what bash-readline users type anyway.
        KeyCode::Char('b') if alt => {
            input_edit::move_cursor_word_backward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Char('f') if alt => {
            input_edit::move_cursor_word_forward(
                &app.global_search.query,
                &mut app.global_search.cursor,
            );
        }
        KeyCode::Char('d') if alt => {
            input_edit::delete_word_forward(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
            );
            mark_query_edited(&mut app.global_search);
        }
        KeyCode::Char('a') if ctrl => {
            app.global_search.cursor = 0;
        }
        KeyCode::Char('e') if ctrl => {
            app.global_search.cursor = app.global_search.query.len();
        }

        KeyCode::Char(c) if !ctrl => {
            input_edit::insert_char(
                &mut app.global_search.query,
                &mut app.global_search.cursor,
                c,
            );
            mark_query_edited(&mut app.global_search);
        }
        _ => {}
    }
}

/// Dispatch one mouse event while the palette is active.
pub fn handle_mouse(mouse: MouseEvent, app: &mut App) {
    let popup = match app.global_search.last_popup_area {
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
                app.global_search.active = false;
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
                app.global_search.selected = idx;
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
            move_selection(&mut app.global_search, -3);
        }
        MouseEventKind::ScrollDown if inside => {
            move_selection(&mut app.global_search, 3);
        }
        _ => {}
    }
}

/// Stamp `last_keystroke_at` so the `App::tick` debounce trips and kicks off
/// a new search for the updated query. Called on every edit (insert, delete,
/// clear, delete-word). `pub(crate)` so both the overlay handler and the
/// Tab::Search input handler in `input.rs` drive the same signal.
///
/// Also clears the per-match `excluded` set: a fresh query produces a
/// fresh result list, and any `(path, line)` pair the user previously
/// opted out of has no semantic carry-over to the new search. Without
/// this, a stale exclusion on a coincidentally-reused `(path, line)` key
/// would silently skip a match the user expects to replace.
pub(crate) fn mark_query_edited(state: &mut GlobalSearchState) {
    state.last_keystroke_at = Some(Instant::now());
    state.excluded.clear();
}

fn move_selection(state: &mut GlobalSearchState, delta: i32) {
    if state.results.is_empty() {
        state.selected = 0;
        return;
    }
    let last = state.results.len() - 1;
    let cur = state.selected as i32;
    let next = (cur + delta).clamp(0, last as i32) as usize;
    state.selected = next;
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
    move_selection(&mut app.global_search, delta);
    schedule_preview_sync(app);
}

/// Mark a preview sync as pending; `App::tick` will fire
/// `navigate_to_selected` once `PREVIEW_SYNC_DEBOUNCE` has elapsed. Each
/// call pushes the deadline forward, so the last nav in a burst wins.
pub fn schedule_preview_sync(app: &mut App) {
    app.global_search.preview_sync_at = Some(Instant::now() + PREVIEW_SYNC_DEBOUNCE);
}

/// Force the next tick to re-run the current query — used by the `r`
/// reload key. Resets `last_searched_query` so the equality check in
/// `maybe_kick_global_search` fails; stamps `last_keystroke_at` so the
/// debounce gate fires. Does nothing when the query is empty (there's
/// nothing to reload).
pub fn reload(app: &mut App) {
    if app.global_search.query.is_empty() {
        return;
    }
    app.global_search.last_searched_query.clear();
    app.global_search.last_keystroke_at = Some(Instant::now());
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
    app.global_search.preview_sync_at = None;
    let Some(hit) = app
        .global_search
        .results
        .get(app.global_search.selected)
        .cloned()
    else {
        return;
    };
    app.preview_highlight = Some(PreviewHighlight {
        path: hit.path.clone(),
        row: hit.line,
        byte_range: hit.byte_range.clone(),
    });
    app.load_preview_for_path(hit.path);
}

// ─── Line-text truncation helpers ────────────────────────────────────────────

/// Truncate `text` to at most [`MAX_LINE_CHARS`] chars at a UTF-8 boundary,
/// returning the truncated copy along with the byte length we ended at.
/// Preserves the invariant that the returned string is valid UTF-8 even if
/// the input had a fragment at the limit.
pub fn truncate_line(text: &str) -> String {
    let mut chars_seen = 0usize;
    let mut byte_end = 0usize;
    for (bi, c) in text.char_indices() {
        if chars_seen >= MAX_LINE_CHARS {
            break;
        }
        byte_end = bi + c.len_utf8();
        chars_seen += 1;
    }
    if byte_end >= text.len() {
        text.to_string()
    } else {
        text[..byte_end].to_string()
    }
}

/// Clip a byte range to a maximum end offset. If the range falls entirely
/// outside the visible slice, return `None` so the UI knows this particular
/// match is off-screen (we still surface the hit — just without the
/// highlight).
pub fn clip_range(range: Range<usize>, max_end: usize) -> Option<Range<usize>> {
    if range.start >= max_end {
        return None;
    }
    let end = range.end.min(max_end);
    Some(range.start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(s.selected, 2);
        move_selection(&mut s, -99);
        assert_eq!(s.selected, 0);
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
        mark_query_edited(&mut s);
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
            selected: 5,
            ..GlobalSearchState::default()
        };
        move_selection(&mut s, 1);
        assert_eq!(s.selected, 0);
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
}
