//! Right-click context menu for the Files tab file tree.
//!
//! Mirrors VSCode's Explorer context menu — the items that make sense
//! in a compact file manager. Not included (by design):
//!
//! - Open in Integrated Terminal — Reef does not embed a shell panel.
//! - Compare / Select for Compare — separate feature.
//!
//! Clipboard items (Cut / Copy / Paste / Duplicate / Copy Path /
//! Copy Relative Path) are wired through `App::file_clipboard` and
//! host clipboard effects. `Paste` reports `is_enabled = false` when
//! the clipboard is empty — render greys it out, dispatch short-circuits.
//!
//! The menu is cheap overlay state. Input adapters decide whether a click
//! picks an item or closes the menu.

use reef_core::file_tree::{FileTreeSortDirection, FileTreeSortField};
use reef_core::menu::{
    MenuEntry,
    MenuEntry::{Item, Separator},
    menu_items, navigate_menu,
};

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
    /// the system clipboard. Multi-select copies all paths joined by
    /// newlines.
    CopyPath,
    /// "Copy Relative Path" — same as `CopyPath` but workdir-relative.
    CopyRelativePath,
    /// Opens the sibling-ordering submenu.
    SortBy,
    SortField(FileTreeSortField),
    SortDirection(FileTreeSortDirection),
    FoldersFirst,
    /// "Reveal in Finder / File Explorer" — shells out to the platform
    /// file manager. Unsupported hosts report a toast instead.
    RevealInFinder,
}

impl ContextMenuItem {
    /// Fixed ordering for rendering — matches VSCode's menu grouping
    /// (clipboard ops first, then create, then modify, then path
    /// helpers, then reveal).
    pub const ALL_FOR_ENTRY: &'static [MenuEntry<ContextMenuItem>] = &[
        Item(Self::Cut),
        Item(Self::Copy),
        Item(Self::Paste),
        Item(Self::Duplicate),
        Separator,
        Item(Self::NewFile),
        Item(Self::NewFolder),
        Separator,
        Item(Self::Rename),
        Item(Self::Delete),
        Separator,
        Item(Self::CopyPath),
        Item(Self::CopyRelativePath),
        Separator,
        Item(Self::SortBy),
        Separator,
        Item(Self::RevealInFinder),
    ];

    /// Empty-space menus offer workspace creation, paste, sorting, and reveal.
    pub const ALL_FOR_ROOT: &'static [MenuEntry<ContextMenuItem>] = &[
        Item(Self::Paste),
        Item(Self::NewFile),
        Item(Self::NewFolder),
        Separator,
        Item(Self::SortBy),
        Separator,
        Item(Self::RevealInFinder),
    ];

    pub const SORT_ITEMS: &'static [MenuEntry<ContextMenuItem>] = &[
        Item(Self::SortField(FileTreeSortField::Name)),
        Item(Self::SortField(FileTreeSortField::Kind)),
        Item(Self::SortField(FileTreeSortField::Modified)),
        Separator,
        Item(Self::SortDirection(FileTreeSortDirection::Ascending)),
        Item(Self::SortDirection(FileTreeSortDirection::Descending)),
        Separator,
        Item(Self::FoldersFirst),
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
    /// `true` while the menu owns input. Up/Down move selection,
    /// Enter activates an item, Right enters sorting, and Left/Escape
    /// returns to the parent or dismisses the root menu.
    pub active: bool,

    /// Anchor in renderer cells — top-left of the menu popup. Set by
    /// `App::open_tree_context_menu` from the pointer event; the renderer
    /// clamps to the screen bounds so the menu never draws outside the
    /// viewport even if the click landed near the edge.
    pub anchor: (u16, u16),

    /// Which file-tree entry (if any) triggered this menu. Used by
    /// the action dispatch: `ContextMenuItem::Rename` on a null entry
    /// is nonsensical, so the dispatch arm bails early.
    /// Index into `app.file_tree.entries` at the moment the menu
    /// was opened.
    pub target_entry_idx: Option<usize>,

    /// Items offered — `ALL_FOR_ENTRY` when an entry was clicked,
    /// `ALL_FOR_ROOT` when the click missed all rows.
    pub entries: Vec<MenuEntry<ContextMenuItem>>,

    /// Item index of the highlighted parent row, excluding separators.
    pub selected: usize,

    /// Highlighted sort row while the submenu owns keyboard input.
    pub sort_selected: Option<usize>,
}

impl ContextMenuState {
    /// Pop the menu up with the anchor- or root-flavoured item list.
    /// `anchor` is in screen cells; `target_entry_idx` is None when
    /// the click didn't land on any tree row.
    pub fn open(&mut self, anchor: (u16, u16), target_entry_idx: Option<usize>) {
        self.active = true;
        self.anchor = anchor;
        self.target_entry_idx = target_entry_idx;
        self.entries = if target_entry_idx.is_some() {
            ContextMenuItem::ALL_FOR_ENTRY.to_vec()
        } else {
            ContextMenuItem::ALL_FOR_ROOT.to_vec()
        };
        self.selected = 0;
        self.sort_selected = None;
    }

    pub fn close(&mut self) {
        *self = Self::default();
    }

    /// Wrap-around navigation on `↑`/`↓` — matches quick-open and
    /// global-search palette behaviour.
    pub fn navigate(&mut self, delta: i32) {
        let (selected, entries) = match self.sort_selected.as_mut() {
            Some(selected) => (selected, ContextMenuItem::SORT_ITEMS),
            None => (&mut self.selected, self.entries.as_slice()),
        };
        *selected = navigate_menu(entries, *selected, delta);
    }

    pub fn current(&self) -> Option<ContextMenuItem> {
        match self.sort_selected {
            Some(selected) => menu_items(ContextMenuItem::SORT_ITEMS)
                .nth(selected)
                .cloned(),
            None => menu_items(&self.entries).nth(self.selected).cloned(),
        }
    }

    pub fn open_sort(&mut self) {
        if self.active
            && let Some(index) =
                menu_items(&self.entries).position(|item| *item == ContextMenuItem::SortBy)
        {
            self.selected = index;
            self.sort_selected = Some(0);
        }
    }

    pub fn back(&mut self) {
        if self.sort_selected.take().is_none() {
            self.close();
        }
    }

    pub fn hover(&mut self, item: &ContextMenuItem) {
        if !self.active {
            return;
        }
        if let Some(index) = menu_items(&self.entries).position(|candidate| candidate == item) {
            self.selected = index;
            if *item == ContextMenuItem::SortBy {
                self.sort_selected.get_or_insert(0);
            } else {
                self.sort_selected = None;
            }
        } else if self.sort_selected.is_some()
            && let Some(index) =
                menu_items(ContextMenuItem::SORT_ITEMS).position(|candidate| candidate == item)
        {
            self.sort_selected = Some(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_closed() {
        let m = ContextMenuState::default();
        assert!(!m.active);
        assert!(m.entries.is_empty());
    }

    #[test]
    fn open_with_entry_shows_full_menu() {
        let mut m = ContextMenuState::default();
        m.open((10, 5), Some(3));
        assert!(m.active);
        assert_eq!(m.anchor, (10, 5));
        assert_eq!(m.target_entry_idx, Some(3));
        assert_eq!(m.entries.len(), ContextMenuItem::ALL_FOR_ENTRY.len());
        // Cut comes first under the VSCode-style ordering.
        assert_eq!(m.current(), Some(ContextMenuItem::Cut));
    }

    #[test]
    fn open_without_entry_shows_root_menu() {
        let mut m = ContextMenuState::default();
        m.open((0, 0), None);
        assert_eq!(m.entries.len(), ContextMenuItem::ALL_FOR_ROOT.len());
        // Per-entry actions must NOT be offered on the root context.
        assert!(!m.entries.contains(&Item(ContextMenuItem::Rename)));
        assert!(!m.entries.contains(&Item(ContextMenuItem::Delete)));
        assert!(!m.entries.contains(&Item(ContextMenuItem::Cut)));
        assert!(!m.entries.contains(&Item(ContextMenuItem::Copy)));
        assert!(!m.entries.contains(&Item(ContextMenuItem::Duplicate)));
        // Paste is offered at the root — clipboard targets project root.
        assert!(m.entries.contains(&Item(ContextMenuItem::Paste)));
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
        let entry: Vec<_> = menu_items(ContextMenuItem::ALL_FOR_ENTRY)
            .cloned()
            .collect();
        assert_eq!(entry[0], ContextMenuItem::Cut);
        assert_eq!(entry[1], ContextMenuItem::Copy);
        assert_eq!(entry[2], ContextMenuItem::Paste);
        assert_eq!(entry[3], ContextMenuItem::Duplicate);
        // Path-copy actions are followed by sorting and reveal.
        let n = entry.len();
        assert_eq!(entry[n - 1], ContextMenuItem::RevealInFinder);
        assert_eq!(entry[n - 2], ContextMenuItem::SortBy);
        assert_eq!(entry[n - 3], ContextMenuItem::CopyRelativePath);
        assert_eq!(entry[n - 4], ContextMenuItem::CopyPath);
    }

    #[test]
    fn navigate_wraps_around() {
        let mut m = ContextMenuState::default();
        m.open((0, 0), Some(0));
        let last = menu_items(&m.entries).count() - 1;
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
        assert!(m.entries.is_empty());
        assert_eq!(m.selected, 0);
        assert!(m.target_entry_idx.is_none());
    }
}
