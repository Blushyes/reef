use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A visible entry in the flattened file tree.
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_expanded: bool,
    pub git_status: Option<char>,
}

/// Manages the file tree state.
pub struct FileTree {
    pub root: PathBuf,
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    expanded: HashSet<PathBuf>,
    git_statuses: HashMap<String, char>,
}

impl FileTree {
    pub fn new(workdir: &Path) -> Self {
        let mut tree = Self {
            root: workdir.to_path_buf(),
            entries: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: HashMap::new(),
        };
        tree.rebuild();
        tree
    }

    pub fn rebuild(&mut self) {
        self.entries = build_entries(&self.root, &self.expanded, &self.git_statuses);
        if !self.entries.is_empty() {
            self.selected = self.selected.min(self.entries.len() - 1);
        } else {
            self.selected = 0;
        }
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

    pub fn git_statuses(&self) -> HashMap<String, char> {
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

    pub fn refresh_git_statuses(
        &mut self,
        staged: &[reef_core::git::FileEntry],
        unstaged: &[reef_core::git::FileEntry],
    ) {
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

pub fn build_entries(
    root: &Path,
    expanded: &HashSet<PathBuf>,
    git_statuses: &HashMap<String, char>,
) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    walk_dir(root, root, expanded, git_statuses, &mut entries, 0);
    entries
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    expanded: &HashSet<PathBuf>,
    git_statuses: &HashMap<String, char>,
    out: &mut Vec<TreeEntry>,
    depth: usize,
) {
    let mut children: Vec<(String, PathBuf, bool)> = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        children.push((name, path, is_dir));
    }

    children.sort_by(|a, b| match (a.2, b.2) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
    });

    for (name, full_path, is_dir) in children {
        let rel = full_path
            .strip_prefix(root)
            .unwrap_or(&full_path)
            .to_path_buf();
        let rel_str = rel.to_string_lossy().to_string();
        let is_expanded = is_dir && expanded.contains(&rel);
        let git_status = git_statuses.get(&rel_str).copied();

        out.push(TreeEntry {
            path: rel.clone(),
            name,
            depth,
            is_dir,
            is_expanded,
            git_status,
        });

        if is_dir && is_expanded {
            walk_dir(root, &full_path, expanded, git_statuses, out, depth + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_core::git::{FileEntry, FileStatus};

    fn make_entry(path: &str, status: FileStatus) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status,
            additions: 0,
            deletions: 0,
        }
    }

    fn make_tree_with_entries(entries: Vec<TreeEntry>) -> FileTree {
        FileTree {
            root: PathBuf::from("/nonexistent"),
            entries,
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: HashMap::new(),
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
    fn navigate_forward() {
        let mut tree =
            make_tree_with_entries(vec![dummy_entry("a"), dummy_entry("b"), dummy_entry("c")]);
        tree.navigate(1);
        assert_eq!(tree.selected, 1);
    }

    #[test]
    fn navigate_backward_at_zero_stays_zero() {
        let mut tree = make_tree_with_entries(vec![dummy_entry("a"), dummy_entry("b")]);
        tree.navigate(-1);
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn navigate_clamps_at_end() {
        let mut tree =
            make_tree_with_entries(vec![dummy_entry("a"), dummy_entry("b"), dummy_entry("c")]);
        tree.navigate(9999);
        assert_eq!(tree.selected, 2);
    }

    #[test]
    fn navigate_no_op_on_empty() {
        let mut tree = make_tree_with_entries(vec![]);
        tree.navigate(1);
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn selected_entry_empty_returns_none() {
        let tree = make_tree_with_entries(vec![]);
        assert!(tree.selected_entry().is_none());
    }

    #[test]
    fn selected_entry_returns_correct_entry() {
        let mut tree = make_tree_with_entries(vec![
            dummy_entry("file0.rs"),
            dummy_entry("file1.rs"),
            dummy_entry("file2.rs"),
        ]);
        tree.selected = 2;
        assert_eq!(tree.selected_entry().unwrap().name, "file2.rs");
    }

    #[test]
    fn refresh_git_statuses_clears_previous() {
        let mut tree = make_tree_with_entries(vec![]);
        tree.git_statuses.insert("old.rs".to_string(), 'X');
        tree.refresh_git_statuses(&[], &[]);
        assert!(tree.git_statuses.is_empty());
    }

    #[test]
    fn refresh_git_statuses_inserts_staged_files() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("src/main.rs", FileStatus::Modified)];
        tree.refresh_git_statuses(&staged, &[]);
        assert_eq!(tree.git_statuses.get("src/main.rs").copied(), Some('M'));
    }

    #[test]
    fn refresh_git_statuses_propagates_to_parent_dir() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("src/main.rs", FileStatus::Added)];
        tree.refresh_git_statuses(&staged, &[]);
        assert!(tree.git_statuses.contains_key("src"));
    }

    #[test]
    fn refresh_git_statuses_unstaged_does_not_overwrite_staged() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("a.rs", FileStatus::Added)];
        let unstaged = vec![make_entry("a.rs", FileStatus::Modified)];
        tree.refresh_git_statuses(&staged, &unstaged);
        assert_eq!(tree.git_statuses.get("a.rs").copied(), Some('A'));
    }

    #[test]
    fn refresh_git_statuses_updates_visible_entries_without_rebuild() {
        let mut src = dummy_entry("src");
        src.is_dir = true;
        let mut file = dummy_entry("main.rs");
        file.path = PathBuf::from("src/main.rs");
        file.depth = 1;
        let mut tree = make_tree_with_entries(vec![src, file]);

        let staged = vec![make_entry("src/main.rs", FileStatus::Modified)];
        tree.refresh_git_statuses(&staged, &[]);

        assert_eq!(tree.entries.len(), 2);
        assert_eq!(tree.entries[0].git_status, Some('●'));
        assert_eq!(tree.entries[1].git_status, Some('M'));
    }
}
