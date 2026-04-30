//! Multi-selection state for the Files tab tree. Mirrors VS Code's
//! Explorer multi-select model:
//!
//! - `s` toggles the current row in/out of the set.
//! - `Shift+↑/↓` (and `Shift+Click`) extend a contiguous range from
//!   the *anchor* — the path where the contiguous selection started.
//! - `Ctrl+Click` toggles a single row without disturbing the rest.
//! - `Esc` clears.
//!
//! The set lives in path space (`PathBuf`) rather than index space
//! because the underlying tree refreshes — collapse / expand /
//! fs_watcher reload — change indices but not paths. Membership
//! survives a tree rebuild.
//!
//! `extend_to` walks the *current visible flattened tree* between the
//! anchor and target, inclusive, and adds every path it sees. Rows
//! outside the visible tree (collapsed under a parent) are out of
//! scope by definition — Explorer-style selection is visual, not
//! semantic.

use crate::file_tree::TreeEntry;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone)]
pub struct SelectionSet {
    /// Selected paths. `BTreeSet` gives stable iteration order so
    /// downstream batch operations (mass delete, mass copy) process
    /// items deterministically — useful when names collide and the
    /// conflict prompt cycles in a predictable order.
    paths: BTreeSet<PathBuf>,
    /// The path where the current contiguous range started. Set by
    /// `replace_with_single` (a fresh click / cursor move that wipes
    /// the selection) and consulted by `extend_to`. Cleared by
    /// `clear`.
    anchor: Option<PathBuf>,
}

impl SelectionSet {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.paths.iter()
    }

    pub fn anchor(&self) -> Option<&Path> {
        self.anchor.as_deref()
    }

    /// Wipes the set and seeds a fresh one with `path`. Sets it as
    /// the new anchor so a follow-up `extend_to` produces a sensible
    /// range. Use this when the user makes a fresh single click or
    /// uses arrow keys without modifiers.
    pub fn replace_with_single(&mut self, path: PathBuf) {
        self.paths.clear();
        self.paths.insert(path.clone());
        self.anchor = Some(path);
    }

    /// Toggle membership of `path`. If `path` was the anchor and is
    /// being removed, the anchor stays (useful for `Shift+Click` after
    /// `Ctrl+Click`-toggling out the anchor — Shift still extends from
    /// the original anchor, matching VS Code).
    pub fn toggle(&mut self, path: PathBuf) {
        if !self.paths.remove(&path) {
            self.paths.insert(path.clone());
            // First toggle on an empty set acts as the anchor.
            if self.anchor.is_none() {
                self.anchor = Some(path);
            }
        }
    }

    /// Extend the selection to include every visible row between the
    /// anchor and `target`, inclusive. If the anchor is unset, falls
    /// back to a single-item selection at `target`.
    ///
    /// `entries` must be the current flattened visible tree. Rows
    /// outside the slice are not added.
    pub fn extend_to(&mut self, target: PathBuf, entries: &[TreeEntry]) {
        let anchor = match &self.anchor {
            Some(a) => a.clone(),
            None => {
                self.replace_with_single(target);
                return;
            }
        };
        let Some(a_idx) = entries.iter().position(|e| e.path == anchor) else {
            // Anchor scrolled out of the tree (collapsed under a
            // parent, deleted, etc.). Reset to a single-row selection.
            self.replace_with_single(target);
            return;
        };
        let Some(t_idx) = entries.iter().position(|e| e.path == target) else {
            return;
        };
        let (lo, hi) = if a_idx <= t_idx {
            (a_idx, t_idx)
        } else {
            (t_idx, a_idx)
        };
        // Replace any prior range with the new one — VS Code's
        // semantics: Shift+arrow grows / shrinks *the* contiguous
        // range, it doesn't union with stray Ctrl+Click toggles.
        self.paths.clear();
        for e in &entries[lo..=hi] {
            self.paths.insert(e.path.clone());
        }
        // Anchor stays where it was — successive Shift+arrows pivot
        // around it, not around the moving cursor.
    }

    pub fn clear(&mut self) {
        self.paths.clear();
        self.anchor = None;
    }

    /// Snapshot the selected paths as an owned `Vec`. Used at the
    /// boundary into batch operations (clipboard, drag, delete) so
    /// the operation's input list is decoupled from later mutations
    /// of the live selection.
    pub fn to_vec(&self) -> Vec<PathBuf> {
        self.paths.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool, depth: usize) -> TreeEntry {
        TreeEntry {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            depth,
            is_dir,
            is_expanded: is_dir,
            git_status: None,
        }
    }

    fn flat() -> Vec<TreeEntry> {
        vec![
            entry("a.rs", false, 0),
            entry("b.rs", false, 0),
            entry("src", true, 0),
            entry("src/lib.rs", false, 1),
            entry("src/main.rs", false, 1),
            entry("README.md", false, 0),
        ]
    }

    #[test]
    fn default_is_empty_no_anchor() {
        let s = SelectionSet::default();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert!(s.anchor().is_none());
    }

    #[test]
    fn replace_with_single_seeds_anchor() {
        let mut s = SelectionSet::default();
        s.replace_with_single(PathBuf::from("a.rs"));
        assert_eq!(s.len(), 1);
        assert!(s.contains(Path::new("a.rs")));
        assert_eq!(s.anchor(), Some(Path::new("a.rs")));
    }

    #[test]
    fn toggle_adds_then_removes() {
        let mut s = SelectionSet::default();
        s.toggle(PathBuf::from("a.rs"));
        assert!(s.contains(Path::new("a.rs")));
        assert_eq!(s.anchor(), Some(Path::new("a.rs")));
        s.toggle(PathBuf::from("a.rs"));
        assert!(!s.contains(Path::new("a.rs")));
        assert!(s.is_empty());
    }

    #[test]
    fn extend_to_with_no_anchor_falls_back_to_single() {
        let mut s = SelectionSet::default();
        s.extend_to(PathBuf::from("README.md"), &flat());
        assert_eq!(s.len(), 1);
        assert!(s.contains(Path::new("README.md")));
        assert_eq!(s.anchor(), Some(Path::new("README.md")));
    }

    #[test]
    fn extend_to_inclusive_range_forward() {
        let entries = flat();
        let mut s = SelectionSet::default();
        s.replace_with_single(PathBuf::from("b.rs"));
        s.extend_to(PathBuf::from("src/main.rs"), &entries);
        assert_eq!(s.len(), 4); // b.rs, src, src/lib.rs, src/main.rs
        for p in ["b.rs", "src", "src/lib.rs", "src/main.rs"] {
            assert!(s.contains(Path::new(p)), "missing {p}");
        }
        // Anchor unchanged — pivots Shift+arrow around the original.
        assert_eq!(s.anchor(), Some(Path::new("b.rs")));
    }

    #[test]
    fn extend_to_inclusive_range_backward() {
        let entries = flat();
        let mut s = SelectionSet::default();
        s.replace_with_single(PathBuf::from("src/main.rs"));
        s.extend_to(PathBuf::from("b.rs"), &entries);
        assert_eq!(s.len(), 4);
        for p in ["b.rs", "src", "src/lib.rs", "src/main.rs"] {
            assert!(s.contains(Path::new(p)), "missing {p}");
        }
    }

    #[test]
    fn extend_to_replaces_prior_range_not_union() {
        let entries = flat();
        let mut s = SelectionSet::default();
        s.replace_with_single(PathBuf::from("a.rs"));
        s.extend_to(PathBuf::from("src/lib.rs"), &entries);
        // Now extend to a smaller range. VS Code shrinks, not unions.
        s.extend_to(PathBuf::from("b.rs"), &entries);
        assert_eq!(s.len(), 2); // a.rs, b.rs
        assert!(s.contains(Path::new("a.rs")));
        assert!(s.contains(Path::new("b.rs")));
        assert!(!s.contains(Path::new("src/lib.rs")));
    }

    #[test]
    fn extend_to_when_anchor_missing_resets_to_target() {
        let entries = flat();
        let mut s = SelectionSet::default();
        s.replace_with_single(PathBuf::from("disappeared.rs"));
        s.extend_to(PathBuf::from("a.rs"), &entries);
        assert_eq!(s.len(), 1);
        assert!(s.contains(Path::new("a.rs")));
    }

    #[test]
    fn extend_to_target_missing_is_noop() {
        let entries = flat();
        let mut s = SelectionSet::default();
        s.replace_with_single(PathBuf::from("a.rs"));
        s.extend_to(PathBuf::from("ghost"), &entries);
        // Set unchanged.
        assert_eq!(s.len(), 1);
        assert!(s.contains(Path::new("a.rs")));
    }

    #[test]
    fn clear_drops_anchor_and_paths() {
        let mut s = SelectionSet::default();
        s.replace_with_single(PathBuf::from("a.rs"));
        s.toggle(PathBuf::from("b.rs"));
        s.clear();
        assert!(s.is_empty());
        assert!(s.anchor().is_none());
    }

    #[test]
    fn iter_yields_btree_order() {
        let mut s = SelectionSet::default();
        s.toggle(PathBuf::from("z.rs"));
        s.toggle(PathBuf::from("a.rs"));
        s.toggle(PathBuf::from("m.rs"));
        let v: Vec<&PathBuf> = s.iter().collect();
        assert_eq!(v[0], &PathBuf::from("a.rs"));
        assert_eq!(v[1], &PathBuf::from("m.rs"));
        assert_eq!(v[2], &PathBuf::from("z.rs"));
    }
}
