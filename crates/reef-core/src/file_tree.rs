use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::git::FileEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub has_children: bool,
    pub is_expanded: bool,
    pub git_status: Option<char>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTreeRowsSplice {
    pub revision: u64,
    pub start: usize,
    pub delete_count: usize,
    pub insert_count: usize,
}

#[derive(Debug, Default)]
pub struct FileTreeState {
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    expanded: HashSet<PathBuf>,
    git_statuses: HashMap<String, char>,
    rows_revision: u64,
    rows_splice: Option<FileTreeRowsSplice>,
}

impl FileTreeState {
    pub fn with_entries(entries: Vec<TreeEntry>) -> Self {
        let rows_revision = u64::from(!entries.is_empty());
        Self {
            entries,
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: HashMap::new(),
            rows_revision,
            rows_splice: None,
        }
    }

    pub fn rows_revision(&self) -> u64 {
        self.rows_revision
    }

    pub fn rows_splice(&self) -> Option<FileTreeRowsSplice> {
        self.rows_splice
    }

    fn advance_rows_revision(&mut self, splice: Option<(usize, usize, usize)>) {
        self.rows_revision = self.rows_revision.wrapping_add(1).max(1);
        self.rows_splice = splice.map(|(start, delete_count, insert_count)| FileTreeRowsSplice {
            revision: self.rows_revision,
            start,
            delete_count,
            insert_count,
        });
    }

    pub fn expanded(&self) -> &HashSet<PathBuf> {
        &self.expanded
    }

    pub fn git_statuses(&self) -> &HashMap<String, char> {
        &self.git_statuses
    }

    pub fn toggle_expand(&mut self, index: usize) {
        if let Some(entry) = self.entries.get_mut(index)
            && entry.is_dir
        {
            let path = entry.path.clone();
            if self.expanded.contains(&path) {
                self.expanded.remove(&path);
                entry.is_expanded = false;
            } else {
                self.expanded.insert(path);
                entry.is_expanded = true;
            }
            self.advance_rows_revision(None);
        }
    }

    pub fn toggle_expand_by_path(&mut self, path: &Path) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.path.as_path() == path && entry.is_dir)
        else {
            return false;
        };
        self.toggle_expand(index);
        true
    }

    pub fn collapse_all(&mut self) {
        let rows_changed = !self.expanded.is_empty()
            || self
                .entries
                .iter()
                .any(|entry| entry.depth > 0 || entry.is_expanded);
        self.expanded.clear();
        self.entries.retain(|entry| entry.depth == 0);
        for entry in &mut self.entries {
            entry.is_expanded = false;
        }
        self.selected = 0;
        if rows_changed {
            self.advance_rows_revision(None);
        }
    }

    pub fn collapse_visible_descendants(&mut self, index: usize) {
        let Some(parent) = self.entries.get(index) else {
            return;
        };
        let parent_depth = parent.depth;
        let end = self.entries[index + 1..]
            .iter()
            .position(|entry| entry.depth <= parent_depth)
            .map(|offset| index + 1 + offset)
            .unwrap_or(self.entries.len());
        let removed = end.saturating_sub(index + 1);
        if removed == 0 {
            return;
        }
        if self.selected > index && self.selected < end {
            self.selected = index;
        } else if self.selected >= end {
            self.selected -= removed;
        }
        self.entries.drain(index + 1..end);
        self.advance_rows_revision(Some((index + 1, removed, 0)));
    }

    pub fn replace_visible_descendants(&mut self, parent_path: &Path, children: Vec<TreeEntry>) {
        let selected_path = self.selected_path();
        let Some(parent_idx) = self
            .entries
            .iter()
            .position(|entry| entry.path == parent_path && entry.is_dir)
        else {
            return;
        };
        if !self.expanded.contains(parent_path) {
            return;
        }
        let parent_depth = self.entries[parent_idx].depth;
        let end = self.entries[parent_idx + 1..]
            .iter()
            .position(|entry| entry.depth <= parent_depth)
            .map(|offset| parent_idx + 1 + offset)
            .unwrap_or(self.entries.len());
        let delete_count = end.saturating_sub(parent_idx + 1);
        let insert_count = children.len();
        if insert_count == 0 {
            self.expanded.remove(parent_path);
            self.entries[parent_idx].has_children = false;
            self.entries[parent_idx].is_expanded = false;
        } else {
            self.entries[parent_idx].has_children = true;
        }
        self.entries.splice(parent_idx + 1..end, children);
        self.advance_rows_revision(Some((parent_idx + 1, delete_count, insert_count)));
        self.selected = selected_path
            .as_ref()
            .and_then(|path| self.entries.iter().position(|entry| &entry.path == path))
            .unwrap_or(parent_idx);
    }

    pub fn navigate(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        if self.selected > last {
            self.selected = if delta > 0 { 0 } else { last };
            return;
        }
        if delta > 0 {
            self.selected = (self.selected + delta as usize).min(last);
        } else {
            self.selected = self.selected.saturating_sub((-delta) as usize);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected = self.entries.len();
    }

    pub fn selected_cleared(&self) -> bool {
        self.selected >= self.entries.len()
    }

    pub fn selected_entry(&self) -> Option<&TreeEntry> {
        self.entries.get(self.selected)
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_entry().map(|entry| entry.path.clone())
    }

    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        self.expanded.iter().cloned().collect()
    }

    pub fn git_statuses_map(&self) -> HashMap<String, char> {
        self.git_statuses.clone()
    }

    pub fn decorate_entries_with_git_statuses(&self, entries: &mut [TreeEntry]) {
        for entry in entries {
            let path = entry.path.to_string_lossy();
            entry.git_status = self.git_statuses.get(path.as_ref()).copied();
        }
    }

    pub fn replace_entries(&mut self, entries: Vec<TreeEntry>, selected_idx: usize) {
        let rows_changed = self.entries != entries;
        self.entries = entries;
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = selected_idx.min(self.entries.len() - 1);
        }
        if rows_changed {
            self.advance_rows_revision(None);
        }
    }

    pub fn reveal(&mut self, rel: &Path) {
        for ancestor in rel.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            self.expanded.insert(ancestor.to_path_buf());
        }
        if let Some(idx) = self.entries.iter().position(|entry| entry.path == rel) {
            self.selected = idx;
        }
    }

    pub fn refresh_git_statuses(&mut self, staged: &[FileEntry], unstaged: &[FileEntry]) {
        self.git_statuses.clear();
        for file in staged {
            self.git_statuses.insert(
                file.path.clone(),
                file.status.label().chars().next().unwrap_or(' '),
            );
        }
        for file in unstaged {
            let ch = file.status.label().chars().next().unwrap_or(' ');
            self.git_statuses.entry(file.path.clone()).or_insert(ch);
        }

        let paths: Vec<String> = self.git_statuses.keys().cloned().collect();
        for path in paths {
            let p = Path::new(&path);
            for ancestor in p.ancestors().skip(1) {
                let ancestor = ancestor.to_string_lossy().to_string();
                if ancestor.is_empty() {
                    break;
                }
                self.git_statuses.entry(ancestor).or_insert('●');
            }
        }
        if self.apply_git_statuses_to_entries() {
            self.advance_rows_revision(None);
        }
    }

    fn apply_git_statuses_to_entries(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            let rel = entry.path.to_string_lossy().to_string();
            let git_status = self.git_statuses.get(&rel).copied();
            if entry.git_status != git_status {
                entry.git_status = git_status;
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{FileEntry, FileStatus};

    fn make_entry(path: &str, status: FileStatus) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status,
            additions: 0,
            deletions: 0,
        }
    }

    fn dummy_entry(name: &str) -> TreeEntry {
        TreeEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            depth: 0,
            is_dir: false,
            has_children: false,
            is_expanded: false,
            git_status: None,
        }
    }

    fn dummy_dir(path: &str) -> TreeEntry {
        let mut entry = dummy_entry(path);
        entry.is_dir = true;
        entry.has_children = true;
        entry
    }

    #[test]
    fn navigate_clamps() {
        let mut tree =
            FileTreeState::with_entries(vec![dummy_entry("a"), dummy_entry("b"), dummy_entry("c")]);
        let rows_revision = tree.rows_revision();
        tree.navigate(999);
        assert_eq!(tree.selected, 2);
        tree.navigate(-999);
        assert_eq!(tree.selected, 0);
        assert_eq!(tree.rows_revision(), rows_revision);
    }

    #[test]
    fn row_presentation_changes_advance_rows_revision() {
        let mut tree = FileTreeState::with_entries(vec![dummy_dir("src")]);
        let initial_revision = tree.rows_revision();

        tree.toggle_expand(0);

        assert!(tree.rows_revision() > initial_revision);
    }

    #[test]
    fn reveal_expands_ancestors() {
        let mut tree = FileTreeState::with_entries(vec![TreeEntry {
            path: PathBuf::from("src/main.rs"),
            name: "main.rs".into(),
            depth: 1,
            is_dir: false,
            has_children: false,
            is_expanded: false,
            git_status: None,
        }]);
        tree.reveal(Path::new("src/main.rs"));
        assert_eq!(tree.selected, 0);
        assert!(tree.expanded.contains(&PathBuf::from("src")));
    }

    #[test]
    fn toggle_expand_by_path_only_toggles_visible_directories() {
        let mut tree = FileTreeState::with_entries(vec![
            dummy_dir("src"),
            dummy_entry("src/main.rs"),
            dummy_entry("README.md"),
        ]);

        assert!(tree.toggle_expand_by_path(Path::new("src")));
        assert!(tree.expanded.contains(&PathBuf::from("src")));
        assert!(tree.toggle_expand_by_path(Path::new("src")));
        assert!(!tree.expanded.contains(&PathBuf::from("src")));
        assert!(!tree.toggle_expand_by_path(Path::new("README.md")));
        assert!(!tree.toggle_expand_by_path(Path::new("missing")));
    }

    #[test]
    fn refresh_git_statuses_propagates_to_parent_dir() {
        let mut tree = FileTreeState::default();
        let staged = vec![make_entry("src/main.rs", FileStatus::Added)];
        tree.refresh_git_statuses(&staged, &[]);
        assert_eq!(tree.git_statuses.get("src/main.rs").copied(), Some('A'));
        assert!(tree.git_statuses.contains_key("src"));
    }

    #[test]
    fn refresh_git_statuses_updates_visible_entries_without_rebuild() {
        let mut src = dummy_entry("src");
        src.is_dir = true;
        let mut file = dummy_entry("main.rs");
        file.path = PathBuf::from("src/main.rs");
        file.depth = 1;
        let mut tree = FileTreeState::with_entries(vec![src, file]);

        let staged = vec![make_entry("src/main.rs", FileStatus::Modified)];
        tree.refresh_git_statuses(&staged, &[]);

        assert_eq!(tree.entries[0].git_status, Some('●'));
        assert_eq!(tree.entries[1].git_status, Some('M'));
    }

    #[test]
    fn decorate_entries_uses_the_current_git_status_snapshot() {
        let mut tree = FileTreeState::default();
        tree.refresh_git_statuses(&[], &[make_entry("src/main.rs", FileStatus::Modified)]);
        let mut entries = vec![dummy_entry("src/main.rs"), dummy_entry("src/lib.rs")];
        entries[0].git_status = Some('A');

        tree.decorate_entries_with_git_statuses(&mut entries);

        assert_eq!(entries[0].git_status, Some('M'));
        assert_eq!(entries[1].git_status, None);
    }

    #[test]
    fn collapse_visible_descendants_removes_only_parent_subtree() {
        let mut src = dummy_dir("src");
        src.is_expanded = true;
        let mut nested = dummy_dir("src/nested");
        nested.depth = 1;
        let mut child = dummy_entry("src/nested/a.rs");
        child.depth = 2;
        let tail = dummy_entry("README.md");
        let mut tree = FileTreeState::with_entries(vec![src, nested, child, tail]);
        tree.selected = 3;

        tree.collapse_visible_descendants(0);

        assert_eq!(
            tree.entries
                .iter()
                .map(|entry| entry.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("src"), Path::new("README.md")]
        );
        assert_eq!(tree.selected, 1);
        assert_eq!(
            tree.rows_splice(),
            Some(FileTreeRowsSplice {
                revision: tree.rows_revision(),
                start: 1,
                delete_count: 2,
                insert_count: 0,
            })
        );
    }

    #[test]
    fn replace_visible_descendants_restores_selected_path() {
        let mut src = dummy_dir("src");
        src.is_expanded = true;
        let mut old = dummy_entry("src/a.rs");
        old.depth = 1;
        let mut tree = FileTreeState::with_entries(vec![src, old]);
        tree.expanded.insert(PathBuf::from("src"));
        tree.selected = 1;
        let mut refreshed = dummy_entry("src/a.rs");
        refreshed.depth = 1;
        let mut added = dummy_entry("src/b.rs");
        added.depth = 1;

        tree.replace_visible_descendants(Path::new("src"), vec![refreshed, added]);

        assert_eq!(
            tree.rows_splice(),
            Some(FileTreeRowsSplice {
                revision: tree.rows_revision(),
                start: 1,
                delete_count: 1,
                insert_count: 2,
            })
        );

        assert_eq!(tree.selected_path().as_deref(), Some(Path::new("src/a.rs")));
        assert_eq!(tree.entries.len(), 3);
    }

    #[test]
    fn collapsed_parent_rejects_late_subtree_result() {
        let src = dummy_dir("src");
        let mut tree = FileTreeState::with_entries(vec![src]);
        let mut child = dummy_entry("src/a.rs");
        child.depth = 1;

        tree.replace_visible_descendants(Path::new("src"), vec![child]);

        assert_eq!(tree.entries.len(), 1);
    }

    #[test]
    fn empty_subtree_resolves_parent_as_a_leaf() {
        let mut src = dummy_dir("src");
        src.is_expanded = true;
        let mut tree = FileTreeState::with_entries(vec![src]);
        tree.expanded.insert(PathBuf::from("src"));

        tree.replace_visible_descendants(Path::new("src"), Vec::new());

        assert!(!tree.entries[0].has_children);
        assert!(!tree.entries[0].is_expanded);
        assert!(!tree.expanded.contains(Path::new("src")));
        assert_eq!(
            tree.rows_splice().map(|splice| splice.insert_count),
            Some(0)
        );
    }
}
