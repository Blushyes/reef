use crate::file_tree::{self, FileTree, PreviewContent};
use crate::fs_watcher;
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, GitRepo, RefLabel};
use crate::mouse::{ClickAction, HitTestRegistry};
use crate::toast::Toast;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Git,
    Files,
    Graph,
}

impl Tab {
    /// Canonical ordering shared by the tab bar renderer and the digit shortcut.
    pub const ALL: &'static [Tab] = &[Tab::Files, Tab::Git, Tab::Graph];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Files => " 📁 Files ",
            Tab::Git => " ⎇ Git ",
            Tab::Graph => " ⑂ Graph ",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Files, // left
    Diff,  // right
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLayout {
    Unified,    // 上下统一视图
    SideBySide, // 左右对比视图
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Compact,  // 只显示变更区域 ± context
    FullFile, // 显示整个文件
}

/// State for the inline Git status sidebar.
#[derive(Debug, Default)]
pub struct GitStatusState {
    pub tree_mode: bool,
    pub collapsed_dirs: HashSet<String>,
    pub confirm_discard: Option<String>,
    pub confirm_push: bool,
    pub confirm_force_push: bool,
    /// Last `git push` failure surfaced as an in-panel banner. Cleared by a
    /// successful push or explicit dismiss. Kept in addition to `App.toasts`
    /// because the banner stays visible across re-renders whereas toasts are
    /// ephemeral.
    pub push_error: Option<String>,
    pub scroll: usize,
}

/// State for the inline commit graph sidebar. Unused until M3.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct GitGraphState {
    pub rows: Vec<GraphRow>,
    pub ref_map: HashMap<String, Vec<RefLabel>>,
    /// `(head_oid, refs_hash)` — revwalk is skipped when these are unchanged,
    /// so workdir edits don't trigger a full re-walk on large repos.
    pub cache_key: Option<(String, u64)>,
    pub selected_idx: usize,
    pub selected_commit: Option<String>,
    pub scroll: usize,
}

/// State for the inline commit-detail editor panel (Tab::Graph right side).
#[derive(Debug)]
pub struct CommitDetailState {
    pub detail: Option<CommitDetail>,
    pub file_diff: Option<(String, DiffContent)>,
    /// Intentionally independent of `App.diff_layout` — the Git tab and the
    /// Graph tab track their diff layout separately (see plan pitfall #1).
    pub diff_layout: DiffLayout,
    pub diff_mode: DiffMode,
    pub files_tree_mode: bool,
    pub files_collapsed: HashSet<String>,
    /// Vertical scroll for the entire panel (header + files + diff), matching
    /// the plugin-era behaviour of one scroll offset for the whole view.
    pub scroll: usize,
}

impl Default for CommitDetailState {
    fn default() -> Self {
        Self {
            detail: None,
            file_diff: None,
            diff_layout: DiffLayout::Unified,
            diff_mode: DiffMode::Compact,
            files_tree_mode: false,
            files_collapsed: HashSet::new(),
            scroll: 0,
        }
    }
}

pub struct App {
    pub repo: Option<GitRepo>,

    // Tab
    pub active_tab: Tab,
    pub active_panel: Panel,

    // ── Git tab state ──
    pub staged_files: Vec<FileEntry>,
    pub unstaged_files: Vec<FileEntry>,
    pub selected_file: Option<SelectedFile>,
    pub diff_content: Option<DiffContent>,
    pub diff_layout: DiffLayout,
    pub diff_mode: DiffMode,
    pub staged_collapsed: bool,
    pub unstaged_collapsed: bool,
    pub file_scroll: usize,
    pub diff_scroll: usize,
    pub diff_h_scroll: usize,

    // ── Files tab state ──
    pub file_tree: FileTree,
    pub preview_content: Option<PreviewContent>,
    pub tree_scroll: usize,
    pub preview_scroll: usize,
    pub preview_h_scroll: usize,

    // Layout
    pub split_percent: u16,
    pub dragging_split: bool,

    // Mouse
    pub hit_registry: HitTestRegistry,
    pub hover_row: Option<u16>,
    pub hover_col: Option<u16>,
    /// (timestamp, column, row) of the last mouse-down — used to detect double-clicks.
    pub last_click: Option<(Instant, u16, u16)>,

    // ── Inline git state ──
    pub git_status: GitStatusState,
    pub git_graph: GitGraphState,
    pub commit_detail: CommitDetailState,
    /// Cross-panel toast queue, surfaced in the status bar. Used for push
    /// success/failure and any future in-app notifications.
    pub toasts: Vec<Toast>,

    /// Host-owned fs watcher channel. `None` when the watcher couldn't start —
    /// the sender inside the thread was dropped so `try_recv` returns `Disconnected`.
    pub fs_watcher_rx: Option<mpsc::Receiver<()>>,

    // Control
    pub should_quit: bool,
    pub select_mode: bool,
    pub show_help: bool,
}

#[derive(Debug, Clone)]
pub struct SelectedFile {
    pub path: String,
    pub is_staged: bool,
}

impl App {
    pub fn new() -> Self {
        let repo = GitRepo::open().ok();
        let workdir = repo
            .as_ref()
            .and_then(|r| r.workdir_path())
            .unwrap_or_else(|| PathBuf::from("."));
        let file_tree = FileTree::new(&workdir);
        let fs_watcher_rx = Some(fs_watcher::spawn(workdir.clone()));
        let (saved_layout, saved_mode) = load_prefs();
        let mut app = Self {
            repo,
            active_tab: Tab::Files,
            active_panel: Panel::Files,
            staged_files: Vec::new(),
            unstaged_files: Vec::new(),
            selected_file: None,
            diff_content: None,
            diff_layout: saved_layout,
            diff_mode: saved_mode,
            staged_collapsed: false,
            unstaged_collapsed: false,
            file_scroll: 0,
            diff_scroll: 0,
            diff_h_scroll: 0,
            file_tree,
            preview_content: None,
            tree_scroll: 0,
            preview_scroll: 0,
            preview_h_scroll: 0,
            split_percent: 30,
            dragging_split: false,
            hit_registry: HitTestRegistry::new(),
            hover_row: None,
            hover_col: None,
            last_click: None,
            git_status: GitStatusState {
                tree_mode: load_bool_pref("status.tree_mode", "tree_mode"),
                ..GitStatusState::default()
            },
            git_graph: GitGraphState::default(),
            commit_detail: CommitDetailState {
                diff_layout: match load_str_pref("commit.diff_layout", "commit_diff_layout")
                    .as_deref()
                {
                    Some("side_by_side") => DiffLayout::SideBySide,
                    _ => DiffLayout::Unified,
                },
                diff_mode: match load_str_pref("commit.diff_mode", "commit_diff_mode").as_deref() {
                    Some("full_file") => DiffMode::FullFile,
                    _ => DiffMode::Compact,
                },
                files_tree_mode: load_bool_pref("commit.files_tree_mode", "commit_files_tree_mode"),
                ..CommitDetailState::default()
            },
            toasts: Vec::new(),
            fs_watcher_rx,
            should_quit: false,
            select_mode: false,
            show_help: false,
        };
        // Migrate legacy prefs (unprefixed host keys + old git.prefs file)
        // into the prefixed namespace. Safe to run on every boot.
        crate::prefs::migrate_legacy_prefs();
        app.refresh_status();
        app
    }

    pub fn refresh_status(&mut self) {
        let Some(ref repo) = self.repo else {
            return;
        };
        let (staged, unstaged) = repo.get_status();
        self.staged_files = staged;
        self.unstaged_files = unstaged;

        self.file_tree
            .refresh_git_statuses(&self.staged_files, &self.unstaged_files);

        // Reconcile selection: check both lists so is_staged stays correct
        // even if the plugin staged/unstaged via a key event that the host
        // didn't directly observe.
        if let Some(ref mut sel) = self.selected_file {
            let in_staged = self.staged_files.iter().any(|f| f.path == sel.path);
            let in_unstaged = self.unstaged_files.iter().any(|f| f.path == sel.path);
            if in_staged {
                sel.is_staged = true;
            } else if in_unstaged {
                sel.is_staged = false;
            } else {
                self.selected_file = None;
                self.diff_content = None;
            }
        }
    }

    /// Rebuild the file tree from disk, applying git decorations when a repo is open.
    /// Safe to call on any workdir — `refresh_status` handles repo/no-repo internally.
    pub fn refresh_file_tree(&mut self) {
        if self.repo.is_some() {
            self.refresh_status();
        } else {
            self.file_tree.rebuild();
        }
    }

    pub fn load_preview(&mut self) {
        if let Some(entry) = self.file_tree.selected_entry() {
            if !entry.is_dir {
                let new_content = file_tree::load_preview(&self.file_tree.root, &entry.path);
                // Preserve scroll when reloading the same file (fs-watcher refresh);
                // reset only when the selected path actually changed (navigation).
                let same_file = matches!(
                    (self.preview_content.as_ref(), new_content.as_ref()),
                    (Some(old), Some(new)) if old.file_path == new.file_path
                );
                self.preview_content = new_content;
                if !same_file {
                    self.preview_scroll = 0;
                    self.preview_h_scroll = 0;
                }
            }
        }
    }

    pub fn select_file(&mut self, path: &str, is_staged: bool) {
        self.selected_file = Some(SelectedFile {
            path: path.to_string(),
            is_staged,
        });
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.load_diff();
    }

    pub fn load_diff(&mut self) {
        let diff = if let (Some(repo), Some(sel)) = (&self.repo, &self.selected_file) {
            let context = match self.diff_mode {
                DiffMode::FullFile => 9999,
                DiffMode::Compact => 3,
            };
            repo.get_diff(&sel.path, sel.is_staged, context)
        } else {
            None
        };
        self.diff_content = diff;
    }

    pub fn toggle_diff_layout(&mut self) {
        self.diff_layout = match self.diff_layout {
            DiffLayout::Unified => DiffLayout::SideBySide,
            DiffLayout::SideBySide => DiffLayout::Unified,
        };
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        save_prefs(self.diff_layout, self.diff_mode);
    }

    pub fn toggle_diff_mode(&mut self) {
        self.diff_mode = match self.diff_mode {
            DiffMode::Compact => DiffMode::FullFile,
            DiffMode::FullFile => DiffMode::Compact,
        };
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.load_diff();
        save_prefs(self.diff_layout, self.diff_mode);
    }

    pub fn stage_file(&mut self, path: &str) {
        let ok = self
            .repo
            .as_ref()
            .map(|r| r.stage_file(path).is_ok())
            .unwrap_or(false);
        if ok {
            // If we were viewing this file, update selection
            if let Some(ref mut sel) = self.selected_file {
                if sel.path == path && !sel.is_staged {
                    sel.is_staged = true;
                }
            }
            self.refresh_status();
            self.load_diff();
        }
    }

    pub fn unstage_file(&mut self, path: &str) {
        let ok = self
            .repo
            .as_ref()
            .map(|r| r.unstage_file(path).is_ok())
            .unwrap_or(false);
        if ok {
            if let Some(ref mut sel) = self.selected_file {
                if sel.path == path && sel.is_staged {
                    sel.is_staged = false;
                }
            }
            self.refresh_status();
            self.load_diff();
        }
    }

    pub fn stage_all(&mut self) {
        let paths: Vec<String> = self.unstaged_files.iter().map(|f| f.path.clone()).collect();
        for p in &paths {
            if let Some(ref repo) = self.repo {
                let _ = repo.stage_file(p);
            }
        }
        if let Some(ref mut sel) = self.selected_file {
            if paths.iter().any(|p| p == &sel.path) {
                sel.is_staged = true;
            }
        }
        self.refresh_status();
        self.load_diff();
        // fs watcher will re-invalidate plugin panels shortly, but invalidate
        // now so the sidebar updates without waiting on the debounce.
    }

    pub fn unstage_all(&mut self) {
        let paths: Vec<String> = self.staged_files.iter().map(|f| f.path.clone()).collect();
        for p in &paths {
            if let Some(ref repo) = self.repo {
                let _ = repo.unstage_file(p);
            }
        }
        if let Some(ref mut sel) = self.selected_file {
            if paths.iter().any(|p| p == &sel.path) {
                sel.is_staged = false;
            }
        }
        self.refresh_status();
        self.load_diff();
    }

    /// Restore the currently-confirmed unstaged file to its HEAD state.
    /// Clears the confirmation banner and selection if the discarded file
    /// was selected, then refreshes status + diff.
    pub fn confirm_discard(&mut self) {
        let Some(path) = self.git_status.confirm_discard.take() else {
            return;
        };
        if let Some(ref repo) = self.repo {
            let _ = repo.restore_file(&path);
        }
        if self
            .selected_file
            .as_ref()
            .map(|s| s.path == path)
            .unwrap_or(false)
        {
            self.selected_file = None;
            self.diff_content = None;
        }
        self.refresh_status();
        self.load_diff();
    }

    /// Rebuild the commit graph iff HEAD or any ref moved since the last build.
    /// Working-tree fs events do NOT invalidate the cache — see plan pitfall #2.
    pub fn refresh_graph(&mut self) {
        const GRAPH_COMMIT_LIMIT: usize = 500;
        let Some(ref repo) = self.repo else {
            self.git_graph.rows.clear();
            self.git_graph.ref_map.clear();
            self.git_graph.cache_key = None;
            return;
        };

        let head = repo.head_oid().unwrap_or_default();
        let refs = repo.list_refs();
        let refs_hash = hash_ref_map(&refs);
        let key = (head, refs_hash);

        if self.git_graph.cache_key.as_ref() == Some(&key) {
            return;
        }

        let commits = repo.list_commits(GRAPH_COMMIT_LIMIT);
        let rows = crate::git::graph::build_graph(&commits);

        // Clamp selection if the graph got shorter (e.g. reset --hard).
        if self.git_graph.selected_idx >= rows.len() {
            self.git_graph.selected_idx = rows.len().saturating_sub(1);
        }
        self.git_graph.selected_commit = rows
            .get(self.git_graph.selected_idx)
            .map(|r| r.commit.oid.clone());

        self.git_graph.rows = rows;
        self.git_graph.ref_map = refs;
        self.git_graph.cache_key = Some(key);

        self.load_commit_detail();
    }

    /// (Re)load commit detail for the currently-selected commit. Clears detail
    /// and any previously-selected file diff whenever the target changes.
    pub fn load_commit_detail(&mut self) {
        self.commit_detail.detail = match (&self.repo, &self.git_graph.selected_commit) {
            (Some(repo), Some(oid)) => repo.get_commit(oid),
            _ => None,
        };
        self.commit_detail.file_diff = None;
    }

    /// Load the inline diff for a file inside the currently-selected commit.
    pub fn load_commit_file_diff(&mut self, path: &str) {
        let context = match self.commit_detail.diff_mode {
            DiffMode::Compact => 3,
            DiffMode::FullFile => 9999,
        };
        self.commit_detail.file_diff = match (&self.repo, &self.git_graph.selected_commit) {
            (Some(repo), Some(oid)) => repo
                .get_commit_file_diff(oid, path, context)
                .map(|d| (path.to_string(), d)),
            _ => None,
        };
    }

    /// Reload the currently-selected commit-file diff — used after toggling
    /// `commit.diff_mode`, which changes the context-lines argument.
    pub fn reload_commit_file_diff(&mut self) {
        let path = self.commit_detail.file_diff.as_ref().map(|(p, _)| p.clone());
        if let Some(path) = path {
            self.load_commit_file_diff(&path);
        }
    }

    /// Move the graph selection by `delta` rows (clamped). Updates
    /// selected_commit and reloads commit detail.
    pub fn move_graph_selection(&mut self, delta: i32) {
        if self.git_graph.rows.is_empty() {
            return;
        }
        let last = self.git_graph.rows.len() - 1;
        let current = self.git_graph.selected_idx as i32;
        let next = (current + delta).clamp(0, last as i32) as usize;
        if next == self.git_graph.selected_idx {
            return;
        }
        self.git_graph.selected_idx = next;
        self.git_graph.selected_commit = self
            .git_graph
            .rows
            .get(next)
            .map(|r| r.commit.oid.clone());
        // Reset commit-detail scroll so the new commit starts at the top.
        self.commit_detail.scroll = 0;
        self.load_commit_detail();
    }

    /// Invoke `git push` (or `--force-with-lease` when `force`), store any
    /// error for display as both a panel banner and a cross-panel toast, and
    /// invalidate the graph cache so Tab::Graph picks up the new remote refs
    /// on the next render.
    pub fn run_push(&mut self, force: bool) {
        let result = self.repo.as_ref().map(|r| r.push(force));
        match result {
            Some(Ok(())) => {
                self.git_status.push_error = None;
                self.toasts.push(Toast::info(if force {
                    "强制推送成功"
                } else {
                    "推送成功"
                }));
            }
            Some(Err(e)) => {
                self.git_status.push_error = Some(e.clone());
                self.toasts.push(Toast::error(format!("推送失败: {e}")));
            }
            None => {}
        }
        // Push advances remote-tracking refs — invalidate the graph cache
        // so Tab::Graph rebuilds on its next render.
        self.git_graph.cache_key = None;
        self.refresh_status();
    }

    pub fn handle_action(&mut self, action: ClickAction) {
        match action {
            ClickAction::SwitchTab(tab) => {
                self.active_tab = tab;
            }
            ClickAction::TreeClick(index) => {
                self.file_tree.selected = index;
                if let Some(entry) = self.file_tree.entries.get(index) {
                    if entry.is_dir {
                        self.file_tree.toggle_expand(index);
                    } else {
                        self.load_preview();
                    }
                }
            }
            ClickAction::SelectFile { path, staged } => {
                self.select_file(&path, staged);
            }
            ClickAction::StageFile(path) => {
                self.stage_file(&path);
            }
            ClickAction::UnstageFile(path) => {
                self.unstage_file(&path);
            }
            ClickAction::ToggleStaged => {
                self.staged_collapsed = !self.staged_collapsed;
            }
            ClickAction::ToggleUnstaged => {
                self.unstaged_collapsed = !self.unstaged_collapsed;
            }
            ClickAction::StartDragSplit => {
                self.dragging_split = true;
            }
            ClickAction::GitCommand { command, args, .. } => {
                // All git.* commands are handled inline now. Try each panel's
                // dispatcher in turn; nothing falls through to the plugin.
                if crate::ui::git_status_panel::handle_command(self, &command, &args) {
                    return;
                }
                if crate::ui::git_graph_panel::handle_command(self, &command, &args) {
                    return;
                }
                let _ = crate::ui::commit_detail_panel::handle_command(self, &command, &args);
            }
        }
    }

    /// Total visible file rows (for keyboard navigation)
    pub fn visible_file_count(&self) -> usize {
        let mut count = 0;
        if !self.staged_files.is_empty() {
            count += 1; // header
            if !self.staged_collapsed {
                count += self.staged_files.len();
            }
        }
        count += 1; // unstaged header
        if !self.unstaged_collapsed {
            count += self.unstaged_files.len();
        }
        count
    }

    pub fn navigate_files(&mut self, delta: i32) {
        // Build a flat list of selectable items
        let mut items: Vec<(String, bool)> = Vec::new();

        if !self.staged_files.is_empty() && !self.staged_collapsed {
            for f in &self.staged_files {
                items.push((f.path.clone(), true));
            }
        }
        if !self.unstaged_collapsed {
            for f in &self.unstaged_files {
                items.push((f.path.clone(), false));
            }
        }

        if items.is_empty() {
            return;
        }

        let current_idx = self
            .selected_file
            .as_ref()
            .and_then(|sel| {
                items
                    .iter()
                    .position(|(p, s)| p == &sel.path && *s == sel.is_staged)
            })
            .unwrap_or(0);

        let new_idx = if delta > 0 {
            (current_idx + delta as usize).min(items.len() - 1)
        } else {
            current_idx.saturating_sub((-delta) as usize)
        };

        let (path, staged) = items[new_idx].clone();
        // Only update host selection state; caller is responsible for syncing to plugin
        // so rapid key repeats can be coalesced into a single command.
        self.selected_file = Some(SelectedFile {
            path,
            is_staged: staged,
        });
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
    }

    /// Called every frame: drain fs-watcher events and refresh caches. Does
    /// NOT invalidate `git_graph.cache_key` — working-tree edits don't move
    /// HEAD or refs, so the commit graph stays valid (see plan pitfall #2).
    pub fn tick(&mut self) {
        let mut fs_dirty = false;
        if let Some(rx) = self.fs_watcher_rx.as_ref() {
            while rx.try_recv().is_ok() {
                fs_dirty = true;
            }
        }
        if fs_dirty {
            self.refresh_file_tree();
            self.load_preview();
            self.load_diff();
        }
    }
}

// ─── Prefs persistence ────────────────────────────────────────────────────────

fn prefs_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config").join("reef");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("prefs"))
}

fn load_prefs() -> (DiffLayout, DiffMode) {
    let default = (DiffLayout::Unified, DiffMode::Compact);
    let path = match prefs_path() {
        Some(p) => p,
        None => return default,
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return default,
    };
    let mut layout = DiffLayout::Unified;
    let mut mode = DiffMode::Compact;
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("layout=") {
            layout = match val.trim() {
                "side_by_side" => DiffLayout::SideBySide,
                _ => DiffLayout::Unified,
            };
        } else if let Some(val) = line.strip_prefix("mode=") {
            mode = match val.trim() {
                "full_file" => DiffMode::FullFile,
                _ => DiffMode::Compact,
            };
        }
    }
    (layout, mode)
}

/// Look up a prefixed key from the unified prefs file, falling back to the
/// legacy unprefixed `git.prefs` file (which the plugin still writes). The
/// fallback path disappears once the plugin is gone in M4/M5.
fn load_str_pref(new_key: &str, legacy_git_key: &str) -> Option<String> {
    if let Some(v) = crate::prefs::get(new_key) {
        return Some(v);
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join(".config")
        .join("reef")
        .join("git.prefs");
    let content = std::fs::read_to_string(&path).ok()?;
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == legacy_git_key {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn load_bool_pref(new_key: &str, legacy_git_key: &str) -> bool {
    load_str_pref(new_key, legacy_git_key)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Stable hash of the ref map — used as part of the graph cache key.
fn hash_ref_map(map: &HashMap<String, Vec<RefLabel>>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut entries: Vec<(&String, &Vec<RefLabel>)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (oid, labels) in entries {
        oid.hash(&mut hasher);
        for label in labels {
            match label {
                RefLabel::Head => 0u8.hash(&mut hasher),
                RefLabel::Branch(s) => {
                    1u8.hash(&mut hasher);
                    s.hash(&mut hasher);
                }
                RefLabel::RemoteBranch(s) => {
                    2u8.hash(&mut hasher);
                    s.hash(&mut hasher);
                }
                RefLabel::Tag(s) => {
                    3u8.hash(&mut hasher);
                    s.hash(&mut hasher);
                }
            }
        }
    }
    hasher.finish()
}

fn save_prefs(layout: DiffLayout, mode: DiffMode) {
    if let Some(path) = prefs_path() {
        let layout_str = match layout {
            DiffLayout::Unified => "unified",
            DiffLayout::SideBySide => "side_by_side",
        };
        let mode_str = match mode {
            DiffMode::Compact => "compact",
            DiffMode::FullFile => "full_file",
        };
        let content = format!("layout={}\nmode={}\n", layout_str, mode_str);
        let _ = std::fs::write(path, content);
    }
}
