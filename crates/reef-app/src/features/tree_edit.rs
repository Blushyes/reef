//! VSCode-style inline rename / new-file / new-folder editing for the
//! Files tab tree.
//!
//! When the user clicks the toolbar `+` button, presses F2 on a selected
//! row, or picks "Rename" / "New File" / "New Folder" from the right-click
//! context menu, the tree sprouts a temporary editable row with a text
//! cursor. Character input goes into `buffer`; Enter commits (via an
//! async fs worker request); Esc cancels.
//!
//! The state lives on `App` (`app.tree_edit`) and is cheap — everything
//! expensive (the actual `fs::File::create` / `fs::rename`) runs on the
//! files worker. This module only handles the UI state + input-side
//! name validation.
//!
//! Priority in `input::handle_key`: this modal slots in **after**
//! quick-open / search / global-search but **before** place mode, so
//! starting a rename while a search prompt is open lets the search owner
//! see the key first (expected), but once we're editing, tabs / clicks /
//! normal keys all get swallowed.

use reef_core::file_ops::FileNameError;
use std::path::PathBuf;

/// What the edit row is for. Drives the placeholder text, the commit
/// action, and whether the row is rendered as an insert (after the
/// anchor) or as a replacement (of the anchor itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEditMode {
    /// Creating a new file under `parent_dir`. Renderer inserts a new
    /// row indented one level deeper than the anchor folder.
    NewFile,
    /// Creating a new folder under `parent_dir`. Same insertion shape
    /// as NewFile, just a different icon + placeholder.
    NewFolder,
    /// Renaming an existing entry. Renderer replaces the anchor row's
    /// name with the live buffer.
    Rename,
}

#[derive(Debug, Default)]
pub struct TreeEditState {
    /// `true` while the modal editor owns input. Input dispatch in
    /// `input::handle_key` short-circuits on this before the tab-
    /// specific handlers run.
    pub active: bool,

    pub mode: Option<TreeEditMode>,

    /// Workdir-relative directory the new entry will land in (for
    /// NewFile / NewFolder), or the directory containing the entry
    /// being renamed (for Rename). Empty means workspace root.
    pub parent_dir: Option<PathBuf>,

    /// For Rename: the original workdir-relative path. None for
    /// NewFile / NewFolder.
    pub rename_target: Option<PathBuf>,

    /// The name being typed. Starts empty for NewFile / NewFolder,
    /// pre-filled with the current basename for Rename.
    pub buffer: String,

    /// Byte offset into `buffer`. Always on a char boundary — the
    /// render path uses display-width arithmetic so a multi-byte
    /// cursor position renders correctly.
    pub cursor: usize,

    /// Index into `file_tree.entries` of the row this edit is
    /// anchored to. For NewFile/NewFolder it's the parent folder's
    /// row (or `None` when creating at project root). For Rename
    /// it's the row being edited. None means "anchor at tree top".
    pub anchor_idx: Option<usize>,

    /// Most recent validation / commit rejection. Cleared on the next
    /// keystroke so typing auto-dismisses it.
    pub error: Option<FileNameError>,
}

impl TreeEditState {
    /// Is the editor currently showing an error banner under the row?
    /// Used by the renderer to decide whether to draw an extra line.
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }

    /// True when there's a real buffer to commit. Distinct from
    /// `active` — we can be active with an empty buffer (user just
    /// opened a New File prompt and hasn't typed yet).
    pub fn has_pending_text(&self) -> bool {
        !self.buffer.trim().is_empty()
    }

    /// Reset to default state. Called on commit success, explicit
    /// cancel, tab switch, or app quit.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_inactive() {
        let s = TreeEditState::default();
        assert!(!s.active);
        assert_eq!(s.buffer, "");
        assert_eq!(s.cursor, 0);
        assert!(s.error.is_none());
        assert!(!s.has_error());
        assert!(!s.has_pending_text());
    }

    #[test]
    fn has_pending_text_ignores_whitespace() {
        let mut s = TreeEditState::default();
        assert!(!s.has_pending_text());
        s.buffer = "   ".into();
        assert!(!s.has_pending_text());
        s.buffer = " foo ".into();
        assert!(s.has_pending_text());
    }

    #[test]
    fn clear_resets_everything() {
        let mut s = TreeEditState {
            active: true,
            mode: Some(TreeEditMode::Rename),
            parent_dir: Some(PathBuf::from("/tmp")),
            rename_target: Some(PathBuf::from("/tmp/a.txt")),
            buffer: "b.txt".into(),
            cursor: 5,
            anchor_idx: Some(3),
            error: Some(FileNameError::InvalidName),
        };
        s.clear();
        assert!(!s.active);
        assert!(s.mode.is_none());
        assert!(s.parent_dir.is_none());
        assert!(s.rename_target.is_none());
        assert_eq!(s.buffer, "");
        assert_eq!(s.cursor, 0);
        assert!(s.anchor_idx.is_none());
        assert!(s.error.is_none());
    }
}
