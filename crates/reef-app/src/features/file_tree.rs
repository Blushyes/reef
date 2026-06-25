use std::path::{Path, PathBuf};

pub use reef_core::file_tree::{FileTreeState, TreeEntry};

pub struct FileTree {
    pub root: PathBuf,
    pub state: FileTreeState,
}

impl FileTree {
    pub fn new(workdir: &Path) -> Self {
        Self {
            root: workdir.to_path_buf(),
            state: FileTreeState::default(),
        }
    }

    pub fn toggle_expand(&mut self, index: usize) {
        self.state.toggle_expand(index);
    }

    pub fn collapse_all(&mut self) {
        self.state.collapse_all();
    }

    pub fn navigate(&mut self, delta: i32) {
        self.state.navigate(delta);
    }

    pub fn clear_selection(&mut self) {
        self.state.clear_selection();
    }

    pub fn selected_cleared(&self) -> bool {
        self.state.selected_cleared()
    }

    pub fn selected_entry(&self) -> Option<&TreeEntry> {
        self.state.selected_entry()
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.state.selected_path()
    }

    pub fn expanded_paths(&self) -> Vec<PathBuf> {
        self.state.expanded_paths()
    }

    pub fn git_statuses(&self) -> std::collections::HashMap<String, char> {
        self.state.git_statuses_map()
    }

    pub fn replace_entries(&mut self, entries: Vec<TreeEntry>, selected_idx: usize) {
        self.state.replace_entries(entries, selected_idx);
    }

    pub fn reveal(&mut self, rel: &Path) {
        self.state.reveal(rel);
    }

    pub fn refresh_git_statuses(
        &mut self,
        staged: &[reef_core::git::FileEntry],
        unstaged: &[reef_core::git::FileEntry],
    ) {
        self.state.refresh_git_statuses(staged, unstaged);
    }
}

impl std::ops::Deref for FileTree {
    type Target = FileTreeState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl std::ops::DerefMut for FileTree {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}
