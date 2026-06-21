//! Intra-tree mouse-drag state machine.
//!
//! Distinct from `place_mode`, which handles **OS → terminal** drag-in
//! (operating system delivers paths as a bracketed-paste payload, no
//! drag-over signal). This module handles **tree-row → tree-row**
//! drag, where crossterm reports the full `Down → Drag → Up` sequence
//! so we can render hover affordances live.
//!
//! State transitions:
//! 1. `Down(Left)` on a tree row → `arm(...)`. Press is recorded but
//!    drag is *not* yet active — a click that doesn't move shouldn't
//!    behave as drag-and-drop.
//! 2. `Drag(Left)` past `DRAG_START_THRESHOLD` from the press point →
//!    caller checks `should_start_drag(col, row)` and, if true, calls
//!    `start(sources, mods)`. Sources are snapshotted *here* (at the
//!    start of the drag) so a mid-drag selection mutation can't change
//!    the payload.
//! 3. While `active`, `update_hover(idx, now)` tracks the auto-expand
//!    timer (same 600 ms VS Code-ish delay as `place_mode`).
//! 4. `Up(Left)` → caller dispatches move / copy based on the live
//!    modifiers (`is_copy_op`); `cancel()` afterwards.
//! 5. `Esc` while active → `cancel()`.
//!
//! Hover-target *resolution* is delegated to `place_mode::
//! resolve_hover_target` — the same VS Code rule (folders drop into
//! themselves, files into their parent) applies to both flows, so we
//! reuse the pure helper rather than re-implementing it.

use crate::place_mode::HOVER_EXPAND_DELAY;
use crossterm::event::KeyModifiers;
use std::path::PathBuf;
use std::time::Instant;

/// Mouse cells the cursor must travel from the press point before a
/// click promotes to a drag. Two cells is the smallest value that
/// reliably distinguishes a deliberate drag from a touchpad-jitter
/// click on terminals where mouse coordinates round to integer cells.
pub const DRAG_START_THRESHOLD: u16 = 2;

/// Cheap snapshot taken at `Down(Left)`. Held in `TreeDragState.press`
/// while we wait to see whether the user is clicking or dragging.
#[derive(Debug, Clone, Copy)]
pub struct DragPress {
    pub start_col: u16,
    pub start_row: u16,
    /// `file_tree.entries` index the user pressed on. Used by the
    /// caller when deciding source paths if no multi-selection is
    /// active.
    pub press_idx: usize,
    pub mods_at_press: KeyModifiers,
}

#[derive(Debug)]
pub struct TreeDragState {
    /// Armed by `Down(Left)`, cleared on `Up`/`cancel` or promoted by
    /// `start`. `None` while idle.
    pub press: Option<DragPress>,
    /// `true` between `start()` and `cancel()`. While active the
    /// renderer overlays a hover-target highlight and `handle_mouse`
    /// short-circuits other interpretations of `Drag`/`Up`.
    pub active: bool,
    /// Workdir-relative paths the drag is carrying. Snapshotted by
    /// `start` from `App::effective_action_paths()`.
    pub sources: Vec<PathBuf>,
    /// Current hover row in flattened-tree index space. `None` when
    /// the cursor isn't over any row (e.g. above/below the panel).
    pub hover_idx: Option<usize>,
    /// When `hover_idx` was first set to its current value. Cleared
    /// after auto-expand fires so a continuous hover only expands
    /// once.
    pub hover_since: Option<Instant>,
    /// Modifiers as of the most recent mouse event during the drag.
    /// `Up(Left)` consults this for move-vs-copy.
    pub modifiers: KeyModifiers,
}

impl Default for TreeDragState {
    fn default() -> Self {
        // `KeyModifiers` has no `Default` (crossterm 0.29) — fall back
        // to `NONE` so the idle state has no implied modifier.
        Self {
            press: None,
            active: false,
            sources: Vec::new(),
            hover_idx: None,
            hover_since: None,
            modifiers: KeyModifiers::NONE,
        }
    }
}

impl TreeDragState {
    /// Record a press without activating drag yet.
    pub fn arm(&mut self, col: u16, row: u16, press_idx: usize, mods: KeyModifiers) {
        self.press = Some(DragPress {
            start_col: col,
            start_row: row,
            press_idx,
            mods_at_press: mods,
        });
    }

    /// Caller checks each `Drag(Left)` event: did the cursor move far
    /// enough from the press to count as a drag?
    pub fn should_start_drag(&self, col: u16, row: u16) -> bool {
        let Some(p) = self.press else { return false };
        let dc = col.abs_diff(p.start_col);
        let dr = row.abs_diff(p.start_row);
        dc >= DRAG_START_THRESHOLD || dr >= DRAG_START_THRESHOLD
    }

    /// Promote the press to an active drag. `sources` is the workdir-
    /// relative path list the drag is carrying — snapshotted now to
    /// freeze it against later selection mutations.
    pub fn start(&mut self, sources: Vec<PathBuf>, mods: KeyModifiers) {
        self.active = true;
        self.sources = sources;
        self.modifiers = mods;
    }

    /// Update the hover target tracker. Mirrors
    /// `PlaceModeState::update_hover` — moving onto a different row
    /// (or off any row) resets the auto-expand timer; staying put
    /// preserves it.
    pub fn update_hover(&mut self, folder_idx: Option<usize>) {
        if self.hover_idx != folder_idx {
            self.hover_idx = folder_idx;
            self.hover_since = folder_idx.map(|_| Instant::now());
        }
    }

    pub fn update_modifiers(&mut self, mods: KeyModifiers) {
        self.modifiers = mods;
    }

    /// Whether enough time has elapsed on the current hover for an
    /// auto-expand. Caller is responsible for clearing `hover_since`
    /// after firing so the timer doesn't repeat.
    pub fn auto_expand_due(&self, now: Instant) -> Option<usize> {
        match (self.hover_idx, self.hover_since) {
            (Some(idx), Some(t)) if now.duration_since(t) >= HOVER_EXPAND_DELAY => Some(idx),
            _ => None,
        }
    }

    pub fn clear_hover_timer(&mut self) {
        self.hover_since = None;
    }

    /// Modifier reading for the move-vs-copy decision at drop. VS
    /// Code's convention: bare drag = move, Alt(Option) drag = copy.
    pub fn is_copy_op(&self) -> bool {
        self.modifiers.contains(KeyModifiers::ALT)
    }

    /// Reset to idle. Called on `Up(Left)` after dispatch, on `Esc`,
    /// and when the press never promotes to a drag.
    pub fn cancel(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_idle() {
        let s = TreeDragState::default();
        assert!(!s.active);
        assert!(s.press.is_none());
        assert!(s.sources.is_empty());
    }

    #[test]
    fn arm_records_press_without_activating() {
        let mut s = TreeDragState::default();
        s.arm(10, 5, 3, KeyModifiers::NONE);
        assert!(!s.active);
        assert!(s.press.is_some());
        let p = s.press.unwrap();
        assert_eq!(p.start_col, 10);
        assert_eq!(p.start_row, 5);
        assert_eq!(p.press_idx, 3);
    }

    #[test]
    fn should_start_drag_requires_threshold() {
        let mut s = TreeDragState::default();
        s.arm(10, 5, 0, KeyModifiers::NONE);
        // Same point: no drag.
        assert!(!s.should_start_drag(10, 5));
        // One cell off: still under threshold (THRESHOLD = 2).
        assert!(!s.should_start_drag(11, 5));
        assert!(!s.should_start_drag(10, 6));
        // Two cells off in either axis: drag starts.
        assert!(s.should_start_drag(12, 5));
        assert!(s.should_start_drag(10, 7));
        // Negative axis works via abs_diff.
        assert!(s.should_start_drag(8, 5));
    }

    #[test]
    fn should_start_drag_without_press_returns_false() {
        let s = TreeDragState::default();
        assert!(!s.should_start_drag(100, 100));
    }

    #[test]
    fn start_activates_and_snapshots_sources() {
        let mut s = TreeDragState::default();
        s.arm(0, 0, 0, KeyModifiers::NONE);
        let srcs = vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")];
        s.start(srcs.clone(), KeyModifiers::ALT);
        assert!(s.active);
        assert_eq!(s.sources, srcs);
        assert!(s.is_copy_op());
    }

    #[test]
    fn update_hover_resets_timer_only_on_change() {
        let mut s = TreeDragState::default();
        s.update_hover(Some(2));
        let first = s.hover_since;
        assert!(first.is_some());
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.update_hover(Some(2));
        assert_eq!(s.hover_since, first);
        s.update_hover(Some(5));
        assert!(s.hover_since > first);
        s.update_hover(None);
        assert!(s.hover_since.is_none());
    }

    #[test]
    fn auto_expand_fires_after_delay() {
        let mut s = TreeDragState::default();
        s.update_hover(Some(7));
        let t0 = s.hover_since.unwrap();
        assert_eq!(s.auto_expand_due(t0), None);
        assert_eq!(s.auto_expand_due(t0 + HOVER_EXPAND_DELAY), Some(7));
    }

    #[test]
    fn modifier_updates_drive_copy_decision() {
        let mut s = TreeDragState::default();
        s.start(vec![], KeyModifiers::NONE);
        assert!(!s.is_copy_op());
        s.update_modifiers(KeyModifiers::ALT);
        assert!(s.is_copy_op());
        s.update_modifiers(KeyModifiers::SHIFT);
        assert!(!s.is_copy_op());
    }

    #[test]
    fn cancel_resets_everything() {
        let mut s = TreeDragState::default();
        s.arm(0, 0, 0, KeyModifiers::NONE);
        s.start(vec![PathBuf::from("a")], KeyModifiers::ALT);
        s.update_hover(Some(3));
        s.cancel();
        assert!(!s.active);
        assert!(s.press.is_none());
        assert!(s.sources.is_empty());
        assert!(s.hover_idx.is_none());
        assert!(s.hover_since.is_none());
    }
}
