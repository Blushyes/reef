use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::git::FileEntry;

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_expanded: bool,
    pub git_status: Option<char>,
}

#[derive(Debug, Default)]
pub struct FileTreeState {
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    expanded: HashSet<PathBuf>,
    git_statuses: HashMap<String, char>,
}

impl FileTreeState {
    pub fn with_entries(entries: Vec<TreeEntry>) -> Self {
        Self {
            entries,
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: HashMap::new(),
        }
    }

    pub fn expanded(&self) -> &HashSet<PathBuf> {
        &self.expanded
    }

    pub fn git_statuses(&self) -> &HashMap<String, char> {
        &self.git_statuses
    }

    pub fn toggle_expand(&mut self, index: usize) {
        if let Some(entry) = self.entries.get(index)
            && entry.is_dir
        {
            let path = entry.path.clone();
            if self.expanded.contains(&path) {
                self.expanded.remove(&path);
            } else {
                self.expanded.insert(path);
            }
        }
    }

    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.selected = 0;
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

    pub fn replace_entries(&mut self, entries: Vec<TreeEntry>, selected_idx: usize) {
        self.entries = entries;
        if self.entries.is_empty() {
            self.selected = 0;
        } else {
            self.selected = selected_idx.min(self.entries.len() - 1);
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
        self.apply_git_statuses_to_entries();
    }

    fn apply_git_statuses_to_entries(&mut self) {
        for entry in &mut self.entries {
            let rel = entry.path.to_string_lossy().to_string();
            entry.git_status = self.git_statuses.get(&rel).copied();
        }
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
            is_expanded: false,
            git_status: None,
        }
    }

    #[test]
    fn navigate_clamps() {
        let mut tree =
            FileTreeState::with_entries(vec![dummy_entry("a"), dummy_entry("b"), dummy_entry("c")]);
        tree.navigate(999);
        assert_eq!(tree.selected, 2);
        tree.navigate(-999);
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn reveal_expands_ancestors() {
        let mut tree = FileTreeState::with_entries(vec![TreeEntry {
            path: PathBuf::from("src/main.rs"),
            name: "main.rs".into(),
            depth: 1,
            is_dir: false,
            is_expanded: false,
            git_status: None,
        }]);
        tree.reveal(Path::new("src/main.rs"));
        assert_eq!(tree.selected, 0);
        assert!(tree.expanded.contains(&PathBuf::from("src")));
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
}
