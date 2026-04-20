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

/// Why a commit was rejected, surfaced to the renderer as a red-bg
/// banner directly under the editable row. Stays populated until the
/// next keystroke clears it (users expect their typing to dismiss the
/// error, matching VSCode's Explorer input behaviour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeEditError {
    /// Empty, all-whitespace, or only-dots (`.` / `..`) — can't make
    /// a file system entry out of that.
    InvalidName,
    /// Contains a path separator (`/` on Unix, `\` on Windows) or a
    /// NUL byte. These would either escape the intended parent
    /// directory or be rejected by the OS at create time.
    IllegalChars,
    /// A sibling with this name already exists. Includes the name so
    /// the error line can echo it back.
    NameAlreadyExists(String),
}

#[derive(Debug, Default)]
pub struct TreeEditState {
    /// `true` while the modal editor owns input. Input dispatch in
    /// `input::handle_key` short-circuits on this before the tab-
    /// specific handlers run.
    pub active: bool,

    pub mode: Option<TreeEditMode>,

    /// Absolute path of the directory the new entry will land in
    /// (for NewFile / NewFolder), or the directory containing the
    /// entry being renamed (for Rename). Used to validate collisions
    /// and to construct the final path on commit.
    pub parent_dir: Option<PathBuf>,

    /// For Rename: the original absolute path, so the worker can do
    /// `fs::rename(original, parent_dir.join(buffer))`. None for
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
    pub error: Option<TreeEditError>,
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

/// Trim + normalise a user-typed filename for display in the banner
/// text and toast messages. Strips ASCII / Unicode control characters
/// so embedded `\n` / `\t` can't break single-line rendering. Leaves
/// the actual buffer (which users may still edit) untouched — this
/// is only for display.
pub fn sanitize_filename(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Validate a candidate basename BEFORE we send it to the worker.
/// Returns `Ok(trimmed)` ready to join onto the parent dir, or the
/// specific reason the name is rejected.
///
/// Collision check (name already exists) is NOT done here because it
/// requires a syscall on the parent dir — the caller (`App::commit_tree_edit`)
/// runs it against the live tree.
pub fn validate_basename(raw: &str) -> Result<String, TreeEditError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TreeEditError::InvalidName);
    }
    // `.` / `..` are meaningful in a shell but can't be created as
    // regular entries. Common slip when the user hits Enter before
    // typing anything.
    if trimmed == "." || trimmed == ".." {
        return Err(TreeEditError::InvalidName);
    }
    // Reject path separators so users can't escape `parent_dir`. We
    // check both `/` and `\` regardless of platform — on Unix `\` is
    // a legal filename char, but allowing it would produce paths that
    // look wrong on Windows copies and break git operations in subtle
    // ways. Disallowing it matches VSCode's Explorer rename dialog.
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(TreeEditError::IllegalChars);
    }
    // NUL is rejected by every filesystem we care about — return a
    // clean error instead of letting the worker surface a cryptic
    // `EILSEQ` / `ERROR_INVALID_NAME` string.
    if trimmed.contains('\0') {
        return Err(TreeEditError::IllegalChars);
    }
    // Control characters (0x01..0x1F, 0x7F) are technically legal on
    // ext4 / APFS but produce unreadable listings. Follow VSCode and
    // reject at the UI layer.
    if trimmed.chars().any(|c| c.is_control()) {
        return Err(TreeEditError::IllegalChars);
    }
    Ok(trimmed.to_string())
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
            error: Some(TreeEditError::InvalidName),
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

    #[test]
    fn sanitize_strips_control_chars_and_trims() {
        assert_eq!(sanitize_filename("  foo.rs  "), "foo.rs");
        assert_eq!(sanitize_filename("a\tb\nc"), "a?b?c");
        assert_eq!(sanitize_filename("中文.rs"), "中文.rs");
    }

    #[test]
    fn validate_rejects_empty_and_dot_names() {
        assert_eq!(validate_basename(""), Err(TreeEditError::InvalidName));
        assert_eq!(validate_basename("   "), Err(TreeEditError::InvalidName));
        assert_eq!(validate_basename("."), Err(TreeEditError::InvalidName));
        assert_eq!(validate_basename(".."), Err(TreeEditError::InvalidName));
    }

    #[test]
    fn validate_rejects_separators_and_nul() {
        assert_eq!(
            validate_basename("foo/bar"),
            Err(TreeEditError::IllegalChars)
        );
        assert_eq!(
            validate_basename("foo\\bar"),
            Err(TreeEditError::IllegalChars)
        );
        assert_eq!(
            validate_basename("foo\0bar"),
            Err(TreeEditError::IllegalChars)
        );
    }

    #[test]
    fn validate_rejects_control_chars() {
        assert_eq!(
            validate_basename("foo\tbar"),
            Err(TreeEditError::IllegalChars)
        );
        assert_eq!(
            validate_basename("foo\nbar"),
            Err(TreeEditError::IllegalChars)
        );
    }

    #[test]
    fn validate_accepts_reasonable_names() {
        assert_eq!(validate_basename("foo.rs"), Ok("foo.rs".into()));
        assert_eq!(validate_basename("  foo.rs  "), Ok("foo.rs".into()));
        // Leading dot (dotfiles) is fine.
        assert_eq!(validate_basename(".env"), Ok(".env".into()));
        assert_eq!(validate_basename(".gitignore"), Ok(".gitignore".into()));
        // Non-ASCII filenames are fine.
        assert_eq!(validate_basename("中文.rs"), Ok("中文.rs".into()));
        // Spaces in the middle are legal (they're legal on every
        // filesystem we target; drag-drop already handles escaping).
        assert_eq!(validate_basename("my notes.md"), Ok("my notes.md".into()));
    }
}
