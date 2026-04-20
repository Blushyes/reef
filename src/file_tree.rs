use std::collections::{HashMap, HashSet};
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
#[derive(Debug)]
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

    /// Regenerate the flat entries list from the filesystem.
    pub fn rebuild(&mut self) {
        self.entries = build_entries(&self.root, &self.expanded, &self.git_statuses);
        // Clamp selection
        if !self.entries.is_empty() {
            self.selected = self.selected.min(self.entries.len() - 1);
        } else {
            self.selected = 0;
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
            }
        }
    }

    /// Collapse every currently-expanded directory. The `expanded` set is
    /// cleared rather than toggling each entry so the next rebuild emits
    /// only the top-level rows. Selection clamps to index 0 so the viewport
    /// doesn't end up pointing past the shortened entry list.
    ///
    /// Does not rebuild by itself — callers drive the async refresh path
    /// (`App::refresh_file_tree_with_target`) so the file worker gets a
    /// chance to also re-read git decorations atomically with the reshape.
    pub fn collapse_all(&mut self) {
        self.expanded.clear();
        self.selected = 0;
    }

    pub fn navigate(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        // Cleared-selection sentinel (`selected >= entries.len()`): treat
        // the first arrow key as "land on an edge" — Down → first row,
        // Up → last row, matching VSCode's Explorer when nothing is
        // selected. Without this, `selected + 1` on the sentinel would
        // arithmetic-overflow.
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

    /// VSCode-style "nothing selected" state. `clear_selection` drops the
    /// highlight so a subsequent toolbar `+ File` / `+ Folder` creates at
    /// the project root, and right-click menu / F2 / Del no-op until the
    /// user picks a row again.
    ///
    /// Implementation: sets `selected` to `entries.len()`, a value that's
    /// always out of range so `selected_entry()` returns `None` and
    /// `is_selected == global_idx` never matches in render. Avoids the
    /// invasive refactor to `Option<usize>` all callers would need.
    pub fn clear_selection(&mut self) {
        self.selected = self.entries.len();
    }

    /// Whether `selected` currently points past the last entry (i.e. the
    /// "cleared" sentinel state).
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

    /// Expand every ancestor directory of `rel` and move `selected` to the
    /// row that displays `rel` in the flattened tree. Used by the quick-open
    /// palette on accept, so the chosen file is visible and the preview
    /// panel shows it immediately. Silently no-ops if the file isn't in the
    /// tree after rebuild (e.g. deleted between index and accept).
    pub fn reveal(&mut self, rel: &Path) {
        for ancestor in rel.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                break;
            }
            self.expanded.insert(ancestor.to_path_buf());
        }
        if let Some(idx) = self.entries.iter().position(|e| e.path == rel) {
            self.selected = idx;
        }
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
        Ok(e) => e,
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

/// Load a file for preview. Returns None if the file can't be read.
///
/// `dark` picks the syntect theme (OneHalfDark vs OneHalfLight) so the
/// highlighted tokens read correctly against whichever UI theme is active.
pub fn load_preview(root: &Path, rel_path: &Path, dark: bool) -> Option<PreviewContent> {
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
        crate::ui::highlight::highlight_file(&rel_str, &lines, dark)
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
