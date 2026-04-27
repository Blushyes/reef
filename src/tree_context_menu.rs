//! Right-click context menu for the Files tab file tree.
//!
//! Mirrors VSCode's Explorer context menu — the items that make sense
//! in a terminal file manager. Not included (by design):
//!
//! - Open in Integrated Terminal — reef doesn't embed a terminal.
//! - Compare / Select for Compare — separate feature.
//!
//! Clipboard items (Cut / Copy / Paste / Duplicate / Copy Path /
//! Copy Relative Path) are wired through `App::file_clipboard` and
//! `crate::clipboard` (OSC 52). `Paste` reports `is_enabled = false`
//! when the clipboard is empty — render greys it out, dispatch
//! short-circuits.
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
    /// "Cut" — mark the anchor entry (or active multi-selection) for
    /// move on the next Paste. Always enabled when an entry is the
    /// anchor; never offered for the root.
    Cut,
    /// "Copy" — mark for duplicate on Paste. Same eligibility as Cut.
    Copy,
    /// "Paste" — drop the file_clipboard contents under the anchor
    /// folder (or its parent for file anchors / project root for
    /// empty-space clicks). Disabled when the clipboard is empty.
    Paste,
    /// "Duplicate" — same-directory copy with auto-`name copy.ext`
    /// rename. Bypasses the clipboard.
    Duplicate,
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
    /// "Copy Path" — write the absolute path of the anchor entry to
    /// the system clipboard (OSC 52). Multi-select copies all paths
    /// joined by newlines.
    CopyPath,
    /// "Copy Relative Path" — same as `CopyPath` but workdir-relative.
    CopyRelativePath,
    /// "Reveal in Finder / File Explorer" — shells out to the platform
    /// file manager. macOS only for now; other OSes get a toast
    /// pointing at the follow-up.
    RevealInFinder,
}

impl ContextMenuItem {
    /// Fixed ordering for rendering — matches VSCode's menu grouping
    /// (clipboard ops first, then create, then modify, then path
    /// helpers, then reveal).
    pub const ALL_FOR_ENTRY: &'static [ContextMenuItem] = &[
        ContextMenuItem::Cut,
        ContextMenuItem::Copy,
        ContextMenuItem::Paste,
        ContextMenuItem::Duplicate,
        ContextMenuItem::NewFile,
        ContextMenuItem::NewFolder,
        ContextMenuItem::Rename,
        ContextMenuItem::Delete,
        ContextMenuItem::CopyPath,
        ContextMenuItem::CopyRelativePath,
        ContextMenuItem::RevealInFinder,
    ];

    /// Menu items offered when the user right-clicks empty space or
    /// the project root. No specific entry is anchored, so per-entry
    /// actions (Cut / Copy / Rename / Delete / Duplicate / paths) are
    /// not offered. Paste *is* offered — clipboard contents drop into
    /// the project root.
    pub const ALL_FOR_ROOT: &'static [ContextMenuItem] = &[
        ContextMenuItem::Paste,
        ContextMenuItem::NewFile,
        ContextMenuItem::NewFolder,
        ContextMenuItem::RevealInFinder,
    ];

    /// Whether this menu item is currently actionable. `Paste` is the
    /// only conditional item — it greys out when the file clipboard
    /// is empty so users see the option exists but isn't ready.
    pub fn is_enabled(&self, clipboard_is_empty: bool) -> bool {
        match self {
            ContextMenuItem::Paste => !clipboard_is_empty,
            _ => true,
        }
    }
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
        // Cut comes first under the VSCode-style ordering.
        assert_eq!(m.current(), Some(ContextMenuItem::Cut));
    }

    #[test]
    fn open_without_entry_shows_root_menu() {
        let mut m = ContextMenuState::default();
        m.open((0, 0), None);
        assert_eq!(m.items.len(), ContextMenuItem::ALL_FOR_ROOT.len());
        // Per-entry actions must NOT be offered on the root context.
        assert!(!m.items.contains(&ContextMenuItem::Rename));
        assert!(!m.items.contains(&ContextMenuItem::Delete));
        assert!(!m.items.contains(&ContextMenuItem::Cut));
        assert!(!m.items.contains(&ContextMenuItem::Copy));
        assert!(!m.items.contains(&ContextMenuItem::Duplicate));
        // Paste is offered at the root — clipboard targets project root.
        assert!(m.items.contains(&ContextMenuItem::Paste));
    }

    #[test]
    fn paste_disabled_when_clipboard_empty() {
        assert!(!ContextMenuItem::Paste.is_enabled(true));
        assert!(ContextMenuItem::Paste.is_enabled(false));
        // Other items are always enabled.
        assert!(ContextMenuItem::Cut.is_enabled(true));
        assert!(ContextMenuItem::CopyPath.is_enabled(true));
        assert!(ContextMenuItem::Delete.is_enabled(true));
    }

    #[test]
    fn entry_menu_grouping_matches_vscode() {
        // First four are clipboard ops; lock the order so a stray
        // refactor that reorders the const surfaces here.
        let entry = ContextMenuItem::ALL_FOR_ENTRY;
        assert_eq!(entry[0], ContextMenuItem::Cut);
        assert_eq!(entry[1], ContextMenuItem::Copy);
        assert_eq!(entry[2], ContextMenuItem::Paste);
        assert_eq!(entry[3], ContextMenuItem::Duplicate);
        // Path-copy actions sit just before Reveal at the bottom.
        let n = entry.len();
        assert_eq!(entry[n - 1], ContextMenuItem::RevealInFinder);
        assert_eq!(entry[n - 2], ContextMenuItem::CopyRelativePath);
        assert_eq!(entry[n - 3], ContextMenuItem::CopyPath);
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
