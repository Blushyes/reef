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

use crossterm::event::KeyEvent;
use reef_app::PickerInput;
#[cfg(test)]
use reef_app::PickerInputOutcome;
#[cfg(test)]
use reef_app::PickerState;

use crate::keymap::{Command, InputScope, Keymap};

/// Outcome of dispatching a key event to [`PickerCore::dispatch_key`].
/// Callers translate these into picker-specific actions:
///
/// ```text
/// Cancel         → close()                   (Esc — close picker only)
/// Quit           → close() + quit effect     (Ctrl+C — close + app quit)
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
/// `Cancel` and `Quit` are split so callers can wire the quit effect
/// in one place — letting them collapse to a single outcome forces
/// every site to re-derive Ctrl+C by re-matching `key.code`, which
/// is the kind of duplication that drifts the first time a new
/// caller forgets to copy the branch.
#[cfg(test)]
type InputOutcome = PickerInputOutcome;

/// Standard picker key dispatch. Two-phase:
///   1. List navigation + close + commit (Esc/Ctrl+C, Enter,
///      Up/Down/Ctrl+J/K/N/P).
///   2. Filter editing via [`crate::input_edit::dispatch_key`].
#[cfg(test)]
pub fn dispatch_key(
    state: &mut PickerState,
    scope: InputScope,
    key: &KeyEvent,
    visible_count: usize,
) -> InputOutcome {
    reef_app::apply_picker_input(state, input_for_key(scope, key), visible_count)
}

pub fn input_for_key(scope: InputScope, key: &KeyEvent) -> PickerInput {
    match Keymap::resolve(scope, key) {
        Some(Command::Close) => return PickerInput::Cancel,
        Some(Command::Quit) => return PickerInput::Quit,
        Some(Command::Confirm) => return PickerInput::Confirm,
        Some(Command::MoveUp) => return PickerInput::MoveSelection(-1),
        Some(Command::MoveDown) => return PickerInput::MoveSelection(1),
        _ => {}
    }

    match crate::input_edit::op_for_key(key) {
        Some(op) => PickerInput::Edit(op),
        None => PickerInput::Unhandled,
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
        let mut c = PickerState {
            filter: "stale".into(),
            cursor: 5,
            selected_idx: 3,
            ..PickerState::default()
        };
        c.open();
        assert!(c.active);
        assert!(c.filter.is_empty());
        assert_eq!(c.cursor, 0);
        assert_eq!(c.selected_idx, 0);
    }

    #[test]
    fn close_clears_state() {
        let mut c = PickerState {
            active: true,
            filter: "x".into(),
            cursor: 1,
            selected_idx: 2,
        };
        c.close();
        assert!(!c.active);
        assert!(c.filter.is_empty());
        assert_eq!(c.cursor, 0);
        assert_eq!(c.selected_idx, 0);
    }

    #[test]
    fn move_selection_clamps() {
        let mut c = PickerState::default();
        c.move_selection(5, 10);
        assert_eq!(c.selected_idx, 4);
        c.move_selection(5, -99);
        assert_eq!(c.selected_idx, 0);
        c.move_selection(0, 1); // empty rows
        assert_eq!(c.selected_idx, 0);
    }

    #[test]
    fn dispatch_esc_returns_cancel() {
        let mut c = PickerState::default();
        let key = k(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            dispatch_key(&mut c, InputScope::QuickOpen, &key, 5),
            InputOutcome::Cancel
        );
    }

    #[test]
    fn dispatch_ctrl_c_returns_quit() {
        let mut c = PickerState::default();
        let key = k(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            dispatch_key(&mut c, InputScope::QuickOpen, &key, 5),
            InputOutcome::Quit
        );
    }

    #[test]
    fn dispatch_enter_returns_confirm() {
        let mut c = PickerState::default();
        let key = k(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            dispatch_key(&mut c, InputScope::QuickOpen, &key, 5),
            InputOutcome::Confirm
        );
    }

    #[test]
    fn dispatch_arrows_and_readline_aliases_move_selection() {
        let mut c = PickerState::default();
        // Down
        let down = k(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            dispatch_key(&mut c, InputScope::QuickOpen, &down, 5),
            InputOutcome::SelectionMoved
        );
        assert_eq!(c.selected_idx, 1);
        // Ctrl+J
        let ctrl_j = k(KeyCode::Char('j'), KeyModifiers::CONTROL);
        dispatch_key(&mut c, InputScope::QuickOpen, &ctrl_j, 5);
        assert_eq!(c.selected_idx, 2);
        // Ctrl+N
        let ctrl_n = k(KeyCode::Char('n'), KeyModifiers::CONTROL);
        dispatch_key(&mut c, InputScope::QuickOpen, &ctrl_n, 5);
        assert_eq!(c.selected_idx, 3);
        // Up
        let up = k(KeyCode::Up, KeyModifiers::NONE);
        dispatch_key(&mut c, InputScope::QuickOpen, &up, 5);
        assert_eq!(c.selected_idx, 2);
        // Ctrl+K
        let ctrl_k = k(KeyCode::Char('k'), KeyModifiers::CONTROL);
        dispatch_key(&mut c, InputScope::QuickOpen, &ctrl_k, 5);
        assert_eq!(c.selected_idx, 1);
        // Ctrl+P
        let ctrl_p = k(KeyCode::Char('p'), KeyModifiers::CONTROL);
        dispatch_key(&mut c, InputScope::QuickOpen, &ctrl_p, 5);
        assert_eq!(c.selected_idx, 0);
    }

    #[test]
    fn dispatch_char_inserts_into_filter_and_resets_selection() {
        let mut c = PickerState {
            selected_idx: 4,
            ..Default::default()
        };
        let key = k(KeyCode::Char('a'), KeyModifiers::NONE);
        let outcome = dispatch_key(&mut c, InputScope::QuickOpen, &key, 10);
        assert_eq!(outcome, InputOutcome::Edited);
        assert_eq!(c.filter, "a");
        assert_eq!(c.cursor, 1);
        assert_eq!(c.selected_idx, 0, "edit must reset list cursor");
    }

    #[test]
    fn dispatch_cursor_motion_does_not_reset_selection() {
        let mut c = PickerState {
            active: true,
            filter: "hello".into(),
            cursor: 5,
            selected_idx: 3,
        };
        let key = k(KeyCode::Left, KeyModifiers::NONE);
        let outcome = dispatch_key(&mut c, InputScope::QuickOpen, &key, 10);
        assert_eq!(outcome, InputOutcome::CursorMoved);
        assert_eq!(c.cursor, 4);
        assert_eq!(c.selected_idx, 3, "pure cursor moves preserve selection");
    }
}
