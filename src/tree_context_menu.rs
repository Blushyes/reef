//! Right-click context menu for the Files tab file tree.
//!
//! Mirrors VSCode's Explorer context menu — the items that make sense
//! in a terminal file manager. Not included (by design):
//!
//! - Cut / Copy / Paste — needs clipboard state, orthogonal to this
//!   feature. The drag-and-drop flow (place mode) already covers the
//!   common copy case.
//! - Copy Path / Copy Relative Path — needs a clipboard crate
//!   (`arboard` or similar). Follow-up.
//! - Open in Integrated Terminal — reef doesn't embed a terminal.
//! - Compare / Select for Compare — separate feature.
//!
//! The menu is a cheap UI state + a renderer that overlays on top of
//! the tree. Clicks inside dispatch a `ClickAction::TreeContextMenuItem`;
//! clicks outside dispatch `ClickAction::TreeContextMenuClose` (registered
//! panel-wide beneath the menu, same way place-mode's root drop zone
//! falls through to cancel).

/// A single action offered by the right-click menu. Most items carry
/// enough context on themselves that the dispatch side doesn't need
/// to look at the menu's anchor — but the anchor still matters for
/// placement so we keep it on `ContextMenuState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuItem {
    /// "New File" — creates under the anchor folder (or project root
    /// if the anchor is a file / is absent). Opens an inline edit row.
    NewFile,
    /// "New Folder" — same placement rules as NewFile.
    NewFolder,
    /// "Rename" — only offered when the anchor is a concrete entry
    /// (file or folder). Starts an inline rename on that entry.
    Rename,
    /// "Delete" — moves the anchor entry to the system Trash after
    /// confirmation (status bar prompt).
    Delete,
    /// "Reveal in Finder / File Explorer" — shells out to the platform
    /// file manager. macOS only for now; other OSes get a toast
    /// pointing at the follow-up.
    RevealInFinder,
}

impl ContextMenuItem {
    /// Fixed ordering for rendering — matches VSCode's menu grouping
    /// (create actions, then modify, then reveal).
    pub const ALL_FOR_ENTRY: &'static [ContextMenuItem] = &[
        ContextMenuItem::NewFile,
        ContextMenuItem::NewFolder,
        ContextMenuItem::Rename,
        ContextMenuItem::Delete,
        ContextMenuItem::RevealInFinder,
    ];

    /// Menu items offered when the user right-clicks empty space or
    /// the project root. No specific entry is anchored, so Rename /
    /// Delete don't apply.
    pub const ALL_FOR_ROOT: &'static [ContextMenuItem] = &[
        ContextMenuItem::NewFile,
        ContextMenuItem::NewFolder,
        ContextMenuItem::RevealInFinder,
    ];
}

#[derive(Debug, Default)]
pub struct ContextMenuState {
    /// `true` while the menu is visible and owns input. Input
    /// dispatch short-circuits: `↑`/`↓` move selection, `Enter`
    /// fires the highlighted item, `Esc` closes, any other key
    /// closes (best-guess of user intent — VSCode also dismisses
    /// the menu on unrelated input).
    pub active: bool,

    /// Anchor in terminal cells — top-left of the menu popup. Set by
    /// `App::open_tree_context_menu` from the mouse event; the
    /// renderer clamps to the screen bounds so the menu never draws
    /// outside the viewport even if the click landed near the edge.
    pub anchor: (u16, u16),

    /// Which file-tree entry (if any) triggered this menu. Used by
    /// the action dispatch: `ContextMenuItem::Rename` on a null entry
    /// is nonsensical, so the dispatch arm bails early.
    /// Index into `app.file_tree.entries` at the moment the menu
    /// was opened.
    pub target_entry_idx: Option<usize>,

    /// Items offered — `ALL_FOR_ENTRY` when an entry was clicked,
    /// `ALL_FOR_ROOT` when the click missed all rows.
    pub items: Vec<ContextMenuItem>,

    /// Index into `items` of the keyboard-highlighted row. Mouse
    /// hover updates this on every frame via the hit registry +
    /// render-time recompute (the menu is redrawn each frame, so
    /// we can afford to reset from hover each render).
    pub selected: usize,
}

impl ContextMenuState {
    /// Pop the menu up with the anchor- or root-flavoured item list.
    /// `anchor` is in screen cells; `target_entry_idx` is None when
    /// the click didn't land on any tree row.
    pub fn open(&mut self, anchor: (u16, u16), target_entry_idx: Option<usize>) {
        self.active = true;
        self.anchor = anchor;
        self.target_entry_idx = target_entry_idx;
        self.items = if target_entry_idx.is_some() {
            ContextMenuItem::ALL_FOR_ENTRY.to_vec()
        } else {
            ContextMenuItem::ALL_FOR_ROOT.to_vec()
        };
        self.selected = 0;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// Wrap-around navigation on `↑`/`↓` — matches quick-open and
    /// global-search palette behaviour.
    pub fn navigate(&mut self, delta: i32) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as i32;
        let mut idx = self.selected as i32 + delta;
        // Rust `%` follows sign of the lhs; clamp manually.
        idx = idx.rem_euclid(n);
        self.selected = idx as usize;
    }

    pub fn current(&self) -> Option<ContextMenuItem> {
        self.items.get(self.selected).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_closed() {
        let m = ContextMenuState::default();
        assert!(!m.active);
        assert!(m.items.is_empty());
    }

    #[test]
    fn open_with_entry_shows_full_menu() {
        let mut m = ContextMenuState::default();
        m.open((10, 5), Some(3));
        assert!(m.active);
        assert_eq!(m.anchor, (10, 5));
        assert_eq!(m.target_entry_idx, Some(3));
        assert_eq!(m.items.len(), ContextMenuItem::ALL_FOR_ENTRY.len());
        assert_eq!(m.current(), Some(ContextMenuItem::NewFile));
    }

    #[test]
    fn open_without_entry_shows_root_menu() {
        let mut m = ContextMenuState::default();
        m.open((0, 0), None);
        assert_eq!(m.items.len(), ContextMenuItem::ALL_FOR_ROOT.len());
        // Rename/Delete must NOT be offered on the root context.
        assert!(!m.items.contains(&ContextMenuItem::Rename));
        assert!(!m.items.contains(&ContextMenuItem::Delete));
    }

    #[test]
    fn navigate_wraps_around() {
        let mut m = ContextMenuState::default();
        m.open((0, 0), Some(0));
        let last = m.items.len() - 1;
        m.navigate(-1);
        assert_eq!(m.selected, last);
        m.navigate(1);
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn close_resets_state() {
        let mut m = ContextMenuState::default();
        m.open((10, 5), Some(2));
        m.close();
        assert!(!m.active);
        assert!(m.items.is_empty());
        assert_eq!(m.selected, 0);
        assert!(m.target_entry_idx.is_none());
    }
}
