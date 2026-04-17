use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A visible entry in the flattened file tree.
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub path: PathBuf, // relative to workdir
    pub name: String,  // display name (filename only)
    pub depth: usize,
    pub is_dir: bool,
    pub is_expanded: bool,
    pub git_status: Option<char>, // 'M', 'A', 'D', '?', etc.
}

/// File preview content.
pub struct PreviewContent {
    pub file_path: String,
    pub lines: Vec<String>,
    pub is_binary: bool,
    pub highlighted: Option<Vec<Vec<(ratatui::style::Style, String)>>>,
}

/// Manages the file tree state.
pub struct FileTree {
    pub root: PathBuf,
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
    expanded: HashSet<PathBuf>,
    git_statuses: std::collections::HashMap<String, char>,
}

impl FileTree {
    pub fn new(workdir: &Path) -> Self {
        let mut tree = Self {
            root: workdir.to_path_buf(),
            entries: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: std::collections::HashMap::new(),
        };
        tree.rebuild();
        tree
    }

    /// Regenerate the flat entries list from the filesystem.
    pub fn rebuild(&mut self) {
        self.entries.clear();
        self.walk_dir(&self.root.clone(), 0);
        // Clamp selection
        if !self.entries.is_empty() {
            self.selected = self.selected.min(self.entries.len() - 1);
        } else {
            self.selected = 0;
        }
    }

    fn walk_dir(&mut self, dir: &Path, depth: usize) {
        let mut children: Vec<(String, PathBuf, bool)> = Vec::new();

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden files and .git
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let is_dir = path.is_dir();
            children.push((name, path, is_dir));
        }

        // Sort: directories first, then files, alphabetically
        children.sort_by(|a, b| match (a.2, b.2) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        });

        for (name, full_path, is_dir) in children {
            let rel = full_path
                .strip_prefix(&self.root)
                .unwrap_or(&full_path)
                .to_path_buf();
            let rel_str = rel.to_string_lossy().to_string();
            let is_expanded = is_dir && self.expanded.contains(&rel);
            let git_status = self.git_statuses.get(&rel_str).copied();

            self.entries.push(TreeEntry {
                path: rel,
                name,
                depth,
                is_dir,
                is_expanded,
                git_status,
            });

            if is_dir && is_expanded {
                self.walk_dir(&full_path, depth + 1);
            }
        }
    }

    pub fn toggle_expand(&mut self, index: usize) {
        if let Some(entry) = self.entries.get(index) {
            if entry.is_dir {
                let path = entry.path.clone();
                if self.expanded.contains(&path) {
                    self.expanded.remove(&path);
                } else {
                    self.expanded.insert(path);
                }
                self.rebuild();
            }
        }
    }

    pub fn navigate(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        if delta > 0 {
            self.selected = (self.selected + delta as usize).min(self.entries.len() - 1);
        } else {
            self.selected = self.selected.saturating_sub((-delta) as usize);
        }
    }

    pub fn selected_entry(&self) -> Option<&TreeEntry> {
        self.entries.get(self.selected)
    }

    /// Merge git status from the host's file lists.
    pub fn refresh_git_statuses(
        &mut self,
        staged: &[crate::git::FileEntry],
        unstaged: &[crate::git::FileEntry],
    ) {
        self.git_statuses.clear();
        for f in staged {
            self.git_statuses.insert(
                f.path.clone(),
                f.status.label().chars().next().unwrap_or(' '),
            );
        }
        for f in unstaged {
            let ch = f.status.label().chars().next().unwrap_or(' ');
            self.git_statuses.entry(f.path.clone()).or_insert(ch);
        }
        // Propagate status to parent directories
        let paths: Vec<String> = self.git_statuses.keys().cloned().collect();
        for path in paths {
            let p = Path::new(&path);
            for ancestor in p.ancestors().skip(1) {
                let a = ancestor.to_string_lossy().to_string();
                if a.is_empty() {
                    break;
                }
                self.git_statuses.entry(a).or_insert('●');
            }
        }
        self.rebuild();
    }
}

/// Load a file for preview. Returns None if the file can't be read.
pub fn load_preview(root: &Path, rel_path: &Path) -> Option<PreviewContent> {
    let full = root.join(rel_path);
    if !full.is_file() {
        return None;
    }

    // Binary detection: check first 8KB for null bytes
    let raw = match std::fs::read(&full) {
        Ok(r) => r,
        Err(_) => return None,
    };

    let check_len = raw.len().min(8192);
    if raw[..check_len].contains(&0) {
        return Some(PreviewContent {
            file_path: rel_path.to_string_lossy().to_string(),
            lines: Vec::new(),
            is_binary: true,
            highlighted: None,
        });
    }

    let content = String::from_utf8_lossy(&raw);
    let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Cap at 10K lines
    let lines = if lines.len() > 10_000 {
        lines[..10_000].to_vec()
    } else {
        lines
    };

    let rel_str = rel_path.to_string_lossy().to_string();
    let highlighted = if raw.len() <= 512 * 1024 && lines.len() <= 5_000 {
        crate::highlight::highlight_file(&rel_str, &lines)
    } else {
        None
    };

    Some(PreviewContent {
        file_path: rel_str,
        lines,
        is_binary: false,
        highlighted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{FileEntry, FileStatus};

    fn make_entry(path: &str, status: FileStatus) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status,
        }
    }

    fn make_tree_with_entries(entries: Vec<TreeEntry>) -> FileTree {
        FileTree {
            root: PathBuf::from("/nonexistent"),
            entries,
            selected: 0,
            expanded: HashSet::new(),
            git_statuses: std::collections::HashMap::new(),
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

    // ── navigate ─────────────────────────────────────────────────────────────

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
        assert_eq!(tree.selected, 0); // no crash, stays at 0
    }

    // ── selected_entry ───────────────────────────────────────────────────────

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

    // ── refresh_git_statuses ─────────────────────────────────────────────────

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
        let ch = tree.git_statuses.get("src/main.rs").copied();
        assert_eq!(ch, Some('M'));
    }

    #[test]
    fn refresh_git_statuses_propagates_to_parent_dir() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("src/main.rs", FileStatus::Added)];
        tree.refresh_git_statuses(&staged, &[]);
        // Parent directory "src" should have been given the propagated marker
        assert!(
            tree.git_statuses.contains_key("src"),
            "parent dir should appear in git_statuses"
        );
    }

    #[test]
    fn refresh_git_statuses_unstaged_does_not_overwrite_staged() {
        let mut tree = make_tree_with_entries(vec![]);
        let staged = vec![make_entry("a.rs", FileStatus::Added)];
        let unstaged = vec![make_entry("a.rs", FileStatus::Modified)];
        // staged sets 'A'; unstaged uses or_insert so 'A' stays
        tree.refresh_git_statuses(&staged, &unstaged);
        assert_eq!(tree.git_statuses.get("a.rs").copied(), Some('A'));
    }
}
