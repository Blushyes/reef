use crate::file_tree::{FileTree, PreviewContent};
use crate::fs_watcher;
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, GitRepo, RefLabel};
use crate::tasks::{AsyncState, TaskCoordinator, WorkerResult};
use crate::ui::mouse::{ClickAction, HitTestRegistry};
use crate::ui::theme::Theme;
use crate::ui::toast::Toast;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Git,
    Files,
    Graph,
    /// Persistent global-search view. Shares `app.global_search` state with
    /// the Space+F overlay — picking up a running query seamlessly when the
    /// user pins the overlay via Alt/Ctrl+Enter, or starts fresh by
    /// switching in via digit key / Tab cycle.
    Search,
}

impl Tab {
    /// Canonical ordering shared by the tab bar renderer and the digit
    /// shortcut. Order mirrors VSCode's Activity Bar (Files → Search → …)
    /// so Search sits adjacent to Files, where it belongs mentally.
    pub const ALL: &'static [Tab] = &[Tab::Files, Tab::Search, Tab::Git, Tab::Graph];

    pub fn label(self) -> &'static str {
        use crate::i18n::{Msg, t};
        match self {
            Tab::Files => t(Msg::TabFiles),
            Tab::Search => t(Msg::TabSearch),
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
    pub ahead_behind: Option<(usize, usize)>,
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
    pub workdir_name: String,
    pub branch_name: String,

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

    /// VSCode-style global-search (Ctrl+Shift+F) palette. While `active`,
    /// input is routed exclusively to `crate::global_search::handle_key`.
    pub global_search: crate::global_search::GlobalSearchState,

    /// Row-scoped highlight to apply in the Files-tab file preview — set by
    /// `global_search::accept` right before it kicks off an async preview
    /// load, consumed when that preview arrives (for scroll centering) and
    /// cleared when the active preview path changes. Rendered by
    /// `ui::file_preview_panel` alongside the in-panel `/` search highlight.
    pub preview_highlight: Option<PreviewHighlight>,

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

    // ── Background work state ──
    pub tasks: TaskCoordinator,
    pub file_tree_load: AsyncState,
    pub preview_load: AsyncState,
    pub git_status_load: AsyncState,
    pub diff_load: AsyncState,
    pub graph_load: AsyncState,
    pub commit_detail_load: AsyncState,
    pub commit_file_diff_load: AsyncState,
    /// Tracks generation + loading for the streaming global-search worker.
    /// Unlike other workers (one request → one `WorkerResult`), global
    /// search emits many `GlobalSearchChunk`s and a terminating
    /// `GlobalSearchDone`; we use `begin()` at kickoff, plain generation
    /// comparisons on each chunk, and `complete_ok` on `Done`.
    pub global_search_load: AsyncState,
    next_git_revalidate_at: Instant,
    next_graph_revalidate_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedFile {
    pub path: String,
    pub is_staged: bool,
}

/// Row-scoped preview highlight carried from `global_search::accept()` to
/// the `file_preview_panel` renderer. Survives the async preview round-trip
/// so the match row gets highlighted the frame the preview lands. Cleared
/// whenever the active preview path no longer matches `path`.
#[derive(Debug, Clone)]
pub struct PreviewHighlight {
    pub path: std::path::PathBuf,
    pub row: usize,
    pub byte_range: std::ops::Range<usize>,
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
        let workdir_name = repo
            .as_ref()
            .map(|r| r.workdir_name())
            .or_else(|| {
                workdir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| "repo".to_string());
        let branch_name = repo.as_ref().map(|r| r.branch_name()).unwrap_or_default();
        let file_tree = FileTree::new(&workdir);
        let fs_watcher_rx = Some(fs_watcher::spawn(workdir.clone()));
        let (saved_layout, saved_mode) = load_prefs();
        let tasks = TaskCoordinator::new();
        let now = Instant::now();
        let mut app = Self {
            repo,
            workdir_name,
            branch_name,
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
            global_search: crate::global_search::GlobalSearchState::default(),
            preview_highlight: None,
            space_leader_at: None,
            last_preview_view_h: 0,
            last_diff_view_h: 0,
            last_commit_detail_view_h: 0,
            tasks,
            file_tree_load: AsyncState::default(),
            preview_load: AsyncState::default(),
            git_status_load: AsyncState::default(),
            diff_load: AsyncState::default(),
            graph_load: AsyncState::default(),
            commit_detail_load: AsyncState::default(),
            commit_file_diff_load: AsyncState::default(),
            global_search_load: AsyncState::default(),
            next_git_revalidate_at: now + Duration::from_millis(800),
            next_graph_revalidate_at: now + Duration::from_millis(1200),
        };
        app.refresh_status();
        app
    }

    pub fn refresh_status(&mut self) {
        if self.repo.is_none() {
            return;
        };
        let generation = self.git_status_load.begin();
        self.tasks
            .refresh_status(generation, self.file_tree.root.clone());
    }

    /// Rebuild the file tree from disk, applying git decorations when a repo is open.
    /// Safe to call on any workdir — `refresh_status` handles repo/no-repo internally.
    pub fn refresh_file_tree(&mut self) {
        self.refresh_file_tree_with_target(self.file_tree.selected_path());
    }

    pub fn refresh_file_tree_with_target(&mut self, selected_path: Option<PathBuf>) {
        let generation = self.file_tree_load.begin();
        self.tasks.rebuild_tree(
            generation,
            self.file_tree.root.clone(),
            self.file_tree.expanded_paths(),
            self.file_tree.git_statuses(),
            selected_path,
            self.file_tree.selected,
        );
    }

    pub fn load_preview(&mut self) {
        if let Some(entry) = self.file_tree.selected_entry() {
            if !entry.is_dir {
                self.load_preview_for_path(entry.path.clone());
            }
        }
    }

    pub fn load_preview_for_path(&mut self, rel_path: PathBuf) {
        // Drop any global-search highlight that points at a different file.
        // `global_search::accept` sets the highlight AND calls this with the
        // target path, so a matching path leaves the highlight intact; a
        // user-driven file switch (navigate_files etc.) clears it.
        if let Some(hl) = self.preview_highlight.as_ref() {
            if hl.path != rel_path {
                self.preview_highlight = None;
            }
        }
        let generation = self.preview_load.begin();
        self.tasks.load_preview(
            generation,
            self.file_tree.root.clone(),
            rel_path,
            self.theme.is_dark,
        );
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
        let Some(sel) = self.selected_file.clone() else {
            self.diff_content = None;
            return;
        };
        if self.repo.is_none() {
            self.diff_content = None;
            return;
        }
        let context = match self.diff_mode {
            DiffMode::FullFile => 9999,
            DiffMode::Compact => 3,
        };
        let generation = self.diff_load.begin();
        self.tasks.load_diff(
            generation,
            self.file_tree.root.clone(),
            sel.path,
            sel.is_staged,
            context,
        );
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
        if self.repo.is_none() {
            self.git_graph.rows.clear();
            self.git_graph.ref_map.clear();
            self.git_graph.cache_key = None;
            return;
        };
        let generation = self.graph_load.begin();
        self.tasks
            .refresh_graph(generation, self.file_tree.root.clone(), GRAPH_COMMIT_LIMIT);
    }

    /// (Re)load commit detail for the currently-selected commit. Clears detail
    /// and any previously-selected file diff whenever the target changes.
    pub fn load_commit_detail(&mut self) {
        self.commit_detail.file_diff = None;
        let Some(oid) = self.git_graph.selected_commit.clone() else {
            self.commit_detail.detail = None;
            return;
        };
        if self.repo.is_none() {
            self.commit_detail.detail = None;
            return;
        }
        let generation = self.commit_detail_load.begin();
        self.tasks
            .load_commit_detail(generation, self.file_tree.root.clone(), oid);
    }

    /// Load the inline diff for a file inside the currently-selected commit.
    pub fn load_commit_file_diff(&mut self, path: &str) {
        let context = match self.commit_detail.diff_mode {
            DiffMode::Compact => 3,
            DiffMode::FullFile => 9999,
        };
        let Some(oid) = self.git_graph.selected_commit.clone() else {
            self.commit_detail.file_diff = None;
            return;
        };
        if self.repo.is_none() {
            self.commit_detail.file_diff = None;
            return;
        }
        let generation = self.commit_file_diff_load.begin();
        self.tasks.load_commit_file_diff(
            generation,
            self.file_tree.root.clone(),
            oid,
            path.to_string(),
            context,
        );
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
                // Push advances remote-tracking refs — mark git/graph data
                // stale so the coordinator refreshes it off the render path.
                self.git_graph.cache_key = None;
                self.git_status_load.mark_stale();
                self.graph_load.mark_stale();
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

    fn drain_task_results(&mut self) {
        use std::sync::mpsc::TryRecvError;
        loop {
            match self.tasks.try_recv() {
                Ok(result) => self.apply_worker_result(result),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn apply_worker_result(&mut self, result: WorkerResult) {
        match result {
            WorkerResult::FileTree { generation, result } => match result {
                Ok(payload) => {
                    if self.file_tree_load.complete_ok(generation) {
                        let before = self.file_tree.selected_path();
                        self.file_tree
                            .replace_entries(payload.entries, payload.selected_idx);
                        self.file_tree
                            .refresh_git_statuses(&self.staged_files, &self.unstaged_files);
                        if before != self.file_tree.selected_path() {
                            self.load_preview();
                        }
                    }
                }
                Err(error) => {
                    self.file_tree_load.complete_err(generation, error);
                }
            },
            WorkerResult::Preview { generation, result } => match result {
                Ok(content) => {
                    if self.preview_load.complete_ok(generation) {
                        let same_file = matches!(
                            (self.preview_content.as_ref(), content.as_ref()),
                            (Some(old), Some(new)) if old.file_path == new.file_path
                        );
                        self.preview_content = content;
                        if !same_file {
                            self.preview_scroll = 0;
                            self.preview_h_scroll = 0;
                        }
                        // If `global_search::accept` stashed a highlight for
                        // this file, re-center once the preview actually
                        // lands. `load_preview_for_path` runs async, so the
                        // scroll has to happen here — setting it inside
                        // `accept()` before the preview exists wouldn't know
                        // the final line count / view height.
                        if let (Some(hl), Some(preview)) = (
                            self.preview_highlight.as_ref(),
                            self.preview_content.as_ref(),
                        ) {
                            if preview.file_path == hl.path.to_string_lossy() {
                                let view_h = self.last_preview_view_h as usize;
                                self.preview_scroll = crate::search::center_scroll(hl.row, view_h);
                            }
                        }
                    }
                }
                Err(error) => {
                    self.preview_load.complete_err(generation, error);
                }
            },
            WorkerResult::GitStatus { generation, result } => match result {
                Ok(payload) => {
                    if self.git_status_load.complete_ok(generation) {
                        let before = self.selected_file.clone();
                        self.staged_files = payload.staged;
                        self.unstaged_files = payload.unstaged;
                        self.git_status.ahead_behind = payload.ahead_behind;
                        self.branch_name = payload.branch_name;

                        self.file_tree
                            .refresh_git_statuses(&self.staged_files, &self.unstaged_files);

                        if let Some(ref mut sel) = self.selected_file {
                            let in_staged = self.staged_files.iter().any(|f| f.path == sel.path);
                            let in_unstaged =
                                self.unstaged_files.iter().any(|f| f.path == sel.path);
                            if in_staged {
                                sel.is_staged = true;
                            } else if in_unstaged {
                                sel.is_staged = false;
                            } else {
                                self.selected_file = None;
                                self.diff_content = None;
                            }
                        }
                        if before != self.selected_file {
                            self.load_diff();
                        }
                    }
                }
                Err(error) => {
                    self.git_status_load.complete_err(generation, error);
                }
            },
            WorkerResult::Diff { generation, result } => match result {
                Ok(diff) => {
                    if self.diff_load.complete_ok(generation) {
                        self.diff_content = diff;
                    }
                }
                Err(error) => {
                    self.diff_load.complete_err(generation, error);
                }
            },
            WorkerResult::Graph { generation, result } => match result {
                Ok(payload) => {
                    if self.graph_load.complete_ok(generation) {
                        let previous_commit = self.git_graph.selected_commit.clone();
                        self.git_graph.rows = payload.rows;
                        self.git_graph.ref_map = payload.ref_map;
                        self.git_graph.cache_key = Some(payload.cache_key);

                        if let Some(ref oid) = previous_commit {
                            if let Some(idx) = self
                                .git_graph
                                .rows
                                .iter()
                                .position(|r| r.commit.oid == *oid)
                            {
                                self.git_graph.selected_idx = idx;
                            }
                        }
                        if self.git_graph.selected_idx >= self.git_graph.rows.len() {
                            self.git_graph.selected_idx =
                                self.git_graph.rows.len().saturating_sub(1);
                        }
                        self.git_graph.selected_commit = self
                            .git_graph
                            .rows
                            .get(self.git_graph.selected_idx)
                            .map(|r| r.commit.oid.clone());

                        if self.git_graph.selected_commit != previous_commit
                            || self.commit_detail.detail.is_none()
                        {
                            self.load_commit_detail();
                        }
                    }
                }
                Err(error) => {
                    self.graph_load.complete_err(generation, error);
                }
            },
            WorkerResult::CommitDetail { generation, result } => match result {
                Ok(detail) => {
                    if self.commit_detail_load.complete_ok(generation) {
                        self.commit_detail.detail = detail;
                    }
                }
                Err(error) => {
                    self.commit_detail_load.complete_err(generation, error);
                }
            },
            WorkerResult::CommitFileDiff { generation, result } => match result {
                Ok(file_diff) => {
                    if self.commit_file_diff_load.complete_ok(generation) {
                        self.commit_detail.file_diff = file_diff;
                    }
                }
                Err(error) => {
                    self.commit_file_diff_load.complete_err(generation, error);
                }
            },
            WorkerResult::GlobalSearchChunk { generation, hits } => {
                // Intermediate event — compare generation manually since
                // AsyncState only has a `complete_ok` helper for terminal
                // results. Leaves `loading=true` while chunks keep arriving.
                if generation == self.global_search_load.generation {
                    self.global_search.results.extend(hits);
                    // Keep same-file hits adjacent. We could maintain this
                    // invariant incrementally since the walker emits files
                    // in directory order, but a per-chunk sort is cheap and
                    // defends against any future parallelisation.
                    self.global_search
                        .results
                        .sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
                    // Chunk arrival can rotate which hit is at `selected`
                    // (typical: query changed, results reset to 0, and the
                    // new top hit differs from the previous preview). Sync
                    // the right panel lazily — only reload when stale, so
                    // streaming a bunch of chunks for one file doesn't
                    // thrash the preview worker.
                    self.sync_search_preview_if_stale();
                }
            }
            WorkerResult::GlobalSearchDone {
                generation,
                truncated,
            } => {
                // Terminal event — `complete_ok` flips loading off and
                // returns false if superseded (then we skip the truncation
                // update too, since the whole result set belongs to an
                // older generation).
                if self.global_search_load.complete_ok(generation) {
                    self.global_search.truncated = truncated;
                    // Zero results: clear the hit-scoped highlight so the
                    // right panel's current preview isn't misleadingly
                    // decorated with a line bar from the previous query.
                    if self.global_search.results.is_empty() && self.active_tab == Tab::Search {
                        self.preview_highlight = None;
                    }
                }
            }
        }
    }

    pub fn set_active_tab(&mut self, tab: Tab) {
        if self.active_tab == tab {
            return;
        }
        self.active_tab = tab;
        match tab {
            Tab::Git => self.git_status_load.mark_stale(),
            Tab::Graph => self.graph_load.mark_stale(),
            Tab::Files => {
                if self.file_tree.entries.is_empty() {
                    self.file_tree_load.mark_stale();
                }
            }
            // Search has no background fetch to mark stale, but we do need
            // to resync the right panel's preview — `preview_highlight`
            // may have been cleared by a file-tree navigation in some
            // other tab, leaving the Search tab's preview pointing at the
            // wrong file.
            Tab::Search => {
                self.sync_search_preview_if_stale();
            }
        }
    }

    pub fn activity_message(&self) -> Option<String> {
        fn from_state(label: &str, state: &AsyncState) -> Option<String> {
            if state.loading {
                Some(format!("{label} refreshing…"))
            } else if let Some(error) = state.error.as_ref() {
                Some(format!("{label} error: {error}"))
            } else if state.stale {
                Some(format!("{label} stale"))
            } else {
                None
            }
        }

        match self.active_tab {
            Tab::Files => from_state("files", &self.file_tree_load)
                .or_else(|| from_state("preview", &self.preview_load)),
            Tab::Git => from_state("git", &self.git_status_load)
                .or_else(|| from_state("diff", &self.diff_load)),
            Tab::Graph => from_state("graph", &self.graph_load)
                .or_else(|| from_state("commit", &self.commit_detail_load))
                .or_else(|| from_state("commit diff", &self.commit_file_diff_load)),
            // Search activity is surfaced in the tab's own footer (`N / M ·
            // scanning…`), not in the global status bar.
            Tab::Search => {
                if self.global_search_load.loading {
                    Some("search scanning…".into())
                } else {
                    from_state("preview", &self.preview_load)
                }
            }
        }
    }

    pub fn handle_action(&mut self, action: ClickAction) {
        match action {
            ClickAction::SwitchTab(tab) => {
                self.set_active_tab(tab);
            }
            ClickAction::TreeClick(index) => {
                self.file_tree.selected = index;
                if let Some(entry) = self.file_tree.entries.get(index) {
                    if entry.is_dir {
                        self.file_tree.toggle_expand(index);
                        let selected_path = self.file_tree.selected_path();
                        self.refresh_file_tree_with_target(selected_path);
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
            // Palette clicks are dispatched inline by their respective
            // `handle_mouse` fns (single-click select, double-click accept)
            // rather than routed through `handle_action`, because the
            // double-click distinction needs `last_click` timing that's only
            // available at the input layer.
            ClickAction::QuickOpenSelect(_) => {}
            // Tab::Search result clicks DO route through here — the tab is
            // not an overlay, so input::handle_mouse lets the click fall
            // through to hit_test + handle_action. Update the selection and
            // trigger live preview.
            ClickAction::GlobalSearchSelect(idx) => {
                if self.active_tab == Tab::Search {
                    self.global_search.selected = idx;
                    crate::global_search::navigate_to_selected(self);
                }
                // Overlay case is unreachable via this path — handled inline
                // in `global_search::handle_mouse`.
            }
            ClickAction::GlobalSearchFocusInput => {
                if self.active_tab == Tab::Search {
                    self.global_search.tab_input_focused = true;
                }
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
        self.drain_task_results();

        let mut fs_dirty = false;
        if let Some(rx) = self.fs_watcher_rx.as_ref() {
            while rx.try_recv().is_ok() {
                fs_dirty = true;
            }
        }
        if fs_dirty {
            self.file_tree_load.mark_stale();
            self.preview_load.mark_stale();
            self.diff_load.mark_stale();
            self.git_status_load.mark_stale();
            // Mark the quick-open index stale so the next palette open picks up
            // the new/deleted files. Rebuilding immediately on every fs
            // event would be wasteful for a palette the user may not open.
            crate::quick_open::mark_stale(&mut self.quick_open);
        }

        self.maybe_kick_global_search();
        self.drain_preview_sync_debounce();
        self.drain_push_result();
        self.kick_active_tab_work();
        self.drain_task_results();
    }

    /// Fire a debounced preview-sync if its deadline has elapsed. Scheduled
    /// by `global_search::schedule_preview_sync` (called from keyboard
    /// navigation); coalesces bursts so holding ↓ doesn't spam the preview
    /// worker. Click / chunk-arrival / pin go through `navigate_to_selected`
    /// directly and bypass this.
    fn drain_preview_sync_debounce(&mut self) {
        let Some(t) = self.global_search.preview_sync_at else {
            return;
        };
        if Instant::now() < t {
            return;
        }
        self.global_search.preview_sync_at = None;
        crate::global_search::navigate_to_selected(self);
    }

    /// Reload the Search tab's right-side preview iff the currently-selected
    /// hit no longer matches what `preview_highlight` is pointing at.
    /// Called after every global-search chunk arrives — without this the
    /// right panel goes stale between "user types new query" and "user
    /// presses ↑↓ manually," which looks like a bug.
    ///
    /// Gated on `active_tab == Tab::Search` so the overlay (which doesn't
    /// render a preview) doesn't waste preview-worker cycles. Cheap when a
    /// burst of chunks all point at the same hit — the staleness check
    /// short-circuits.
    fn sync_search_preview_if_stale(&mut self) {
        if self.active_tab != Tab::Search {
            return;
        }
        let Some(hit) = self
            .global_search
            .results
            .get(self.global_search.selected)
            .cloned()
        else {
            return;
        };
        let stale = match &self.preview_highlight {
            Some(hl) => hl.path != hit.path || hl.row != hit.line,
            None => true,
        };
        if stale {
            crate::global_search::navigate_to_selected(self);
        }
    }

    /// Fire a global-search task if the query has changed and the debounce
    /// window has elapsed. Uses `AsyncState::begin()` for the generation
    /// bump + loading flag (same pattern as every other worker); adds a
    /// cooperative `cancel` flag swap since AsyncState doesn't model abort.
    fn maybe_kick_global_search(&mut self) {
        let Some(t) = self.global_search.last_keystroke_at else {
            return;
        };
        if Instant::now().duration_since(t) < crate::global_search::DEBOUNCE {
            return;
        }
        if self.global_search.query == self.global_search.last_searched_query {
            self.global_search.last_keystroke_at = None;
            return;
        }

        // Tell the previous worker (if any) to bail. A fresh Arc makes sure
        // the new task's observation of `cancel` is independent from the
        // flag we just flipped.
        self.global_search
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let new_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.global_search.cancel = new_cancel.clone();

        self.global_search.results.clear();
        self.global_search.truncated = false;
        self.global_search.selected = 0;
        self.global_search.scroll = 0;
        // New query → fresh results → start from smart-view. Leaving a
        // stale h-scroll here would mean the first chunks land already
        // offset, which looks like a bug.
        self.global_search.results_h_scroll = 0;
        self.global_search.last_searched_query = self.global_search.query.clone();
        self.global_search.last_keystroke_at = None;

        if self.global_search.query.is_empty() {
            // No worker to send. Still bump+complete the AsyncState so any
            // late Done from the previous (now-cancelled) worker is dropped
            // via generation mismatch, and `loading` correctly reads false.
            let g = self.global_search_load.begin();
            self.global_search_load.complete_ok(g);
            // Clear the hit-scoped preview highlight too — without results
            // there's nothing to point at, and keeping the old one leaves
            // a ghost band on the right panel's last-loaded file.
            self.preview_highlight = None;
            return;
        }

        let new_gen = self.global_search_load.begin();
        self.tasks.search_all(
            new_gen,
            new_cancel,
            self.file_tree.root.clone(),
            self.global_search.query.clone(),
        );
    }

    fn kick_active_tab_work(&mut self) {
        let now = Instant::now();

        if self.file_tree_load.should_request() {
            self.refresh_file_tree();
        }

        match self.active_tab {
            Tab::Files => {
                if self.preview_load.should_request() {
                    self.load_preview();
                }
            }
            Tab::Git => {
                let should_poll_git = self.repo.is_some() && now >= self.next_git_revalidate_at;
                if self.git_status_load.should_request()
                    || (should_poll_git && !self.git_status_load.loading)
                {
                    self.refresh_status();
                    self.next_git_revalidate_at = now + Duration::from_secs(2);
                }
                if self.diff_load.should_request() {
                    self.load_diff();
                }
            }
            Tab::Graph => {
                let should_poll_graph = self.repo.is_some() && now >= self.next_graph_revalidate_at;
                if self.graph_load.should_request()
                    || (self.repo.is_some()
                        && self.git_graph.rows.is_empty()
                        && !self.graph_load.loading)
                    || (should_poll_graph && !self.graph_load.loading)
                {
                    self.refresh_graph();
                    self.next_graph_revalidate_at = now + Duration::from_secs(5);
                }
                if self.commit_detail_load.should_request() {
                    self.load_commit_detail();
                }
                if self.commit_file_diff_load.should_request() {
                    self.reload_commit_file_diff();
                }
            }
            Tab::Search => {
                // The search worker is kicked by `maybe_kick_global_search`
                // at the top of tick() — only user keystrokes re-run it, not
                // tab activation. Preview is demand-driven by selection
                // changes via `sync_search_preview_if_stale` /
                // `navigate_to_selected`.
                //
                // What we DO handle here: fs-watcher marks `preview_load`
                // stale → reload the currently-selected hit's file. Using
                // `self.load_preview()` would be wrong — it reads
                // `file_tree.selected`, which in the Search tab points at
                // whatever the Files tab was looking at last, not at the
                // current hit.
                if self.preview_load.should_request()
                    && let Some(hit) = self
                        .global_search
                        .results
                        .get(self.global_search.selected)
                        .cloned()
                {
                    self.load_preview_for_path(hit.path);
                }
            }
        }
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
