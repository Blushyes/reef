use crate::file_tree::{self, FileTree, PreviewContent};
use crate::fs_watcher;
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, GitRepo, RefLabel};
use crate::ui::mouse::{ClickAction, HitTestRegistry};
use crate::ui::theme::Theme;
use crate::ui::toast::Toast;
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
        use crate::i18n::{Msg, t};
        match self {
            Tab::Files => t(Msg::TabFiles),
            Tab::Git => t(Msg::TabGit),
            Tab::Graph => t(Msg::TabGraph),
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

/// State for the inline commit graph sidebar.
#[derive(Debug, Default)]
pub struct GitGraphState {
    pub rows: Vec<GraphRow>,
    pub ref_map: HashMap<String, Vec<RefLabel>>,
    /// `(head_oid, refs_hash)` — revwalk is skipped when these are unchanged,
    /// so workdir edits don't trigger a full re-walk on large repos.
    pub cache_key: Option<(String, u64)>,
    pub selected_idx: usize,
    pub selected_commit: Option<String>,
    pub scroll: usize,
    /// `selected_idx` observed on the previous render. Used to distinguish
    /// selection-change follow (bring the selected commit into view) from
    /// user-initiated scroll (leave the viewport alone). Mirrors #13's fix
    /// for the Files tab — without this, mouse-wheel scroll snapped back to
    /// the selected commit on the next tick.
    pub last_rendered_selected: Option<usize>,
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
    /// Vertical scroll for the entire panel (header + files + diff). One
    /// offset covers the whole view — the commit detail is rendered as a
    /// single list rather than split scroll regions.
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
    /// The `file_tree.selected` value we observed on the previous render.
    /// Used by the Files-tab tree panel to distinguish "selection just changed
    /// (scroll the viewport to keep it visible)" from "user scrolled the
    /// viewport themselves (leave it alone)".
    pub last_rendered_tree_selected: Option<usize>,
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

    /// `true` while a background `git push` is in flight. Blocks additional
    /// pushes and lets the status panel render a "推送中…" indicator.
    pub push_in_flight: bool,
    /// Receives `(force, result)` from the push worker thread. Drained in
    /// `App::tick`; once the result is consumed we drop the channel.
    pub push_rx: Option<mpsc::Receiver<(bool, Result<(), String>)>>,

    /// Host-owned fs watcher channel. `None` when the watcher couldn't start —
    /// the sender inside the thread was dropped so `try_recv` returns `Disconnected`.
    pub fs_watcher_rx: Option<mpsc::Receiver<()>>,

    // Control
    pub should_quit: bool,
    pub select_mode: bool,
    pub show_help: bool,

    /// Set by the input layer when the user asks to edit a file. Consumed
    /// by the main loop, which needs to own the terminal to suspend/resume
    /// around `$EDITOR`. Absolute path.
    pub pending_edit: Option<PathBuf>,

    /// Active color theme. Chosen in `main.rs` before raw-mode entry (so the
    /// OSC 11 probe doesn't leak onto the TUI) and passed into `App::new`.
    pub theme: Theme,

    /// In-panel vim-style search (`/`, `?`, `n`, `N`). See `crate::search`.
    pub search: crate::search::SearchState,

    /// VSCode-style quick-open palette. While `active`, input is routed
    /// exclusively to `crate::quick_open::handle_key` (see input.rs).
    pub quick_open: crate::quick_open::QuickOpenState,

    /// Timestamp of the most recent bare-Space keystroke in the global
    /// keymap. `Some(t)` means a Space leader is primed and waiting for a
    /// follow-up key within `input::LEADER_TIMEOUT`. The palette-side
    /// leader has its own slot inside `QuickOpenState` — separate so they
    /// can't interfere across mode transitions.
    pub space_leader_at: Option<std::time::Instant>,

    /// Last-rendered content height (in rows) for each right-side panel.
    /// Search jumps read these to center the match in view. Written by the
    /// panel's render fn every frame; defaults to 0 until the first render.
    pub last_preview_view_h: u16,
    pub last_diff_view_h: u16,
    pub last_commit_detail_view_h: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedFile {
    pub path: String,
    pub is_staged: bool,
}

impl App {
    pub fn new(theme: Theme) -> Self {
        // Fold pre-1.0 unprefixed keys (`layout=`, `mode=`) and the retired
        // `~/.config/reef/git.prefs` into the current prefixed namespace
        // BEFORE any `prefs::get` runs. Order matters: `load_prefs` below
        // reads `diff.layout` / `diff.mode`, and the `GitStatusState` /
        // `CommitDetailState` initializers read `status.*` / `commit.*` —
        // all of those keys only exist after the migrator has run on a
        // legacy install.
        crate::prefs::migrate_legacy_prefs();

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
            last_rendered_tree_selected: None,
            preview_scroll: 0,
            preview_h_scroll: 0,
            split_percent: 30,
            dragging_split: false,
            hit_registry: HitTestRegistry::new(),
            hover_row: None,
            hover_col: None,
            last_click: None,
            git_status: GitStatusState {
                tree_mode: crate::prefs::get_bool("status.tree_mode"),
                ..GitStatusState::default()
            },
            git_graph: GitGraphState::default(),
            commit_detail: CommitDetailState {
                diff_layout: match crate::prefs::get("commit.diff_layout").as_deref() {
                    Some("side_by_side") => DiffLayout::SideBySide,
                    _ => DiffLayout::Unified,
                },
                diff_mode: match crate::prefs::get("commit.diff_mode").as_deref() {
                    Some("full_file") => DiffMode::FullFile,
                    _ => DiffMode::Compact,
                },
                files_tree_mode: crate::prefs::get_bool("commit.files_tree_mode"),
                ..CommitDetailState::default()
            },
            toasts: Vec::new(),
            push_in_flight: false,
            push_rx: None,
            fs_watcher_rx,
            should_quit: false,
            select_mode: false,
            show_help: false,
            pending_edit: None,
            theme,
            search: crate::search::SearchState::default(),
            quick_open: crate::quick_open::QuickOpenState::from_prefs(),
            space_leader_at: None,
            last_preview_view_h: 0,
            last_diff_view_h: 0,
            last_commit_detail_view_h: 0,
        };
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
        // if the file has just moved between sections (e.g. an fs-watcher
        // refresh after an external `git add`).
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
                let new_content =
                    file_tree::load_preview(&self.file_tree.root, &entry.path, self.theme.is_dark);
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
        let path = self
            .commit_detail
            .file_diff
            .as_ref()
            .map(|(p, _)| p.clone());
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
        self.git_graph.selected_commit =
            self.git_graph.rows.get(next).map(|r| r.commit.oid.clone());
        // Reset commit-detail scroll so the new commit starts at the top.
        self.commit_detail.scroll = 0;
        self.load_commit_detail();
    }

    /// Kick off a `git push` in the background. Returns immediately; the
    /// result is collected in `App::tick` when the worker thread posts it
    /// back through `push_rx`. If a push is already in flight the new
    /// request is dropped — we don't want two pushes racing on the same
    /// refs. UI surfaces the in-flight state via `self.push_in_flight`.
    pub fn run_push(&mut self, force: bool) {
        if self.push_in_flight {
            return;
        }
        let Some(workdir) = self.repo.as_ref().and_then(|r| r.workdir_path()) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.push_rx = Some(rx);
        self.push_in_flight = true;
        std::thread::spawn(move || {
            let result = crate::git::push_at(&workdir, force);
            // Recv side may have been dropped by the time we finish (e.g.
            // user quit mid-push); ignore the send error.
            let _ = tx.send((force, result));
        });
    }

    /// Called from `tick()`. If the push worker has posted a result, fold
    /// it into App state (toast + push_error banner + graph-cache bust +
    /// status refresh) and drop the channel. If the worker dropped its
    /// sender without posting (panic, etc.), release the in-flight flag
    /// and surface an error toast so the user can try again.
    fn drain_push_result(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = self.push_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((force, result)) => {
                self.push_in_flight = false;
                self.push_rx = None;
                match result {
                    Ok(()) => {
                        use crate::i18n::{Msg, t};
                        self.git_status.push_error = None;
                        self.toasts.push(Toast::info(if force {
                            t(Msg::ForcePushSuccess)
                        } else {
                            t(Msg::PushSuccess)
                        }));
                    }
                    Err(e) => {
                        self.git_status.push_error = Some(e.clone());
                        self.toasts
                            .push(Toast::error(crate::i18n::push_failed_toast(&e)));
                    }
                }
                // Push advances remote-tracking refs — invalidate the graph
                // cache so Tab::Graph rebuilds on its next render.
                self.git_graph.cache_key = None;
                self.refresh_status();
            }
            Err(TryRecvError::Empty) => {
                // Worker still running. Check again next tick.
            }
            Err(TryRecvError::Disconnected) => {
                // Worker dropped the sender without sending — the only way
                // this happens is a panic inside the thread (push_at
                // itself always sends). Recover so the user can retry.
                self.push_in_flight = false;
                self.push_rx = None;
                self.toasts.push(Toast::error(crate::i18n::t(
                    crate::i18n::Msg::PushThreadCrashed,
                )));
            }
        }
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
                // Try each panel's dispatcher in turn. Unknown commands are
                // silently dropped — no external handler to fall through to.
                if crate::ui::git_status_panel::handle_command(self, &command, &args) {
                    return;
                }
                if crate::ui::git_graph_panel::handle_command(self, &command, &args) {
                    return;
                }
                let _ = crate::ui::commit_detail_panel::handle_command(self, &command, &args);
            }
            // Quick-open palette clicks are dispatched inline by
            // `quick_open::handle_mouse` (single-click select, double-click
            // accept) rather than routed through `handle_action`, because
            // the double-click distinction needs `last_click` timing that's
            // only available at the input layer. This arm is unreachable
            // under normal flow but keeps the match exhaustive.
            ClickAction::QuickOpenSelect(_) => {}
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
        // Defer `load_diff()` to main.rs after the event-drain loop so rapid
        // key repeats coalesce into a single diff load.
        self.selected_file = Some(SelectedFile {
            path,
            is_staged: staged,
        });
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
    }

    /// Called every frame: drain fs-watcher events and the push worker's
    /// result channel, refreshing caches on any change. Does NOT invalidate
    /// `git_graph.cache_key` on fs events — working-tree edits don't move
    /// HEAD or refs, so the commit graph stays valid (see plan pitfall #2).
    /// Push completion handles its own cache_key bust separately.
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
            // Mark the quick-open index stale so the next palette open picks up
            // the new/deleted files. Rebuilding immediately on every fs
            // event would be wasteful for a palette the user may not open.
            crate::quick_open::mark_stale(&mut self.quick_open);
        }

        self.drain_push_result();
    }
}

// ─── Prefs persistence ────────────────────────────────────────────────────────

/// Load the Git tab's diff layout + mode from the unified prefs file.
/// Keys are `diff.layout` and `diff.mode`; missing keys fall back to
/// defaults. `migrate_legacy_prefs` runs first in `App::new` so any old
/// unprefixed `layout=` / `mode=` entries have been renamed by the time
/// we get here.
fn load_prefs() -> (DiffLayout, DiffMode) {
    let layout = match crate::prefs::get("diff.layout").as_deref() {
        Some("side_by_side") => DiffLayout::SideBySide,
        _ => DiffLayout::Unified,
    };
    let mode = match crate::prefs::get("diff.mode").as_deref() {
        Some("full_file") => DiffMode::FullFile,
        _ => DiffMode::Compact,
    };
    (layout, mode)
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
    crate::prefs::set(
        "diff.layout",
        match layout {
            DiffLayout::Unified => "unified",
            DiffLayout::SideBySide => "side_by_side",
        },
    );
    crate::prefs::set(
        "diff.mode",
        match mode {
            DiffMode::Compact => "compact",
            DiffMode::FullFile => "full_file",
        },
    );
}
