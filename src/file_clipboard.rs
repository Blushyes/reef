//! Internal file clipboard for the Files tab — VS Code-style Cut / Copy /
//! Paste. This is *not* the OSC 52 system clipboard (`crate::clipboard`),
//! which is used only for `Copy Path` / `Copy Relative Path`. The internal
//! clipboard tracks paths the user has marked for an upcoming `Paste`,
//! plus whether the operation is move (Cut) or duplicate (Copy).
//!
//! The state is intentionally minimal — no IO, no async, no path
//! resolution. Mutation of the underlying files happens elsewhere
//! (`tasks::FilesTask::{MovePaths, CopyPaths}`); this module only
//! describes intent.
//!
//! Lifecycle:
//! - `set` (with `ClipMode::Cut` or `Copy`): replace any prior contents.
//! - Successful Paste of a Cut → caller invokes `clear` (the source
//!   was moved, holding stale paths would be misleading).
//! - Successful Paste of a Copy → caller leaves the clipboard alone
//!   so a second Paste reuses the same sources (matches VS Code).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipMode {
    /// Move on Paste. Source row rendered dimmed until cleared.
    Cut,
    /// Duplicate on Paste. No visual change to source.
    Copy,
}

#[derive(Debug, Default, Clone)]
pub struct FileClipboard {
    pub mode: Option<ClipMode>,
    /// Workdir-relative paths. Order is preserved from the user's
    /// selection so the first one decides where the post-paste cursor
    /// lands.
    pub paths: Vec<PathBuf>,
}

impl FileClipboard {
    /// Replace the clipboard with `paths` under `mode`. An empty
    /// `paths` is rejected (no-op) — callers should `clear()` instead
    /// of "set with empty list", which would leave `mode` set with no
    /// payload and confuse `is_empty()`.
    pub fn set(&mut self, mode: ClipMode, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.mode = Some(mode);
        self.paths = paths;
    }

    pub fn clear(&mut self) {
        self.mode = None;
        self.paths.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.mode.is_none() || self.paths.is_empty()
    }

    pub fn is_cut(&self) -> bool {
        matches!(self.mode, Some(ClipMode::Cut))
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.iter().any(|p| p == path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn default_is_empty() {
        let c = FileClipboard::default();
        assert!(c.is_empty());
        assert!(!c.is_cut());
        assert_eq!(c.mode, None);
    }

    #[test]
    fn set_cut_replaces_existing() {
        let mut c = FileClipboard::default();
        c.set(ClipMode::Cut, vec![p("a.txt")]);
        assert_eq!(c.mode, Some(ClipMode::Cut));
        assert_eq!(c.paths, vec![p("a.txt")]);
        assert!(c.is_cut());
        c.set(ClipMode::Cut, vec![p("b.txt"), p("c.txt")]);
        assert_eq!(c.paths, vec![p("b.txt"), p("c.txt")]);
    }

    #[test]
    fn set_with_empty_paths_is_a_noop() {
        // Avoids the silly state Some(Cut) + paths=[] which would
        // confuse `is_empty()`.
        let mut c = FileClipboard::default();
        c.set(ClipMode::Cut, vec![]);
        assert!(c.is_empty());
        assert_eq!(c.mode, None);
    }

    #[test]
    fn set_copy_then_set_cut_replaces_mode() {
        let mut c = FileClipboard::default();
        c.set(ClipMode::Copy, vec![p("a.txt")]);
        assert!(!c.is_cut());
        c.set(ClipMode::Cut, vec![p("a.txt")]);
        assert!(c.is_cut());
    }

    #[test]
    fn clear_resets_state() {
        let mut c = FileClipboard::default();
        c.set(ClipMode::Cut, vec![p("a.txt")]);
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.mode, None);
        assert!(c.paths.is_empty());
    }

    #[test]
    fn contains_finds_exact_path() {
        let mut c = FileClipboard::default();
        c.set(ClipMode::Copy, vec![p("src/a.rs"), p("src/b.rs")]);
        assert!(c.contains(Path::new("src/a.rs")));
        assert!(c.contains(Path::new("src/b.rs")));
        assert!(!c.contains(Path::new("src/c.rs")));
        assert!(!c.contains(Path::new("a.rs")));
    }
}
