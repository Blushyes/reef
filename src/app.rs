use crate::backend::{Backend, LocalBackend};
use crate::file_tree::{FileTree, PreviewContent};
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, DiffContent, FileEntry, GitRepo, RefLabel};
use crate::tasks::{AsyncState, TaskCoordinator, WorkerResult};
use crate::ui::highlight::StyledToken;
use crate::ui::mouse::{ClickAction, HitTestRegistry};
use crate::ui::theme::Theme;
use crate::ui::toast::Toast;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
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

/// What the user is about to discard when the confirmation banner is up.
/// `File` is the original single-file ↺ flow; `Folder` covers the tree-mode
/// per-directory button; `Section` covers the header-level "discard all"
/// button for the staged or unstaged list. Staged targets get reset to
/// HEAD (unstage + restore); unstaged targets just restore from the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardTarget {
    File(String),
    Folder { is_staged: bool, path: String },
    Section { is_staged: bool },
}

/// State for the inline Git status sidebar.
#[derive(Debug, Default)]
pub struct GitStatusState {
    pub tree_mode: bool,
    pub collapsed_dirs: HashSet<String>,
    pub confirm_discard: Option<DiscardTarget>,
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

/// Syntect tokens for one line of a diff. `Arc` so the render pipeline can
/// pass them through `tokens_for` / pairing state without per-frame deep
/// clones (commit_detail's `build_rows` rebuilds every frame; on 10k-line
/// diffs this was 10k vec-of-String clones per keystroke).
pub type LineTokens = Arc<Vec<StyledToken>>;

/// Highlighted diff tokens: `out[hunk][line]` holds the syntect-colored
/// tokens for the line at that position in `DiffContent.hunks[h].lines[l]`.
/// `None` means the file's extension / name didn't resolve a syntax; rendering
/// falls back to plain per-tag colors.
pub type DiffHighlighted = Vec<Vec<LineTokens>>;

/// A diff plus its optional syntax-highlighted tokens. Used for the Git-tab
/// working/staged diff (no path needed — the selected file is tracked
/// elsewhere) where `CommitFileDiff` would be overkill.
#[derive(Debug, Clone)]
pub struct HighlightedDiff {
    pub diff: DiffContent,
    pub highlighted: Option<DiffHighlighted>,
}

/// A loaded commit-file diff plus its optional syntax-highlighted tokens.
/// Kept at the app/UI layer (not in `src/git`) so the git module stays free
/// of ratatui types (the SBS/Unified renderers own all styling).
#[derive(Debug, Clone)]
pub struct CommitFileDiff {
    pub path: String,
    pub diff: DiffContent,
    pub highlighted: Option<DiffHighlighted>,
}

/// State for the inline commit-detail editor panel (Tab::Graph right side).
#[derive(Debug)]
pub struct CommitDetailState {
    pub detail: Option<CommitDetail>,
    pub file_diff: Option<CommitFileDiff>,
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
    /// Horizontal scroll for Unified layout. Shared across the header /
    /// files list / diff rows — the panel renders as a single list, and
    /// `clip_spans` applies this offset uniformly per row. SBS uses the
    /// two fields below instead (each half scrolls independently).
    pub diff_h_scroll: usize,
    /// Horizontal scroll for the SBS left half (old version). Independent
    /// of the right half and of `diff_h_scroll` — switching diff layouts
    /// preserves all three.
    pub sbs_left_h_scroll: usize,
    /// Horizontal scroll for the SBS right half (new version).
    pub sbs_right_h_scroll: usize,
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
            diff_h_scroll: 0,
            sbs_left_h_scroll: 0,
            sbs_right_h_scroll: 0,
        }
    }
}

pub struct App {
    /// The active backend — LocalBackend for `reef` invoked normally, or
    /// RemoteBackend when `main.rs` passes `--agent-exec`. Kept behind
    /// `Arc<dyn Backend>` so workers can cheaply clone a handle.
    pub backend: Arc<dyn Backend>,
    /// Legacy cached `GitRepo` handle — used by the synchronous
    /// stage/unstage/restore/push paths in `App` that predate the backend
    /// trait. `None` when cwd is not a git repo or when the active backend
    /// is remote (no local `git2` handle available). New code should go
    /// through `self.backend` instead.
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
    pub diff_content: Option<HighlightedDiff>,
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

    /// Ctrl+O hosts picker overlay. Driven by the outer `'session:` loop
    /// in `main.rs` — picking a host populates `pending_ssh_target` and
    /// sets `should_quit_session`, the main loop then tears down the
    /// current App and rebuilds it with the new backend.
    pub hosts_picker: crate::hosts_picker::HostsPickerState,

    /// Populated by the hosts picker on confirm. `main.rs` inspects this
    /// after `should_quit_session` fires and uses it to build the next
    /// `RemoteBackend`. Cleared once consumed.
    pub pending_ssh_target: Option<crate::hosts_picker::SshTarget>,

    /// Set by the hosts picker (via `request_session_swap`) to ask
    /// `main.rs` to exit the current loop body and start a new one with
    /// a fresh backend. Distinct from `should_quit` so the outer loop
    /// can tell "quit reef" from "switch connection".
    pub should_quit_session: bool,

    /// Row-scoped highlight to apply in the Files-tab file preview — set by
    /// `global_search::accept` right before it kicks off an async preview
    /// load, consumed when that preview arrives (for scroll centering) and
    /// cleared when the active preview path changes. Rendered by
    /// `ui::file_preview_panel` alongside the in-panel `/` search highlight.
    pub preview_highlight: Option<PreviewHighlight>,

    /// VSCode-style drag-and-drop destination picker. While `place_mode.active`,
    /// input is routed exclusively to `input::handle_key` / `handle_mouse`
    /// place-mode branches (see `crate::place_mode`).
    pub place_mode: crate::place_mode::PlaceModeState,

    /// Inline editor for the Files-tab tree — VSCode-style new file /
    /// new folder / rename prompt. While `tree_edit.active`, input
    /// dispatch fully owns the keyboard (typing goes into the buffer,
    /// Enter commits, Esc cancels).
    pub tree_edit: crate::tree_edit::TreeEditState,

    /// Right-click context menu for the Files tab tree. Also takes
    /// full input ownership while visible.
    pub tree_context_menu: crate::tree_context_menu::ContextMenuState,

    /// Pending Move-to-Trash / Hard-Delete confirmation. The status
    /// bar takes over with `⚠ Delete foo? (y / Esc)` while this is
    /// `Some`. Cleared on confirm or cancel.
    pub tree_delete_confirm: Option<TreeDeletePending>,

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
    /// Tracks the in-flight drag-and-drop copy kicked off from place mode.
    /// Used to drop stale results (the user could cancel + re-enter place
    /// mode before a long directory copy finishes) and to show a "copying…"
    /// hint if the operation takes long enough to notice.
    pub file_copy_load: AsyncState,
    /// Tracks any in-flight CreateFile / CreateFolder / Rename / Trash
    /// / HardDelete. Used to drop stale generations and to prevent
    /// rage-click / re-commit from stacking requests on the worker.
    pub fs_mutation_load: AsyncState,
    /// Path to auto-select after the next FsMutation completes. Set
    /// by `commit_tree_edit` to the new file / folder / renamed path
    /// so the tree rebuild after the mutation lands with the new
    /// entry highlighted (matches VSCode: create/rename → new row is
    /// selected). Consumed by `apply_worker_result::FsMutation`.
    /// `None` for delete operations — selecting a just-trashed path
    /// would be nonsense.
    pub fs_mutation_select_on_done: Option<PathBuf>,
    next_git_revalidate_at: Instant,
    next_graph_revalidate_at: Instant,
}

/// What the user is about to delete once they confirm the status-bar
/// prompt. `hard` distinguishes Shift+Delete (permanent) from the
/// default Delete (Trash).
#[derive(Debug, Clone)]
pub struct TreeDeletePending {
    pub path: PathBuf,
    pub display_name: String,
    pub is_dir: bool,
    pub hard: bool,
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
    /// Local-backend entry point. Keeps the pre-Backend signature so the
    /// existing integration tests (`tests/ui_snapshots.rs`,
    /// `tests/app_error_paths.rs`) stay byte-for-byte unchanged.
    pub fn new(theme: Theme) -> Self {
        let backend = Arc::new(
            LocalBackend::open_cwd().unwrap_or_else(|_| LocalBackend::open_at(PathBuf::from("."))),
        );
        Self::new_with_backend(theme, backend)
    }

    /// Backend-aware entry point. `main.rs` picks the backend (Local vs
    /// Remote) before calling this; everything inside `App` only sees the
    /// trait object.
    pub fn new_with_backend(theme: Theme, backend: Arc<dyn Backend>) -> Self {
        // Fold pre-1.0 unprefixed keys (`layout=`, `mode=`) and the retired
        // `~/.config/reef/git.prefs` into the current prefixed namespace
        // BEFORE any `prefs::get` runs. Order matters: `load_prefs` below
        // reads `diff.layout` / `diff.mode`, and the `GitStatusState` /
        // `CommitDetailState` initializers read `status.*` / `commit.*` —
        // all of those keys only exist after the migrator has run on a
        // legacy install.
        crate::prefs::migrate_legacy_prefs();

        // `repo` is kept for the legacy stage/unstage/restore/push paths in
        // `App` (and for back-compat with existing tests that assert
        // `app.repo.is_none()`). It mirrors the backend's repo view — when
        // the backend is local it reflects cwd; when it's remote we have
        // no local git handle at all.
        let workdir = backend.workdir_path();
        let repo = GitRepo::open_at(&workdir).ok();
        let workdir_name = backend.workdir_name();
        let branch_name = backend.branch_name();
        let file_tree = FileTree::new(&workdir);
        let fs_watcher_rx = Some(backend.subscribe_fs_events());
        let (saved_layout, saved_mode) = load_prefs();
        let tasks = TaskCoordinator::new();
        let now = Instant::now();
        let mut app = Self {
            backend,
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
            hosts_picker: crate::hosts_picker::HostsPickerState::default(),
            pending_ssh_target: None,
            should_quit_session: false,
            preview_highlight: None,
            place_mode: crate::place_mode::PlaceModeState::default(),
            tree_edit: crate::tree_edit::TreeEditState::default(),
            tree_context_menu: crate::tree_context_menu::ContextMenuState::default(),
            tree_delete_confirm: None,
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
            file_copy_load: AsyncState::default(),
            fs_mutation_load: AsyncState::default(),
            fs_mutation_select_on_done: None,
            next_git_revalidate_at: now + Duration::from_millis(800),
            next_graph_revalidate_at: now + Duration::from_millis(1200),
        };
        app.refresh_status();
        app.refresh_file_tree();
        app
    }

    pub fn refresh_status(&mut self) {
        if !self.backend.has_repo() {
            return;
        };
        let generation = self.git_status_load.begin();
        self.tasks
            .refresh_status(generation, Arc::clone(&self.backend));
    }

    /// Enter the drag-and-drop destination picker. Switches to the Files
    /// tab so the user can see the tree they're about to drop into, then
    /// stores the sources for the banner + eventual copy. Called from
    /// `input::handle_paste` when a paste payload resolves to existing
    /// on-disk paths.
    ///
    /// Refuses the transition in two situations that would otherwise
    /// leave the user stranded:
    ///
    /// - `select_mode` is active — mouse capture is off in that mode,
    ///   so the user would have no way to click a drop target. The
    ///   toast points them at the `v` escape hatch.
    /// - a place-mode copy is already in flight — overwriting
    ///   `sources` would invalidate the worker's generation and the
    ///   previous copy's completion result (including the success
    ///   toast and tree refresh) would be silently dropped.
    pub fn enter_place_mode(&mut self, sources: Vec<PathBuf>) {
        if sources.is_empty() {
            return;
        }
        if self.select_mode {
            self.toasts
                .push(Toast::warn(crate::i18n::place_mode_blocked_by_select_mode()));
            return;
        }
        if self.file_copy_load.loading {
            self.toasts.push(Toast::warn(
                crate::i18n::place_mode_blocked_by_in_flight_copy(),
            ));
            return;
        }
        // Close any competing modal UI so place mode is the single
        // source of truth. Without this, a drop during a quick-open
        // palette session would leave both modal flags true: the
        // palette keeps owning keyboard input (priority-ordered above
        // place mode in `handle_key`), the search prompt would still
        // commandeer the status bar instead of the PLACE badge, and
        // the user would need two Esc presses to fully unwind.
        self.quick_open.active = false;
        if self.search.active {
            crate::search::exit_cancel(self);
        }
        self.show_help = false;
        // Also drop any Files-tab tree modals — otherwise the user
        // would be in place mode (render path switches) AND still
        // carry a half-typed tree_edit buffer invisibly, or still
        // have a pending delete confirm taking over the status bar.
        self.tree_edit.clear();
        self.tree_context_menu.close();
        self.tree_delete_confirm = None;
        self.set_active_tab(Tab::Files);
        self.place_mode.active = true;
        self.place_mode.sources = sources;
    }

    /// Leave place mode without copying — Esc, right-click, or a click on a
    /// non-droppable area all land here.
    pub fn exit_place_mode(&mut self) {
        self.place_mode.active = false;
        self.place_mode.sources.clear();
    }

    /// Kick off the async copy into `dest_dir`. Takes `self.place_mode.sources`
    /// by clone so the state can be cleared by the caller if it chooses to —
    /// but in normal flow we keep sources around until the worker result
    /// arrives so the banner stays visible while copying.
    ///
    /// De-duped against in-flight copies: a rage-click on a second folder
    /// before the first copy returns would otherwise `begin()` a new
    /// generation and invalidate the prior one — the first copy still
    /// runs on disk but its completion toast / tree refresh never fire.
    /// `enter_place_mode` has the same guard for paste-level entry; this
    /// handles the mouse-level commit path.
    pub fn request_file_copy(&mut self, sources: Vec<PathBuf>, dest_dir: PathBuf) {
        if self.file_copy_load.loading {
            self.toasts.push(Toast::warn(
                crate::i18n::place_mode_blocked_by_in_flight_copy(),
            ));
            return;
        }
        // External drag-drop onto a remote workdir is handled by
        // `backend.upload_from_local` (scp under the hood) inside the
        // worker — no UI guard needed. Intra-tree copies (sources all
        // under the workdir) go through `backend.copy_file` /
        // `copy_dir_recursive` on the agent side.
        let generation = self.file_copy_load.begin();
        self.tasks
            .copy_files(generation, Arc::clone(&self.backend), sources, dest_dir);
    }

    // ── Files-tab tree actions: New File / New Folder / Rename / Delete ──

    /// Open the inline editor for a new file / new folder under
    /// `parent_dir`, or a rename of `rename_target`. Closes any competing
    /// modal first so place-mode / context-menu / delete-confirm don't
    /// fight with the editor for input ownership.
    ///
    /// `anchor_idx` is the visible-row index the editable row will
    /// render under (the parent folder for creates, the target entry
    /// itself for rename). `None` means the edit row attaches to the
    /// top of the tree — used when creating at project root.
    pub fn begin_tree_edit(
        &mut self,
        mode: crate::tree_edit::TreeEditMode,
        parent_dir: PathBuf,
        rename_target: Option<PathBuf>,
        anchor_idx: Option<usize>,
    ) {
        if self.fs_mutation_load.loading {
            self.toasts
                .push(Toast::warn(crate::i18n::tree_op_blocked_by_in_flight()));
            return;
        }
        // Close competing modals so tree-edit owns the screen.
        self.tree_context_menu.close();
        self.tree_delete_confirm = None;
        self.exit_place_mode();
        self.set_active_tab(Tab::Files);

        let buffer = match &rename_target {
            Some(p) => p
                .file_name()
                .and_then(|n| n.to_str())
                .map(String::from)
                .unwrap_or_default(),
            None => String::new(),
        };
        let cursor = buffer.len();
        self.tree_edit = crate::tree_edit::TreeEditState {
            active: true,
            mode: Some(mode),
            parent_dir: Some(parent_dir),
            rename_target,
            buffer,
            cursor,
            anchor_idx,
            error: None,
        };
    }

    /// Validate `tree_edit.buffer` and kick off the matching worker
    /// task. On validation failure we set `tree_edit.error` and stay
    /// active so the user can fix the name.
    pub fn commit_tree_edit(&mut self) {
        // Critical race guard: a previous commit might still be
        // in-flight (worker not done). Without this, a second Enter
        // press would `fs_mutation_load.begin()` again → the older
        // generation's result arrives, gen-mismatches, gets silently
        // dropped — the earlier file DID get created on disk but the
        // user sees no toast for it; the second CreateFile then fails
        // with EEXIST and fires an error toast. Data integrity is
        // fine, but the UX is outright wrong.
        if self.fs_mutation_load.loading {
            return;
        }
        let Some(mode) = self.tree_edit.mode else {
            self.tree_edit.clear();
            return;
        };
        let Some(parent_dir) = self.tree_edit.parent_dir.clone() else {
            self.tree_edit.clear();
            return;
        };
        let name = match crate::tree_edit::validate_basename(&self.tree_edit.buffer) {
            Ok(n) => n,
            Err(err) => {
                self.tree_edit.error = Some(err);
                return;
            }
        };
        let target_path = parent_dir.join(&name);

        // Collision check runs at commit (not render) because it needs
        // a syscall — keeps typing cheap.
        //
        // Rename's "new == old" is fine (no-op, close the editor).
        if let Some(old) = &self.tree_edit.rename_target {
            if old == &target_path {
                self.tree_edit.clear();
                return;
            }
        }
        if target_path.exists() {
            self.tree_edit.error = Some(crate::tree_edit::TreeEditError::NameAlreadyExists(name));
            return;
        }

        let generation = self.fs_mutation_load.begin();
        // Remember where to land selection after the worker comes back.
        // `refresh_file_tree_with_target` wants a workdir-relative path
        // (that's the shape `TreeEntry::path` carries), so strip the
        // absolute prefix here. Outside-of-workdir paths shouldn't be
        // possible in practice, but if they slip through we just fall
        // back to the existing selection at refresh time.
        self.fs_mutation_select_on_done = target_path
            .strip_prefix(&self.file_tree.root)
            .ok()
            .map(|p| p.to_path_buf());
        // `Backend` write methods take workdir-relative paths (that's the
        // same shape the wire protocol uses, so a remote backend can ship
        // the call over the socket without re-encoding). Fall back to the
        // absolute path on the rare "outside workdir" case so the local
        // backend still does the right thing.
        let new_rel = target_path
            .strip_prefix(&self.file_tree.root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| target_path.clone());
        let display_new = name.clone();
        match mode {
            crate::tree_edit::TreeEditMode::NewFile => {
                self.tasks
                    .create_file(generation, Arc::clone(&self.backend), new_rel, display_new);
            }
            crate::tree_edit::TreeEditMode::NewFolder => {
                self.tasks.create_folder(
                    generation,
                    Arc::clone(&self.backend),
                    new_rel,
                    display_new,
                );
            }
            crate::tree_edit::TreeEditMode::Rename => {
                let Some(old) = self.tree_edit.rename_target.clone() else {
                    self.tree_edit.clear();
                    self.fs_mutation_select_on_done = None;
                    return;
                };
                let old_rel = old
                    .strip_prefix(&self.file_tree.root)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| old.clone());
                let old_name = old
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(String::from)
                    .unwrap_or_else(|| old.to_string_lossy().to_string());
                self.tasks.rename_path(
                    generation,
                    Arc::clone(&self.backend),
                    old_rel,
                    new_rel,
                    old_name,
                    display_new,
                );
            }
        }
        // Keep state until the worker result arrives — the render
        // loop then sees `fs_mutation_load.loading` to disable the
        // input briefly. `apply_worker_result` clears `tree_edit` on
        // success.
    }

    pub fn cancel_tree_edit(&mut self) {
        self.tree_edit.clear();
    }

    /// Right-click opened a context menu over `target_entry_idx`
    /// (or None for a click that missed all rows). `anchor` is the
    /// mouse column/row in screen cells; the renderer will clamp
    /// to the viewport.
    pub fn open_tree_context_menu(&mut self, target_entry_idx: Option<usize>, anchor: (u16, u16)) {
        if self.place_mode.active || self.tree_edit.active {
            return;
        }
        // NOTE: we deliberately do NOT move `file_tree.selected` to the
        // right-clicked row. The menu carries its own `target_entry_idx`
        // so Rename / Delete / etc. know what to operate on; leaving
        // selection alone matches VSCode's Explorer (right-click never
        // moves the selection highlight) and — critically — stops the
        // underlying row's `selection_bg` from stretching across the
        // full width and visually fighting with the popup.
        self.tree_context_menu.open(anchor, target_entry_idx);
    }

    pub fn close_tree_context_menu(&mut self) {
        self.tree_context_menu.close();
    }

    /// Translate a picked `ContextMenuItem` into the corresponding
    /// App action. Called from `input` when the user clicks / keys
    /// on a menu row.
    pub fn dispatch_context_menu_item(&mut self, item: crate::tree_context_menu::ContextMenuItem) {
        use crate::tree_context_menu::ContextMenuItem as I;
        let target_idx = self.tree_context_menu.target_entry_idx;
        self.tree_context_menu.close();
        match item {
            I::NewFile => {
                let (parent, anchor) = self.resolve_create_anchor(target_idx);
                self.begin_tree_edit(
                    crate::tree_edit::TreeEditMode::NewFile,
                    parent,
                    None,
                    anchor,
                );
            }
            I::NewFolder => {
                let (parent, anchor) = self.resolve_create_anchor(target_idx);
                self.begin_tree_edit(
                    crate::tree_edit::TreeEditMode::NewFolder,
                    parent,
                    None,
                    anchor,
                );
            }
            I::Rename => {
                let Some(idx) = target_idx else { return };
                let Some(entry) = self.file_tree.entries.get(idx).cloned() else {
                    return;
                };
                let abs = self.file_tree.root.join(&entry.path);
                let parent = abs
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.file_tree.root.clone());
                self.begin_tree_edit(
                    crate::tree_edit::TreeEditMode::Rename,
                    parent,
                    Some(abs),
                    Some(idx),
                );
            }
            I::Delete => {
                let Some(idx) = target_idx else { return };
                let Some(entry) = self.file_tree.entries.get(idx).cloned() else {
                    return;
                };
                let abs = self.file_tree.root.join(&entry.path);
                self.prompt_tree_delete(abs, entry.is_dir, /*hard=*/ false);
            }
            I::RevealInFinder => {
                // Reveal-in-Finder opens the LOCAL file manager; over ssh
                // the target path doesn't exist on this machine, so the
                // action is always wrong. Guard at the caller layer so
                // the user gets a clear "not supported" toast instead of
                // "file not found" from the platform command.
                if self.backend.is_remote() {
                    self.toasts.push(Toast::warn(
                        "Reveal in Finder is not supported on remote workdirs",
                    ));
                    return;
                }
                let path = match target_idx {
                    Some(idx) => self
                        .file_tree
                        .entries
                        .get(idx)
                        .map(|e| self.file_tree.root.join(&e.path))
                        .unwrap_or_else(|| self.file_tree.root.clone()),
                    None => self.file_tree.root.clone(),
                };
                if let Err(msg) = crate::reveal::reveal_in_finder(&path) {
                    // Platforms we don't support get the unsupported toast
                    // instead of the raw error — it's a cleaner UX hint.
                    let text = if msg.contains("not supported") {
                        crate::i18n::tree_reveal_unsupported_platform()
                    } else {
                        msg
                    };
                    self.toasts.push(Toast::error(text));
                }
            }
        }
    }

    /// Given the entry the user clicked (or `None` for empty-space),
    /// pick the parent directory the new file/folder should land in,
    /// plus the visible row index the editable row anchors under.
    ///
    /// Rules:
    /// - Clicked on a folder → create INSIDE that folder. Auto-expands
    ///   the folder first if it's currently collapsed so the edit row
    ///   is actually visible.
    /// - Clicked on a file → create as a SIBLING (under the file's
    ///   parent folder). Anchor at the file's own row — good enough;
    ///   the render-side insertion logic handles this cleanly.
    /// - Clicked on empty space / None → create at project root.
    fn resolve_create_anchor(
        &mut self,
        target_entry_idx: Option<usize>,
    ) -> (PathBuf, Option<usize>) {
        let Some(idx) = target_entry_idx else {
            return (self.file_tree.root.clone(), None);
        };
        let Some(entry) = self.file_tree.entries.get(idx).cloned() else {
            return (self.file_tree.root.clone(), None);
        };
        if entry.is_dir {
            let abs = self.file_tree.root.join(&entry.path);
            // Auto-expand collapsed folder so the editable child row
            // actually renders. The refresh is async; `anchor_idx` will
            // remain valid in the meantime (the existing folder row
            // doesn't move), and the edit row renders right after it
            // regardless of expansion state because it's keyed on
            // anchor_idx, not on the children's indices.
            if !entry.is_expanded {
                self.file_tree.toggle_expand(idx);
                self.refresh_file_tree_with_target(self.file_tree.selected_path());
            }
            (abs, Some(idx))
        } else {
            // File → create next to it. The file's parent is the
            // clicked entry's parent on disk.
            let abs = self.file_tree.root.join(&entry.path);
            let parent = abs
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.file_tree.root.clone());
            (parent, Some(idx))
        }
    }

    /// Pop the status-bar delete-confirm prompt. `hard` controls
    /// Trash vs. `fs::remove_*`; the prompt text adjusts accordingly.
    pub fn prompt_tree_delete(&mut self, path: PathBuf, is_dir: bool, hard: bool) {
        let display_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(crate::tree_edit::sanitize_filename)
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        self.tree_context_menu.close();
        self.tree_delete_confirm = Some(TreeDeletePending {
            path,
            display_name,
            is_dir,
            hard,
        });
    }

    /// User pressed Y on the delete confirm. Dispatches the matching
    /// worker task and clears the prompt.
    pub fn confirm_tree_delete(&mut self) {
        // Same generation-bump race as commit_tree_edit: a previous
        // trash/hard-delete might still be running. Keep the confirm
        // in place (don't `.take()`) so the user's Y press isn't
        // lost — they can retry when the prior op completes.
        if self.fs_mutation_load.loading {
            self.toasts
                .push(Toast::warn(crate::i18n::tree_op_blocked_by_in_flight()));
            return;
        }
        let Some(pending) = self.tree_delete_confirm.take() else {
            return;
        };
        let generation = self.fs_mutation_load.begin();
        // Convert the (absolute) selection path to a workdir-relative
        // PathBuf for the Backend call. The UI still stores `abs` because
        // it came from `file_tree.root.join(entry.path)` — the display
        // name is derived before we lose the absolute form.
        let first_name = pending.display_name.clone();
        let rel = pending
            .path
            .strip_prefix(&self.file_tree.root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| pending.path.clone());
        if pending.hard {
            self.tasks.hard_delete_paths(
                generation,
                Arc::clone(&self.backend),
                vec![rel],
                first_name,
            );
        } else {
            self.tasks
                .trash_paths(generation, Arc::clone(&self.backend), vec![rel], first_name);
        }
    }

    pub fn cancel_tree_delete(&mut self) {
        self.tree_delete_confirm = None;
    }

    // ── Hosts picker (Ctrl+O) ────────────────────────────────────────────

    /// Open the hosts picker overlay, seeding it from the current user's
    /// `~/.ssh/config` plus the persisted recent-targets list. Errors
    /// reading the config aren't fatal — we show an empty picker so the
    /// user can still switch via the path-input mode.
    pub fn open_hosts_picker(&mut self) {
        let parsed = crate::hosts::parse_ssh_config().unwrap_or_default();
        let recent = crate::hosts_picker::load_recent();
        self.hosts_picker.open(parsed, recent);
    }

    /// Close the picker without connecting.
    pub fn close_hosts_picker(&mut self) {
        self.hosts_picker.close();
    }

    /// Commit the picker's current selection. On success, stash the
    /// target for `main.rs` to consume and flip the session-swap flag —
    /// we don't build the new backend here because the outer loop owns
    /// the terminal teardown/setup dance around the connect.
    pub fn confirm_hosts_picker(&mut self) {
        let Some(target) = self.hosts_picker.confirm() else {
            return;
        };
        // Persist the chosen target to the recents list before handing
        // control back to `main.rs` — even if the subsequent connect
        // fails, the user probably still wants it surfaced next time.
        let mut current = crate::hosts_picker::load_recent();
        current = crate::hosts_picker::bump_recent(current, target.clone());
        crate::hosts_picker::save_recent(&current);

        self.hosts_picker.close();
        self.pending_ssh_target = Some(target);
        self.should_quit_session = true;
    }

    /// Collapse every expanded folder and async-refresh the tree so
    /// the render path picks up the shorter row list.
    pub fn collapse_all_tree_entries(&mut self) {
        self.file_tree.collapse_all();
        let selected_path = self.file_tree.selected_path();
        self.refresh_file_tree_with_target(selected_path);
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
            Arc::clone(&self.backend),
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
            Arc::clone(&self.backend),
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
        if !self.backend.has_repo() {
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
            Arc::clone(&self.backend),
            sel.path,
            sel.is_staged,
            context,
            self.theme.is_dark,
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
        let ok = self.backend.stage(path).is_ok();
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
        let ok = self.backend.unstage(path).is_ok();
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
            let _ = self.backend.stage(p);
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
            let _ = self.backend.unstage(p);
        }
        if let Some(ref mut sel) = self.selected_file {
            if paths.iter().any(|p| p == &sel.path) {
                sel.is_staged = false;
            }
        }
        self.refresh_status();
        self.load_diff();
    }

    /// Apply the currently-pending discard target. Clears the confirmation
    /// banner, drops the selection if the discarded path(s) include it,
    /// then refreshes status + diff.
    ///
    /// Semantics by target:
    /// * `File` — restore a single unstaged file to its HEAD state (existing
    ///   ↺ behaviour).
    /// * `Folder { is_staged }` — for every file currently listed under that
    ///   directory prefix, do a section-flavoured revert (see `Section`).
    /// * `Section { is_staged }` — for every file in the section: if staged,
    ///   unstage then restore workdir to HEAD (full revert); if unstaged,
    ///   restore workdir to index.
    pub fn confirm_discard(&mut self) {
        let Some(target) = self.git_status.confirm_discard.take() else {
            return;
        };
        let discarded_paths = self.apply_discard_target(&target);
        if let Some(sel) = self.selected_file.as_ref() {
            if discarded_paths.contains(&sel.path) {
                self.selected_file = None;
                self.diff_content = None;
            }
        }
        self.refresh_status();
        self.load_diff();
    }

    fn apply_discard_target(&mut self, target: &DiscardTarget) -> HashSet<String> {
        let mut touched: HashSet<String> = HashSet::new();
        // Post-M4: discard goes through `backend.revert_path` so
        // RemoteBackend gets the same folder/section semantics as local.
        // We ignore errors (matches the pre-M4 `let _ = repo.…` pattern):
        // the refresh_status + load_diff that follow will reflect whatever
        // actually landed on disk, and a partial failure on one path in a
        // folder discard shouldn't block the rest.
        match target {
            DiscardTarget::File(path) => {
                let _ = self.backend.revert_path(path, /*is_staged=*/ false);
                touched.insert(path.clone());
            }
            DiscardTarget::Folder { is_staged, path } => {
                let source: Vec<String> = if *is_staged {
                    self.staged_files.iter().map(|f| f.path.clone()).collect()
                } else {
                    self.unstaged_files.iter().map(|f| f.path.clone()).collect()
                };
                for p in source {
                    if folder_contains(path, &p) {
                        let _ = self.backend.revert_path(&p, *is_staged);
                        touched.insert(p);
                    }
                }
            }
            DiscardTarget::Section { is_staged } => {
                let source: Vec<String> = if *is_staged {
                    self.staged_files.iter().map(|f| f.path.clone()).collect()
                } else {
                    self.unstaged_files.iter().map(|f| f.path.clone()).collect()
                };
                for p in source {
                    let _ = self.backend.revert_path(&p, *is_staged);
                    touched.insert(p);
                }
            }
        }
        touched
    }

    /// Rebuild the commit graph iff HEAD or any ref moved since the last build.
    /// Working-tree fs events do NOT invalidate the cache — see plan pitfall #2.
    pub fn refresh_graph(&mut self) {
        const GRAPH_COMMIT_LIMIT: usize = 500;
        if !self.backend.has_repo() {
            self.git_graph.rows.clear();
            self.git_graph.ref_map.clear();
            self.git_graph.cache_key = None;
            return;
        };
        let generation = self.graph_load.begin();
        self.tasks
            .refresh_graph(generation, Arc::clone(&self.backend), GRAPH_COMMIT_LIMIT);
    }

    /// (Re)load commit detail for the currently-selected commit. Clears detail
    /// and any previously-selected file diff whenever the target changes.
    pub fn load_commit_detail(&mut self) {
        self.commit_detail.file_diff = None;
        // Different commit → different content; reset all three h_scrolls so
        // the panel starts at the left edge. Keeps the scrollbar out of
        // "offset that only made sense for the prior commit" states.
        self.commit_detail.diff_h_scroll = 0;
        self.commit_detail.sbs_left_h_scroll = 0;
        self.commit_detail.sbs_right_h_scroll = 0;
        let Some(oid) = self.git_graph.selected_commit.clone() else {
            self.commit_detail.detail = None;
            return;
        };
        if !self.backend.has_repo() {
            self.commit_detail.detail = None;
            return;
        }
        let generation = self.commit_detail_load.begin();
        self.tasks
            .load_commit_detail(generation, Arc::clone(&self.backend), oid);
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
        if !self.backend.has_repo() {
            self.commit_detail.file_diff = None;
            return;
        }
        // Different file → reset h_scrolls so the new diff starts at the
        // left edge. Same-path reload (e.g. after toggling diff_mode) keeps
        // scroll state so the user doesn't lose their place.
        let is_new_file = self
            .commit_detail
            .file_diff
            .as_ref()
            .map(|d| d.path.as_str() != path)
            .unwrap_or(true);
        if is_new_file {
            self.commit_detail.diff_h_scroll = 0;
            self.commit_detail.sbs_left_h_scroll = 0;
            self.commit_detail.sbs_right_h_scroll = 0;
        }
        let generation = self.commit_file_diff_load.begin();
        self.tasks.load_commit_file_diff(
            generation,
            Arc::clone(&self.backend),
            oid,
            path.to_string(),
            context,
            self.theme.is_dark,
        );
    }

    /// Reload the currently-selected commit-file diff — used after toggling
    /// `commit.diff_mode`, which changes the context-lines argument.
    pub fn reload_commit_file_diff(&mut self) {
        let path = self
            .commit_detail
            .file_diff
            .as_ref()
            .map(|d| d.path.clone());
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
        if !self.backend.has_repo() {
            return;
        }
        let backend = Arc::clone(&self.backend);
        let (tx, rx) = mpsc::channel();
        self.push_rx = Some(rx);
        self.push_in_flight = true;
        std::thread::spawn(move || {
            let result = backend.push(force).map_err(|e| e.to_string());
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
                        // Re-validate tree_edit's anchor against the
                        // freshly-replaced entries. fs-watcher bounces
                        // (an external save, git operation, etc.)
                        // can reshape the tree while the user is
                        // mid-edit; without this guard the edit row
                        // either renders in the wrong spot or falls
                        // off the end.
                        if self.tree_edit.active {
                            let len = self.file_tree.entries.len();
                            if let Some(idx) = self.tree_edit.anchor_idx {
                                // Rename needs a stricter check than
                                // Create: a tree shift that keeps the
                                // idx in-range but swaps the entry
                                // underneath it leaves the edit row
                                // visually attached to the wrong file.
                                // Commit would then try to rename a
                                // still-existing `rename_target` path
                                // that the user can no longer see, and
                                // fail with ENOENT if the original was
                                // also renamed externally. Detect the
                                // mismatch by comparing the row's
                                // current absolute path against
                                // `rename_target`.
                                let stale = match self.tree_edit.mode {
                                    Some(crate::tree_edit::TreeEditMode::Rename) => {
                                        let current = self
                                            .file_tree
                                            .entries
                                            .get(idx)
                                            .map(|e| self.file_tree.root.join(&e.path));
                                        current.as_ref() != self.tree_edit.rename_target.as_ref()
                                    }
                                    _ => idx >= len,
                                };
                                if stale {
                                    match self.tree_edit.mode {
                                        Some(crate::tree_edit::TreeEditMode::Rename) => {
                                            // Can't synthesise a valid
                                            // rename anchor if the target
                                            // entry moved or is gone.
                                            // Cancel; the user can redo
                                            // F2 after they orient.
                                            self.tree_edit.clear();
                                        }
                                        _ => {
                                            // Create: degrade to
                                            // create-at-root so the typed
                                            // buffer stays visible.
                                            self.tree_edit.anchor_idx = None;
                                            self.tree_edit.parent_dir =
                                                Some(self.file_tree.root.clone());
                                        }
                                    }
                                }
                            }
                        }
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
            WorkerResult::FileCopy { generation, result } => match result {
                Ok(count) => {
                    if self.file_copy_load.complete_ok(generation) {
                        self.toasts
                            .push(Toast::info(crate::i18n::place_mode_copied(count)));
                        self.exit_place_mode();
                        // The fs-watcher will eventually notice, but refresh
                        // synchronously so the user sees their newly-placed
                        // files immediately.
                        self.refresh_file_tree();
                    }
                }
                Err(error) => {
                    if self.file_copy_load.complete_err(generation, error.clone()) {
                        self.toasts
                            .push(Toast::error(crate::i18n::place_mode_copy_failed(&error)));
                        self.exit_place_mode();
                        // `complete_err` sets stale=true + error=Some so
                        // `should_request()` would re-fire, and
                        // `activity_message` would surface "copy error:
                        // …" in the status bar indefinitely after the
                        // toast is gone. The error has already been
                        // surfaced; clear the flags so the status bar
                        // goes back to normal on the next frame.
                        self.file_copy_load.stale = false;
                        self.file_copy_load.error = None;
                    }
                }
            },
            WorkerResult::FsMutation {
                generation,
                kind,
                result,
            } => match result {
                Ok(()) => {
                    if self.fs_mutation_load.complete_ok(generation) {
                        let toast = crate::i18n::fs_mutation_success_toast(&kind);
                        self.toasts.push(Toast::info(toast));
                        // Inline edit / delete confirm are no longer relevant
                        // after a successful mutation — clean up before the
                        // tree rebuild runs so the renderer doesn't briefly
                        // show a stale cursor on a row that's about to move.
                        self.tree_edit.clear();
                        self.tree_delete_confirm = None;
                        // Select the newly-created / renamed entry if we
                        // stashed one on dispatch. Delete paths leave
                        // `fs_mutation_select_on_done` as `None` so the
                        // tree keeps its current selection.
                        let target = self.fs_mutation_select_on_done.take();
                        if target.is_some() {
                            self.refresh_file_tree_with_target(target);
                        } else {
                            self.refresh_file_tree();
                        }
                    }
                }
                Err(error) => {
                    if self
                        .fs_mutation_load
                        .complete_err(generation, error.clone())
                    {
                        let toast = crate::i18n::fs_mutation_error_toast(&kind, &error);
                        self.toasts.push(Toast::error(toast));
                        // Leave `tree_edit` alone on error so the user can
                        // fix the buffer and retry. Same as the drag-drop
                        // path: clear stale/error flags so activity_message
                        // doesn't double-surface the toast after it fades.
                        self.tree_delete_confirm = None;
                        // Drop the pending auto-select — the target path
                        // was never created / renamed, so trying to focus
                        // it would be a stale lookup at best.
                        self.fs_mutation_select_on_done = None;
                        self.fs_mutation_load.stale = false;
                        self.fs_mutation_load.error = None;
                    }
                }
            },
        }
    }

    pub fn set_active_tab(&mut self, tab: Tab) {
        if self.active_tab == tab {
            return;
        }
        let was_files = self.active_tab == Tab::Files;
        self.active_tab = tab;
        // Leaving the Files tab cancels any Files-tab-scoped modal —
        // tree edit row, context menu, delete confirm. Those modals
        // are invisible on other tabs, so leaving them armed would
        // let a stray key or click fire them from a tab where the
        // corresponding file tree isn't even being rendered.
        if was_files {
            self.tree_edit.clear();
            self.tree_context_menu.close();
            self.tree_delete_confirm = None;
        }
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
            Tab::Files => from_state("copy", &self.file_copy_load)
                .or_else(|| from_state("files", &self.file_tree_load))
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
            ClickAction::PlaceModeFolder(index) => {
                // Confirm a place-mode drop onto a specific folder. Resolve
                // the entry's absolute path and hand off to the worker.
                // Stale indices (e.g. the tree rebuilt out from under us)
                // or accidental clicks on non-directory rows fall back to a
                // cancel — safer than silently dropping to an unrelated
                // destination.
                let dest = self.file_tree.entries.get(index).and_then(|entry| {
                    if entry.is_dir {
                        Some(self.file_tree.root.join(&entry.path))
                    } else {
                        None
                    }
                });
                match dest {
                    Some(dest_dir) => {
                        let sources = self.place_mode.sources.clone();
                        self.request_file_copy(sources, dest_dir);
                    }
                    None => self.exit_place_mode(),
                }
            }
            ClickAction::PlaceModeRoot => {
                let sources = self.place_mode.sources.clone();
                let dest_dir = self.file_tree.root.clone();
                self.request_file_copy(sources, dest_dir);
            }
            ClickAction::FileTreeToolbarNewFile => {
                let target = self.toolbar_create_target();
                let (parent, anchor) = self.resolve_create_anchor(target);
                self.begin_tree_edit(
                    crate::tree_edit::TreeEditMode::NewFile,
                    parent,
                    None,
                    anchor,
                );
            }
            ClickAction::FileTreeToolbarNewFolder => {
                let target = self.toolbar_create_target();
                let (parent, anchor) = self.resolve_create_anchor(target);
                self.begin_tree_edit(
                    crate::tree_edit::TreeEditMode::NewFolder,
                    parent,
                    None,
                    anchor,
                );
            }
            ClickAction::FileTreeToolbarRefresh => {
                self.refresh_file_tree();
                self.refresh_status();
            }
            ClickAction::FileTreeToolbarCollapse => {
                self.collapse_all_tree_entries();
            }
            ClickAction::TreeContextMenuItem(item) => {
                self.dispatch_context_menu_item(item);
            }
            ClickAction::TreeContextMenuClose => {
                self.close_tree_context_menu();
            }
            ClickAction::HostsPickerSelect(idx) => {
                // Mouse click on a hosts-picker row: move selection to
                // that row and (for paths that already have a target)
                // commit. The picker's own keyboard path goes through a
                // different method, so here we just re-use `move_selection`
                // by computing the delta.
                let current = self.hosts_picker.selected_idx;
                let delta = idx as i32 - current as i32;
                self.hosts_picker.move_selection(delta);
                // Enter path-mode immediately so user can type /path and
                // hit Enter — matches the overlay's keyboard UX.
                self.hosts_picker.enter_path_mode();
            }
            ClickAction::TreeClearSelection => {
                // Left-click on empty tree space → drop the selection
                // highlight. Next toolbar `+ File` / `+ Folder` lands
                // at the project root. Any in-progress inline edit is
                // also cancelled, matching VSCode's "click elsewhere
                // discards the pending name" behaviour.
                self.file_tree.clear_selection();
                if self.tree_edit.active {
                    self.tree_edit.clear();
                }
            }
        }
    }

    /// Pick the "create anchor" target for a toolbar `+ File` / `+ Folder`
    /// click. Uses the current tree selection; falls back to `None`
    /// (= project root) when the user has explicitly cleared it or the
    /// tree is empty.
    ///
    /// `resolve_create_anchor` then handles the folder-vs-file split —
    /// selection on a folder creates INSIDE, selection on a file creates
    /// as a sibling.
    fn toolbar_create_target(&self) -> Option<usize> {
        let sel = self.file_tree.selected;
        if self.file_tree.entries.get(sel).is_some() {
            Some(sel)
        } else {
            None
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
        self.tick_place_mode_auto_expand();
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
            Arc::clone(&self.backend),
            self.global_search.query.clone(),
        );
    }

    /// VSCode-style hover auto-expand. When the cursor rests on a
    /// collapsed folder for `HOVER_EXPAND_DELAY`, expand it so the user
    /// can keep drilling into deep targets without round-tripping through
    /// a click. Render writes the hover tracker; we just check the
    /// timer here.
    ///
    /// Guarded on `file_tree_load.loading` so a slow tree rebuild doesn't
    /// re-fire the expand on every tick — we'd otherwise pile up tree
    /// rebuild generations until the worker caught up.
    fn tick_place_mode_auto_expand(&mut self) {
        if !self.place_mode.active {
            return;
        }
        if self.file_tree_load.loading {
            return;
        }
        let Some(idx) = self.place_mode.auto_expand_due(Instant::now()) else {
            return;
        };
        let should_expand = self
            .file_tree
            .entries
            .get(idx)
            .map(|e| e.is_dir && !e.is_expanded)
            .unwrap_or(false);
        // Clear the timer regardless of whether we expand — hovering on
        // an already-expanded folder shouldn't keep re-firing every
        // frame, and leaving the timestamp set would do exactly that.
        self.place_mode.hover_since = None;
        if !should_expand {
            return;
        }
        self.file_tree.toggle_expand(idx);
        let selected_path = self.file_tree.selected_path();
        self.refresh_file_tree_with_target(selected_path);
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
                let has_repo = self.backend.has_repo();
                let should_poll_git = has_repo && now >= self.next_git_revalidate_at;
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
                let has_repo = self.backend.has_repo();
                let should_poll_graph = has_repo && now >= self.next_graph_revalidate_at;
                if self.graph_load.should_request()
                    || (has_repo && self.git_graph.rows.is_empty() && !self.graph_load.loading)
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

// ─── Discard helpers ──────────────────────────────────────────────────────────

/// True when `file_path` lives under the directory at `folder_path` (direct
/// child or deeper). Tolerates a trailing slash on `folder_path` and handles
/// the edge case where `file_path` *is* `folder_path` — that should never
/// happen from UI-driven targets but keeps the Folder discard flow safe if
/// a caller constructs one programmatically.
fn folder_contains(folder_path: &str, file_path: &str) -> bool {
    let prefix = format!("{}/", folder_path.trim_end_matches('/'));
    file_path == folder_path || file_path.starts_with(&prefix)
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

#[cfg(test)]
mod tests {
    use super::folder_contains;

    #[test]
    fn folder_contains_direct_child() {
        assert!(folder_contains("src/ui", "src/ui/a.rs"));
    }

    #[test]
    fn folder_contains_nested_child() {
        assert!(folder_contains("src", "src/ui/panels/git.rs"));
    }

    #[test]
    fn folder_contains_does_not_eat_sibling_prefix() {
        // The classic "src/ui" vs "src/ui-helper.rs" bug — naive prefix
        // match without the trailing slash would misfire here.
        assert!(!folder_contains("src/ui", "src/ui-helper.rs"));
    }

    #[test]
    fn folder_contains_exact_path_match() {
        // Defensive: DiscardTarget::Folder with a file path still reverts
        // that one file instead of silently doing nothing.
        assert!(folder_contains("src/main.rs", "src/main.rs"));
    }

    #[test]
    fn folder_contains_rejects_unrelated_path() {
        assert!(!folder_contains("src/ui", "tests/foo.rs"));
    }

    #[test]
    fn folder_contains_tolerates_trailing_slash() {
        assert!(folder_contains("src/ui/", "src/ui/a.rs"));
    }

    #[test]
    fn folder_contains_empty_path_is_noop() {
        // The sidebar never builds a `Folder { path: "" }` target — the
        // tree walk always starts inside a named section — so we don't
        // need empty-prefix semantics. Document the actual behavior:
        // the synthetic "/" prefix won't match any normal file path,
        // which makes an empty target a safe no-op rather than a
        // "revert everything" footgun.
        assert!(!folder_contains("", "anything.rs"));
    }
}
