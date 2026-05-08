use crate::backend::{
    Backend, LocalBackend, RepoDiscoverOpts, WorkspaceRepoMeta, normalize_repo_root_rel,
};
use crate::file_tree::{FileTree, PreviewBody, PreviewContent};
use crate::git::graph::GraphRow;
use crate::git::{CommitDetail, CommitInfo, DiffContent, FileEntry, GitRepo, RefLabel};
use crate::tasks::{AsyncState, TaskCoordinator, WorkerResult};
use crate::ui::highlight::StyledToken;
use crate::ui::mouse::{ClickAction, HitTestRegistry};
use crate::ui::theme::Theme;
use crate::ui::toast::Toast;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Worker-produced `StatefulProtocol` carried back to the main thread
/// so it can be slotted into the current `ThreadProtocol`. The
/// `generation` matches the corresponding `preview_load` request — a
/// mismatch on arrival (user has since selected a different file)
/// means the build is stale and gets dropped.
pub struct BuiltProtocol {
    pub generation: u64,
    pub protocol: ratatui_image::protocol::StatefulProtocol,
}

/// How long a preview request sits in `preview_schedule` before the
/// worker is kicked. 80 ms is below the ~100 ms threshold where users
/// perceive delay but well above the keystroke rate of arrow-repeat,
/// so rapid scrubbing coalesces into a single load.
const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(80);
const SELECTED_GIT_REPO_PREF: &str = "status.selected_repo";

/// Pagination + table-selection state for the SQLite preview card.
/// Lives `Some` for as long as the current `preview_content` is a
/// `PreviewBody::Database`; rebuilt from `info.initial_page` whenever
/// a new preview lands and the file changed (see
/// `apply_worker_result`).
///
/// **Cache invariant**: `col_widths` + `total_table_w` are derived
/// from `(selected_table, current_rows)`. Any mutation of those two
/// fields must call [`Self::recompute_layout`] before the next
/// render, or the cached widths will desync from the data and the
/// table will visually misalign. Every mutation site in this file
/// honors that — when adding new ones, follow suit.
///
/// `current_rows` is the rows shown right now. On `[`/`]`/`PgUp`/`PgDn`
/// we re-issue `Backend::db_load_page` synchronously and replace this
/// vec on success. SQLite's open + LIMIT/OFFSET is sub-millisecond
/// locally; over SSH it's an RPC round-trip (~10-50 ms typical) — a
/// brief stall on flaky links is the accepted trade-off for keeping
/// the navigation path simple.
#[derive(Debug, Clone)]
pub struct DbPreviewState {
    /// Workdir-relative path of the SQLite file the state belongs to.
    /// Compared against `preview_content.file_path` on every render to
    /// catch the "file changed but state didn't get cleared" race.
    pub path: String,
    /// Index into `info.tables`. Bounds-checked at every step.
    pub selected_table: usize,
    /// Zero-based page index. `offset = page * rows_per_page`.
    pub page: u64,
    /// The rows currently visible. Each inner Vec is one row's cells
    /// in column order; length equals
    /// `min(rows_per_page, table.row_count - offset)`.
    pub current_rows: Vec<Vec<reef_sqlite_preview::SqliteValue>>,
    /// Page-size used to compute `offset` on the next request.
    pub rows_per_page: u32,
    /// Cached natural column widths for the current
    /// `(selected_table, current_rows)` combo. Recomputed by
    /// [`Self::recompute_layout`] on every mutation that changes
    /// either input. The render path consults the cache once per
    /// frame instead of re-walking 50 rows × N columns of UTF-8
    /// width math on every keystroke during h-scroll.
    pub col_widths: Vec<usize>,
    /// Cached `Σcol_widths + (n−1)·sep_w` paired with `col_widths`.
    /// Used as the upper bound when clamping `preview_h_scroll`.
    pub total_table_w: usize,
}

/// Navigation actions exposed by the SQLite preview keybindings. Kept
/// as an enum (rather than separate methods) so the input dispatcher
/// stays a single match arm per key — `db_navigate` does the bounds
/// math + the RPC round-trip in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbNav {
    PrevPage,
    NextPage,
    PrevTable,
    NextTable,
    FirstPage,
    LastPage,
}

impl DbPreviewState {
    fn from_initial(path: &str, info: &reef_sqlite_preview::DatabaseInfo) -> Self {
        let mut s = Self {
            path: path.to_string(),
            selected_table: info.selected_table,
            page: 0,
            current_rows: info.initial_page.rows.clone(),
            rows_per_page: crate::file_tree::INITIAL_DB_PAGE_ROWS,
            col_widths: Vec::new(),
            total_table_w: 0,
        };
        s.recompute_layout(info);
        s
    }

    /// Refresh `col_widths` + `total_table_w` from the current
    /// `(selected_table, current_rows)` combo. Cheap-ish on its own
    /// (O(rows × cols × avg_str_len)) but expensive when called per
    /// frame — this method is the seam where we DO compute, so the
    /// render path can stay zero-cost on h-scroll.
    pub fn recompute_layout(&mut self, info: &reef_sqlite_preview::DatabaseInfo) {
        let columns = info
            .tables
            .get(self.selected_table)
            .map(|t| t.columns.as_slice())
            .unwrap_or(&[]);
        self.col_widths = crate::ui::db_preview::natural_column_widths(columns, &self.current_rows);
        self.total_table_w = crate::ui::db_preview::total_table_width(&self.col_widths);
    }
}

/// Largest valid page index for a table at a given page size. Tables
/// with zero rows still have one (empty) page, so we floor at 0
/// rather than letting `(0/N)-1` wrap to `u64::MAX`.
fn max_page_for(table: &reef_sqlite_preview::TableSummary, page_size: u32) -> u64 {
    if page_size == 0 {
        return 0;
    }
    let pages = table.row_count.div_ceil(page_size as u64);
    pages.saturating_sub(1)
}

/// How long we wait after a preview lands before firing neighbor
/// prefetches. Short enough that a user who pauses to look still gets
/// the next step cached; long enough that a user pressing the next
/// key 5-50 ms after a landing never queues prefetches in front of
/// their real `LoadPreview`. `load_preview_for_path` clears the
/// schedule on every keystroke, so rapid scrubbing never fires
/// prefetch at all.
const PREFETCH_DELAY: Duration = Duration::from_millis(300);

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
    /// 左列(或两列布局里的唯一左列)。Git/Files/Search tab 用作 tree /
    /// 文件列表;Graph tab 用作 graph 侧栏。
    Files,
    /// Graph tab 三列布局里的"中间列"——commit 元数据 + 改动文件树。
    /// 只在 Graph tab 三列模式下有效;其他 tab 不会切到这里。
    Commit,
    /// 右列,通常是 diff 或 preview。Graph tab 三列模式下指最右侧
    /// 的 diff 栏;二列模式下是 commit detail(含内联 diff)。
    Diff,
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
    pub branches: Vec<String>,

    // ─── Commit input (VSCode-style "Source Control" message box) ───
    /// Draft commit message buffer. Freeform UTF-8 — newlines are
    /// literal `\n`, which the render layer splits into one row per
    /// line and the commit shell-out passes through on stdin
    /// unchanged.
    pub commit_message: String,
    /// Byte offset of the caret inside `commit_message`. Kept in sync
    /// by the shared `input_edit` helpers so cursor motion matches the
    /// other text inputs in the app (search, quick-open, global-search).
    pub commit_cursor: usize,
    /// `true` while the commit box is focused for typing — gates
    /// character input in `handle_key_git` so `s`/`u`/`d` chords don't
    /// fire when the user is writing a message. Cleared by Esc, by
    /// submitting the commit, or by clicking anywhere outside the input.
    pub commit_editing: bool,
    /// Last `git commit` failure surfaced as an in-panel banner, same
    /// lifetime semantics as `push_error`. Kept out of the toast queue
    /// so the user can read a multi-line hook rejection without it
    /// timing out.
    pub commit_error: Option<String>,
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
    /// Anchor index for Shift-extended range selection. `None` = single-select
    /// mode; `Some(i)` pairs with `selected_idx` as cursor. `i == selected_idx`
    /// is normalised to single-select by `selected_range` / `is_range`.
    /// Cleared on plain nav, refresh, tab-internal search jump, and Esc.
    pub selection_anchor: Option<usize>,
    pub scroll: usize,
    /// `selected_idx` observed on the previous render. Used to distinguish
    /// selection-change follow (bring the selected commit into view) from
    /// user-initiated scroll (leave the viewport alone). Mirrors #13's fix
    /// for the Files tab — without this, mouse-wheel scroll snapped back to
    /// the selected commit on the next tick.
    pub last_rendered_selected: Option<usize>,
}

#[derive(Debug)]
pub struct RepoCatalogState {
    pub repos: Vec<WorkspaceRepoMeta>,
    pub selected_git_repo: Option<PathBuf>,
    pub discover_load: AsyncState,
    pub max_depth: usize,
    pub include_nested: bool,
    pub max_repos: Option<usize>,
    pub truncated: bool,
}

impl Default for RepoCatalogState {
    fn default() -> Self {
        let opts = RepoDiscoverOpts::default();
        Self {
            repos: Vec::new(),
            selected_git_repo: None,
            discover_load: AsyncState::default(),
            max_depth: opts.max_depth,
            include_nested: opts.include_nested,
            max_repos: opts.max_repos,
            truncated: false,
        }
    }
}

impl RepoCatalogState {
    fn from_prefs() -> Self {
        let mut state = Self::default();
        if let Some(raw) = crate::prefs::get(SELECTED_GIT_REPO_PREF)
            && let Ok(repo_root_rel) = normalize_repo_root_rel(Path::new(&raw))
        {
            state.selected_git_repo = Some(repo_root_rel);
        }
        state
    }

    pub fn discover_opts(&self) -> RepoDiscoverOpts {
        RepoDiscoverOpts {
            max_depth: self.max_depth,
            include_nested: self.include_nested,
            max_repos: self.max_repos,
        }
    }

    fn reconcile_selected_repo(&mut self) -> Option<(PathBuf, Option<PathBuf>)> {
        let previous = self.selected_git_repo.clone()?;
        if self.repos.iter().any(|repo| repo.repo_root_rel == previous) {
            return None;
        }

        self.selected_git_repo = match self.repos.as_slice() {
            [only] => Some(only.repo_root_rel.clone()),
            _ => None,
        };
        Some((previous, self.selected_git_repo.clone()))
    }

    fn auto_select_repo(&mut self) {
        if let Some(selected) = self.selected_git_repo.as_ref()
            && self
                .repos
                .iter()
                .any(|repo| &repo.repo_root_rel == selected)
        {
            return;
        }
        self.selected_git_repo = match self.repos.as_slice() {
            [only] => Some(only.repo_root_rel.clone()),
            _ => None,
        };
    }
}

impl GitGraphState {
    /// Inclusive `(lo, hi)` bounds of the current selection. Returns
    /// `(selected_idx, selected_idx)` when there's no anchor or the anchor
    /// has collapsed onto the cursor (= single-select).
    pub fn selected_range(&self) -> (usize, usize) {
        match self.selection_anchor {
            Some(a) if a != self.selected_idx => {
                (a.min(self.selected_idx), a.max(self.selected_idx))
            }
            _ => (self.selected_idx, self.selected_idx),
        }
    }

    pub fn is_range(&self) -> bool {
        matches!(self.selection_anchor, Some(a) if a != self.selected_idx)
    }

    /// Visual mode is armed iff the anchor is set, regardless of whether
    /// the cursor has actually moved away from it. Used for UI affordances
    /// (status bar badge, input dispatch, mouse click semantics) where
    /// "armed but collapsed" and "armed + extended" behave identically;
    /// use `is_range` when a real range of ≥2 commits is required (e.g.
    /// the data loader needs oldest ≠ newest to make `diff_tree_to_tree`
    /// meaningful).
    pub fn in_visual_mode(&self) -> bool {
        self.selection_anchor.is_some()
    }

    /// Linear scan for the row holding `oid`. Hot enough (status bar clicks,
    /// worker-result merges, focus commands all go through here) to warrant
    /// a named helper — without it, `rows.iter().position(|r| r.commit.oid == oid)`
    /// spread across six sites and drifted on `&str` vs `&String` match.
    pub fn find_row_by_oid(&self, oid: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.commit.oid == oid)
    }
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

/// Range-mode summary for the Graph tab's right panel. Shown instead of
/// `CommitDetail` when the user has a Shift-extended range. Files are the
/// net `parent(oldest).tree → newest.tree` delta (matches IntelliJ); the
/// per-commit list is a snapshot of `rows[lo..=hi]` taken on the main
/// thread, so the worker only needs to compute `files`.
#[derive(Debug, Clone)]
pub struct RangeDetail {
    pub oldest_oid: String,
    pub newest_oid: String,
    pub commit_count: usize,
    pub commits: Vec<CommitInfo>,
    pub files: Vec<FileEntry>,
}

/// State for the inline commit-detail editor panel (Tab::Graph right side).
#[derive(Debug)]
pub struct CommitDetailState {
    pub detail: Option<CommitDetail>,
    /// Range-mode payload. Mutually exclusive with `detail` in practice —
    /// `reload_graph_selection` flips the panel into exactly one of the two
    /// modes and clears the other.
    pub range_detail: Option<RangeDetail>,
    pub file_diff: Option<CommitFileDiff>,
    /// Intentionally independent of `App.diff_layout` — the Git tab and the
    /// Graph tab track their diff layout separately (see plan pitfall #1).
    pub diff_layout: DiffLayout,
    pub diff_mode: DiffMode,
    pub files_tree_mode: bool,
    pub files_collapsed: HashSet<String>,
    /// Vertical scroll for the entire panel (header + files + diff in
    /// 2-col fallback;header + files only in 3-col mode).
    pub scroll: usize,
    /// Vertical scroll for the Graph 3-col diff column. Independent of
    /// `scroll` so the user can pan the diff viewport without the commit
    /// metadata / file tree jumping around. Unused in 2-col fallback —
    /// there the diff is inline and pans via `scroll`.
    pub file_diff_scroll: usize,
    /// Horizontal scroll for Unified layout. In 2-col mode this applies
    /// to the whole row list (metadata + files + inline diff). In 3-col
    /// mode the inline diff is gone from this panel, so this field only
    /// pans the metadata / file-tree rows; the standalone diff column
    /// uses the `file_diff_*_h_scroll` triad below.
    pub diff_h_scroll: usize,
    /// SBS horizontal scrolls for the mid-panel view (only meaningful
    /// in 2-col fallback, where the inline SBS diff is part of this
    /// panel's row stream).
    pub sbs_left_h_scroll: usize,
    pub sbs_right_h_scroll: usize,
    /// Horizontal scrolls for the 3-col diff column. Kept separate from
    /// the mid-panel `diff_h_scroll`/`sbs_*` triad above so panning the
    /// commit metadata doesn't shift the diff viewport and vice versa.
    /// Unused in 2-col fallback.
    pub file_diff_h_scroll: usize,
    pub file_diff_sbs_left_h_scroll: usize,
    pub file_diff_sbs_right_h_scroll: usize,
}

impl Default for CommitDetailState {
    fn default() -> Self {
        Self {
            detail: None,
            range_detail: None,
            file_diff: None,
            diff_layout: DiffLayout::Unified,
            diff_mode: DiffMode::Compact,
            files_tree_mode: false,
            files_collapsed: HashSet::new(),
            scroll: 0,
            file_diff_scroll: 0,
            diff_h_scroll: 0,
            sbs_left_h_scroll: 0,
            sbs_right_h_scroll: 0,
            file_diff_h_scroll: 0,
            file_diff_sbs_left_h_scroll: 0,
            file_diff_sbs_right_h_scroll: 0,
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
    pub repo_catalog: RepoCatalogState,

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
    /// SBS 模式下左右列各自的横向滚动偏移。Unified 模式用 `diff_h_scroll`,
    /// 切到 SBS 时两列独立滚动 —— 旧版本 / 新版本的行宽差异常常较大,
    /// 独立滚比同步滚更符合直觉。键盘 ←/→ 同步两侧,鼠标滚按光标所在
    /// 侧路由到其中一个。语义跟 `commit_detail.sbs_left_h_scroll` 一致。
    pub sbs_left_h_scroll: usize,
    pub sbs_right_h_scroll: usize,

    // ── Files tab state ──
    pub file_tree: FileTree,
    pub preview_content: Option<PreviewContent>,
    /// Terminal-capability probe for image rendering. `None` on terminals
    /// with no graphics-protocol support or when the user set
    /// `REEF_IMAGE_PROTOCOL=off`. Populated once at startup by
    /// `images::probe_picker` in `main.rs` (before raw mode); stays set
    /// for the life of the session — the terminal capabilities don't
    /// change mid-run.
    pub image_picker: Option<ratatui_image::picker::Picker>,
    /// Resize-aware protocol state for the currently-previewed image.
    /// Built on the main thread in `apply_worker_result` when the worker
    /// hands back a decoded `DynamicImage`; cleared when a text/binary
    /// body arrives or `image_picker` is `None`.
    ///
    /// Wrapped in `ThreadProtocol` so the resize+encode step (tens of
    /// ms for Kitty on large images) runs on a dedicated worker thread
    /// instead of blocking the render frame. During the resize window
    /// the inner `StatefulProtocol` is held by the worker and render
    /// no-ops on the image area — the rest of the UI stays responsive.
    pub preview_image_protocol: Option<ratatui_image::thread::ThreadProtocol>,
    /// Sender handed to every fresh `ThreadProtocol` so it can dispatch
    /// resize requests to the background worker.
    pub preview_resize_tx: mpsc::Sender<ratatui_image::thread::ResizeRequest>,
    /// Drained in `tick` — each message carries a resized protocol the
    /// main thread puts back via `ThreadProtocol::update_resized_protocol`.
    pub preview_resize_rx: mpsc::Receiver<ratatui_image::thread::ResizeResponse>,
    /// Drained in `tick` — each message carries a freshly-built
    /// `StatefulProtocol` that was constructed off-thread to avoid
    /// blocking the render loop on `Picker::new_resize_protocol`'s
    /// full-image SipHash pass (~16-30 ms for a 2048² image).
    pub preview_build_rx: mpsc::Receiver<BuiltProtocol>,
    /// Sender cloned into each protocol-build worker spawn.
    pub preview_build_tx: mpsc::Sender<BuiltProtocol>,
    /// Monotonic counter incremented every time `apply_worker_result`
    /// constructs a fresh `StatefulProtocol`. Tests observe this to tell
    /// "reused existing protocol" from "rebuilt" — address-based identity
    /// doesn't work because the Option slot lives at a fixed offset. Not
    /// used in any production codepath.
    pub preview_image_protocol_builds: u64,
    /// Debounced preview-load schedule. `load_preview_for_path` writes
    /// `(path, fire_deadline)` here instead of dispatching immediately;
    /// `tick` fires it when the deadline passes. Arrow-scrubbing through
    /// a directory of images no longer decodes every file flown past —
    /// only the last selection survives the `PREVIEW_DEBOUNCE` window.
    pub preview_schedule: Option<(PathBuf, Instant)>,
    /// Deadline for firing neighbor prefetch. Set when a preview lands;
    /// cleared on the next `load_preview_for_path`. Prefetch fires only
    /// if the deadline elapses without a fresh selection — i.e. the
    /// user paused long enough to justify warming the cache.
    pub prefetch_schedule: Option<Instant>,
    /// Path of the preview currently being decoded by the worker.
    /// Populated at dispatch, cleared when the result arrives (or on
    /// error). Render consults this + `preview_schedule` to decide
    /// whether to show a "loading …" placeholder for a different file
    /// instead of the stale previous preview.
    pub preview_in_flight_path: Option<PathBuf>,
    pub tree_scroll: usize,
    /// The `file_tree.selected` value we observed on the previous render.
    /// Used by the Files-tab tree panel to distinguish "selection just changed
    /// (scroll the viewport to keep it visible)" from "user scrolled the
    /// viewport themselves (leave it alone)".
    pub last_rendered_tree_selected: Option<usize>,
    pub preview_scroll: usize,
    pub preview_h_scroll: usize,

    /// 应用级文字选中状态(preview 面板)。Some = 正在拖 / 刚松开但仍要
    /// 显示高亮;None = 无选中。坐标在文件空间(line_index, byte_offset),
    /// 与滚动无关。
    pub preview_selection: Option<crate::ui::selection::PreviewSelection>,
    /// 上一帧 preview 面板(含头部与分隔线)的整体 Rect,mouse handler 用于
    /// hit test 判断鼠标是否在 preview 区域内。None = 尚未渲染过或者不在
    /// 当前 tab。
    pub last_preview_rect: Option<ratatui::layout::Rect>,
    /// SQLite preview pagination state — `Some` exactly when
    /// `preview_content` carries `PreviewBody::Database` for `path`.
    /// Reset and rebuilt by `apply_worker_result` on every preview
    /// land; mutated in-place by the `[`/`]`/`PgUp`/`PgDn` navigation
    /// path. See `db_navigate` for the action enum and the synchronous
    /// `Backend::db_load_page` round-trip those keys trigger.
    pub db_preview_state: Option<DbPreviewState>,
    /// `Some(buffer)` while the `g`-prefix page-jump input is active.
    /// Holds the digits typed so far. Enter parses and jumps via
    /// `db_navigate_to_page`; Esc or a non-digit non-control key
    /// cancels. While `Some`, the input dispatcher (see `handle_key`)
    /// fully owns the keyboard — no other binding fires.
    pub db_goto_input: Option<String>,
    /// Vertical / horizontal scroll axis-lock state. The dispatcher
    /// `observe()`s the firing axis and `locked()`-checks the
    /// orthogonal one — single-event trackpad noise on the
    /// orthogonal axis falls below the streak threshold and never
    /// arms the lock. See [`crate::input::AxisLock`] for the streak
    /// + gap rules.
    pub vertical_scroll_lock: crate::input::AxisLock,
    pub horizontal_scroll_lock: crate::input::AxisLock,
    /// 上一帧 preview 内容行的起点(content_x, content_y)与 gutter 宽度。
    /// mouse handler 据此把终端列行坐标映射回文件行/列。
    pub last_preview_content_origin: Option<(u16, u16, u16)>,
    /// 连击计数器:记录上一次 preview 区 Down(Left) 的时间/位置/次数,用于
    /// 检测双击(选词)和三击(选行)。与全局 `last_click` 独立,不干扰
    /// hit_registry 的 double-click 逻辑。
    pub preview_click_state: Option<(Instant, u16, u16, u8)>,

    /// Diff 面板选中状态——Git tab 的 diff 和 Graph tab 3-col 右栏共用
    /// 同一组字段(它们永远不会同时处于激活状态)。SBS 模式下锚点
    /// 绑定一侧(左/右),跨侧拖拽把列 clamp 回本侧。None 表示无选中。
    pub diff_selection: Option<crate::ui::selection::DiffSelection>,
    /// 上一帧 diff 面板的整体 rect,供 hit test 使用。跟
    /// `last_preview_rect` 并列。切到不渲染 diff 的 tab 时被
    /// `ui::render` 在帧头清零。
    pub last_diff_rect: Option<ratatui::layout::Rect>,
    /// 上一帧 diff 面板的几何快照 + 行文本快照,鼠标处理把终端列行坐标
    /// 映射回 `(side, display_row, byte_offset)`,以及在 Up 时从缓存的
    /// 行文本里抽取选中区间写到剪贴板。只活一帧,下一帧 render 要么覆写
    /// 要么被清零。
    pub last_diff_hit: Option<crate::ui::selection::DiffHit>,
    /// 连击计数器:diff 面板的 Down(Left) 序列,和 `preview_click_state`
    /// 语义一致,但分开存以防两个 tab 之间的点击串扰。
    pub diff_click_state: Option<(Instant, u16, u16, u8)>,

    // Layout
    pub split_percent: u16,
    /// 侧边栏是否可见。关闭后左列(Panel::Files)不参与渲染,也不占宽度;
    /// `graph_sidebar_width` 在 hidden 时短路返回 0,让所有共享宽度计算
    /// (hit-testing、h-scroll 路由、drag zone 注册)自然跟随。Graph tab
    /// 3-col 模式下关闭侧边栏会退化为 [Commit | Diff] 双列。
    pub sidebar_visible: bool,
    /// 第一次 hide 弹一条 Toast 帮用户找回入口,后续保持安静。不持久化:
    /// Ctrl+B 在每个新会话里都允许让人迷茫一次。
    pub sidebar_hide_hint_shown: bool,
    pub dragging_split: bool,
    /// Graph tab 三列布局下,中间 commit 列与右侧 diff 列的分割位置,用
    /// "非 graph 区域的百分比" 表示,从左向右计:中间列占 `100 -
    /// graph_diff_split_percent`%,右侧 diff 列占 `graph_diff_split_percent`%。
    /// 默认 60 —— diff 比元数据略宽一些。只在 `graph_uses_three_col()`
    /// 返回 true 时有效。
    pub graph_diff_split_percent: u16,
    /// 是否正在拖拽 Graph tab 中间|右侧分割线。跟 `dragging_split` 并列,
    /// 互不影响。
    pub dragging_graph_diff_split: bool,
    /// Cache of the most recent rendered body width. `ui::render` writes
    /// this every frame so that logic running outside the render path
    /// (search target resolution, panel normalization, handlers that
    /// need to know layout) can ask "are we in 3-col Graph right now"
    /// without threading terminal size through every call site.
    pub last_total_width: u16,

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

    /// `true` while a background `git commit` is in flight. Blocks
    /// additional commit attempts and lets the status panel render a
    /// "提交中…" indicator.
    pub commit_in_flight: bool,
    /// Receives the commit worker's result. Same drain-on-tick pattern
    /// as `push_rx`.
    pub commit_rx: Option<mpsc::Receiver<Result<(), String>>>,

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

    /// VS Code-style internal file clipboard. Holds the paths and the
    /// move-vs-copy intent set by the latest `Cut` / `Copy`. Consumed
    /// (and cleared, for Cut) by `paste_into`.
    pub file_clipboard: crate::file_clipboard::FileClipboard,

    /// Multi-selection set for the Files-tab tree (`s` toggle,
    /// Shift+arrow range, Shift/Ctrl-click). All clipboard / drag /
    /// delete operations operate on this set when it's non-empty AND
    /// includes the current cursor row; otherwise they fall back to
    /// the single cursor row.
    pub file_selection: crate::file_selection::SelectionSet,

    /// Intra-tree mouse drag state. Mutually exclusive with
    /// `place_mode.active` (OS→TUI drag) — the input dispatcher gates
    /// on the active flag.
    pub tree_drag: crate::tree_drag::TreeDragState,

    /// Modal paste-conflict prompt. `Some` while the user is stepping
    /// through `[R]eplace [S]kip [K]eep both [A]pply to all [C]ancel`
    /// decisions in the status bar. `paste_into` opens it; `input`
    /// keys advance it; `complete_paste_resolution` dispatches the
    /// final batch when it drains.
    pub paste_conflict: Option<crate::paste_conflict::PasteConflictPrompt>,

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

/// Pure predicate: does the Graph tab want 3-col layout given these
/// inputs? Factored out so tests can exercise the switch matrix without
/// instantiating a full `App`. `App::graph_uses_three_col` forwards here.
pub(crate) fn compute_uses_three_col(
    active_tab: Tab,
    total_width: u16,
    has_file_diff: bool,
    load_in_flight: bool,
) -> bool {
    active_tab == Tab::Graph
        && total_width >= App::GRAPH_THREE_COL_MIN_WIDTH
        && (has_file_diff || load_in_flight)
}

/// Pure layout: left-sidebar width given the split percent. Kept free-
/// standing so `ui::render`, hit-testing, and h-scroll routing share one
/// definition; the `App::graph_sidebar_width` method forwards here.
pub(crate) fn compute_sidebar_width(total_width: u16, split_percent: u16) -> u16 {
    let raw = (total_width as u32 * split_percent as u32 / 100) as u16;
    raw.max(10).min(total_width.saturating_sub(20))
}

/// Pure layout: 3-col widths `(graph, commit, diff)` summing to
/// `total_width`. Callers pass an already-clamped `sidebar_w` (from
/// `compute_sidebar_width` or `App::graph_sidebar_width`); when the
/// sidebar is hidden it's 0 and the full width is redistributed
/// between commit and diff using `graph_diff_split_percent`. See
/// `App::graph_three_col_widths` for caller rules.
pub(crate) fn compute_three_col_widths(
    total_width: u16,
    sidebar_w: u16,
    graph_diff_split_percent: u16,
) -> (u16, u16, u16) {
    let remainder = total_width.saturating_sub(sidebar_w);
    let diff_w_raw = (remainder as u32 * graph_diff_split_percent as u32 / 100) as u16;
    // `.max(20)` + `.min(remainder - 20)` keeps both sub-columns usable
    // even when `graph_diff_split_percent` hits its drag clamp edges.
    let diff_w = diff_w_raw.max(20).min(remainder.saturating_sub(20));
    let commit_w = remainder.saturating_sub(diff_w);
    (sidebar_w, commit_w, diff_w)
}

impl App {
    /// Local-backend entry point. Threads `image_picker` straight through
    /// to `new_with_backend`. Tests construct via `new(Theme::dark(), None)`;
    /// `main.rs` constructs via `new_with_backend` so it can pick the
    /// backend (Local vs Remote) up front.
    pub fn new(theme: Theme, image_picker: Option<ratatui_image::picker::Picker>) -> Self {
        let backend = Arc::new(
            LocalBackend::open_cwd().unwrap_or_else(|_| LocalBackend::open_at(PathBuf::from("."))),
        );
        Self::new_with_backend(theme, backend, image_picker)
    }

    pub fn new_with_backend(
        theme: Theme,
        backend: Arc<dyn Backend>,
        image_picker: Option<ratatui_image::picker::Picker>,
    ) -> Self {
        // Fold pre-1.0 unprefixed keys (`layout=`, `mode=`) and the retired
        // `~/.config/reef/git.prefs` into the current prefixed namespace
        // BEFORE any `prefs::get` runs. Order matters: `load_prefs` below
        // reads `diff.layout` / `diff.mode`, and the `GitStatusState` /
        // `CommitDetailState` initializers read `status.*` / `commit.*` —
        // all of those keys only exist after the migrator has run on a
        // legacy install.
        crate::prefs::migrate_legacy_prefs();

        // Spin up the image-resize worker. Every `ThreadProtocol` we
        // build later clones this sender; the receiver is drained in
        // `tick`. Keeping the worker alive for the life of App means
        // subsequent preview switches reuse the same channel (and its
        // serial ordering guarantees — no "request N finished after
        // request N+1" races).
        let (preview_resize_tx, preview_resize_rx) = {
            let (req_tx, req_rx) = mpsc::channel::<ratatui_image::thread::ResizeRequest>();
            let (resp_tx, resp_rx) = mpsc::channel::<ratatui_image::thread::ResizeResponse>();
            std::thread::Builder::new()
                .name("reef-image-resize".into())
                .spawn(move || {
                    while let Ok(req) = req_rx.recv() {
                        if let Ok(resp) = req.resize_encode() {
                            let _ = resp_tx.send(resp);
                        }
                    }
                })
                .ok();
            (req_tx, resp_rx)
        };

        // Channel for `StatefulProtocol` construction results. Protocol
        // construction hashes every pixel of the decoded image (~16-30 ms
        // for 2048² RGBA on main thread) — so we spawn a one-shot thread
        // per build request and merge the result back via this channel.
        let (preview_build_tx, preview_build_rx) = mpsc::channel::<BuiltProtocol>();

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
            repo_catalog: RepoCatalogState::from_prefs(),
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
            sbs_left_h_scroll: 0,
            sbs_right_h_scroll: 0,
            file_tree,
            preview_content: None,
            image_picker,
            preview_image_protocol: None,
            preview_resize_tx,
            preview_resize_rx,
            preview_build_tx,
            preview_build_rx,
            preview_image_protocol_builds: 0,
            preview_schedule: None,
            prefetch_schedule: None,
            preview_in_flight_path: None,
            tree_scroll: 0,
            last_rendered_tree_selected: None,
            preview_scroll: 0,
            preview_h_scroll: 0,
            preview_selection: None,
            last_preview_rect: None,
            db_preview_state: None,
            db_goto_input: None,
            vertical_scroll_lock: crate::input::AxisLock::new(),
            horizontal_scroll_lock: crate::input::AxisLock::new(),
            last_preview_content_origin: None,
            preview_click_state: None,
            diff_selection: None,
            last_diff_rect: None,
            last_diff_hit: None,
            diff_click_state: None,
            split_percent: 30,
            sidebar_visible: true,
            sidebar_hide_hint_shown: false,
            dragging_split: false,
            graph_diff_split_percent: 60,
            dragging_graph_diff_split: false,
            last_total_width: 0,
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
            commit_in_flight: false,
            commit_rx: None,
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
            file_clipboard: crate::file_clipboard::FileClipboard::default(),
            file_selection: crate::file_selection::SelectionSet::default(),
            tree_drag: crate::tree_drag::TreeDragState::default(),
            paste_conflict: None,
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
        app.refresh_repo_catalog();
        app.refresh_file_tree();
        app
    }

    /// Minimum total width for the Graph tab's 3-column layout. Below this
    /// the panel falls back to the 2-column layout with the diff rendered
    /// inline inside `commit_detail_panel` (the pre-split behaviour).
    /// Chosen so the middle column still shows readable file names and the
    /// diff column has at least ~40 cols for content after its gutter.
    pub const GRAPH_THREE_COL_MIN_WIDTH: u16 = 100;

    /// Width of the left (graph / tree / status) sidebar for the current
    /// frame. Single source of truth for the `split_percent → columns`
    /// clamp so `ui::render`, mouse hit-testing, and h-scroll routing
    /// never disagree about where the boundary is. Mirror of the
    /// `.max(10).min(total - 20)` clamp `ui::render` has applied since
    /// v0 — factored here so `input::*` and the render stay aligned
    /// even when `split_percent` lands near the extremes.
    pub fn graph_sidebar_width(&self, total_width: u16) -> u16 {
        if !self.sidebar_visible {
            return 0;
        }
        compute_sidebar_width(total_width, self.split_percent)
    }

    /// Widths for the Graph 3-col layout: `(graph, commit, diff)`. Sum
    /// equals `total_width`. Only meaningful when `graph_uses_three_col()`
    /// is true; callers outside the render path should gate on that
    /// first. The `(20, 20)` floors keep both right-side columns usable
    /// when either `split_percent` or `graph_diff_split_percent` is
    /// near its edge — matches `ui::render`'s constraint math.
    pub fn graph_three_col_widths(&self, total_width: u16) -> (u16, u16, u16) {
        compute_three_col_widths(
            total_width,
            self.graph_sidebar_width(total_width),
            self.graph_diff_split_percent,
        )
    }

    /// Whether the Graph tab should render with 3 columns right now —
    /// graph | commit metadata+files | diff. True when a file diff is
    /// loaded (or currently loading) AND the terminal is wide enough.
    /// Other tabs and narrow terminals fall back to the existing 2-col
    /// layout where the diff is inline under the file list.
    ///
    /// Callers that need this in non-render contexts (search target
    /// resolution, panel normalization) should read `last_total_width`
    /// — `ui::render` caches it every frame before any panel runs.
    pub fn graph_uses_three_col(&self) -> bool {
        compute_uses_three_col(
            self.active_tab,
            self.last_total_width,
            self.commit_detail.file_diff.is_some(),
            self.commit_file_diff_load.loading,
        )
    }

    /// Drop the in-panel diff selection and its click counter. Called
    /// whenever the underlying row list is about to shift out from under
    /// the cached `(row_idx, byte_offset)` anchor — file swap, layout /
    /// mode toggle, tab switch, 3-col→2-col transition. Keeping this in
    /// one place means future reset sites can't miss the click counter.
    pub fn clear_diff_selection(&mut self) {
        self.diff_selection = None;
        self.diff_click_state = None;
    }

    /// Drop `Panel::Commit` back to a two-column-compatible panel when the
    /// layout no longer exposes a middle column (narrow terminal, diff
    /// unloaded, tab switched away). Prevents the user from being stuck
    /// focusing a column that isn't rendered. Called at the top of
    /// `ui::render` alongside `last_total_width` update.
    ///
    /// Also drops a stale diff selection if we just lost the panel that
    /// owned it — row indices from the old frame would overlay on top of
    /// whatever renders next (commit_detail's flat row list doesn't match
    /// `DiffHit.rows`), producing a bogus highlight.
    pub fn normalize_active_panel(&mut self) {
        if self.active_panel == Panel::Commit && !self.graph_uses_three_col() {
            self.active_panel = Panel::Diff;
        }
        // Sidebar hidden → Panel::Files has no rendered column. Demote here
        // as a safety net even though `toggle_sidebar` already does it; a
        // future code path that flips `sidebar_visible` without going through
        // the toggle can't leave focus stranded.
        if !self.sidebar_visible && self.active_panel == Panel::Files {
            self.active_panel = Panel::Diff;
        }
        // If we're on Graph and the 3-col diff column isn't visible anymore,
        // any selection was anchored into rows that no panel will render.
        if self.active_tab == Tab::Graph
            && self.diff_selection.is_some()
            && !self.graph_uses_three_col()
        {
            self.clear_diff_selection();
        }
    }

    /// Toggle the left sidebar's visibility. Hiding collapses the left
    /// column to 0 width (via `graph_sidebar_width`'s short-circuit); all
    /// mouse hit-testing, h-scroll routing, and drag-zone registration key
    /// off that same value so they stay consistent. Focus on `Panel::Files`
    /// gets moved to `Panel::Diff` — the sidebar panel wouldn't render
    /// otherwise and keyboard nav would aim at nothing. Any in-flight
    /// column drag is cancelled so releasing the mouse after the hide
    /// doesn't snap a phantom split_percent.
    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        if !self.sidebar_visible {
            if self.active_panel == Panel::Files {
                self.active_panel = Panel::Diff;
            }
            self.dragging_split = false;
            self.dragging_graph_diff_split = false;
            // First-time-per-session hint so the user can find the way
            // back without scanning the help popup. Subsequent hides
            // stay quiet — the tab-bar button glyph already telegraphs
            // the state.
            if !self.sidebar_hide_hint_shown {
                self.sidebar_hide_hint_shown = true;
                self.toasts.push(Toast::info(crate::i18n::t(
                    crate::i18n::Msg::SidebarHiddenHint,
                )));
            }
        }
    }

    pub fn refresh_status(&mut self) {
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let generation = self.git_status_load.begin();
        self.tasks
            .refresh_status(generation, Arc::clone(&self.backend), repo_root_rel);
    }

    fn status_repo_root_rel(&self) -> Option<PathBuf> {
        if let Some(repo_root_rel) = self.repo_catalog.selected_git_repo.as_ref() {
            return Some(repo_root_rel.clone());
        }
        if self.backend.has_repo() {
            return Some(PathBuf::from("."));
        }
        None
    }

    fn clear_git_status_snapshot(&mut self) {
        self.staged_files.clear();
        self.unstaged_files.clear();
        self.selected_file = None;
        self.diff_content = None;
        self.git_status.ahead_behind = None;
        self.git_status.branches.clear();
        self.branch_name.clear();
        self.file_tree.refresh_git_statuses(&[], &[]);
    }

    fn clear_graph_snapshot(&mut self) {
        self.git_graph.rows.clear();
        self.git_graph.ref_map.clear();
        self.git_graph.cache_key = None;
        self.git_graph.selected_idx = 0;
        self.git_graph.selected_commit = None;
        self.git_graph.selection_anchor = None;
        self.commit_detail.detail = None;
        self.commit_detail.range_detail = None;
        self.commit_detail.file_diff = None;
    }

    pub fn refresh_repo_catalog(&mut self) {
        let generation = self.repo_catalog.discover_load.begin();
        let opts = self.repo_catalog.discover_opts();
        self.tasks
            .discover_repos(generation, Arc::clone(&self.backend), opts);
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

    // ── VS Code-style file clipboard / paste / drag (intra-tree) ────────

    /// Workdir-relative paths the next clipboard / drag / delete
    /// operation should target. VS Code rule: if the multi-selection
    /// contains the current cursor row, the whole selection is the
    /// payload; otherwise the cursor alone wins. This keeps a stray
    /// click from quietly losing a selection set the user built up.
    pub fn effective_action_paths(&self) -> Vec<PathBuf> {
        let cursor = self.file_tree.selected_path();
        if let Some(cursor_path) = cursor.as_ref()
            && !self.file_selection.is_empty()
            && self.file_selection.contains(cursor_path)
        {
            return self.file_selection.to_vec();
        }
        cursor.into_iter().collect()
    }

    /// Workdir-relative directory the next Paste should drop into.
    /// VS Code rule: cursor on a folder → into that folder; cursor on
    /// a file → into its parent; nothing selected → project root.
    pub fn paste_target_dir(&self) -> PathBuf {
        match self.file_tree.selected_entry() {
            Some(entry) if entry.is_dir => entry.path.clone(),
            Some(entry) => entry.path.parent().map(PathBuf::from).unwrap_or_default(),
            None => PathBuf::new(),
        }
    }

    /// Mark `paths` as Cut. Replaces any prior clipboard. Render
    /// reads `file_clipboard.is_cut()` + `contains` directly per
    /// visible row so there's no eager stamping to do here.
    pub fn mark_cut(&mut self, paths: Vec<PathBuf>) {
        self.file_clipboard
            .set(crate::file_clipboard::ClipMode::Cut, paths);
    }

    /// Mark `paths` as Copy. Copy mode does not visually mark source
    /// rows (matches VS Code).
    pub fn mark_copy(&mut self, paths: Vec<PathBuf>) {
        self.file_clipboard
            .set(crate::file_clipboard::ClipMode::Copy, paths);
    }

    pub fn clear_clipboard(&mut self) {
        self.file_clipboard.clear();
    }

    /// Look up a path in the cached tree to determine its type. Falls
    /// back to local filesystem probe when the tree doesn't have the
    /// entry (path outside the visible/expanded subset). Returns
    /// `None` when neither source can resolve the entry — caller
    /// should bail with a warning.
    fn entry_is_dir(&self, rel: &Path) -> Option<bool> {
        if let Some(entry) = self.file_tree.entries.iter().find(|e| e.path == rel) {
            return Some(entry.is_dir);
        }
        if !self.backend.is_remote() {
            let abs = self.file_tree.root.join(rel);
            return Some(abs.is_dir());
        }
        None
    }

    /// Existing basenames (single path component) in `dest_rel`. Used
    /// for paste conflict detection and `next_copy_name`.
    ///
    /// Local backend: enumerates the directory directly via `std::fs`
    /// for completeness — a hidden file we haven't expanded must still
    /// surface as a conflict.
    /// Remote backend: walks the cached tree (collapsed siblings stay
    /// invisible; a real conflict at a non-listed path falls through
    /// to the worker, which surfaces `BackendError::PathExists` as a
    /// toast).
    fn existing_basenames_in_dir(&self, dest_rel: &Path) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        if !self.backend.is_remote() {
            let abs = self.file_tree.root.join(dest_rel);
            if let Ok(rd) = std::fs::read_dir(&abs) {
                for entry in rd.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        out.insert(name.to_string());
                    }
                }
                return out;
            }
        }
        // Remote (or local read_dir failed) — fall back to cached tree.
        let dest_for_filter = dest_rel.to_path_buf();
        for e in &self.file_tree.entries {
            let parent = e.path.parent().map(PathBuf::from).unwrap_or_default();
            if parent == dest_for_filter {
                out.insert(e.name.clone());
            }
        }
        out
    }

    /// Drop the file_clipboard contents into `dest_rel` per VS Code
    /// semantics. Same-directory copies auto-rename via `next_copy_name`;
    /// cross-directory conflicts open `paste_conflict` for resolution.
    /// No-conflict items dispatch immediately on the worker; with
    /// conflicts present, the prompt drives a second-stage dispatch
    /// from `complete_paste_resolution`.
    pub fn paste_into(&mut self, dest_rel: PathBuf) {
        if self.file_clipboard.is_empty() {
            self.toasts
                .push(Toast::warn(crate::i18n::paste_clipboard_empty()));
            return;
        }
        if self.fs_mutation_load.loading {
            self.toasts
                .push(Toast::warn(crate::i18n::tree_op_blocked_by_in_flight()));
            return;
        }
        let op = match self.file_clipboard.mode {
            Some(m) => m,
            None => return,
        };
        let sources = self.file_clipboard.paths.clone();
        self.dispatch_paste_op(op, dest_rel, sources);
    }

    /// Same-directory Copy shortcut for the keyboard `D` binding.
    /// Drives off the active selection / cursor.
    pub fn duplicate_selection(&mut self) {
        self.duplicate_paths(self.effective_action_paths());
    }

    /// Same-directory Copy shortcut taking explicit paths. Used by
    /// the right-click menu (which targets the menu's anchor row, not
    /// `effective_action_paths()`) so the action operates on the
    /// right-clicked file even when the cursor sits on a different
    /// row. Pure path I/O — does not mutate `file_selection`.
    fn duplicate_paths(&mut self, sources: Vec<PathBuf>) {
        if sources.is_empty() {
            return;
        }
        if self.fs_mutation_load.loading {
            self.toasts
                .push(Toast::warn(crate::i18n::tree_op_blocked_by_in_flight()));
            return;
        }
        // Group by parent dir — duplicating a multi-selection that
        // spans dirs is uncommon but should "just work" by treating
        // each source as paste-into-its-own-parent. We implement the
        // common case (one parent) directly; the uncommon case falls
        // back to one batch per parent.
        use std::collections::BTreeMap;
        let mut by_parent: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for s in sources {
            let parent = s.parent().map(PathBuf::from).unwrap_or_default();
            by_parent.entry(parent).or_default().push(s);
        }
        for (parent, group) in by_parent {
            self.dispatch_paste_op(crate::file_clipboard::ClipMode::Copy, parent, group);
            // Multiple parents → multiple batches; but `fs_mutation_load`
            // serialises them: we only fire the first, the rest get the
            // "operation in flight" toast. Acceptable v1 behaviour —
            // duplicating across parents is a corner case.
            if self.fs_mutation_load.loading {
                break;
            }
        }
    }

    /// Compute decisions for `(op, dest_rel, sources)` and either
    /// dispatch directly (no conflicts) or open `paste_conflict`
    /// (cross-dir conflicts present). Same-dir conflicts auto-rename
    /// without prompting.
    ///
    /// The smart classification logic lives in
    /// `paste_conflict::classify_paste` (pure, unit-tested). This
    /// method is glue: snapshot the destination's existing names,
    /// run classification, surface advisory toasts (self-descent
    /// blocks, same-dir Cut no-ops), and dispatch.
    fn dispatch_paste_op(
        &mut self,
        op: crate::file_clipboard::ClipMode,
        dest_rel: PathBuf,
        sources: Vec<PathBuf>,
    ) {
        // RACE: `existing` is a snapshot at decision-time. Between
        // here and worker dispatch, `fs_watcher` events from the
        // current process or external tools can add or remove names
        // at the destination. Acceptable because:
        //   - `Resolution::Replace` items pre-trash anyway, so a new
        //     same-named entry that appeared mid-window is still
        //     overwritten (matches the user's "no conflict" view).
        //   - `KeepBoth(name)` falls through to `BackendError::PathExists`
        //     if the chosen rename was claimed by another writer.
        //   - Same-dir auto-rename uses the snapshot transiently;
        //     intra-batch collisions are tracked by `classify_paste`.
        let existing = self.existing_basenames_in_dir(&dest_rel);
        let cls = crate::paste_conflict::classify_paste(op, &dest_rel, &sources, &existing);

        // Advisory toast on blocked items — single toast for the
        // whole batch rather than one per row, matches VS Code's
        // single-line "1 file already exists / cannot drop into
        // itself" message style.
        if cls.self_descent_blocked > 0 {
            self.toasts
                .push(Toast::warn(crate::i18n::paste_self_into_descendant()));
        }

        if cls.pending.is_empty() {
            self.dispatch_paste_resolved(op, dest_rel, cls.auto_decisions);
        } else {
            self.paste_conflict = Some(crate::paste_conflict::PasteConflictPrompt::new(
                op,
                dest_rel,
                cls.auto_decisions,
                cls.pending,
            ));
        }
    }

    /// Send a fully-resolved paste batch to the worker.
    fn dispatch_paste_resolved(
        &mut self,
        op: crate::file_clipboard::ClipMode,
        dest_rel: PathBuf,
        decisions: Vec<(PathBuf, crate::paste_conflict::Resolution)>,
    ) {
        use crate::paste_conflict::Resolution;
        // Filter Skip/Cancel so the worker doesn't see noops, but
        // emit a "nothing to paste" toast if everything got skipped.
        let actionable: Vec<_> = decisions
            .into_iter()
            .filter(|(_, r)| !matches!(r, Resolution::Skip | Resolution::Cancel))
            .collect();
        if actionable.is_empty() {
            self.toasts
                .push(Toast::info(crate::i18n::paste_nothing_to_do()));
            return;
        }

        // Stash the post-paste cursor target — the first decision's
        // landing path. Picking the first source preserves the user's
        // mental model ("the thing I was looking at moves into here").
        let post_target: Option<PathBuf> = actionable.first().map(|(src, r)| {
            let basename = match r {
                Resolution::KeepBoth(name) => name.clone(),
                _ => src
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(String::from)
                    .unwrap_or_default(),
            };
            dest_rel.join(basename)
        });
        self.fs_mutation_select_on_done = post_target;

        // Build PasteItem list with is_dir resolved from the tree.
        let items: Vec<crate::tasks::PasteItem> = actionable
            .into_iter()
            .map(|(source, resolution)| {
                let is_dir = self.entry_is_dir(&source).unwrap_or(false);
                crate::tasks::PasteItem {
                    source,
                    is_dir,
                    resolution,
                }
            })
            .collect();

        let generation = self.fs_mutation_load.begin();
        match op {
            crate::file_clipboard::ClipMode::Cut => {
                self.tasks
                    .move_paths(generation, Arc::clone(&self.backend), items, dest_rel);
                // Cut-paste consumes the clipboard. VS Code's
                // semantics: a single Cut feeds one Paste.
                self.clear_clipboard();
            }
            crate::file_clipboard::ClipMode::Copy => {
                self.tasks
                    .copy_paths(generation, Arc::clone(&self.backend), items, dest_rel);
                // Copy-paste leaves the clipboard intact so a second
                // Paste can replay the same sources.
            }
        }
        // Selection drops after a successful operation — VS Code also
        // doesn't preserve the source set across a paste; the new
        // landing rows get focus instead.
        self.file_selection.clear();
    }

    /// Process the user's response to the current `paste_conflict`
    /// prompt. `r` is a one-item resolution; pass `apply_to_all =
    /// true` to drain the rest of the queue with the same answer
    /// (Replace / Skip only — KeepBoth needs per-item rename names,
    /// Cancel is independent of apply-to-all).
    pub fn resolve_paste_conflict(
        &mut self,
        r: crate::paste_conflict::Resolution,
        apply_to_all: bool,
    ) {
        let Some(prompt) = self.paste_conflict.as_mut() else {
            return;
        };
        if apply_to_all {
            prompt.resolve_all_with(r);
        } else {
            prompt.resolve_one(r);
        }
        if prompt.is_done() {
            // Drain the prompt; dispatch unless the user picked Cancel.
            let prompt = self.paste_conflict.take().unwrap();
            let cancelled = prompt.was_cancelled();
            let op = prompt.op();
            let dest = prompt.dest_dir().to_path_buf();
            let decisions = prompt.into_decisions();
            if cancelled {
                self.toasts
                    .push(Toast::info(crate::i18n::paste_cancelled()));
            } else {
                self.dispatch_paste_resolved(op, dest, decisions);
            }
        }
    }

    /// Cancel the prompt without committing any pending dispositions.
    /// Auto-resolved (no-conflict) items are dropped too — VS Code's
    /// Cancel halts the entire batch, not just the prompted item.
    pub fn cancel_paste_conflict(&mut self) {
        if self.paste_conflict.take().is_some() {
            self.toasts
                .push(Toast::info(crate::i18n::paste_cancelled()));
        }
    }

    /// Compute a Keep-Both basename for the current prompt item using
    /// the destination directory's existing names.
    pub fn keep_both_name_for_current_conflict(&self) -> Option<String> {
        let prompt = self.paste_conflict.as_ref()?;
        let item = prompt.current()?;
        let basename = item.source.file_name().and_then(|s| s.to_str())?;
        let existing = self.existing_basenames_in_dir(prompt.dest_dir());
        Some(crate::paste_conflict::next_copy_name(basename, &existing))
    }

    /// Write the absolute (or workdir-relative) paths of the active
    /// selection / cursor row to the system clipboard via OSC 52.
    /// Used by the keyboard binding; menu actions go through
    /// `copy_paths_to_clipboard` directly with the menu's anchor.
    pub fn copy_path_to_clipboard(&mut self, relative: bool) {
        self.copy_paths_to_clipboard(self.effective_action_paths(), relative);
    }

    /// Write `paths` to the system clipboard via OSC 52 — absolute
    /// paths when `relative = false`, workdir-relative otherwise.
    /// Multi-input produces newline-separated paths (VS Code's
    /// "Copy Path" / "Copy Relative Path" multi-row behaviour).
    /// Pure path I/O — does not mutate `file_selection`.
    fn copy_paths_to_clipboard(&mut self, paths: Vec<PathBuf>, relative: bool) {
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        let workdir = self.file_tree.root.clone();
        // Single contiguous String buffer — skips an intermediate
        // `Vec<String>` and the N small allocations it'd require.
        // `Path::to_string_lossy` is `Cow<str>`; `push_str(&cow)`
        // dereferences to `&str` without forcing it `Owned`, so the
        // happy-path UTF-8 case copies bytes once into `payload`.
        let mut payload = String::new();
        let mut had_lossy = false;
        for rel in paths {
            let target = if relative { rel } else { workdir.join(rel) };
            let s = target.to_string_lossy();
            if matches!(s, std::borrow::Cow::Owned(_)) {
                had_lossy = true;
            }
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(&s);
        }
        match crate::clipboard::copy_to_clipboard(&payload) {
            Ok(()) => {
                if had_lossy {
                    self.toasts
                        .push(Toast::warn(crate::i18n::copy_path_lossy_utf8()));
                } else {
                    let toast = if relative {
                        crate::i18n::copy_relative_path_done(count)
                    } else {
                        crate::i18n::copy_path_done(count)
                    };
                    self.toasts.push(Toast::info(toast));
                }
            }
            Err(e) => {
                self.toasts
                    .push(Toast::error(format!("Copy path failed: {e}")));
            }
        }
    }

    // ── Intra-tree mouse drag (VS Code-style move/copy on drop) ─────────

    /// Promote the press recorded by `Down(Left)` to an active drag.
    /// Snapshots `effective_action_paths()` *now* — a mid-drag
    /// selection mutation can't change what's being carried.
    pub fn begin_tree_drag(&mut self, mods: crossterm::event::KeyModifiers) {
        if self.tree_drag.active {
            return;
        }
        let sources = self.effective_action_paths();
        if sources.is_empty() {
            self.tree_drag.cancel();
            return;
        }
        // Place mode is the OS→TUI flow; intra-tree drag overlays its
        // hover affordances over the same tree, so the two are
        // mutually exclusive at the active-flag level.
        if self.place_mode.active {
            return;
        }
        self.tree_drag.start(sources, mods);
    }

    pub fn update_tree_drag_hover(&mut self, idx: Option<usize>) {
        self.tree_drag.update_hover(idx);
    }

    pub fn update_tree_drag_modifiers(&mut self, mods: crossterm::event::KeyModifiers) {
        self.tree_drag.update_modifiers(mods);
    }

    /// Fire any due hover-auto-expand. Call once per tick from the
    /// main loop while a drag is active. Safe to call when idle.
    ///
    /// Stale-index window: `tree_drag.hover_idx` is set on the most
    /// recent `Drag(Left)` mouse event, against the entry list as
    /// it stood then. Between Drag and tick (≤16 ms typically, plus
    /// the 600 ms `HOVER_EXPAND_DELAY`), `fs_watcher` could fire and
    /// trigger a tree rebuild — invalidating `idx`. The `is_dir` /
    /// `!is_expanded` checks below double as the staleness guard:
    /// a rebuilt tree where `idx` now points at a file (or out of
    /// range) silently no-ops, and the next `Drag` event recomputes
    /// `hover_idx` against the new list.
    pub fn tick_tree_drag_auto_expand(&mut self) {
        if !self.tree_drag.active {
            return;
        }
        let now = std::time::Instant::now();
        if let Some(idx) = self.tree_drag.auto_expand_due(now) {
            // Folder that's still collapsed → expand it. Mirrors
            // `place_mode`'s identical block.
            if let Some(entry) = self.file_tree.entries.get(idx).cloned()
                && entry.is_dir
                && !entry.is_expanded
            {
                self.file_tree.toggle_expand(idx);
                self.refresh_file_tree_with_target(self.file_tree.selected_path());
            }
            self.tree_drag.clear_hover_timer();
        }
    }

    /// Mouse `Up(Left)` while drag is active — translate the hovered
    /// row to a destination folder and dispatch move (default) or
    /// copy (Alt held).
    pub fn commit_tree_drag(&mut self, release_mods: crossterm::event::KeyModifiers) {
        if !self.tree_drag.active {
            return;
        }
        self.tree_drag.update_modifiers(release_mods);
        let is_copy = self.tree_drag.is_copy_op();
        // Resolve dest via the same hover-target rule as place mode.
        let dest_rel: PathBuf = match self.tree_drag.hover_idx {
            Some(idx) => {
                use crate::place_mode::HoverTarget;
                match crate::place_mode::resolve_hover_target(&self.file_tree.entries, idx) {
                    HoverTarget::Folder { folder_idx, .. } => self
                        .file_tree
                        .entries
                        .get(folder_idx)
                        .map(|e| e.path.clone())
                        .unwrap_or_default(),
                    HoverTarget::Root => PathBuf::new(),
                }
            }
            None => {
                // Released over no row (tree panel empty space, or
                // off-tree). VS Code's Explorer drops here onto the
                // workspace root — match that. The terminal can't
                // distinguish "outside the tree panel" from "below
                // the last row" cleanly, so we accept both as root.
                PathBuf::new()
            }
        };
        let sources = std::mem::take(&mut self.tree_drag.sources);
        self.tree_drag.cancel();
        let op = if is_copy {
            crate::file_clipboard::ClipMode::Copy
        } else {
            crate::file_clipboard::ClipMode::Cut
        };
        if self.fs_mutation_load.loading {
            self.toasts
                .push(Toast::warn(crate::i18n::tree_op_blocked_by_in_flight()));
            return;
        }
        self.dispatch_paste_op(op, dest_rel, sources);
    }

    pub fn cancel_tree_drag(&mut self) {
        self.tree_drag.cancel();
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
        // Disabled items (e.g. Paste when the clipboard is empty)
        // close the menu but skip the action — the user clicked a
        // greyed-out row, and silently doing nothing matches VS
        // Code's behaviour.
        if !item.is_enabled(self.file_clipboard.is_empty()) {
            self.tree_context_menu.close();
            return;
        }
        let target_idx = self.tree_context_menu.target_entry_idx;
        self.tree_context_menu.close();
        // The right-click menu stamps `target_entry_idx` independently
        // of `file_tree.selected`. For clipboard / path actions we
        // want them to operate on the right-clicked row, even if the
        // selection cursor is elsewhere — temporarily seed a single-
        // path action set from the menu's anchor when there's no
        // active multi-selection containing the row.
        let anchor_paths = |this: &Self| -> Vec<PathBuf> {
            if let Some(idx) = target_idx
                && let Some(entry) = this.file_tree.entries.get(idx)
            {
                let p = entry.path.clone();
                if !this.file_selection.is_empty() && this.file_selection.contains(&p) {
                    this.file_selection.to_vec()
                } else {
                    vec![p]
                }
            } else {
                this.effective_action_paths()
            }
        };
        match item {
            I::Cut => {
                let paths = anchor_paths(self);
                self.mark_cut(paths);
            }
            I::Copy => {
                let paths = anchor_paths(self);
                self.mark_copy(paths);
            }
            I::Paste => {
                if !self.file_clipboard.is_empty() {
                    // Right-click on a folder lands paste inside it;
                    // on a file lands in its parent; on empty tree
                    // space (ALL_FOR_ROOT, target_idx == None) lands
                    // at the workspace root. We deliberately do NOT
                    // fall through to `paste_target_dir()` for the
                    // root case — that would pull the unrelated
                    // cursor row's parent and hide the user's clear
                    // intent ("paste at the root").
                    let dest = target_idx
                        .and_then(|idx| self.file_tree.entries.get(idx))
                        .map(|entry| {
                            if entry.is_dir {
                                entry.path.clone()
                            } else {
                                entry.path.parent().map(PathBuf::from).unwrap_or_default()
                            }
                        })
                        .unwrap_or_default();
                    self.paste_into(dest);
                }
            }
            I::Duplicate => {
                // Menu actions take their target paths directly; the
                // user's `file_selection` (and its anchor) stays
                // untouched. `anchor_paths` already implements the
                // "if menu row is part of the selection, act on the
                // whole selection; otherwise just the menu row" rule.
                let paths = anchor_paths(self);
                self.duplicate_paths(paths);
            }
            I::CopyPath => {
                let paths = anchor_paths(self);
                self.copy_paths_to_clipboard(paths, false);
            }
            I::CopyRelativePath => {
                let paths = anchor_paths(self);
                self.copy_paths_to_clipboard(paths, true);
            }
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

    /// Refresh the currently-selected file's preview *now*, bypassing the
    /// `PREVIEW_DEBOUNCE` window. For "I just edited this file in
    /// $EDITOR — show me the new contents" moments: the user is settled
    /// on the file (no scrubbing in play), so the debounce that exists to
    /// coalesce ↓-hold key-repeats just reads as noticeable lag before
    /// the post-edit preview lands.
    pub fn reload_preview_now(&mut self) {
        let Some(entry) = self.file_tree.selected_entry() else {
            return;
        };
        if entry.is_dir {
            return;
        }
        let path = entry.path.clone();
        self.preview_schedule = None;
        self.prefetch_schedule = None;
        self.dispatch_preview_load(path);
    }

    /// Navigate the SQLite preview card. Called from the input
    /// dispatcher when the focused panel is the preview pane and
    /// `preview_content.body` is `Database`. Computes the new
    /// `(table, page)` window, issues a synchronous
    /// `Backend::db_load_page` round-trip, and replaces
    /// `db_preview_state.current_rows` on success. Errors land as a
    /// warning toast.
    ///
    /// Runs synchronously on the main thread: locally sub-ms, over
    /// SSH a single RPC round-trip (~10-50 ms). Brief UI stall on
    /// flaky links is the accepted trade-off for keeping nav simple.
    pub fn db_navigate(&mut self, action: DbNav) {
        // Snapshot the database info we need from the current preview.
        // We only mutate state when everything below resolves cleanly,
        // so a stale preview / wrong body / missing state silently
        // no-ops rather than panicking.
        let info = match self.preview_content.as_ref().map(|p| &p.body) {
            Some(PreviewBody::Database(info)) => info.clone(),
            _ => return,
        };
        if info.tables.is_empty() {
            return;
        }
        let state = match self.db_preview_state.as_ref() {
            Some(s) => s.clone(),
            None => return,
        };
        let max_table = info.tables.len() - 1;
        let cur_table = state.selected_table.min(max_table);

        // Compute the target (table_idx, page) tuple without touching
        // self.* yet. `max_page_for` clamps based on the cached
        // row_count from the initial preview — page totals don't
        // refresh until a fresh `load_preview` lands, which is fine
        // because read-only previews can't change row counts under us.
        let (new_table, new_page) = match action {
            DbNav::PrevPage => (cur_table, state.page.saturating_sub(1)),
            DbNav::NextPage => (
                cur_table,
                (state.page + 1).min(max_page_for(&info.tables[cur_table], state.rows_per_page)),
            ),
            DbNav::PrevTable => (cur_table.saturating_sub(1), 0),
            DbNav::NextTable => ((cur_table + 1).min(max_table), 0),
            DbNav::FirstPage => (cur_table, 0),
            DbNav::LastPage => (
                cur_table,
                max_page_for(&info.tables[cur_table], state.rows_per_page),
            ),
        };

        // No-op when the action would land on the same window — keeps
        // PgUp on page 0 / NextTable at the last table from issuing a
        // pointless RPC.
        if new_table == cur_table && new_page == state.page {
            return;
        }

        let table_name = info.tables[new_table].name.clone();
        let offset = new_page.saturating_mul(state.rows_per_page as u64);
        let limit = state.rows_per_page;
        let path = PathBuf::from(&state.path);

        let table_changed = new_table != cur_table;
        match self.backend.db_load_page(&path, &table_name, offset, limit) {
            Ok(page) => {
                if let Some(s) = self.db_preview_state.as_mut() {
                    s.selected_table = new_table;
                    s.page = new_page;
                    s.current_rows = page.rows;
                    s.recompute_layout(&info);
                }
                // New page / new table → reset within-page row offset
                // so the user lands at row 1, not whatever scroll
                // position they left the previous page at.
                self.preview_scroll = 0;
                // Table change → also reset horizontal scroll, since
                // the new table's natural column widths almost
                // certainly differ and a leftover h_scroll would land
                // on a meaningless mid-column position. Page-only
                // navigation keeps h_scroll so the user can keep
                // reading the same column across pages.
                if table_changed {
                    self.preview_h_scroll = 0;
                }
            }
            Err(e) => {
                self.toasts
                    .push(Toast::warn(format!("sqlite page load failed: {e}")));
            }
        }
    }

    /// Jump to a specific 1-based page in the currently-selected
    /// table. Used by the `g`-prefix page-jump input. Out-of-range
    /// pages clamp to the last valid page rather than failing — the
    /// input prompt enforces that user-typed numbers are well-formed
    /// before calling this, but range clamping here keeps the contract
    /// loose enough that a future caller can pass `u64::MAX` and get
    /// "last page" without an error path.
    pub fn db_navigate_to_page(&mut self, page_one_based: u64) {
        let info = match self.preview_content.as_ref().map(|p| &p.body) {
            Some(PreviewBody::Database(info)) => info.clone(),
            _ => return,
        };
        // Empty-tables guard — without it, `info.tables[table_idx]`
        // below panics on a fresh DB with no user tables that the
        // user somehow invokes `g`-prefix on.
        if info.tables.is_empty() {
            return;
        }
        let state = match self.db_preview_state.as_ref() {
            Some(s) => s.clone(),
            None => return,
        };
        let table_idx = state.selected_table.min(info.tables.len() - 1);
        let max_page = max_page_for(&info.tables[table_idx], state.rows_per_page);
        let target_page = page_one_based.saturating_sub(1).min(max_page);
        if target_page == state.page {
            return;
        }
        let table_name = info.tables[table_idx].name.clone();
        let offset = target_page.saturating_mul(state.rows_per_page as u64);
        let path = PathBuf::from(&state.path);
        match self
            .backend
            .db_load_page(&path, &table_name, offset, state.rows_per_page)
        {
            Ok(page) => {
                if let Some(s) = self.db_preview_state.as_mut() {
                    s.page = target_page;
                    s.current_rows = page.rows;
                    s.recompute_layout(&info);
                }
                self.preview_scroll = 0;
            }
            Err(e) => {
                self.toasts
                    .push(Toast::warn(format!("sqlite page load failed: {e}")));
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
        // Debounce: stash the path + deadline and let `tick` kick the
        // worker once the scrubbing settles. Holding ↓ across 20 PNGs
        // used to enqueue 20 full decodes (generation tokens dropped
        // the 19 stale *results*, but the worker already did the work).
        // With the window in place we do one decode for the final stop.
        self.preview_schedule = Some((rel_path, Instant::now() + PREVIEW_DEBOUNCE));
        // Cancel any pending neighbor prefetch — the user is on the
        // move, don't warm caches for files they'll flip past.
        self.prefetch_schedule = None;
    }

    /// Immediate-dispatch path for `load_preview_for_path`. Callers that
    /// have a specific path already loaded and want to refresh it RIGHT
    /// NOW (fs-watcher refresh of the currently-displayed file, where
    /// there's no scrubbing in play) bypass the debounce window.
    fn dispatch_preview_load(&mut self, rel_path: PathBuf) {
        let generation = self.preview_load.begin();
        self.preview_in_flight_path = Some(rel_path.clone());
        // Skip the image decode when we can't render pixels anyway —
        // the worker will return a metadata-only `ImagePreview` with
        // dims + format + size, and the render path shows the
        // "image preview unavailable" card instead. Saves 50-200 ms
        // per PNG on non-graphics terminals (legacy Terminal.app, SSH,
        // `REEF_IMAGE_PROTOCOL=off`).
        let wants_decoded_image = self.image_picker.is_some();
        self.tasks.load_preview(
            generation,
            Arc::clone(&self.backend),
            rel_path,
            self.theme.is_dark,
            wants_decoded_image,
        );
    }

    /// Pull any completed resize responses from the background
    /// resize worker and merge them into the current protocol. Drops
    /// stale responses whose id doesn't match (the `ThreadProtocol`
    /// bumps its id each time it dispatches, so a resize for an older
    /// selection arrives as a no-op after the user already switched).
    fn drain_preview_resize_responses(&mut self) {
        while let Ok(resp) = self.preview_resize_rx.try_recv() {
            if let Some(proto) = self.preview_image_protocol.as_mut() {
                proto.update_resized_protocol(resp);
            }
        }
    }

    /// Pick up freshly-built `StatefulProtocol`s and slot them into the
    /// current `ThreadProtocol`. A result is only applied when its
    /// generation matches the current in-flight `preview_load` — if
    /// the user switched files before the build completed, the build
    /// is stale and gets dropped; the next `BuiltProtocol` arriving
    /// with the new generation wins.
    fn drain_preview_protocol_builds(&mut self) {
        while let Ok(built) = self.preview_build_rx.try_recv() {
            if built.generation != self.preview_load.generation {
                continue;
            }
            if let Some(proto) = self.preview_image_protocol.as_mut() {
                proto.replace_protocol(built.protocol);
            }
        }
    }

    /// Fire the deferred neighbor prefetch if the user has stayed on
    /// the same file for `PREFETCH_DELAY`. Any `load_preview_for_path`
    /// cancels the schedule, so this never fires during active
    /// scrubbing — the preview worker stays clear for the user's
    /// actual next `LoadPreview`.
    fn drain_prefetch_schedule(&mut self) {
        let Some(deadline) = self.prefetch_schedule else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        self.prefetch_schedule = None;
        if self.preview_schedule.is_some() {
            return;
        }
        self.prefetch_preview_neighbors();
    }

    /// Fire any debounced preview request whose deadline has elapsed.
    /// Called from `tick`. Uses the STORED path, not the current
    /// selection — if the user scrubbed past this file already, they'll
    /// have replaced the schedule with the new path themselves.
    fn drain_preview_schedule(&mut self) {
        let Some((_, deadline)) = self.preview_schedule.as_ref() else {
            return;
        };
        if Instant::now() < *deadline {
            return;
        }
        let (path, _) = self.preview_schedule.take().expect("checked above");
        self.dispatch_preview_load(path);
    }

    /// After a preview lands, warm the cache for the next/prev file in
    /// the tree. The decode cost is paid on an idle preview worker so
    /// by the time the user presses ↓↑ again, `backend.load_preview`
    /// hits the cache instead of running a fresh 50-200 ms decode.
    ///
    /// Only runs on the Files tab and when no modal (tree edit, place
    /// mode, context menu, delete confirm) owns input — prefetching
    /// during editing would queue pointless work.
    fn prefetch_preview_neighbors(&self) {
        if self.active_tab != Tab::Files {
            return;
        }
        if self.place_mode.active
            || self.tree_edit.active
            || self.tree_context_menu.active
            || self.tree_delete_confirm.is_some()
        {
            return;
        }
        let sel = self.file_tree.selected;
        let entries = &self.file_tree.entries;
        if entries.is_empty() || sel >= entries.len() {
            return;
        }
        let candidates = [
            sel.checked_sub(1),
            (sel + 1 < entries.len()).then_some(sel + 1),
        ];
        let wants_decoded_image = self.image_picker.is_some();
        for idx in candidates.into_iter().flatten() {
            let entry = &entries[idx];
            if entry.is_dir {
                continue;
            }
            self.tasks.prefetch_preview(
                Arc::clone(&self.backend),
                entry.path.clone(),
                self.theme.is_dark,
                wants_decoded_image,
            );
        }
    }

    pub fn select_file(&mut self, path: &str, is_staged: bool) {
        self.selected_file = Some(SelectedFile {
            path: path.to_string(),
            is_staged,
        });
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
        self.clear_diff_selection();
        self.load_diff();
    }

    pub fn load_diff(&mut self) {
        let Some(sel) = self.selected_file.clone() else {
            self.diff_content = None;
            return;
        };
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            self.diff_content = None;
            return;
        };
        let context = match self.diff_mode {
            DiffMode::FullFile => 9999,
            DiffMode::Compact => 3,
        };
        let generation = self.diff_load.begin();
        self.tasks.load_diff(
            generation,
            Arc::clone(&self.backend),
            repo_root_rel,
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
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
        // Unified↔SBS row counts differ (SBS pairs adjacent - / + lines),
        // so a selection anchored in one layout doesn't map into the other.
        self.clear_diff_selection();
        save_prefs(self.diff_layout, self.diff_mode);
    }

    pub fn toggle_diff_mode(&mut self) {
        self.diff_mode = match self.diff_mode {
            DiffMode::Compact => DiffMode::FullFile,
            DiffMode::FullFile => DiffMode::Compact,
        };
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
        self.clear_diff_selection();
        self.load_diff();
        save_prefs(self.diff_layout, self.diff_mode);
    }

    pub fn stage_file(&mut self, path: &str) {
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let ok = self.backend.stage_for(&repo_root_rel, path).is_ok();
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
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let ok = self.backend.unstage_for(&repo_root_rel, path).is_ok();
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
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let paths: Vec<String> = self.unstaged_files.iter().map(|f| f.path.clone()).collect();
        for p in &paths {
            let _ = self.backend.stage_for(&repo_root_rel, p);
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
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let paths: Vec<String> = self.staged_files.iter().map(|f| f.path.clone()).collect();
        for p in &paths {
            let _ = self.backend.unstage_for(&repo_root_rel, p);
        }
        if let Some(ref mut sel) = self.selected_file {
            if paths.iter().any(|p| p == &sel.path) {
                sel.is_staged = false;
            }
        }
        self.refresh_status();
        self.load_diff();
    }

    pub fn stage_folder(&mut self, folder_path: &str) {
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let paths: Vec<String> = self
            .unstaged_files
            .iter()
            .filter(|f| folder_contains(folder_path, &f.path))
            .map(|f| f.path.clone())
            .collect();
        for p in &paths {
            let _ = self.backend.stage_for(&repo_root_rel, p);
        }
        if let Some(ref mut sel) = self.selected_file {
            if paths.iter().any(|p| p == &sel.path) {
                sel.is_staged = true;
            }
        }
        self.refresh_status();
        self.load_diff();
    }

    pub fn unstage_folder(&mut self, folder_path: &str) {
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let paths: Vec<String> = self
            .staged_files
            .iter()
            .filter(|f| folder_contains(folder_path, &f.path))
            .map(|f| f.path.clone())
            .collect();
        for p in &paths {
            let _ = self.backend.unstage_for(&repo_root_rel, p);
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
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return touched;
        };
        // Discard still uses the original Git semantics; the only multi-repo
        // addition is selecting which repository root those operations run in.
        // We ignore errors (matches the pre-M4 `let _ = repo.…` pattern):
        // the refresh_status + load_diff that follow will reflect whatever
        // actually landed on disk, and a partial failure on one path in a
        // folder discard shouldn't block the rest.
        match target {
            DiscardTarget::File(path) => {
                let _ =
                    self.backend
                        .revert_path_for(&repo_root_rel, path, /*is_staged=*/ false);
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
                        let _ = self.backend.revert_path_for(&repo_root_rel, &p, *is_staged);
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
                    let _ = self.backend.revert_path_for(&repo_root_rel, &p, *is_staged);
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
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            self.git_graph.rows.clear();
            self.git_graph.ref_map.clear();
            self.git_graph.cache_key = None;
            return;
        };
        let generation = self.graph_load.begin();
        self.tasks.refresh_graph(
            generation,
            Arc::clone(&self.backend),
            repo_root_rel,
            GRAPH_COMMIT_LIMIT,
        );
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
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            self.commit_detail.detail = None;
            return;
        };
        let generation = self.commit_detail_load.begin();
        self.tasks
            .load_commit_detail(generation, Arc::clone(&self.backend), repo_root_rel, oid);
    }

    /// (Re)load the range-mode payload for the current Shift-extended
    /// selection. Fills per-commit metadata synchronously from the cached
    /// `rows` slice (no git walk needed) and dispatches the file-list
    /// computation — `parent(oldest).tree → newest.tree` — to the graph
    /// worker. No-ops when the selection is actually a single row.
    pub fn load_commit_range_detail(&mut self) {
        self.commit_detail.file_diff = None;
        self.commit_detail.diff_h_scroll = 0;
        self.commit_detail.sbs_left_h_scroll = 0;
        self.commit_detail.sbs_right_h_scroll = 0;
        if !self.git_graph.is_range() {
            self.commit_detail.range_detail = None;
            return;
        }
        let (lo, hi) = self.git_graph.selected_range();
        let Some(oldest_row) = self.git_graph.rows.get(hi) else {
            self.commit_detail.range_detail = None;
            return;
        };
        let Some(newest_row) = self.git_graph.rows.get(lo) else {
            self.commit_detail.range_detail = None;
            return;
        };
        // `rows` is newest-first (revwalk order), so the higher index is the
        // chronologically older commit; `parent(oldest)` is the baseline.
        let oldest_oid = oldest_row.commit.oid.clone();
        let newest_oid = newest_row.commit.oid.clone();
        let commits: Vec<CommitInfo> = self.git_graph.rows[lo..=hi]
            .iter()
            .map(|r| r.commit.clone())
            .collect();
        let commit_count = commits.len();
        // Seed with empty files so the panel has something to render while
        // the worker computes the union list. Worker result replaces it on
        // arrival via generation match.
        self.commit_detail.range_detail = Some(RangeDetail {
            oldest_oid: oldest_oid.clone(),
            newest_oid: newest_oid.clone(),
            commit_count,
            commits,
            files: Vec::new(),
        });
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let generation = self.commit_detail_load.begin();
        self.tasks.load_commit_range_detail(
            generation,
            Arc::clone(&self.backend),
            repo_root_rel,
            oldest_oid,
            newest_oid,
        );
    }

    /// Dispatch the correct loader based on the current selection shape.
    /// Single-commit selection → `load_commit_detail`; range selection →
    /// `load_commit_range_detail`. Callers who previously called
    /// `load_commit_detail` directly on selection change should switch to
    /// this so range mode is exercised automatically.
    pub fn reload_graph_selection(&mut self) {
        if self.git_graph.is_range() {
            self.commit_detail.detail = None;
            self.load_commit_range_detail();
        } else {
            self.commit_detail.range_detail = None;
            self.load_commit_detail();
        }
    }

    /// Load the inline diff for a file inside the currently-selected commit
    /// or commit range. Routes to range-file-diff plumbing when a range is
    /// active so the diff baseline matches the file list.
    ///
    /// In 3-col mode the right column owns the diff, so picking a file also
    /// moves focus there — the user's next arrow-key pans the viewport
    /// instead of scrolling the commit metadata they were already looking at.
    pub fn load_commit_file_diff(&mut self, path: &str) {
        // Move focus to the diff column so scroll keys target it after the
        // click. Only when we're actually on the Graph tab — some paths
        // (search restore) call this while on other tabs.
        if self.active_tab == Tab::Graph && self.last_total_width >= Self::GRAPH_THREE_COL_MIN_WIDTH
        {
            self.active_panel = Panel::Diff;
        }
        let context = match self.commit_detail.diff_mode {
            DiffMode::Compact => 3,
            DiffMode::FullFile => 9999,
        };
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            self.commit_detail.file_diff = None;
            return;
        };
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
            self.commit_detail.file_diff_scroll = 0;
            self.commit_detail.file_diff_h_scroll = 0;
            self.commit_detail.file_diff_sbs_left_h_scroll = 0;
            self.commit_detail.file_diff_sbs_right_h_scroll = 0;
            self.clear_diff_selection();
        }
        if self.git_graph.is_range() {
            let Some(range) = self.commit_detail.range_detail.as_ref() else {
                self.commit_detail.file_diff = None;
                return;
            };
            let (oldest, newest) = (range.oldest_oid.clone(), range.newest_oid.clone());
            let generation = self.commit_file_diff_load.begin();
            self.tasks.load_range_file_diff(
                generation,
                Arc::clone(&self.backend),
                repo_root_rel,
                oldest,
                newest,
                path.to_string(),
                context,
                self.theme.is_dark,
            );
            return;
        }
        let Some(oid) = self.git_graph.selected_commit.clone() else {
            self.commit_detail.file_diff = None;
            return;
        };
        let generation = self.commit_file_diff_load.begin();
        self.tasks.load_commit_file_diff(
            generation,
            Arc::clone(&self.backend),
            repo_root_rel,
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

    /// Move the graph selection by `delta` rows (clamped). Clears any
    /// Shift-anchor (plain nav drops range mode) and reloads the single
    /// commit detail.
    pub fn move_graph_selection(&mut self, delta: i32) {
        if self.git_graph.rows.is_empty() {
            return;
        }
        // Plain navigation always collapses to single-select. `anchor=None`
        // means `reload_graph_selection()` below (and the cursor-didn't-move
        // branch) always take the single-commit path — we call
        // `load_commit_detail()` directly to skip that dispatch hop.
        self.git_graph.selection_anchor = None;
        let last = self.git_graph.rows.len() - 1;
        let current = self.git_graph.selected_idx as i32;
        let next = (current + delta).clamp(0, last as i32) as usize;
        if next == self.git_graph.selected_idx {
            // Edge-clamp with a stale range still visible (user was in
            // visual mode, pressed plain ↓ at the bottom). Clear the
            // range payload so the panel snaps back to single-commit.
            if self.commit_detail.range_detail.is_some() {
                self.commit_detail.range_detail = None;
                self.load_commit_detail();
            }
            return;
        }
        self.git_graph.selected_idx = next;
        self.git_graph.selected_commit =
            self.git_graph.rows.get(next).map(|r| r.commit.oid.clone());
        self.commit_detail.scroll = 0;
        self.commit_detail.range_detail = None;
        self.load_commit_detail();
    }

    /// Shift-extend the selection by `delta` rows. Sets the anchor to the
    /// current cursor on first call, then moves the cursor; the range always
    /// spans `[min(anchor, cursor), max(anchor, cursor)]`. When the cursor
    /// collapses back onto the anchor the selection normalises to single.
    pub fn extend_graph_selection(&mut self, delta: i32) {
        if self.git_graph.rows.is_empty() {
            return;
        }
        if self.git_graph.selection_anchor.is_none() {
            self.git_graph.selection_anchor = Some(self.git_graph.selected_idx);
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
        self.commit_detail.scroll = 0;
        self.reload_graph_selection();
    }

    /// Drop any Shift-anchor, collapsing to single-select on the current
    /// cursor. No-op when not in range mode.
    pub fn clear_graph_range(&mut self) {
        if self.git_graph.selection_anchor.take().is_some() {
            self.commit_detail.scroll = 0;
            self.commit_detail.range_detail = None;
            self.reload_graph_selection();
        }
    }

    /// Kick off a `git commit` in the background. Snapshots the current
    /// draft message, hands it to a worker thread, and returns. Empty /
    /// whitespace-only drafts are rejected client-side (same contract as
    /// the VSCode SCM "Commit" button, which is disabled until the
    /// input has content) so the worker never hits the `commit_at`
    /// guard. A commit already in flight drops the request to avoid
    /// racing pre-commit hooks against a concurrent run.
    pub fn run_commit(&mut self) {
        if self.commit_in_flight {
            return;
        }
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let message = self.git_status.commit_message.trim().to_string();
        if message.is_empty() {
            return;
        }
        // Nothing staged → fail fast with an in-panel banner rather than
        // hitting `git commit` for a "nothing to commit" error
        // response. Matches VSCode behaviour (Commit button is disabled
        // when the staged tree is empty; we don't emulate auto-stage-on-
        // commit yet).
        if self.staged_files.is_empty() {
            self.git_status.commit_error =
                Some(crate::i18n::t(crate::i18n::Msg::CommitNothingStaged).to_string());
            return;
        }
        let backend = Arc::clone(&self.backend);
        let (tx, rx) = mpsc::channel();
        self.commit_rx = Some(rx);
        self.commit_in_flight = true;
        std::thread::spawn(move || {
            let result = backend
                .commit_for(&repo_root_rel, &message)
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    /// Called from `tick()`. Folds the commit worker's result into App
    /// state — success clears the draft, pops a toast, and marks caches
    /// stale (HEAD moved, graph cache key is now wrong); failure lands
    /// in `git_status.commit_error` as a banner so the user can read
    /// hook output without losing their draft.
    fn drain_commit_result(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = self.commit_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.commit_in_flight = false;
                self.commit_rx = None;
                match result {
                    Ok(()) => {
                        use crate::i18n::{Msg, t};
                        self.git_status.commit_error = None;
                        self.git_status.commit_message.clear();
                        self.git_status.commit_cursor = 0;
                        self.git_status.commit_editing = false;
                        // Previously-selected file is almost certainly no
                        // longer in staged/unstaged after the commit —
                        // reload the diff so the right pane doesn't
                        // linger on stale hunks until the next nav.
                        self.load_diff();
                        self.toasts.push(Toast::info(t(Msg::CommitSuccess)));
                    }
                    Err(e) => {
                        self.git_status.commit_error = Some(e.clone());
                        self.toasts
                            .push(Toast::error(crate::i18n::commit_failed_toast(&e)));
                    }
                }
                // A new commit advances HEAD — bust the graph cache and
                // mark status stale so the new HEAD / ahead-behind land
                // on the next coordinator pass. Mirrors
                // `drain_push_result`.
                self.git_graph.cache_key = None;
                self.git_status_load.mark_stale();
                self.graph_load.mark_stale();
            }
            Err(TryRecvError::Empty) => {
                // Worker still running.
            }
            Err(TryRecvError::Disconnected) => {
                self.commit_in_flight = false;
                self.commit_rx = None;
                self.toasts.push(Toast::error(crate::i18n::t(
                    crate::i18n::Msg::CommitThreadCrashed,
                )));
            }
        }
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
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        let backend = Arc::clone(&self.backend);
        let (tx, rx) = mpsc::channel();
        self.push_rx = Some(rx);
        self.push_in_flight = true;
        std::thread::spawn(move || {
            let result = backend
                .push_for(&repo_root_rel, force)
                .map_err(|e| e.to_string());
            // Recv side may have been dropped by the time we finish (e.g.
            // user quit mid-push); ignore the send error.
            let _ = tx.send((force, result));
        });
    }

    pub fn checkout_branch(&mut self, branch: &str) {
        let branch = branch.trim();
        if branch.is_empty() || branch == self.branch_name {
            return;
        }
        let Some(repo_root_rel) = self.status_repo_root_rel() else {
            return;
        };
        match self.backend.checkout_branch_for(&repo_root_rel, branch) {
            Ok(()) => {
                self.selected_file = None;
                self.diff_content = None;
                self.git_status.confirm_discard = None;
                self.clear_graph_snapshot();
                self.refresh_status();
                self.graph_load.invalidate_stale();
            }
            Err(e) => {
                self.toasts
                    .push(Toast::error(format!("Checkout failed: {e}")));
            }
        }
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
            WorkerResult::RepoCatalog { generation, result } => match result {
                Ok(resp) => {
                    if self.repo_catalog.discover_load.complete_ok(generation) {
                        let selected_before = self.repo_catalog.selected_git_repo.clone();
                        let effective_before = selected_before
                            .clone()
                            .or_else(|| self.backend.has_repo().then(|| PathBuf::from(".")));
                        self.repo_catalog.repos = resp.repos;
                        self.repo_catalog.truncated = resp.truncated;
                        let reconciled = self.repo_catalog.reconcile_selected_repo();
                        if selected_before.is_none() {
                            self.repo_catalog.auto_select_repo();
                        }
                        if let Some((previous, selected)) = reconciled {
                            self.toasts.push(Toast::info(format!(
                                "Saved repository selection '{}' is no longer available",
                                crate::backend::repo_key(&previous)
                            )));
                            if let Some(selected) = selected.as_ref() {
                                crate::prefs::set(
                                    SELECTED_GIT_REPO_PREF,
                                    &crate::backend::repo_key(selected),
                                );
                            } else {
                                crate::prefs::remove(SELECTED_GIT_REPO_PREF);
                            }
                        }
                        if self.repo_catalog.selected_git_repo != selected_before {
                            if self.status_repo_root_rel() != effective_before {
                                self.clear_git_status_snapshot();
                            }
                            self.clear_graph_snapshot();
                            self.refresh_status();
                            self.graph_load.invalidate_stale();
                        }
                    }
                }
                Err(error) => {
                    self.repo_catalog
                        .discover_load
                        .complete_err(generation, error);
                }
            },
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
                Ok(mut content) => {
                    if self.preview_load.complete_ok(generation) {
                        // Worker has landed — the loading indicator can stop
                        // pointing at the in-flight path.
                        self.preview_in_flight_path = None;
                        let same_file = matches!(
                            (self.preview_content.as_ref(), content.as_ref()),
                            (Some(old), Some(new)) if old.file_path == new.file_path
                        );
                        // Decide the protocol fate in three buckets:
                        //
                        // 1. Same-file re-load where old and new are both
                        //    images with identical (bytes_on_disk, w, h,
                        //    format) — a conservative "pixels probably
                        //    didn't change" heuristic that covers
                        //    re-selecting the same file. Keep the existing
                        //    protocol so ratatui-image doesn't re-encode
                        //    and the UI doesn't flicker.
                        // 2. Other Image bodies — build a fresh protocol
                        //    by moving the decoded `DynamicImage` out of
                        //    the worker payload (so we don't keep two
                        //    copies of the pixels alive).
                        // 3. Non-image bodies or no picker — drop any
                        //    stale protocol so we don't keep a previous
                        //    image lingering.
                        let reuse_protocol = same_file
                            && self.preview_image_protocol.is_some()
                            && matches!(
                                (
                                    self.preview_content.as_ref().map(|c| &c.body),
                                    content.as_ref().map(|c| &c.body),
                                ),
                                (
                                    Some(crate::file_tree::PreviewBody::Image(old)),
                                    Some(crate::file_tree::PreviewBody::Image(new)),
                                ) if old.bytes_on_disk == new.bytes_on_disk
                                    && old.width_px == new.width_px
                                    && old.height_px == new.height_px
                                    && old.format == new.format
                            );
                        if !reuse_protocol {
                            // Two-step protocol swap-in:
                            //   1. Immediately install an EMPTY
                            //      ThreadProtocol so render can already
                            //      enter the image branch (it no-ops on
                            //      the image area until inner lands).
                            //   2. Spawn a one-shot thread to run
                            //      `Picker::new_resize_protocol` — that
                            //      call hashes the full decoded image
                            //      which on a 2048² RGBA is ~16-30 ms
                            //      of main-thread work we can't afford
                            //      during a frame. Result flows back
                            //      via `preview_build_tx` and gets
                            //      merged in `drain_preview_protocol_builds`.
                            let dyn_img = match (
                                self.image_picker.as_ref(),
                                content.as_mut().map(|c| &mut c.body),
                            ) {
                                (Some(_), Some(crate::file_tree::PreviewBody::Image(img))) => {
                                    img.image.take()
                                }
                                _ => None,
                            };
                            if let (Some(dyn_img), Some(picker)) =
                                (dyn_img, self.image_picker.as_ref())
                            {
                                self.preview_image_protocol =
                                    Some(ratatui_image::thread::ThreadProtocol::new(
                                        self.preview_resize_tx.clone(),
                                        None,
                                    ));
                                self.preview_image_protocol_builds += 1;
                                let picker_clone = picker.clone();
                                let build_tx = self.preview_build_tx.clone();
                                let build_gen = generation;
                                std::thread::Builder::new()
                                    .name("reef-image-build".into())
                                    .spawn(move || {
                                        let proto = picker_clone.new_resize_protocol(dyn_img);
                                        let _ = build_tx.send(BuiltProtocol {
                                            generation: build_gen,
                                            protocol: proto,
                                        });
                                    })
                                    .ok();
                            } else {
                                // Non-image body, or no picker available.
                                self.preview_image_protocol = None;
                            }
                        } else if let Some(crate::file_tree::PreviewBody::Image(img)) =
                            content.as_mut().map(|c| &mut c.body)
                        {
                            // Drop the new DynamicImage — the kept
                            // protocol already has its own copy.
                            img.image = None;
                        }
                        self.preview_content = content;
                        if !same_file {
                            self.preview_scroll = 0;
                            self.preview_h_scroll = 0;
                            self.preview_selection = None;
                            self.preview_click_state = None;
                        }
                        // SQLite preview state hygiene. The state must be
                        // `Some` exactly when the current preview is a
                        // Database body, with `path` matching. Rebuild on
                        // every preview land that changes the file or the
                        // body shape; preserve across same-file refreshes
                        // (theme change, fs-watcher kick) so the user
                        // doesn't snap back to page 0 on an unrelated
                        // re-decode.
                        match self
                            .preview_content
                            .as_ref()
                            .map(|p| (&p.body, &p.file_path))
                        {
                            Some((PreviewBody::Database(info), path)) => {
                                let state_matches = self
                                    .db_preview_state
                                    .as_ref()
                                    .map(|s| s.path == *path)
                                    .unwrap_or(false);
                                if !state_matches {
                                    self.db_preview_state =
                                        Some(DbPreviewState::from_initial(path, info));
                                }
                            }
                            _ => {
                                self.db_preview_state = None;
                            }
                        }
                        // The goto-input is short-lived — drop it on
                        // any preview transition so a half-typed page
                        // number doesn't survive a file switch.
                        self.db_goto_input = None;
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
                        // Defer prefetch: if we fired right now, a user
                        // who presses ↓ within ~50 ms of this landing
                        // would find two prefetch decodes already
                        // queued ahead of their real `LoadPreview`.
                        // Schedule instead, and `load_preview_for_path`
                        // cancels if the user moves first.
                        if self.preview_schedule.is_none() {
                            self.prefetch_schedule = Some(Instant::now() + PREFETCH_DELAY);
                        }
                    }
                }
                Err(error) => {
                    self.preview_load.complete_err(generation, error);
                    self.preview_in_flight_path = None;
                }
            },
            WorkerResult::GitStatus { generation, result } => match result {
                Ok(payload) => {
                    if self.git_status_load.complete_ok(generation) {
                        let before = self.selected_file.clone();
                        self.staged_files = payload.staged;
                        self.unstaged_files = payload.unstaged;
                        self.git_status.ahead_behind = payload.ahead_behind;
                        self.git_status.branches = payload.branches;
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
                        // Snapshot the anchor's OID before the re-walk
                        // overwrites `rows`. The graph is revalidated every
                        // ~5s on a timer (`next_graph_revalidate_at`), so
                        // dropping the anchor here would silently exit the
                        // user's visual mode every few seconds — the
                        // "range disappears" symptom. We relocate by OID
                        // instead, same as `selected_commit` below.
                        let previous_anchor_oid = self
                            .git_graph
                            .selection_anchor
                            .and_then(|idx| self.git_graph.rows.get(idx))
                            .map(|r| r.commit.oid.clone());
                        // Detect whether the incoming payload is a no-op
                        // (HEAD + ref-hash unchanged). The worker always
                        // re-walks; this cheap comparison on the main
                        // thread lets us skip a range-detail reload — and
                        // the `file_diff = None` reset that comes with it
                        // — when nothing actually changed. Without this,
                        // visual-mode users watching a file diff saw it
                        // blink every 5s.
                        let cache_key_changed =
                            self.git_graph.cache_key.as_ref() != Some(&payload.cache_key);

                        self.git_graph.rows = payload.rows;
                        self.git_graph.ref_map = payload.ref_map;
                        self.git_graph.cache_key = Some(payload.cache_key);

                        if let Some(ref oid) = previous_commit
                            && let Some(idx) = self.git_graph.find_row_by_oid(oid)
                        {
                            self.git_graph.selected_idx = idx;
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

                        // Relocate the anchor by OID. Only clear when the
                        // anchor commit genuinely vanished from history
                        // (rebase / amend / prune rewrote it away).
                        let anchor_survived = previous_anchor_oid
                            .as_ref()
                            .and_then(|oid| self.git_graph.find_row_by_oid(oid));
                        self.git_graph.selection_anchor = anchor_survived;
                        if anchor_survived.is_none() {
                            self.commit_detail.range_detail = None;
                        }

                        // Reload the right panel only when something
                        // actually shifted. In visual mode we skip the
                        // range rebuild when cache_key matched — rows
                        // are byte-identical so the cached range_detail
                        // still describes the same slice. Single-select
                        // keeps its pre-existing short-circuit on
                        // selected_commit identity.
                        if self.git_graph.selection_anchor.is_some() {
                            if cache_key_changed {
                                self.reload_graph_selection();
                            }
                        } else if self.git_graph.selected_commit != previous_commit
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
            WorkerResult::RangeDetail { generation, result } => match result {
                Ok(files) => {
                    if self.commit_detail_load.complete_ok(generation) {
                        if let Some(rd) = self.commit_detail.range_detail.as_mut() {
                            rd.files = files;
                        }
                    }
                }
                Err(error) => {
                    self.commit_detail_load.complete_err(generation, error);
                }
            },
            WorkerResult::RangeFileDiff { generation, result } => match result {
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
        // Preview selection is scoped to the Files/Search tabs that render
        // the preview panel. Switching away clears it so no stale highlight
        // appears on return and the click-count resets cleanly.
        self.preview_selection = None;
        self.preview_click_state = None;
        // Same for diff-panel selection — the Git tab and the Graph tab
        // 3-col diff column share this state, and tab-switching between
        // them (or to Files/Search) should start fresh.
        self.clear_diff_selection();
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
            ClickAction::ToggleSidebar => {
                self.toggle_sidebar();
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
            ClickAction::StartDragGraphDiffSplit => {
                self.dragging_graph_diff_split = true;
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
            ClickAction::DbPrevPage => {
                self.db_navigate(DbNav::PrevPage);
            }
            ClickAction::DbNextPage => {
                self.db_navigate(DbNav::NextPage);
            }
            ClickAction::DbGotoPage(page) => {
                self.db_navigate_to_page(page);
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
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
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
            self.repo_catalog.discover_load.mark_stale();
            // Mark the quick-open index stale so the next palette open picks up
            // the new/deleted files. Rebuilding immediately on every fs
            // event would be wasteful for a palette the user may not open.
            crate::quick_open::mark_stale(&mut self.quick_open);
        }

        self.maybe_kick_global_search();
        self.drain_preview_sync_debounce();
        self.drain_preview_schedule();
        self.drain_prefetch_schedule();
        self.drain_preview_resize_responses();
        self.drain_preview_protocol_builds();
        self.drain_push_result();
        self.drain_commit_result();
        self.kick_active_tab_work();
        self.tick_place_mode_auto_expand();
        self.tick_tree_drag_auto_expand();
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

        if self.repo_catalog.discover_load.should_request() {
            self.refresh_repo_catalog();
        }

        match self.active_tab {
            Tab::Files => {
                if self.preview_load.should_request() {
                    self.load_preview();
                }
            }
            Tab::Git => {
                let has_repo = self.status_repo_root_rel().is_some();
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
                let has_repo = self.status_repo_root_rel().is_some();
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
    use super::{GitGraphState, folder_contains};

    #[test]
    fn graph_state_selected_range_single_without_anchor() {
        let g = GitGraphState {
            selected_idx: 5,
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (5, 5));
        assert!(!g.is_range());
    }

    #[test]
    fn graph_state_selected_range_collapsed_anchor_is_single() {
        // `V` just entered visual mode — anchor sits on the cursor.
        let g = GitGraphState {
            selected_idx: 3,
            selection_anchor: Some(3),
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (3, 3));
        assert!(!g.is_range());
    }

    #[test]
    fn graph_state_selected_range_anchor_above_cursor() {
        // (lo, hi) is always sorted — cursor can be above or below anchor.
        let g = GitGraphState {
            selected_idx: 2,
            selection_anchor: Some(7),
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (2, 7));
        assert!(g.is_range());
    }

    #[test]
    fn graph_state_selected_range_anchor_below_cursor() {
        let g = GitGraphState {
            selected_idx: 9,
            selection_anchor: Some(4),
            ..Default::default()
        };
        assert_eq!(g.selected_range(), (4, 9));
        assert!(g.is_range());
    }

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

    // ── Graph layout math ────────────────────────────────────────────────

    use super::{Tab, compute_sidebar_width, compute_three_col_widths, compute_uses_three_col};

    // ── graph_uses_three_col switch matrix ───────────────────────────────

    #[test]
    fn uses_three_col_needs_graph_tab() {
        // Exact same inputs, only the tab changes — 3-col is Graph-only.
        assert!(compute_uses_three_col(Tab::Graph, 200, true, false));
        assert!(!compute_uses_three_col(Tab::Git, 200, true, false));
        assert!(!compute_uses_three_col(Tab::Files, 200, true, false));
        assert!(!compute_uses_three_col(Tab::Search, 200, true, false));
    }

    #[test]
    fn uses_three_col_needs_min_width() {
        // 99 cols → below `GRAPH_THREE_COL_MIN_WIDTH` (100) → 2-col fallback.
        assert!(!compute_uses_three_col(Tab::Graph, 99, true, false));
        assert!(compute_uses_three_col(Tab::Graph, 100, true, false));
    }

    #[test]
    fn uses_three_col_needs_file_diff_or_loading() {
        // Graph tab + wide terminal but nothing to show in the diff column.
        assert!(!compute_uses_three_col(Tab::Graph, 200, false, false));
        // Either flag on activates 3-col.
        assert!(compute_uses_three_col(Tab::Graph, 200, true, false));
        assert!(compute_uses_three_col(Tab::Graph, 200, false, true));
        // Loading-in-flight during a file click: 3-col stays active so the
        // "loading…" banner renders where the diff will land instead of
        // flashing 2-col for a frame.
    }

    #[test]
    fn sidebar_width_clamps_min_10() {
        // split_percent = 5 → raw = 5 → floor at 10.
        assert_eq!(compute_sidebar_width(100, 5), 10);
    }

    #[test]
    fn sidebar_width_clamps_to_leave_20_for_right() {
        // split_percent = 95 on width 100 → raw = 95, but right panel
        // needs at least 20 cols → clamp to 80.
        assert_eq!(compute_sidebar_width(100, 95), 80);
    }

    #[test]
    fn sidebar_width_passes_mid_range_through() {
        // Default 30% on a generous terminal.
        assert_eq!(compute_sidebar_width(200, 30), 60);
    }

    #[test]
    fn three_col_widths_sum_to_total() {
        // Regression guard: if the rounding ever changes, the assert below
        // pins the invariant `graph + commit + diff == total_width`. Hit-
        // testing + h-scroll routing rely on this exactly.
        let (g, c, d) = compute_three_col_widths(200, 60, 60);
        assert_eq!(g + c + d, 200);
    }

    #[test]
    fn three_col_widths_diff_floor_20() {
        // graph_diff_split_percent = 0 shouldn't squeeze diff to 0 —
        // `.max(20)` keeps it usable.
        let (_, _, d) = compute_three_col_widths(200, 60, 0);
        assert!(d >= 20, "diff col floored at 20, got {d}");
    }

    #[test]
    fn three_col_widths_commit_floor_20() {
        // graph_diff_split_percent = 100 shouldn't squeeze commit to 0 —
        // `.min(remainder - 20)` leaves commit at least 20 cols.
        let (_, c, _) = compute_three_col_widths(200, 60, 100);
        assert!(c >= 20, "commit col floored at 20, got {c}");
    }

    #[test]
    fn three_col_widths_default_proportions() {
        // Default tuning: sidebar_w=60 (30% of 200), graph_diff_split_percent=60
        // on a 200-wide terminal. Sanity-check the split feels right.
        let (g, c, d) = compute_three_col_widths(200, 60, 60);
        assert_eq!(g, 60);
        // remainder = 140, diff = 60% of 140 = 84, commit = 56.
        assert_eq!(d, 84);
        assert_eq!(c, 56);
    }

    #[test]
    fn three_col_widths_at_min_terminal_width() {
        // At the 100-col 3-col threshold the floors kick in hard:
        // graph = 30, remainder = 70, diff = 42 (rounded from 60% * 70),
        // commit = 28. All columns remain ≥ 20.
        let (g, c, d) = compute_three_col_widths(100, 30, 60);
        assert_eq!(g + c + d, 100);
        assert!(c >= 20 && d >= 20, "both right cols ≥ 20: c={c} d={d}");
    }

    #[test]
    fn three_col_widths_share_sidebar_with_2col() {
        // Whether the UI ends up in 2-col or 3-col, the graph sidebar
        // width is the same value. `graph_diff_column_start` and
        // `focus_panel_under_cursor` depend on this.
        let sidebar_2 = compute_sidebar_width(150, 30);
        let (graph_3, _, _) = compute_three_col_widths(150, sidebar_2, 60);
        assert_eq!(sidebar_2, graph_3);
    }

    #[test]
    fn three_col_widths_redistribute_when_sidebar_zero() {
        // Sidebar hidden → sidebar_w=0; commit and diff fill the full
        // width using `graph_diff_split_percent`. Diff stays 60% of 200
        // and commit takes the rest, instead of inheriting the narrow
        // commit_w computed off the (now nonexistent) sidebar slot.
        let (g, c, d) = compute_three_col_widths(200, 0, 60);
        assert_eq!(g, 0);
        assert_eq!(g + c + d, 200);
        assert_eq!(d, 120);
        assert_eq!(c, 80);
    }
}
