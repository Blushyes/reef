//! Shared state + key dispatch for "filter-driven overlay" pickers.
//!
//! Four overlays in reef share the same skeleton: filter input on top,
//! a list of candidate rows below, Esc closes / Enter commits / arrow
//! keys (plus Ctrl+J/K/N/P readline aliases) navigate the list. Each
//! one used to hand-roll the input loop on top of
//! [`crate::input_edit::dispatch_key`]; this module collapses the
//! shared scaffolding into a single struct + dispatcher so individual
//! pickers only need to supply their domain-specific bits
//! (`visible_rows()`, `commit(row)`, leader-chord / extra shortcuts).
//!
//! Pickers that consume this:
//! - `graph_branch_picker` (`b` key on the Graph tab)
//! - `hosts_picker` (Ctrl+O — note: also owns a secondary `path_buffer`
//!   that's edited via `input_edit::dispatch_key` directly, not through
//!   PickerCore, because PickerCore tracks one buffer at a time)
//! - `quick_open` (Space+P palette)
//! - `global_search` overlay (Space+F)
//!
//! `find_widget` is intentionally NOT a consumer: it has a filter and a
//! "current match" cursor that walks the underlying text, not a
//! drop-down list, so it doesn't share PickerCore's selection model.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

/// Shared state for a filter-driven picker overlay. Each consuming
/// picker holds this as a field (e.g. `pub core: PickerCore`) and
/// keeps its domain-specific data (`all_hosts`, `recent`,
/// `space_leader_at`, …) alongside.
#[derive(Debug, Default)]
pub struct PickerCore {
    pub active: bool,
    /// Text in the filter input. Edited via `dispatch_key` / paste.
    pub filter: String,
    /// Byte offset into `filter` (UTF-8 char boundary). Maintained
    /// alongside `filter` by [`crate::input_edit`].
    pub cursor: usize,
    /// Index into `visible_rows()` of the row currently highlighted.
    /// Always reset to 0 on filter edit so the next match lands at the
    /// top — matches the UX convention every reef picker uses.
    pub selected_idx: usize,
    /// Cached popup rect for mouse hit-testing. Set by the panel
    /// renderer each frame, read by the click/scroll handler.
    pub last_popup_area: Option<Rect>,
}

/// Outcome of dispatching a key event to [`PickerCore::dispatch_key`].
/// Callers translate these into picker-specific actions:
///
/// ```text
/// Cancel         → close()                   (Esc — close picker only)
/// Quit           → close() + should_quit     (Ctrl+C — close + app quit)
/// Confirm        → look up the row at `selected_idx`, act on it
/// Edited         → recompute the candidate list against `filter`
/// Rejected       → a printable char was recognised but a future
///                  filtered dispatcher refused to insert it. Today
///                  PickerCore uses the non-filtered backend so this
///                  is unreachable, but the variant is reserved so
///                  swapping in `dispatch_key_filtered` later forces
///                  every caller to update its match (compile-time
///                  safety net).
/// SelectionMoved → no-op (renderer already sees the new cursor)
/// CursorMoved    → no-op (filter buffer cursor moved inside text)
/// Unhandled      → the picker fully consumed the key as a no-op; do
///                  NOT forward it elsewhere. Overlays own keyboard
///                  while active so unknown keys are intentionally
///                  swallowed rather than bubbled up to global hotkeys.
/// ```
///
/// `Cancel` and `Quit` are split so callers can wire `should_quit`
/// in one place — letting them collapse to a single outcome forces
/// every site to re-derive Ctrl+C by re-matching `key.code`, which
/// is the kind of duplication that drifts the first time a new
/// caller forgets to copy the branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOutcome {
    Cancel,
    Quit,
    Confirm,
    Edited,
    Rejected,
    SelectionMoved,
    CursorMoved,
    Unhandled,
}

impl PickerCore {
    /// Open the overlay, resetting filter / cursor / selection.
    /// Domain-specific data (cached candidates, recents, etc.) should
    /// be set by the caller *before* calling this — `open()` is the
    /// last step that flips `active` to true.
    pub fn open(&mut self) {
        self.filter.clear();
        self.cursor = 0;
        self.selected_idx = 0;
        self.active = true;
    }

    /// Close the overlay and wipe transient state (filter, cursor,
    /// selection, mouse rect). Caller is responsible for any picker-
    /// specific cleanup (e.g. clearing `path_buffer` in hosts_picker).
    pub fn close(&mut self) {
        self.active = false;
        self.filter.clear();
        self.cursor = 0;
        self.selected_idx = 0;
        self.last_popup_area = None;
    }

    /// Clamp `selected_idx` against the current visible row count and
    /// move it by `delta` (negative = up). Used by the standard
    /// key/scroll handlers and by callers that need to drive selection
    /// from outside the key path (mouse wheel, PageUp/PageDown with a
    /// view_h step).
    pub fn move_selection(&mut self, visible_count: usize, delta: i32) {
        if visible_count == 0 {
            self.selected_idx = 0;
            return;
        }
        let last = visible_count as i32 - 1;
        let next = (self.selected_idx as i32 + delta).clamp(0, last);
        self.selected_idx = next as usize;
    }

    /// Standard picker key dispatch. Two-phase:
    ///   1. List navigation + close + commit (Esc/Ctrl+C, Enter,
    ///      Up/Down/Ctrl+J/K/N/P).
    ///   2. Filter editing via [`crate::input_edit::dispatch_key`].
    ///
    /// `visible_count` is the row count caller's `visible_rows()` would
    /// produce *right now*; needed for selection clamping. Pass 0 when
    /// the picker has no rows yet (the cursor will just snap to 0).
    ///
    /// Callers that own extra shortcuts (leader chords, Alt-modifier
    /// toggles, Ctrl+P mode-switch in hosts_picker) should match those
    /// BEFORE calling `dispatch_key` and `return` early; otherwise the
    /// editor table would consume them as plain inserts.
    pub fn dispatch_key(&mut self, key: &KeyEvent, visible_count: usize) -> InputOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Phase 1: list nav + close + commit.
        match key.code {
            KeyCode::Esc => return InputOutcome::Cancel,
            KeyCode::Char('c') if ctrl => return InputOutcome::Quit,
            KeyCode::Enter => return InputOutcome::Confirm,
            KeyCode::Up => {
                self.move_selection(visible_count, -1);
                return InputOutcome::SelectionMoved;
            }
            KeyCode::Down => {
                self.move_selection(visible_count, 1);
                return InputOutcome::SelectionMoved;
            }
            KeyCode::Char('k' | 'p') if ctrl => {
                self.move_selection(visible_count, -1);
                return InputOutcome::SelectionMoved;
            }
            KeyCode::Char('j' | 'n') if ctrl => {
                self.move_selection(visible_count, 1);
                return InputOutcome::SelectionMoved;
            }
            _ => {}
        }

        // Phase 2: shared editor table. Reset selection on any genuine
        // edit so the filtered list always opens at row 0.
        let outcome = crate::input_edit::dispatch_key(key, &mut self.filter, &mut self.cursor);
        match outcome {
            crate::input_edit::Outcome::Edited => {
                self.selected_idx = 0;
                InputOutcome::Edited
            }
            crate::input_edit::Outcome::CursorOnly => InputOutcome::CursorMoved,
            // PickerCore uses the non-filtered dispatcher so Rejected
            // can't fire today. Surface it as a distinct outcome
            // anyway so a future swap to `dispatch_key_filtered`
            // forces every caller (each currently has a `Rejected =>`
            // arm) to make an explicit choice instead of folding
            // rejection into CursorMoved.
            crate::input_edit::Outcome::Rejected => InputOutcome::Rejected,
            crate::input_edit::Outcome::Unhandled => InputOutcome::Unhandled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn open_resets_filter_and_selection() {
        let mut c = PickerCore {
            filter: "stale".into(),
            cursor: 5,
            selected_idx: 3,
            ..Default::default()
        };
        c.open();
        assert!(c.active);
        assert!(c.filter.is_empty());
        assert_eq!(c.cursor, 0);
        assert_eq!(c.selected_idx, 0);
    }

    #[test]
    fn close_clears_state() {
        let mut c = PickerCore {
            active: true,
            filter: "x".into(),
            cursor: 1,
            selected_idx: 2,
            last_popup_area: Some(Rect::new(0, 0, 1, 1)),
        };
        c.close();
        assert!(!c.active);
        assert!(c.filter.is_empty());
        assert_eq!(c.cursor, 0);
        assert_eq!(c.selected_idx, 0);
        assert!(c.last_popup_area.is_none());
    }

    #[test]
    fn move_selection_clamps() {
        let mut c = PickerCore::default();
        c.move_selection(5, 10);
        assert_eq!(c.selected_idx, 4);
        c.move_selection(5, -99);
        assert_eq!(c.selected_idx, 0);
        c.move_selection(0, 1); // empty rows
        assert_eq!(c.selected_idx, 0);
    }

    #[test]
    fn dispatch_esc_returns_cancel() {
        let mut c = PickerCore::default();
        assert_eq!(
            c.dispatch_key(&k(KeyCode::Esc, KeyModifiers::NONE), 5),
            InputOutcome::Cancel
        );
    }

    #[test]
    fn dispatch_ctrl_c_returns_quit() {
        let mut c = PickerCore::default();
        assert_eq!(
            c.dispatch_key(&k(KeyCode::Char('c'), KeyModifiers::CONTROL), 5),
            InputOutcome::Quit
        );
    }

    #[test]
    fn dispatch_enter_returns_confirm() {
        let mut c = PickerCore::default();
        assert_eq!(
            c.dispatch_key(&k(KeyCode::Enter, KeyModifiers::NONE), 5),
            InputOutcome::Confirm
        );
    }

    #[test]
    fn dispatch_arrows_and_readline_aliases_move_selection() {
        let mut c = PickerCore::default();
        // Down
        assert_eq!(
            c.dispatch_key(&k(KeyCode::Down, KeyModifiers::NONE), 5),
            InputOutcome::SelectionMoved
        );
        assert_eq!(c.selected_idx, 1);
        // Ctrl+J
        c.dispatch_key(&k(KeyCode::Char('j'), KeyModifiers::CONTROL), 5);
        assert_eq!(c.selected_idx, 2);
        // Ctrl+N
        c.dispatch_key(&k(KeyCode::Char('n'), KeyModifiers::CONTROL), 5);
        assert_eq!(c.selected_idx, 3);
        // Up
        c.dispatch_key(&k(KeyCode::Up, KeyModifiers::NONE), 5);
        assert_eq!(c.selected_idx, 2);
        // Ctrl+K
        c.dispatch_key(&k(KeyCode::Char('k'), KeyModifiers::CONTROL), 5);
        assert_eq!(c.selected_idx, 1);
        // Ctrl+P
        c.dispatch_key(&k(KeyCode::Char('p'), KeyModifiers::CONTROL), 5);
        assert_eq!(c.selected_idx, 0);
    }

    #[test]
    fn dispatch_char_inserts_into_filter_and_resets_selection() {
        let mut c = PickerCore {
            selected_idx: 4,
            ..Default::default()
        };
        let outcome = c.dispatch_key(&k(KeyCode::Char('a'), KeyModifiers::NONE), 10);
        assert_eq!(outcome, InputOutcome::Edited);
        assert_eq!(c.filter, "a");
        assert_eq!(c.cursor, 1);
        assert_eq!(c.selected_idx, 0, "edit must reset list cursor");
    }

    #[test]
    fn dispatch_cursor_motion_does_not_reset_selection() {
        let mut c = PickerCore {
            filter: "hello".into(),
            cursor: 5,
            selected_idx: 3,
            active: true,
            ..Default::default()
        };
        let outcome = c.dispatch_key(&k(KeyCode::Left, KeyModifiers::NONE), 10);
        assert_eq!(outcome, InputOutcome::CursorMoved);
        assert_eq!(c.cursor, 4);
        assert_eq!(c.selected_idx, 3, "pure cursor moves preserve selection");
    }
}
