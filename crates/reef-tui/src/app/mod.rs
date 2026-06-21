use crate::file_tree::FileTree;
use crate::tasks::{AsyncState, TaskCoordinator, WorkerResult};
use crate::ui::confirm_modal::ConfirmModal;
use crate::ui::highlight::StyledToken;
use crate::ui::mouse::{ClickAction, HitTestRegistry};
use crate::ui::theme::Theme;
use crate::ui::toast::Toast;
use reef_core::diff::{DiffContent, DiffDisplay, DiffLayout};
use reef_core::git::graph::GraphRow;
use reef_core::git::tree as git_tree;
use reef_core::git::{CommitDetail, CommitInfo, FileEntry, GitRepo, GraphScope, RefLabel};
use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent};
use reef_io::{Backend, LocalBackend};
use std::collections::{HashMap, HashSet};
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Code-navigation request side (gd / gr / Ctrl+click / nav stack /
/// LSP refine / post-jump highlight). A child `impl App` block, kept in
/// its own file so the ~900-line subsystem doesn't bloat this module.
pub mod nav;

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

/// Pagination + object-selection state for the SQLite preview card.
/// Lives `Some` for as long as the current `preview_content` is a
/// `PreviewBody::Database`; rebuilt from `info.initial_page` whenever
/// a new preview lands and the file changed (see
/// `apply_worker_result`).
///
/// **Cache invariant**: `col_widths` + `total_table_w` are derived
/// from `(selection, current_rows)`. Any mutation of those two
/// fields must call [`Self::recompute_layout`] before the next
/// render, or the cached widths will desync from the data and the
/// table will visually misalign. Every mutation site in this file
/// honors that — when adding new ones, follow suit.
///
/// `current_rows` is the rows shown right now. On `[`/`]`/`PgUp`/`PgDn`
/// we re-issue `Backend::db_load_page_qualified` synchronously and
/// replace this vec on success. SQLite's open + LIMIT/OFFSET is
/// sub-millisecond locally; over SSH it's an RPC round-trip (~10-50 ms
/// typical) — a brief stall on flaky links is the accepted trade-off
/// for keeping the navigation path simple.
#[derive(Debug, Clone)]
pub struct DbPreviewState {
    /// Workdir-relative path of the SQLite file the state belongs to.
    /// Compared against `preview_content.path` on every render to
    /// catch the "file changed but state didn't get cleared" race.
    pub path: String,
    /// Currently selected object addressed by schema-qualified key.
    pub selection: reef_sqlite_preview::DbObjectKey,
    /// Schemas the sidebar currently shows expanded.
    pub expanded: std::collections::BTreeSet<String>,
    /// Zero-based page index. `offset = page * rows_per_page`.
    pub page: u64,
    /// The rows currently visible. Each inner Vec is one row's cells
    /// in column order; length equals
    /// `min(rows_per_page, object.row_count - offset)`.
    pub current_rows: Vec<Vec<reef_sqlite_preview::SqliteValue>>,
    pub rows_per_page: u32,
    /// Cached natural column widths for the current
    /// `(selection, current_rows)` combo. Recomputed by
    /// [`Self::recompute_layout`] on every mutation that changes
    /// either input. The render path consults the cache once per
    /// frame instead of re-walking 50 rows × N columns of UTF-8
    /// width math on every keystroke during h-scroll.
    pub col_widths: Vec<usize>,
    /// Cached `Σcol_widths + (n−1)·sep_w` paired with `col_widths`.
    /// Used as the upper bound when clamping `preview_h_scroll`.
    pub total_table_w: usize,
    /// Detail-pane payload for the currently-selected index / trigger,
    /// or `None` when the current selection is a table / view (data
    /// grid takes over) or when the detail RPC hasn't landed yet.
    pub detail: Option<reef_sqlite_preview::DbObjectDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbNav {
    PrevPage,
    NextPage,
    /// Previous row-bearing object in the current schema's flat list.
    PrevTable,
    /// Next row-bearing object in the current schema's flat list.
    NextTable,
    FirstPage,
    LastPage,
}

impl DbPreviewState {
    pub fn from_initial(path: &str, info: &reef_sqlite_preview::DatabaseInfoV2) -> Self {
        // `default_object` is None only when every schema is empty —
        // fall back to a synthetic key on the default schema so the
        // struct is well-formed; renderers branch on `info.lookup()`
        // returning None to draw the empty state.
        let selection =
            info.default_object
                .clone()
                .unwrap_or_else(|| reef_sqlite_preview::DbObjectKey {
                    schema: info.default_schema.clone(),
                    name: String::new(),
                    kind: reef_sqlite_preview::DbObjectKind::Table,
                });
        let mut expanded = std::collections::BTreeSet::new();
        expanded.insert(info.default_schema.clone());
        let columns: &[reef_sqlite_preview::ColumnInfo] = info
            .lookup(&selection)
            .map(|o| o.columns.as_slice())
            .unwrap_or(&[]);
        let mut s = Self {
            path: path.to_string(),
            selection,
            expanded,
            page: 0,
            current_rows: info.initial_page.rows.clone(),
            rows_per_page: reef_core::preview::INITIAL_DB_PAGE_ROWS,
            col_widths: Vec::new(),
            total_table_w: 0,
            detail: None,
        };
        s.recompute_layout(columns);
        s
    }

    /// Refresh `col_widths` + `total_table_w` from the current
    /// `(columns, current_rows)` combo. Cheap on its own; called by
    /// nav helpers right after they swap in fresh rows. Render path
    /// only reads the cached values, never recomputes.
    pub fn recompute_layout(&mut self, columns: &[reef_sqlite_preview::ColumnInfo]) {
        self.col_widths = crate::ui::db_preview::natural_column_widths(columns, &self.current_rows);
        self.total_table_w = crate::ui::db_preview::total_table_width(&self.col_widths);
    }
}

/// Largest valid page index for an object at a given page size.
/// Objects without a populated `row_count` (views, indexes, triggers)
/// floor to 0 — pagination only meaningful when we know the total.
fn max_page_for_object(object: &reef_sqlite_preview::DbObject, page_size: u32) -> u64 {
    if page_size == 0 {
        return 0;
    }
    let rows = object.row_count.unwrap_or(0);
    let pages = rows.div_ceil(page_size as u64);
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

/// `Settings` is a full-screen takeover that hides the tab bar; the
/// background work coordinator keeps running so the four-tab snapshots
/// are fresh on return.
///
/// `FocusedPreview` is the "纯预览" mode — same full-screen takeover
/// pattern as Settings, but the body is the active tab's preview/diff
/// panel maximised. Entered via `Space+V` or `reef <file>` from CLI;
/// Esc returns to `Main`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Main,
    Settings,
    FocusedPreview,
}

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

/// 一行 changed-file 数据,FocusedPreview 文件 picker 用来渲染 + 派发。
/// 数据源由 [`FocusedPreviewFileSource`] 标注:同样的文件名在 Git tab
/// 可能同时出现在 staged + unstaged,跟 reef 内部其它面板一致地视作两行。
#[derive(Debug, Clone)]
pub struct FocusedPreviewFileRow {
    pub path: String,
    pub status: char,
    pub source: FocusedPreviewFileSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPreviewFileSource {
    GitStaged,
    GitUnstaged,
    GraphCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Compact,  // 只显示变更区域 ± context
    FullFile, // 显示整个文件
}

impl DiffMode {
    pub fn pref_str(self) -> &'static str {
        match self {
            DiffMode::Compact => "compact",
            DiffMode::FullFile => "full_file",
        }
    }

    pub fn from_pref_str(s: &str) -> Self {
        match s {
            "full_file" => DiffMode::FullFile,
            _ => DiffMode::Compact,
        }
    }
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

/// Maximum number of recent branches we keep in `GitGraphState::recent_branches`.
/// Past this count the picker stops being useful and starts being noise.
pub(crate) const GRAPH_RECENT_BRANCHES_MAX: usize = 5;

/// State for the inline commit graph sidebar.
#[derive(Debug, Default)]
pub struct GitGraphState {
    pub rows: Vec<GraphRow>,
    pub ref_map: HashMap<String, Vec<RefLabel>>,
    /// `(head_oid, refs_hash, scope_hash)` — revwalk is skipped when these
    /// are unchanged, so workdir edits don't trigger a full re-walk on
    /// large repos. The scope component invalidates the cache when the
    /// user picks a different branch via the picker.
    pub cache_key: Option<(String, u64, u64)>,
    /// Active walk scope. Defaults to `AllRefs` (historical behaviour).
    /// Mutating this should be followed by `cache_key = None` + a refresh.
    pub scope: GraphScope,
    /// Most-recently-used fully-qualified refs surfaced at the top of the
    /// branch picker. Persisted across sessions via `prefs::set`.
    pub recent_branches: Vec<String>,
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
pub type DiffHighlighted = reef_core::diff::DiffHighlighted<LineTokens>;

/// A diff plus its optional syntax-highlighted tokens. Used for the Git-tab
/// working/staged diff (no path needed — the selected file is tracked
/// elsewhere) where `CommitFileDiff` would be overkill.
///
/// `display` is the worker-built render cache (per-frame display rows +
/// mouse-hit row_texts for both unified and SBS layouts). Wrapped in `Arc` so
/// cloning the struct doesn't clone the cache, and so the renderer's
/// `hit_slot` can share the same Vec without per-frame copy.
///
/// **Invariant**: `display` must be built from `diff` + `highlighted` via
/// `HighlightedDiff::new`. Mutating `diff` or `highlighted` in place (both
/// fields are pub for ergonomics) without rebuilding `display` desyncs
/// the render-time row vectors from the underlying data — render shows
/// stale text while `ranges_on_row` indexes into the new content. Always
/// reassign the whole struct via `::new(...)` for content-changing
/// updates.
#[derive(Debug, Clone)]
pub struct HighlightedDiff {
    pub diff: DiffContent,
    /// Syntax-highlighted tokens, shared as `Arc<DiffHighlighted>` so the
    /// process-wide highlight cache can hand the same allocation to every
    /// caller without cloning the deep `Vec<Vec<Arc<...>>>` structure.
    /// `None` means the file's extension didn't resolve a syntax (or we
    /// skipped highlighting due to size guards).
    pub highlighted: Option<Arc<DiffHighlighted>>,
    pub display: Arc<DiffDisplay<LineTokens>>,
}

impl HighlightedDiff {
    /// Build a `HighlightedDiff` and its render-cache (`DiffDisplay`) in one
    /// step. Worker callers use this so tests don't have to know about the
    /// cache wiring; pass the highlight result (or `None` if no syntax
    /// resolved / highlighting was skipped).
    pub fn new(diff: DiffContent, highlighted: Option<Arc<DiffHighlighted>>) -> Self {
        let display = Arc::new(DiffDisplay::build(&diff, highlighted.as_deref()));
        Self {
            diff,
            highlighted,
            display,
        }
    }
}

/// A loaded commit-file diff plus its optional syntax-highlighted tokens.
/// Kept at the app/UI layer (not in `src/git`) so the git module stays free
/// of ratatui types (the SBS/Unified renderers own all styling).
#[derive(Debug, Clone)]
pub struct CommitFileDiff {
    pub path: String,
    pub diff: DiffContent,
    pub highlighted: Option<Arc<DiffHighlighted>>,
    pub display: Arc<DiffDisplay<LineTokens>>,
}

impl CommitFileDiff {
    pub fn new(path: String, diff: DiffContent, highlighted: Option<Arc<DiffHighlighted>>) -> Self {
        let display = Arc::new(DiffDisplay::build(&diff, highlighted.as_deref()));
        Self {
            path,
            diff,
            highlighted,
            display,
        }
    }
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

    // Tab
    pub active_tab: Tab,
    pub active_panel: Panel,

    pub view_mode: ViewMode,
    pub settings: crate::settings::SettingsState,

    /// FocusedPreview 左上角 ☰ 按钮触发的悬浮文件 picker。`open` 控制
    /// 弹窗显示;`selected` 是高亮行的索引,指向 `focused_preview_file_entries`
    /// 返回的扁平 Vec。
    pub focused_preview_files_open: bool,
    pub focused_preview_files_selected: usize,

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
    /// Cursor byte-offset into `db_goto_input` when it's `Some`. Only
    /// meaningful in tandem with `db_goto_input`; reset to 0 each time
    /// the input opens. Allows mid-buffer editing (Left/Right, Home/End,
    /// Ctrl+A/E, etc.) via [`crate::input_edit`].
    pub db_goto_cursor: usize,
    /// Vertical / horizontal scroll axis-lock state. The dispatcher
    /// `observe()`s the firing axis and `locked()`-checks the
    /// orthogonal one — single-event trackpad noise on the
    /// orthogonal axis falls below the streak threshold and never
    /// arms the lock. See [`crate::input::AxisLock`] for the streak
    /// + gap rules.
    pub vertical_scroll_lock: crate::input::AxisLock,
    pub horizontal_scroll_lock: crate::input::AxisLock,
    /// Per-axis step-size pacers for wheel/trackpad input. Detect
    /// trackpad-cadence (~12-16 ms inter-event) vs wheel-cadence
    /// (~100 ms+) and return 1 vs 3 lines per event respectively,
    /// with mild acceleration on sustained trackpad swipes. Keeps a
    /// gentle two-finger swipe from flying 30-90 lines per gesture.
    /// See [`crate::input::ScrollPacer`].
    pub vertical_scroll_pacer: crate::input::ScrollPacer,
    pub horizontal_scroll_pacer: crate::input::ScrollPacer,
    /// 上一帧 preview 内容行的起点(content_x, content_y)与 gutter 宽度。
    /// mouse handler 据此把终端列行坐标映射回文件行/列。
    pub last_preview_content_origin: Option<(u16, u16, u16)>,
    /// Markdown reading view content origin. Kept separate from
    /// `last_preview_content_origin` so source navigation never treats
    /// rendered Markdown rows as file byte coordinates.
    pub last_markdown_content_origin: Option<(u16, u16)>,
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
    pub commit_detail_selection: Option<crate::ui::selection::PreviewSelection>,
    pub last_commit_detail_rect: Option<ratatui::layout::Rect>,
    pub last_commit_detail_hit: Option<crate::ui::selection::CommitDetailHit>,
    pub commit_detail_click_state: Option<(Instant, u16, u16, u8)>,
    /// 拖拽选中过程中,上一次观察到的鼠标 `(column, row)`。终端在鼠标
    /// 不动时不会再发 Drag 事件,所以 VSCode 风格的"鼠标移出视口后
    /// 自动滚动"必须每帧重放最后已知位置——`tick_drag_autoscroll`
    /// 读这个字段。Down/Drag 时由 selection handler 写入,Up 时清空;
    /// 没有进行中的拖拽时为 None。
    pub last_drag_mouse: Option<(u16, u16)>,
    /// 上一次 preview / diff 自动滚动触发的时刻,用作速率限制。距离视口边缘
    /// 越远 `tick_drag_autoscroll` 取的间隔越短,但单帧最快也不超过这个节流
    /// 闸,避免 60Hz tick 把视口直接甩飞。None 表示当前帧首次触发。两个
    /// 面板各持一份,避免 Up 丢事件时两边互相干扰彼此的节流。
    pub preview_autoscroll_at: Option<Instant>,
    pub diff_autoscroll_at: Option<Instant>,

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

    /// VSCode-style "Find" widget. Independent of `search` (the vim `/`
    /// state machine) and mutually exclusive with it — opening either
    /// clears the other. Floats in the upper-right of the active content
    /// panel; supports SBS diff (per-side targeting) and Match Case /
    /// Whole Word / Regex toggles. See `crate::find_widget`.
    pub find_widget: crate::find_widget::FindWidgetState,

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

    /// `b` branch picker (Graph tab). Owns keyboard / mouse while
    /// `active`, picks a fully-qualified ref or `[ All refs ]` →
    /// `App::set_graph_scope` swaps the scope and refreshes.
    pub graph_branch_picker: crate::graph_branch_picker::GraphBranchPickerState,

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
    /// `ui::preview` alongside the in-panel `/` search highlight.
    pub preview_highlight: Option<PreviewHighlight>,

    /// Explicit location history. `gd` / `gr` / picker accepts push the
    /// **pre-jump** state here before switching surfaces; later
    /// Alt/Ctrl+Alt Left/Right restore snapshots across preview, diff,
    /// search, quick-open, and LSP jumps.
    pub location_history: reef_core::history::History<crate::app::nav::LocationSnapshot>,

    /// Multi-candidate popup overlay. `Some` while the user is picking
    /// between several intra-file definitions; closes on Enter / Esc /
    /// click-outside. While open, owns keyboard navigation (Up/Down)
    /// and routes click via `HitTestRegistry`.
    pub nav_candidates: Option<crate::app::nav::NavCandidatesPopup>,

    /// Identifier currently lit up under a Ctrl+hover gesture. `Some`
    /// while the user holds Ctrl over a clickable token in the preview
    /// pane; the render path overlays an UNDERLINE + accent fg on
    /// `(line, byte_range)` to advertise that a click would jump.
    /// Cleared on every Mouse Moved event that lacks CONTROL.
    pub ctrl_hover_target: Option<(usize, std::ops::Range<usize>)>,

    /// Same Ctrl+hover affordance, but for the diff panel (which carries
    /// no `FileParse`, so the hovered identifier is found by `word_at_byte`
    /// on the row text). Carries the display-row + byte range + SBS side so
    /// the diff renderer underlines the right half. Cleared on any Moved
    /// without CONTROL or off the diff.
    pub diff_ctrl_hover: Option<crate::ui::selection::DiffHover>,

    /// Workspace symbol index for cross-file `gd` / `gr`. Built once
    /// on repo open (`build_nav_workspace`), rebuilt lazily after
    /// fs_watcher invalidations. `None` before the first build
    /// completes — cross-file resolution falls back to intra-file
    /// during that window.
    ///
    /// Held by `Arc` because the popup can outlive the index when a
    /// rebuild kicks off mid-display.
    pub nav_workspace: Option<std::sync::Arc<reef_core::nav::WorkspaceIndex>>,

    /// Inflight-tracker for workspace index builds. Uses the standard
    /// generation / loading / stale flags — unlike `goto_definition`
    /// (which is intent dispatch), a workspace build is a snapshot
    /// load that benefits from the full AsyncState contract.
    pub nav_workspace_load: AsyncState,

    /// Phase 3 supervisor state per language — drives the status-bar
    /// badge. Missing keys are treated as `LspBadge::Off`.
    pub lsp_states: std::collections::HashMap<reef_core::nav::NavLang, reef_core::nav::LspBadge>,

    /// Cached "is this LSP binary on PATH?" per language. Populated off
    /// the render path by `refresh_lsp_installed` so the status-bar badge and Settings rows
    /// never stat the filesystem during render — `locate_binary` walks
    /// every PATH dir and was previously called every frame.
    pub lsp_installed: std::collections::HashMap<reef_core::nav::NavLang, bool>,

    /// Phase 3 LSP refine cache. Keyed by `(lang, identifier_text)`
    /// rather than byte offset so a click on `foo` anywhere benefits
    /// from a prior LSP refine on `foo`. The next `gd` consults this
    /// before falling back to tree-sitter / workspace results — never
    /// re-jumps the cursor from a refine that lands after the jump.
    pub nav_refine_cache:
        std::collections::HashMap<(reef_core::nav::NavLang, String), reef_core::nav::LspLocation>,

    /// Generation counter for LSP refine dispatches. Used by
    /// `nav_pending_lsp_jump` to drop stale responses when the user
    /// clicks somewhere else mid-flight.
    pub nav_refine_gen: u64,

    /// Monotonic epoch bumped every time `nav_refine_cache` is cleared
    /// (an `fs_dirty` pulse — a file may have moved the symbol). Each
    /// refine dispatch captures the current epoch; the `LspRefineDone`
    /// handler refuses to insert a response whose epoch is older than
    /// the current one, since its location was resolved against a
    /// now-stale source snapshot. Without this guard a refine in flight
    /// across a cache-clear repopulates the cache with a pre-edit
    /// location, and the next `gd` jumps to the wrong line.
    ///
    /// Because `fs_watcher` is coarse (a path-less `()` pulse), this is
    /// conservative: ANY fs event between a refine's dispatch and its
    /// response drops the insert, even when the edited file is unrelated
    /// to the clicked symbol. That is self-healing — the dropped result
    /// only skips a cache *write*; the jump still happens, and the next
    /// `gd` at the same position re-dispatches with the current epoch and
    /// caches normally once the fs quiets. The precise fix (invalidate
    /// only the changed paths) needs `Backend::subscribe_fs_events` to
    /// carry paths and is deferred; under continuous churn the whole
    /// cache is being legitimately cleared anyway, so little is lost.
    pub nav_refine_epoch: u64,

    /// Pending LSP-only goto-def request — Vue (and any future
    /// `has_semantic_queries() == false` language). Set when `gd` /
    /// Ctrl+click fires the LSP request; consumed by tick's
    /// `LspRefineDone` drain when the matching response arrives.
    /// Mirrors VSCode's Vue extension: the client sends
    /// `textDocument/definition { uri, position }` and waits for the
    /// server (Volar) to do the SFC → virtual TS mapping.
    pub nav_pending_lsp_jump: Option<crate::app::nav::NavPendingJump>,

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

    /// Generic centered yes/no confirm modal. Owns mouse + keyboard
    /// input while `Some`. Built by callers via `App::show_confirm` —
    /// see e.g. `prompt_tree_delete` for the delete-file consumer.
    pub confirm_modal: Option<ConfirmModal>,

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
    pub paste_conflict: Option<reef_core::file_ops::PasteConflictPrompt>,

    /// Timestamp of the most recent bare-Space keystroke in the global
    /// keymap. `Some(t)` means a Space leader is primed and waiting for a
    /// follow-up key within `input::LEADER_TIMEOUT`. The palette-side
    /// leader has its own slot inside `QuickOpenState` — separate so they
    /// can't interfere across mode transitions.
    pub space_leader_at: Option<std::time::Instant>,

    /// Timestamp of the last bare `g` keystroke. `Some(t)` means a vim-style
    /// `gg` chord is primed and waiting for the second `g` within 500ms; on
    /// the second hit we scroll the active preview to the top. Suppressed in
    /// search / overlay / SQLite-preview contexts — see `handle_key`.
    pub g_pending_at: Option<std::time::Instant>,

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
    /// Tracks the in-flight `FilesTask::ReplaceInFiles` batch. The
    /// generation is consumed by `WorkerResult::ReplaceProgress` /
    /// `ReplaceDone` so a stale completion can't silently clear the
    /// "replacing…" badge after the user starts a fresh batch.
    pub replace_load: AsyncState,
    next_git_revalidate_at: Instant,
    next_graph_revalidate_at: Instant,
}

/// What the user is about to delete once they confirm. Passed by value
/// through the modal's `on_confirm` closure rather than stored on
/// `App` directly — keeping it module-private and out of the App struct
/// avoids a second source of truth for "is a delete pending" alongside
/// `confirm_modal`. The Clone bound lets `execute_tree_delete` re-open
/// the modal with the same payload when it has to back off ("operation
/// still running" toast).
#[derive(Debug, Clone)]
struct TreeDeletePending {
    path: PathBuf,
    display_name: String,
    is_dir: bool,
    hard: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedFile {
    pub path: String,
    pub is_staged: bool,
}

/// Row-scoped preview highlight carried from `global_search::accept()` to
/// the `ui::preview` renderer. Survives the async preview round-trip
/// so the match row gets highlighted the frame the preview lands. Cleared
/// whenever the active preview path no longer matches `path`.
#[derive(Debug, Clone)]
pub struct PreviewHighlight {
    pub path: std::path::PathBuf,
    pub row: usize,
    pub byte_range: std::ops::Range<usize>,
    /// Fade lifecycle, carried INSIDE the highlight so it can never
    /// desync from the highlight's presence (one `Option`, not two
    /// loosely-coupled fields). Set by `set_preview_highlight`
    /// (fading) / `set_preview_highlight_persistent`.
    pub fade: HighlightFade,
    /// UTF-16 `start..end` columns awaiting byte-range resolution. Set
    /// for a CROSS-FILE LSP definition jump, whose target source isn't
    /// loaded yet when the highlight is created — so `byte_range` starts
    /// empty and `resolve_pending_highlight` converts these columns to a
    /// real byte range (highlighting the symbol, not just the row) once
    /// the destination preview lands. `None` for same-file jumps (already
    /// resolved) and non-LSP highlights (global search carries its own
    /// byte range).
    pub pending_utf16: Option<std::ops::Range<u32>>,
}

/// Fade lifecycle of a `preview_highlight`.
///
/// - `Persistent` — global-search locator band; never auto-fades (the
///   user reads against it).
/// - `Pending` — a nav-jump reveal band whose target file is still
///   loading (cross-file). The TTL countdown is deferred until the
///   file is on screen so the band isn't gone before the user sees the
///   destination — but a hard `armed_at` cap clears it if the load
///   never lands (deleted/unreadable file), so it can't leak.
/// - `Counting` — reveal band on screen; clears `since + TTL`.
#[derive(Debug, Clone, Copy)]
pub enum HighlightFade {
    Persistent,
    Pending { armed_at: std::time::Instant },
    Counting { since: std::time::Instant },
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
            active_tab: Tab::Files,
            active_panel: Panel::Files,
            view_mode: ViewMode::Main,
            settings: crate::settings::SettingsState::default(),
            focused_preview_files_open: false,
            focused_preview_files_selected: 0,
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
            db_goto_cursor: 0,
            vertical_scroll_lock: crate::input::AxisLock::new(),
            horizontal_scroll_lock: crate::input::AxisLock::new(),
            vertical_scroll_pacer: crate::input::ScrollPacer::new(),
            horizontal_scroll_pacer: crate::input::ScrollPacer::new(),
            last_preview_content_origin: None,
            last_markdown_content_origin: None,
            preview_click_state: None,
            diff_selection: None,
            last_diff_rect: None,
            last_diff_hit: None,
            diff_click_state: None,
            commit_detail_selection: None,
            last_commit_detail_rect: None,
            last_commit_detail_hit: None,
            commit_detail_click_state: None,
            last_drag_mouse: None,
            preview_autoscroll_at: None,
            diff_autoscroll_at: None,
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
            git_graph: {
                let (scope, recent) = load_graph_scope_pref();
                GitGraphState {
                    scope,
                    recent_branches: recent,
                    ..GitGraphState::default()
                }
            },
            commit_detail: CommitDetailState {
                diff_layout: crate::prefs::get("commit.diff_layout")
                    .as_deref()
                    .map(DiffLayout::from_pref_str)
                    .unwrap_or(DiffLayout::Unified),
                diff_mode: crate::prefs::get("commit.diff_mode")
                    .as_deref()
                    .map(DiffMode::from_pref_str)
                    .unwrap_or(DiffMode::Compact),
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
            show_help: false,
            pending_edit: None,
            theme,
            search: crate::search::SearchState::default(),
            find_widget: crate::find_widget::FindWidgetState::default(),
            quick_open: crate::quick_open::QuickOpenState::from_prefs(),
            global_search: crate::global_search::GlobalSearchState::default(),
            hosts_picker: crate::hosts_picker::HostsPickerState::default(),
            graph_branch_picker: crate::graph_branch_picker::GraphBranchPickerState::default(),
            pending_ssh_target: None,
            should_quit_session: false,
            preview_highlight: None,
            location_history: reef_core::history::History::new(Self::NAV_HISTORY_CAP),
            nav_candidates: None,
            ctrl_hover_target: None,
            diff_ctrl_hover: None,
            nav_workspace: None,
            nav_workspace_load: AsyncState::default(),
            lsp_states: std::collections::HashMap::new(),
            lsp_installed: std::collections::HashMap::new(),
            nav_refine_cache: std::collections::HashMap::new(),
            nav_refine_gen: 0,
            nav_refine_epoch: 0,
            nav_pending_lsp_jump: None,
            place_mode: crate::place_mode::PlaceModeState::default(),
            tree_edit: crate::tree_edit::TreeEditState::default(),
            tree_context_menu: crate::tree_context_menu::ContextMenuState::default(),
            confirm_modal: None,
            file_clipboard: crate::file_clipboard::FileClipboard::default(),
            file_selection: crate::file_selection::SelectionSet::default(),
            tree_drag: crate::tree_drag::TreeDragState::default(),
            paste_conflict: None,
            space_leader_at: None,
            g_pending_at: None,
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
            replace_load: AsyncState::default(),
            next_git_revalidate_at: now + Duration::from_millis(800),
            next_graph_revalidate_at: now + Duration::from_millis(1200),
        };
        app.refresh_status();
        app.refresh_file_tree();
        // Phase 2: kick the workspace symbol index build immediately on
        // repo open (user decision — "立即构建"). Skipped in SSH mode:
        // the index walks the local filesystem, and the index isn't
        // useful for files that live on a remote host.
        app.dispatch_nav_workspace_build();
        // Probe which LSP binaries are installed ONCE here (off the
        // render path) so the status-bar badge / Settings rows read a
        // cached map instead of walking PATH every frame.
        app.refresh_lsp_installed();
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

    pub fn clear_commit_detail_selection(&mut self) {
        self.commit_detail_selection = None;
        self.commit_detail_click_state = None;
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

    /// Commit the current Tab::Search replace batch. Buckets the
    /// currently-included matches by path into `ReplaceItem`s and
    /// dispatches `FilesTask::ReplaceInFiles`. No-op when replace mode
    /// is closed, a previous batch is still in flight, the result list
    /// is empty, or every match has been opted out via the per-row
    /// checkbox. Bound to `Ctrl/Alt+Enter`, plain `Enter` from the
    /// replace input, and the `[Apply]` footer button.
    pub fn commit_replace_in_files(&mut self) {
        if !self.global_search.replace_open || self.replace_load.loading {
            return;
        }
        if self.global_search.results.is_empty() {
            return;
        }
        if self.global_search.included_count() == 0 {
            return;
        }
        // Bucket included results by path so each file gets one
        // worker dispatch with its line-list. `BTreeMap` orders by
        // `PathBuf`, which happens to match the streaming results
        // (the worker emits hits sorted by path) — same ordering
        // either way, just made explicit here.
        use std::collections::BTreeMap;
        let mut buckets: BTreeMap<PathBuf, Vec<crate::tasks::ReplaceLine>> = BTreeMap::new();
        for (idx, hit) in self.global_search.results.iter().enumerate() {
            if !self.global_search.is_match_included(idx) {
                continue;
            }
            buckets
                .entry(hit.path.clone())
                .or_default()
                .push(crate::tasks::ReplaceLine {
                    line_no: hit.line,
                    expected_text: hit.line_text.clone(),
                });
        }
        let items: Vec<crate::tasks::ReplaceItem> = buckets
            .into_iter()
            .map(|(path, lines)| crate::tasks::ReplaceItem { path, lines })
            .collect();
        if items.is_empty() {
            return;
        }
        self.global_search.replace_progress = None;
        // `replace_load.begin()` flips `loading=true` and bumps the
        // generation; the footer reads `replace_load.loading` for the
        // in-flight indicator.
        let generation = self.replace_load.begin();
        self.tasks.replace_in_files(
            generation,
            self.backend.clone(),
            self.global_search.core.filter.clone(),
            self.global_search.replace_text.clone(),
            items,
        );
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
    /// Refuses the transition when a place-mode copy is already in
    /// flight — overwriting `sources` would invalidate the worker's
    /// generation and the previous copy's completion result (including
    /// the success toast and tree refresh) would be silently dropped.
    pub fn enter_place_mode(&mut self, sources: Vec<PathBuf>) {
        if sources.is_empty() {
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
        self.quick_open.core.active = false;
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
        self.dismiss_confirm();
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
            .set(reef_core::file_ops::ClipMode::Cut, paths);
    }

    /// Mark `paths` as Copy. Copy mode does not visually mark source
    /// rows (matches VS Code).
    pub fn mark_copy(&mut self, paths: Vec<PathBuf>) {
        self.file_clipboard
            .set(reef_core::file_ops::ClipMode::Copy, paths);
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
            self.dispatch_paste_op(reef_core::file_ops::ClipMode::Copy, parent, group);
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
        op: reef_core::file_ops::ClipMode,
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
        let cls = reef_core::file_ops::classify_paste(op, &dest_rel, &sources, &existing);

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
            self.paste_conflict = Some(reef_core::file_ops::PasteConflictPrompt::new(
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
        op: reef_core::file_ops::ClipMode,
        dest_rel: PathBuf,
        decisions: Vec<(PathBuf, reef_core::file_ops::Resolution)>,
    ) {
        use reef_core::file_ops::Resolution;
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
            reef_core::file_ops::ClipMode::Cut => {
                self.tasks
                    .move_paths(generation, Arc::clone(&self.backend), items, dest_rel);
                // Cut-paste consumes the clipboard. VS Code's
                // semantics: a single Cut feeds one Paste.
                self.clear_clipboard();
            }
            reef_core::file_ops::ClipMode::Copy => {
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
        r: reef_core::file_ops::Resolution,
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
        Some(reef_core::file_ops::next_copy_name(basename, &existing))
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
            reef_core::file_ops::ClipMode::Copy
        } else {
            reef_core::file_ops::ClipMode::Cut
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
        self.dismiss_confirm();
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
        let name = match reef_core::file_ops::validate_basename(&self.tree_edit.buffer) {
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
            self.tree_edit.error =
                Some(reef_core::file_ops::FileNameError::NameAlreadyExists(name));
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

    /// Pop the centered delete-confirm modal. `hard` controls Trash
    /// vs. `fs::remove_*`; the primary-button label adjusts accordingly,
    /// while the title is a generic "Confirm" framing (the tone +
    /// button color carry the severity cue, not the title text).
    /// The `on_confirm` closure captures the path/is_dir/hard tuple so
    /// the modal doesn't need a side-channel struct on `App`.
    pub fn prompt_tree_delete(&mut self, path: PathBuf, is_dir: bool, hard: bool) {
        use crate::ui::confirm_modal::{ConfirmModal, ModalTone};
        let display_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(reef_core::file_ops::sanitize_filename)
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        self.tree_context_menu.close();
        let body = crate::i18n::tree_delete_body(&display_name, is_dir, hard);
        let primary_label = if hard {
            crate::i18n::confirm_delete_label()
        } else {
            crate::i18n::confirm_trash_label()
        };
        let pending = TreeDeletePending {
            path,
            display_name,
            is_dir,
            hard,
        };
        self.show_confirm(ConfirmModal {
            title: crate::i18n::confirm_destructive_title(),
            tone: ModalTone::Danger,
            body,
            primary_label,
            cancel_label: crate::i18n::confirm_cancel_label(),
            confirm_keys: vec!['y', 'Y'],
            on_confirm: Box::new(move |app| app.execute_tree_delete(pending)),
            on_cancel: Box::new(|_| {}),
        });
    }

    /// Run the actual delete after the user confirmed. If a previous
    /// fs mutation is still in flight, re-open the modal with the same
    /// pending so the user can retry once the worker drains (matches
    /// the original `confirm_tree_delete` retry semantics — Y wasn't
    /// silently lost).
    fn execute_tree_delete(&mut self, pending: TreeDeletePending) {
        if self.fs_mutation_load.loading {
            self.toasts
                .push(Toast::warn(crate::i18n::tree_op_blocked_by_in_flight()));
            self.prompt_tree_delete(pending.path, pending.is_dir, pending.hard);
            return;
        }
        let generation = self.fs_mutation_load.begin();
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

    // ── Confirm modal (generic yes/no overlay) ───────────────────────────

    /// Show the centered `ConfirmModal`. If another modal is already
    /// up, its `on_cancel` fires first so the caller can rely on
    /// "every modal that opens eventually resolves via one of its
    /// callbacks". Without this, opening modal B over modal A would
    /// silently drop A's cancel closure — a leak for any caller that
    /// uses cancel for cleanup.
    pub fn show_confirm(&mut self, modal: ConfirmModal) {
        if self.confirm_modal.is_some() {
            self.fire_confirm_cancel();
        }
        self.confirm_modal = Some(modal);
    }

    /// Force-close the modal without firing any callback. Used by
    /// "competing modal opens" paths (place mode, tree edit, tab
    /// switch) and by successful/failed mutation completion.
    pub fn dismiss_confirm(&mut self) {
        self.confirm_modal = None;
    }

    /// Fire the primary callback. The modal is `take`n first so the
    /// closure receives a clean `&mut App` (the closure may e.g.
    /// `show_confirm` again to retry).
    pub fn fire_confirm_primary(&mut self) {
        if let Some(modal) = self.confirm_modal.take() {
            (modal.on_confirm)(self);
        }
    }

    /// Fire the cancel callback. Same `take`-first contract as
    /// `fire_confirm_primary`.
    pub fn fire_confirm_cancel(&mut self) {
        if let Some(modal) = self.confirm_modal.take() {
            (modal.on_cancel)(self);
        }
    }

    // ── Hosts picker (Ctrl+O) ────────────────────────────────────────────

    /// Open the hosts picker overlay, seeding it from the current user's
    /// `~/.ssh/config` plus the persisted recent-targets list. Errors
    /// reading the config aren't fatal — we show an empty picker so the
    /// user can still switch via the path-input mode.
    pub fn open_hosts_picker(&mut self) {
        let parsed = reef_core::hosts::parse_ssh_config().unwrap_or_default();
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
        // Close first, then act on the (optional) target. Same shape as
        // `confirm_graph_branch_picker` — without it, a `confirm()` of
        // None (filter matched zero hosts + Enter) would early-return
        // and leave the overlay open trapping the keyboard, same UX
        // trap that R3 of the previous review fixed for the graph
        // picker.
        let target = self.hosts_picker.confirm();
        self.hosts_picker.close();
        let Some(target) = target else {
            return;
        };
        // Persist the chosen target to the recents list before handing
        // control back to `main.rs` — even if the subsequent connect
        // fails, the user probably still wants it surfaced next time.
        let mut current = crate::hosts_picker::load_recent();
        current = crate::hosts_picker::bump_recent(current, target.clone());
        crate::hosts_picker::save_recent(&current);

        self.pending_ssh_target = Some(target);
        self.should_quit_session = true;
    }

    /// Open the Graph tab's branch picker. Pulls the branch list out
    /// of the cached `ref_map` (already loaded by `refresh_graph`) and
    /// seeds the recents from `GitGraphState::recent_branches`.
    ///
    /// Refuses to open before the first graph payload has populated
    /// `ref_map`: at cold start with a persisted `Branch(X)` scope,
    /// an empty `ref_map` would render the picker as
    /// `[ All refs ]`-only, and a stray Enter would silently overwrite
    /// the user's persisted choice via `set_graph_scope(AllRefs)`.
    /// Toast instead and let the user retry once the first revwalk lands.
    pub fn open_graph_branch_picker(&mut self) {
        if self.git_graph.ref_map.is_empty() {
            self.toasts
                .push(Toast::info(crate::i18n::graph_picker_not_ready_toast()));
            return;
        }
        // If the persisted scope points at a branch that's no longer
        // in ref_map (e.g. ref deleted between sessions; the
        // background revalidator's stale-branch fallback hasn't run
        // yet), fall back to AllRefs HERE so the picker doesn't open
        // already-pointing at the AllRefs sentinel row by silent
        // accident — pressing Enter would otherwise overwrite the
        // persisted branch with no toast. Surface the same toast the
        // worker fallback uses, then proceed with the user's normal
        // picker flow against AllRefs.
        if let GraphScope::Branch(target) = &self.git_graph.scope.clone() {
            let still_present = self.git_graph.ref_map.values().any(|labels| {
                labels.iter().any(|label| match label {
                    RefLabel::Branch(name) => format!("refs/heads/{name}") == *target,
                    RefLabel::RemoteBranch(name) => format!("refs/remotes/{name}") == *target,
                    _ => false,
                })
            });
            if !still_present {
                let short = shorthand_for_full_ref(target).to_string();
                self.git_graph
                    .recent_branches
                    .retain(|existing| existing != target);
                self.toasts
                    .push(Toast::info(crate::i18n::graph_scope_stale_branch_toast(
                        &short,
                    )));
                self.apply_scope_no_refresh(GraphScope::AllRefs);
                self.refresh_graph();
            }
        }
        let recent = self.git_graph.recent_branches.clone();
        let scope = self.git_graph.scope.clone();
        self.graph_branch_picker
            .open(&self.git_graph.ref_map, recent, &scope);
    }

    pub fn close_graph_branch_picker(&mut self) {
        self.graph_branch_picker.close();
    }

    /// Apply the picker's current selection and close the overlay. A
    /// `confirm()` of `None` (filter matched zero rows + Enter) also
    /// closes the overlay so the user can never get input-trapped on
    /// an empty result list.
    pub fn confirm_graph_branch_picker(&mut self) {
        let scope = self.graph_branch_picker.confirm();
        self.graph_branch_picker.close();
        if let Some(scope) = scope {
            self.set_graph_scope(scope);
        }
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
    /// `(object, page)` window via the row-bearing flat list and
    /// delegates to [`Self::db_apply_page`]. Runs synchronously on
    /// the main thread — local sub-ms, SSH 10-50ms RPC.
    pub fn db_navigate(&mut self, action: DbNav) {
        let Some((cur_key, cur_page, rows_per_page)) = self
            .db_preview_state
            .as_ref()
            .map(|s| (s.selection.clone(), s.page, s.rows_per_page))
        else {
            return;
        };
        let Some(info) = self.preview_database_info() else {
            return;
        };
        let visible: Vec<&reef_sqlite_preview::DbObject> = info.iter_row_bearing().collect();
        if visible.is_empty() {
            return;
        }
        let cur_idx = visible
            .iter()
            .position(|o| o.name == cur_key.name && o.kind == cur_key.kind)
            .unwrap_or(0)
            .min(visible.len() - 1);
        let max_idx = visible.len() - 1;
        let (new_idx, new_page) = match action {
            DbNav::PrevPage => (cur_idx, cur_page.saturating_sub(1)),
            DbNav::NextPage => (
                cur_idx,
                (cur_page + 1).min(max_page_for_object(visible[cur_idx], rows_per_page)),
            ),
            DbNav::PrevTable => (cur_idx.saturating_sub(1), 0),
            DbNav::NextTable => ((cur_idx + 1).min(max_idx), 0),
            DbNav::FirstPage => (cur_idx, 0),
            DbNav::LastPage => (
                cur_idx,
                max_page_for_object(visible[cur_idx], rows_per_page),
            ),
        };
        if new_idx == cur_idx && new_page == cur_page {
            return;
        }
        let new_key = visible[new_idx].key();
        self.db_apply_page(
            new_key,
            new_page,
            /* reset_h_scroll = */ new_idx != cur_idx,
        );
    }

    /// Toggle the expand / collapse state of one schema in the
    /// sidebar. No row reload — the data grid keeps its selection.
    pub fn db_toggle_schema(&mut self, name: &str) {
        let Some(state) = self.db_preview_state.as_mut() else {
            return;
        };
        if !state.expanded.remove(name) {
            state.expanded.insert(name.to_string());
        }
    }

    /// Switch the SQLite preview's selection to a specific
    /// schema-qualified object. Row-bearing kinds load page 0
    /// synchronously; non-row kinds (Index / Trigger) stash the
    /// structural detail in `state.detail` for the detail pane.
    pub fn db_select_object(&mut self, key: reef_sqlite_preview::DbObjectKey) {
        let current = self.db_preview_state.as_ref().map(|s| s.selection.clone());
        if current.as_ref() == Some(&key) {
            return;
        }
        if key.kind.has_rows() {
            self.db_apply_page(key, 0, /* reset_h_scroll = */ true);
        } else {
            self.db_apply_detail(key);
        }
    }

    /// Jump to a specific 1-based page in the currently-selected
    /// object. Out-of-range pages clamp to the last valid page
    /// rather than failing.
    pub fn db_navigate_to_page(&mut self, page_one_based: u64) {
        let Some((selection, cur_page, rows_per_page)) = self
            .db_preview_state
            .as_ref()
            .map(|s| (s.selection.clone(), s.page, s.rows_per_page))
        else {
            return;
        };
        if !selection.kind.has_rows() {
            return;
        }
        let Some(info) = self.preview_database_info() else {
            return;
        };
        let Some(object) = info.lookup(&selection) else {
            return;
        };
        let max_page = max_page_for_object(object, rows_per_page);
        let target_page = page_one_based.saturating_sub(1).min(max_page);
        if target_page == cur_page {
            return;
        }
        self.db_apply_page(selection, target_page, /* reset_h_scroll = */ false);
    }

    /// Borrow the `DatabaseInfoV2` from the current preview body, or
    /// `None` when the preview is missing / not a database.
    fn preview_database_info(&self) -> Option<&reef_sqlite_preview::DatabaseInfoV2> {
        match self.preview_content.as_ref()?.body {
            reef_core::preview::PreviewBody::Database(ref info) => Some(info),
            _ => None,
        }
    }

    /// Shared row-load path for `db_navigate*` and `db_select_object`.
    /// Issues the synchronous backend RPC, updates state on success,
    /// resets scroll, surfaces errors as a warning toast.
    fn db_apply_page(
        &mut self,
        key: reef_sqlite_preview::DbObjectKey,
        page: u64,
        reset_h_scroll: bool,
    ) {
        let Some((path, rows_per_page)) = self
            .db_preview_state
            .as_ref()
            .map(|s| (PathBuf::from(&s.path), s.rows_per_page))
        else {
            return;
        };
        let offset = page.saturating_mul(rows_per_page as u64);
        let page_data = match self
            .backend
            .db_load_page(&path, &key, offset, rows_per_page)
        {
            Ok(p) => p,
            Err(e) => {
                self.toasts
                    .push(Toast::warn(format!("sqlite page load failed: {e}")));
                return;
            }
        };
        // Snapshot column metadata before the &mut self.db_preview_state
        // borrow so the recompute uses the canonical info-side columns.
        let columns: Vec<reef_sqlite_preview::ColumnInfo> = self
            .preview_database_info()
            .and_then(|info| info.lookup(&key).map(|o| o.columns.clone()))
            .unwrap_or_default();
        if let Some(s) = self.db_preview_state.as_mut() {
            s.selection = key;
            s.page = page;
            s.current_rows = page_data.rows;
            s.detail = None;
            s.recompute_layout(&columns);
        }
        self.preview_scroll = 0;
        if reset_h_scroll {
            self.preview_h_scroll = 0;
        }
    }

    /// Shared detail-load path for Index / Trigger selection clicks.
    fn db_apply_detail(&mut self, key: reef_sqlite_preview::DbObjectKey) {
        let Some(path) = self
            .db_preview_state
            .as_ref()
            .map(|s| PathBuf::from(&s.path))
        else {
            return;
        };
        let detail = match self.backend.db_load_object_detail(&path, &key) {
            Ok(d) => d,
            Err(e) => {
                self.toasts
                    .push(Toast::warn(format!("sqlite detail load failed: {e}")));
                return;
            }
        };
        if let Some(s) = self.db_preview_state.as_mut() {
            s.selection = key;
            s.detail = Some(detail);
            s.current_rows.clear();
            s.col_widths.clear();
            s.total_table_w = 0;
        }
        self.preview_scroll = 0;
        self.preview_h_scroll = 0;
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
        // A new preview is incoming — drop any in-flight `gg` chord so a
        // bare `g` that lands while `preview_content` still holds the
        // *previous* body can't be misinterpreted. Specifically: when
        // the new body is a .db, `is_sqlite_preview()` (which reads the
        // old preview_content) returns false during the load window, so
        // the chord arms instead of opening db_goto. Clearing here makes
        // the very next `g` after a navigation always re-arm cleanly.
        self.g_pending_at = None;
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
            || self.confirm_modal.is_some()
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

    pub fn toggle_status_tree_mode(&mut self) {
        self.git_status.tree_mode = !self.git_status.tree_mode;
        crate::prefs::set_bool("status.tree_mode", self.git_status.tree_mode);
    }

    pub fn toggle_commit_diff_layout(&mut self) {
        self.commit_detail.diff_layout = match self.commit_detail.diff_layout {
            DiffLayout::Unified => DiffLayout::SideBySide,
            DiffLayout::SideBySide => DiffLayout::Unified,
        };
        self.clear_commit_detail_selection();
        crate::prefs::set(
            "commit.diff_layout",
            self.commit_detail.diff_layout.pref_str(),
        );
    }

    pub fn toggle_commit_diff_mode(&mut self) {
        self.commit_detail.diff_mode = match self.commit_detail.diff_mode {
            DiffMode::Compact => DiffMode::FullFile,
            DiffMode::FullFile => DiffMode::Compact,
        };
        self.clear_commit_detail_selection();
        crate::prefs::set("commit.diff_mode", self.commit_detail.diff_mode.pref_str());
        // The compact↔full-file flip changes the context-lines argument the
        // worker uses, so the cached diff is now wrong shape — refetch.
        self.reload_commit_file_diff();
    }

    pub fn toggle_commit_files_tree_mode(&mut self) {
        self.commit_detail.files_tree_mode = !self.commit_detail.files_tree_mode;
        self.clear_commit_detail_selection();
        crate::prefs::set_bool("commit.files_tree_mode", self.commit_detail.files_tree_mode);
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

    pub fn stage_folder(&mut self, folder_path: &str) {
        let paths: Vec<String> = self
            .unstaged_files
            .iter()
            .filter(|f| folder_contains(folder_path, &f.path))
            .map(|f| f.path.clone())
            .collect();
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

    pub fn unstage_folder(&mut self, folder_path: &str) {
        let paths: Vec<String> = self
            .staged_files
            .iter()
            .filter(|f| folder_contains(folder_path, &f.path))
            .map(|f| f.path.clone())
            .collect();
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

    /// Rebuild the commit graph iff HEAD or any ref (or the active scope)
    /// moved since the last build. Working-tree fs events do NOT
    /// invalidate the cache — see plan pitfall #2.
    pub fn refresh_graph(&mut self) {
        const GRAPH_COMMIT_LIMIT: usize = 500;
        if !self.backend.has_repo() {
            self.git_graph.rows.clear();
            self.git_graph.ref_map.clear();
            self.git_graph.cache_key = None;
            return;
        };
        let generation = self.graph_load.begin();
        self.tasks.refresh_graph(
            generation,
            Arc::clone(&self.backend),
            GRAPH_COMMIT_LIMIT,
            self.git_graph.scope.clone(),
        );
    }

    /// Switch the graph's walk scope to `scope`, push the previous branch
    /// onto the recents list (newest-first, deduped, capped), reset
    /// selection / scroll, invalidate the graph cache and trigger a
    /// refresh. Persists `graph.scope` and `graph.scope.recent` so the
    /// choice survives across sessions.
    pub fn set_graph_scope(&mut self, scope: GraphScope) {
        if self.git_graph.scope == scope {
            return;
        }
        if let GraphScope::Branch(full_ref) = &scope {
            self.git_graph
                .recent_branches
                .retain(|existing| existing != full_ref);
            self.git_graph.recent_branches.insert(0, full_ref.clone());
            if self.git_graph.recent_branches.len() > GRAPH_RECENT_BRANCHES_MAX {
                self.git_graph
                    .recent_branches
                    .truncate(GRAPH_RECENT_BRANCHES_MAX);
            }
        }
        self.apply_scope_no_refresh(scope);
        self.refresh_graph();
    }

    /// Inner half of [`set_graph_scope`]: swap the scope, reset every
    /// selection / scroll cursor that's about to become meaningless,
    /// invalidate the graph cache, and persist. Does NOT trigger a
    /// refresh — the caller (`set_graph_scope` or the stale-branch
    /// fallback in the worker-result merge) owns that side-effect.
    ///
    /// Resetting selection here is load-bearing: without it the new
    /// scope's first frame would render with `scroll` / anchor values
    /// from the old branch, leaving the user staring at empty rows or
    /// a stale range highlight until the next nav keystroke.
    ///
    /// Bumps `commit_detail_load` / `commit_file_diff_load` so any
    /// in-flight load issued under the previous selection won't pass
    /// its `complete_ok` generation check on arrival — otherwise a
    /// late detail payload would repaint the just-cleared right panel
    /// with the prior commit's metadata.
    fn apply_scope_no_refresh(&mut self, scope: GraphScope) {
        self.git_graph.scope = scope;
        self.git_graph.cache_key = None;
        // Old rows belong to the previous scope — keeping them around
        // until the new payload lands paints the wrong tip-of-branch
        // at `selected_idx = 0`. `ref_map` is intentionally NOT
        // cleared: it's used to render ref chips on commits as soon
        // as the new payload arrives, and the picker's cold-start
        // guard reads from it too.
        self.git_graph.rows.clear();
        self.git_graph.selected_idx = 0;
        self.git_graph.scroll = 0;
        self.git_graph.selection_anchor = None;
        self.git_graph.last_rendered_selected = None;
        self.git_graph.selected_commit = None;
        self.commit_detail.detail = None;
        self.commit_detail.range_detail = None;
        self.commit_detail.file_diff = None;
        // Invalidate in-flight detail / file-diff loads so a late
        // payload tied to the previous selection fails its
        // `complete_ok` check on arrival. Using `invalidate` instead
        // of `begin` is load-bearing here: `begin` would set
        // `loading=true` with no follow-up dispatcher (especially for
        // `commit_file_diff_load`, which only kicks when the user
        // picks a file), leaving the status bar stuck on
        // "commit diff refreshing…" indefinitely.
        self.commit_detail_load.invalidate();
        self.commit_file_diff_load.invalidate();
        // Drop any active CommitGraph search overlay — its `matches`
        // are byte-ranges into the rows we just cleared, so `n` / `N`
        // would jump to indices that no longer correspond to anything
        // visible. Searches targeting other panels (file preview,
        // commit detail body, diff) are unaffected; they don't index
        // into `git_graph.rows`.
        if matches!(
            self.search.target,
            Some(crate::search::SearchTarget::CommitGraph)
        ) {
            self.search.clear();
        }
        persist_graph_scope(&self.git_graph.scope, &self.git_graph.recent_branches);
    }

    /// (Re)load commit detail for the currently-selected commit. Clears detail
    /// and any previously-selected file diff whenever the target changes.
    pub fn load_commit_detail(&mut self) {
        self.commit_detail.file_diff = None;
        self.clear_commit_detail_selection();
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

    /// (Re)load the range-mode payload for the current Shift-extended
    /// selection. Fills per-commit metadata synchronously from the cached
    /// `rows` slice (no git walk needed) and dispatches the file-list
    /// computation — `parent(oldest).tree → newest.tree` — to the graph
    /// worker. No-ops when the selection is actually a single row.
    pub fn load_commit_range_detail(&mut self) {
        self.commit_detail.file_diff = None;
        self.clear_commit_detail_selection();
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
        if !self.backend.has_repo() {
            return;
        }
        let generation = self.commit_detail_load.begin();
        self.tasks.load_commit_range_detail(
            generation,
            Arc::clone(&self.backend),
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
            self.clear_commit_detail_selection();
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
        if !self.backend.has_repo() {
            return;
        }
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
            let result = backend.commit(&message).map_err(|e| e.to_string());
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
                Ok(mut content) => {
                    if self.preview_load.complete_ok(generation) {
                        // Worker has landed — the loading indicator can stop
                        // pointing at the in-flight path.
                        self.preview_in_flight_path = None;
                        let same_file = matches!(
                            (self.preview_content.as_ref(), content.as_ref()),
                            (Some(old), Some(new)) if old.path == new.path
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
                                    Some(reef_core::preview::PreviewBody::Image(old)),
                                    Some(reef_core::preview::PreviewBody::Image(new)),
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
                                (Some(_), Some(reef_core::preview::PreviewBody::Image(img))) => {
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
                        } else if let Some(reef_core::preview::PreviewBody::Image(img)) =
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
                        match self.preview_content.as_ref().map(|p| (&p.body, &p.path)) {
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
                        self.db_goto_cursor = 0;
                        // If `global_search::accept` stashed a highlight for
                        // this file, re-center once the preview actually
                        // lands. `load_preview_for_path` runs async, so the
                        // scroll has to happen here — setting it inside
                        // `accept()` before the preview exists wouldn't know
                        // the final line count / view height.
                        if let Some(hl) = self.preview_highlight.as_ref() {
                            if self.preview_is_for(&hl.path) {
                                let view_h = self.last_preview_view_h as usize;
                                self.preview_scroll = crate::search::center_scroll(hl.row, view_h);
                            }
                        }
                        // A cross-file LSP jump deferred its symbol-range
                        // resolution until the destination source was
                        // loaded — do it now so the identifier (not just
                        // the row) lights up.
                        self.resolve_pending_highlight();
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
                    if self.preview_load.complete_err(generation, error) {
                        // `complete_err` flips `stale = true`, which would
                        // make `should_request()` re-fire the same load on
                        // the next tick — and if the failure is a decoder
                        // panic, we'd just panic again, sticking the UI in
                        // a permanent "loading…" loop. Clear stale so the
                        // panic'd file stays failed until something else
                        // (file switch, fs-watcher kick) marks it dirty.
                        self.preview_load.stale = false;
                        self.preview_load.error = None;
                        self.preview_in_flight_path = None;
                    }
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
                            // Preserve the user's staged/unstaged choice when the file
                            // exists in both sections — flipping it would swap the diff
                            // out from under them right after they clicked.
                            let still_in_current = if sel.is_staged {
                                in_staged
                            } else {
                                in_unstaged
                            };
                            if !still_in_current {
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
                        // Stale branch fallback: scope-targeted walk
                        // returned nothing AND the freshly-walked
                        // `ref_map` confirms the branch is genuinely
                        // missing. The extra `ref_map` lookup matters
                        // because `list_commits` also returns
                        // `Vec::new()` on transient `revwalk()` errors
                        // (libgit2 lock contention, fs jitter, …); we
                        // don't want a one-tick hiccup to drop a
                        // healthy branch from recents and surface a
                        // misleading "gone" toast.
                        if self.git_graph.scope != GraphScope::AllRefs
                            && payload.rows.is_empty()
                            && matches!(payload.scope, GraphScope::Branch(_))
                            && self.git_graph.scope == payload.scope
                            && payload_scope_ref_missing(&payload)
                        {
                            if let GraphScope::Branch(missing) = &payload.scope {
                                self.git_graph
                                    .recent_branches
                                    .retain(|existing| existing != missing);
                                let short = shorthand_for_full_ref(missing);
                                self.toasts.push(Toast::info(
                                    crate::i18n::graph_scope_stale_branch_toast(short),
                                ));
                            }
                            self.apply_scope_no_refresh(GraphScope::AllRefs);
                            self.refresh_graph();
                            return;
                        }
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
                        self.dismiss_confirm();
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
                        self.dismiss_confirm();
                        // Drop the pending auto-select — the target path
                        // was never created / renamed, so trying to focus
                        // it would be a stale lookup at best.
                        self.fs_mutation_select_on_done = None;
                        self.fs_mutation_load.stale = false;
                        self.fs_mutation_load.error = None;
                    }
                }
            },
            WorkerResult::ReplaceProgress {
                generation,
                files_done,
                files_total,
            } => {
                // Stale chunks (from a superseded batch) just no-op —
                // the AsyncState gates the apply path so no UI state
                // changes anyway. Keeping them quiet avoids a flicker
                // where the progress text rewinds.
                if generation != self.replace_load.generation {
                    return;
                }
                self.global_search.replace_progress = Some((files_done, files_total));
            }
            WorkerResult::ReplaceDone { generation, result } => {
                if !self.replace_load.complete_ok(generation) {
                    return;
                }
                // `complete_ok` already flipped `loading=false`.
                self.global_search.replace_progress = None;
                match result {
                    Ok(summary) => {
                        let mut text = format!(
                            "{} {} / {}",
                            crate::i18n::t(crate::i18n::Msg::ReplaceSummaryToast),
                            summary.lines_replaced,
                            summary.files_changed,
                        );
                        if summary.skipped_stale > 0 {
                            text.push_str(&format!(
                                " · {} {}",
                                summary.skipped_stale,
                                crate::i18n::t(crate::i18n::Msg::ReplaceSkippedStaleSuffix),
                            ));
                        }
                        if summary.skipped_too_large > 0 {
                            text.push_str(&format!(
                                " · {} {}",
                                summary.skipped_too_large,
                                crate::i18n::t(crate::i18n::Msg::ReplaceTooLargeSuffix),
                            ));
                        }
                        if !summary.errors.is_empty() {
                            text.push_str(&format!(" · {} error(s)", summary.errors.len()));
                        }
                        self.toasts.push(Toast::info(text));
                        // Drop stale exclusions and rerun the search;
                        // the file content changed under our feet, so
                        // the on-screen results are about to be wrong.
                        self.global_search.excluded.clear();
                        crate::global_search::reload(self);
                        // Git decorations on the file tree need a
                        // refresh — replaced files now show as
                        // modified in the status sidebar.
                        self.refresh_status();
                    }
                    Err(e) => {
                        self.toasts
                            .push(Toast::error(format!("replace failed: {e}")));
                    }
                }
            }
            WorkerResult::NavWorkspaceBuilt { generation, result } => {
                if !self.nav_workspace_load.complete_ok(generation) {
                    return;
                }
                match result {
                    Ok(index) => {
                        self.nav_workspace = Some(std::sync::Arc::new(index));
                    }
                    Err(_) => {
                        // Phase 2 build failures are silent for now —
                        // intra-file nav (Phase 1) still works without
                        // the workspace index. Phase 3 may surface
                        // these via a status-bar badge.
                        self.nav_workspace = None;
                    }
                }
            }
            WorkerResult::LspRefineDone {
                generation,
                epoch,
                lang,
                identifier,
                location,
            } => {
                // The cache is keyed by POSITION (`lang, path:line:col`
                // via `refine_key`), never by bare name. We ALSO check
                // `nav_pending_lsp_jump` and execute the jump when the
                // response matches the request currently waiting (the
                // Vue / LSP-only path). Mirrors VSCode's Vue extension:
                // send request, wait, jump.
                //
                // Convert the server's absolute path to workdir-relative
                // ONCE, here at cache-write time, so every cache reader
                // gets a ready-to-use relative path. `None` = no
                // definition OR outside the workspace (e.g. a dep).
                let rel_location = location.as_ref().and_then(|loc| {
                    self.workdir_relative(&loc.path)
                        .map(|rel| reef_core::nav::LspLocation {
                            path: rel,
                            line: loc.line,
                            character: loc.character,
                            character_end: loc.character_end,
                        })
                });
                let key = (lang, identifier.clone());
                // Only ever INSERT on a hit. A `None` must NOT remove a
                // cached entry: two requests for the same position key
                // can be in flight (impatient double-`gd` while the
                // server is still indexing), and a late "no definition"
                // response would otherwise evict the good answer a
                // newer request just cached. Stale entries are bounded
                // anyway — the whole cache is cleared on any fs change.
                //
                // Epoch guard: if the cache was cleared (a file edit)
                // since this refine was dispatched, the response's
                // location was resolved from now-stale bytes. Skip the
                // insert so it can't repopulate the just-cleared cache
                // with a pre-edit line. The pending-jump branch below is
                // still allowed to run for `generation`-matched requests
                // — a slightly stale one-shot jump is acceptable, but
                // poisoning the cache for every future `gd` is not.
                let epoch_fresh = epoch == self.nav_refine_epoch;
                if let Some(loc) = &rel_location
                    && epoch_fresh
                {
                    self.nav_refine_cache.insert(key.clone(), loc.clone());
                }
                if let Some(pending) = self.nav_pending_lsp_jump.as_ref()
                    && pending.lang == lang
                    && pending.cache_key == identifier
                    && pending.generation == generation
                {
                    let pending = self.nav_pending_lsp_jump.take().expect("just checked");
                    match rel_location {
                        Some(loc) => {
                            // A stale popup must not survive underneath
                            // an async jump.
                            self.nav_candidates = None;
                            self.nav_push_back(pending.origin);
                            self.nav_jump_to_lsp(&loc);
                        }
                        None if location.is_some() => {
                            self.toasts.push(Toast::info(
                                "Definition is outside the workspace".to_string(),
                            ));
                        }
                        None => {
                            // Server answered "no definition".
                            self.toasts
                                .push(Toast::info("No definition found".to_string()));
                        }
                    }
                }
            }
            WorkerResult::LspStateChange { lang, state } => {
                self.handle_lsp_state_change(lang, state);
            }
        }
    }

    /// Open Settings. No-op when already open so a stray re-entry
    /// can't silently discard an in-progress inline text edit. The
    /// pref-cache refresh keeps the page reading from memory rather
    /// than disk on every render.
    pub fn open_settings(&mut self) {
        if self.view_mode == ViewMode::Settings {
            return;
        }
        self.view_mode = ViewMode::Settings;
        self.settings.editor_edit = None;
        crate::settings::refresh_pref_cache(&mut self.settings);
        // Re-probe LSP binaries so the Code Navigation rows reflect any
        // out-of-band install (e.g. the user ran `cargo install
        // rust-analyzer` in another terminal since launch). Cheap, and
        // off the render path. Without this, a row could read "Missing"
        // and re-install an already-present server.
        self.refresh_lsp_installed();
    }

    /// Esc semantics — uncommitted buffer discarded, Enter is the
    /// explicit commit.
    pub fn close_settings(&mut self) {
        self.view_mode = ViewMode::Main;
        self.settings.editor_edit = None;
    }

    /// Enter "pure preview" (纯预览) — the active tab's preview/diff
    /// panel takes the whole screen. On entry we move `active_panel`
    /// onto the content column so scroll keys route the right way;
    /// `ui::render` decides what to show based on `active_tab`. Esc
    /// flips `view_mode` back to `Main`.
    ///
    /// No-op when not in `Main`. Doesn't validate that there's
    /// something to show — an empty/loading preview is a normal state
    /// to render, just maximised.
    pub fn enter_focused_preview(&mut self) {
        if self.view_mode != ViewMode::Main {
            return;
        }
        // Pick the panel that owns the content column for this tab. The
        // Graph tab in 2-col mode draws its inline diff via the Commit
        // panel; everywhere else the Diff panel is the content column.
        self.active_panel = match self.active_tab {
            Tab::Graph if !self.graph_uses_three_col() => Panel::Commit,
            _ => Panel::Diff,
        };
        self.view_mode = ViewMode::FocusedPreview;
        // Chord state is scoped to a view mode in users' mental model —
        // entering 纯预览 with `gg` half-pressed and resuming it on a
        // different panel would feel like a glitch. Clear on the
        // boundary.
        self.g_pending_at = None;
        self.space_leader_at = None;
    }

    /// CLI entry point — `reef <file>` lands here after the workdir
    /// has been opened at the file's parent (or the repo root, when
    /// the file is inside a git repo — see `resolve_local_path`).
    /// Drops straight into FocusedPreview against the given
    /// workdir-relative path.
    ///
    /// Race-fix history: `App::new_with_backend` already fires an
    /// async `refresh_file_tree` with an *empty* `selected_path`
    /// snapshot. A naïve "reveal then dispatch_preview_load" would
    /// lose the race — the worker comes back with `selected_idx = 0`,
    /// `replace_entries` overwrites our selection, and the post-rebuild
    /// `before != after` check fires a second `load_preview()` that
    /// clobbers the in-flight foo.rs preview with whatever entries[0]
    /// happens to be (often README.md).
    ///
    /// Fix: re-fire `refresh_file_tree_with_target(Some(rel))`
    /// *after* setting the expanded ancestors via `reveal`. The new
    /// task supersedes the prior one (generation bump), the worker
    /// now captures the right `selected_path` snapshot, and
    /// `apply_worker_result`'s eventual `load_preview()` for the
    /// post-rebuild selection lands on the same `rel` path — same
    /// path means dedupe at the dispatch layer (or a benign redundant
    /// load against the file we actually want).
    pub fn enter_focused_preview_with_file(&mut self, rel: PathBuf) {
        self.set_active_tab(Tab::Files);
        // Expand ancestor directories so the row is visible once the
        // tree task lands. `reveal` no-ops the selection move on empty
        // entries but always populates `self.expanded` — and the new
        // refresh_file_tree_with_target call below reads from
        // `file_tree.expanded_paths()` at dispatch time, so the
        // expanded ancestors do reach the worker.
        self.file_tree.reveal(&rel);
        // Supersede the App::new tree task with one that knows the
        // target path. The worker uses this to pin `selected_idx` and
        // skips the wrong-file post-rebuild preview load.
        self.refresh_file_tree_with_target(Some(rel.clone()));
        // Start the preview immediately so the maximised panel isn't
        // blank during the (~10ms) tree-rebuild window.
        self.preview_schedule = None;
        self.prefetch_schedule = None;
        self.dispatch_preview_load(rel);
        self.enter_focused_preview();
    }

    pub fn close_focused_preview(&mut self) {
        if self.view_mode != ViewMode::FocusedPreview {
            return;
        }
        self.view_mode = ViewMode::Main;
        // Mirror `enter_focused_preview`: clear chord state on the
        // view-mode boundary so a `gg` half-press doesn't carry from
        // Main into FocusedPreview or vice versa.
        self.g_pending_at = None;
        self.space_leader_at = None;
        // Defensive: clear picker state so a future view_mode escape
        // path (toast-driven, fatal-error overlay, etc.) that bypasses
        // close_focused_preview_files can't leave the bool stuck `true`
        // and re-paint the picker the next time the user enters
        // FocusedPreview on a different tab. Today every reachable
        // close path already runs close_focused_preview_files first,
        // but the invariant is implicit — make it explicit here.
        self.focused_preview_files_open = false;
        self.focused_preview_files_selected = 0;
    }

    /// Space+V routing: enter from Main, exit from FocusedPreview.
    /// No-op while Settings owns the screen so a stray chord can't
    /// silently discard an in-progress settings edit.
    pub fn toggle_focused_preview(&mut self) {
        match self.view_mode {
            ViewMode::Main => self.enter_focused_preview(),
            ViewMode::FocusedPreview => self.close_focused_preview(),
            ViewMode::Settings => {}
        }
    }

    /// Shared gate for the ☰ chip + file picker: whether the chip
    /// affordance is visually present right now. The renderer in
    /// `focused_preview_panel::render` checks this, and the input
    /// handler's `o` key shortcut checks the same predicate so the
    /// two states never disagree (the original bug had keyboard
    /// opening a picker the renderer refused to draw on Graph 2-col).
    ///
    /// Graph 2-col uses `commit_detail_panel`'s header (commit
    /// metadata + inline files tree), which doesn't match the
    /// `path_display + tag_str` layout the chip's wash math assumes —
    /// so the chip is deliberately not rendered there, and the `o`
    /// shortcut likewise no-ops.
    pub fn focused_preview_chip_visible(&self) -> bool {
        if !self.backend.has_repo() {
            return false;
        }
        match self.active_tab {
            Tab::Git => true,
            Tab::Graph => self.graph_uses_three_col(),
            _ => false,
        }
    }

    // ── FocusedPreview floating file picker ──────────────────────────

    /// Snapshot of the diff-changed file list to render in the popup.
    /// Built fresh each call from `staged_files + unstaged_files` (Git)
    /// or `commit_detail.detail.files` (Graph). Sorted by path so the
    /// indented "tree-ish" layout in the popup is stable.
    pub fn focused_preview_file_entries(&self) -> Vec<FocusedPreviewFileRow> {
        match self.active_tab {
            Tab::Git => {
                let mut out: Vec<FocusedPreviewFileRow> = Vec::new();
                for f in &self.staged_files {
                    out.push(FocusedPreviewFileRow {
                        path: f.path.clone(),
                        status: f.status.label().chars().next().unwrap_or(' '),
                        source: FocusedPreviewFileSource::GitStaged,
                    });
                }
                for f in &self.unstaged_files {
                    out.push(FocusedPreviewFileRow {
                        path: f.path.clone(),
                        status: f.status.label().chars().next().unwrap_or(' '),
                        source: FocusedPreviewFileSource::GitUnstaged,
                    });
                }
                out.sort_by(|a, b| a.path.cmp(&b.path));
                out
            }
            Tab::Graph => {
                let Some(detail) = self.commit_detail.detail.as_ref() else {
                    return Vec::new();
                };
                let mut out: Vec<FocusedPreviewFileRow> = detail
                    .files
                    .iter()
                    .map(|f| FocusedPreviewFileRow {
                        path: f.path.clone(),
                        status: f.status.label().chars().next().unwrap_or(' '),
                        source: FocusedPreviewFileSource::GraphCommit,
                    })
                    .collect();
                out.sort_by(|a, b| a.path.cmp(&b.path));
                out
            }
            // Files / Search tabs have no concept of "changed files" —
            // the chip isn't rendered there, so this returns empty.
            _ => Vec::new(),
        }
    }

    /// Open the floating picker. Snaps the highlighted row to whatever
    /// file the diff is currently showing, so ↑/↓ navigation feels like
    /// it picks up where the user already is. Falls back to row 0 if
    /// the current target isn't in the list (e.g. no diff loaded yet).
    ///
    /// Git tab subtlety: a file can legitimately appear in both staged
    /// and unstaged at the same time (committed + further edited). The
    /// snap must compare `is_staged` too — otherwise opening the picker
    /// while viewing the unstaged diff would snap to the staged row
    /// (sort order puts staged first) and a no-op Enter would silently
    /// switch the diff target from unstaged to staged.
    pub fn open_focused_preview_files(&mut self) {
        let entries = self.focused_preview_file_entries();
        if entries.is_empty() {
            return;
        }
        let idx = match self.active_tab {
            Tab::Git => {
                let sel = self.selected_file.as_ref();
                sel.and_then(|s| {
                    let target_src = if s.is_staged {
                        FocusedPreviewFileSource::GitStaged
                    } else {
                        FocusedPreviewFileSource::GitUnstaged
                    };
                    entries
                        .iter()
                        .position(|e| e.path == s.path && e.source == target_src)
                })
                .unwrap_or(0)
            }
            Tab::Graph => self
                .commit_detail
                .file_diff
                .as_ref()
                .and_then(|d| entries.iter().position(|e| e.path == d.path))
                .unwrap_or(0),
            _ => 0,
        };
        self.focused_preview_files_selected = idx;
        self.focused_preview_files_open = true;
    }

    pub fn close_focused_preview_files(&mut self) {
        self.focused_preview_files_open = false;
    }

    pub fn toggle_focused_preview_files(&mut self) {
        if self.focused_preview_files_open {
            self.close_focused_preview_files();
        } else {
            self.open_focused_preview_files();
        }
    }

    pub fn move_focused_preview_files_selection(&mut self, delta: i32) {
        let len = self.focused_preview_file_entries().len();
        if len == 0 {
            return;
        }
        let cur = self.focused_preview_files_selected as i32;
        let next = (cur + delta).rem_euclid(len as i32);
        self.focused_preview_files_selected = next as usize;
    }

    /// Apply the highlighted row: load the corresponding file's diff
    /// and close the picker. The diff render path then picks up the
    /// new target on the next frame.
    pub fn confirm_focused_preview_files_selection(&mut self) {
        let entries = self.focused_preview_file_entries();
        let Some(row) = entries.get(self.focused_preview_files_selected).cloned() else {
            return;
        };
        self.apply_focused_preview_file_pick(&row);
        self.close_focused_preview_files();
    }

    /// Mouse-click variant — picks by absolute index, used by the
    /// `PickFocusedPreviewFile(usize)` action.
    pub fn pick_focused_preview_file(&mut self, idx: usize) {
        let entries = self.focused_preview_file_entries();
        let Some(row) = entries.get(idx).cloned() else {
            return;
        };
        self.focused_preview_files_selected = idx;
        self.apply_focused_preview_file_pick(&row);
        self.close_focused_preview_files();
    }

    fn apply_focused_preview_file_pick(&mut self, row: &FocusedPreviewFileRow) {
        match row.source {
            FocusedPreviewFileSource::GitStaged => self.select_file(&row.path, true),
            FocusedPreviewFileSource::GitUnstaged => self.select_file(&row.path, false),
            FocusedPreviewFileSource::GraphCommit => self.load_commit_file_diff(&row.path),
        }
    }

    pub fn set_active_tab(&mut self, tab: Tab) {
        if self.active_tab == tab {
            return;
        }
        let was_files = self.active_tab == Tab::Files;
        self.active_tab = tab;
        // Drop any in-flight chord state. Without this, "press `g` →
        // mouse-click another tab within 500 ms → press `g`" would fire
        // `scroll_active_preview_to_top` against the *new* tab, which
        // can resync diff scroll / commit selection unexpectedly.
        // Symmetric `space_leader_at` reset for the same reason: a
        // primed leader that crosses a tab boundary no longer maps to
        // the chord targets the user intended.
        self.g_pending_at = None;
        self.space_leader_at = None;
        // Leaving the Files tab cancels any Files-tab-scoped modal —
        // tree edit row, context menu, delete confirm. Those modals
        // are invisible on other tabs, so leaving them armed would
        // let a stray key or click fire them from a tab where the
        // corresponding file tree isn't even being rendered.
        if was_files {
            self.tree_edit.clear();
            self.tree_context_menu.close();
            self.dismiss_confirm();
        }
        // Preview selection is scoped to the Files/Search tabs that render
        // the preview panel. Switching away clears it so no stale highlight
        // appears on return and the click-count resets cleanly.
        self.preview_selection = None;
        self.preview_click_state = None;
        self.clear_commit_detail_selection();
        // FocusedPreview file picker is also tab-scoped (entries come
        // from the active tab's diff list). Switching tabs while the
        // picker is open would carry the popup state to a tab whose
        // changed-file list is unrelated.
        self.focused_preview_files_open = false;
        self.focused_preview_files_selected = 0;
        // Same for diff-panel selection — the Git tab and the Graph tab
        // 3-col diff column share this state, and tab-switching between
        // them (or to Files/Search) should start fresh.
        self.clear_diff_selection();
        // The find widget is anchored to whatever content panel was
        // active when it opened; carrying it across tabs would paint
        // highlights on rows the user can't see. Close (which also
        // restores pre-find scroll) before the tab actually changes.
        crate::find_widget::close(self);
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
            ClickAction::OpenSettings => {
                self.open_settings();
            }
            ClickAction::OpenHostsPicker => {
                self.open_hosts_picker();
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
            ClickAction::StartDragSplit => {
                self.dragging_split = true;
            }
            ClickAction::StartDragGraphDiffSplit => {
                self.dragging_graph_diff_split = true;
            }
            ClickAction::GitCommand { command, args } => {
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
            // Graph branch picker: overlay-only, dispatched inline in
            // `input::handle_mouse_graph_branch_picker`, never reaches here.
            ClickAction::GraphBranchPickerSelect(_) => {}
            // Tab::Search result clicks DO route through here — the tab is
            // not an overlay, so input::handle_mouse lets the click fall
            // through to hit_test + handle_action. Update the selection and
            // trigger live preview.
            ClickAction::GlobalSearchSelect(idx) => {
                if self.active_tab == Tab::Search {
                    self.global_search.core.selected_idx = idx;
                    crate::global_search::navigate_to_selected(self);
                }
                // Overlay case is unreachable via this path — handled inline
                // in `global_search::handle_mouse`.
            }
            ClickAction::GlobalSearchFocusInput => {
                if self.active_tab == Tab::Search {
                    self.global_search.focus = crate::global_search::SearchPanelFocus::FindInput;
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
            ClickAction::FileTreeToolbarCollapse => {
                self.collapse_all_tree_entries();
            }
            ClickAction::TreeContextMenuItem(item) => {
                self.dispatch_context_menu_item(item);
            }
            ClickAction::TreeContextMenuClose => {
                self.close_tree_context_menu();
            }
            ClickAction::NavCandidateSelect(idx) => {
                // Move selection to the clicked row, then commit. A
                // double-click semantics here would be safer (single
                // = move, double = pick), but the tree context menu
                // commits on single click too — keep them consistent.
                if let Some(popup) = self.nav_candidates.as_mut() {
                    if idx < popup.candidates.len() {
                        popup.selected = idx;
                    }
                }
                self.nav_pick_candidate();
            }
            ClickAction::NavCandidatesClose => {
                self.nav_close_candidates();
            }
            ClickAction::OpenMarkdownLink(target) => {
                self.open_markdown_link(&target);
            }
            ClickAction::HostsPickerSelect(idx) => {
                // Mouse click on a hosts-picker row: move selection to
                // that row and (for paths that already have a target)
                // commit. The picker's own keyboard path goes through a
                // different method, so here we just re-use `move_selection`
                // by computing the delta.
                let current = self.hosts_picker.core.selected_idx;
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
            ClickAction::DbSelectObject { schema, name, kind } => {
                self.db_select_object(reef_sqlite_preview::DbObjectKey { schema, name, kind });
            }
            ClickAction::DbToggleSchema(name) => {
                self.db_toggle_schema(&name);
            }
            ClickAction::FindWidgetClose => {
                crate::find_widget::close(self);
            }
            ClickAction::FindWidgetNext => {
                if !self.find_widget.matches.is_empty() {
                    crate::find_widget::step(self, /*reverse=*/ false);
                }
            }
            ClickAction::FindWidgetPrev => {
                if !self.find_widget.matches.is_empty() {
                    crate::find_widget::step(self, /*reverse=*/ true);
                }
            }
            ClickAction::FindWidgetToggleCase => {
                if self.find_widget.active {
                    self.find_widget.match_case = !self.find_widget.match_case;
                    crate::find_widget::recompute(self);
                }
            }
            ClickAction::FindWidgetToggleWord => {
                if self.find_widget.active {
                    self.find_widget.whole_word = !self.find_widget.whole_word;
                    crate::find_widget::recompute(self);
                }
            }
            ClickAction::FindWidgetToggleRegex => {
                if self.find_widget.active {
                    self.find_widget.regex = !self.find_widget.regex;
                    crate::find_widget::recompute(self);
                }
            }
            ClickAction::SearchToggleReplace => {
                if self.active_tab == Tab::Search {
                    self.global_search.replace_open = !self.global_search.replace_open;
                    if !self.global_search.replace_open
                        && matches!(
                            self.global_search.focus,
                            crate::global_search::SearchPanelFocus::ReplaceInput
                        )
                    {
                        self.global_search.focus =
                            crate::global_search::SearchPanelFocus::FindInput;
                    }
                }
            }
            ClickAction::GlobalSearchFocusReplaceInput => {
                if self.active_tab == Tab::Search && self.global_search.replace_open {
                    self.global_search.focus = crate::global_search::SearchPanelFocus::ReplaceInput;
                }
            }
            ClickAction::SearchToggleMatch(idx) => {
                self.global_search.toggle_match_excluded(idx);
            }
            ClickAction::SearchApplyReplace => {
                self.commit_replace_in_files();
            }
            ClickAction::SettingsRow(idx) => {
                if self.view_mode == ViewMode::Settings {
                    self.settings.select(idx);
                    // LSP rows are actionable buttons ("Enter to
                    // install") — a click should install, matching the
                    // user's expectation and every other clickable list
                    // in the UI. Other settings rows keep the
                    // click-selects / Enter-activates convention so a
                    // stray click can't flip a pref.
                    if let crate::settings::SettingItem::Lsp(lang) = self.settings.selected() {
                        self.activate_lsp_row(lang);
                    }
                }
            }
            ClickAction::ConfirmModalPrimary => {
                self.fire_confirm_primary();
            }
            ClickAction::ConfirmModalCancel => {
                self.fire_confirm_cancel();
            }
            ClickAction::ToggleFocusedPreviewFiles => {
                self.toggle_focused_preview_files();
            }
            ClickAction::PickFocusedPreviewFile(idx) => {
                self.pick_focused_preview_file(idx);
            }
            ClickAction::CloseFocusedPreviewFiles => {
                self.close_focused_preview_files();
            }
        }
    }

    pub fn open_markdown_link(&mut self, target: &str) {
        if Self::is_url_link(target) {
            match Self::open_external_url(target) {
                Ok(()) => self.toasts.push(Toast::info(format!("Opened {target}"))),
                Err(e) => self
                    .toasts
                    .push(Toast::error(format!("Open link failed: {e}"))),
            }
            return;
        }

        let Some(rel) = self.markdown_file_link_target(target) else {
            self.toasts
                .push(Toast::warn(format!("Markdown link not found: {target}")));
            return;
        };
        self.push_location_before_jump();
        self.set_active_tab(Tab::Files);
        self.file_tree.reveal(&rel);
        self.refresh_file_tree_with_target(Some(rel.clone()));
        self.load_preview_for_path(rel);
    }

    fn is_url_link(target: &str) -> bool {
        url::Url::parse(target).is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "mailto"))
    }

    fn open_external_url(target: &str) -> Result<(), String> {
        let mut command = Self::open_external_url_command(target)?;
        command.spawn().map(|_| ()).map_err(|e| e.to_string())
    }

    #[cfg(target_os = "macos")]
    fn open_external_url_command(target: &str) -> Result<std::process::Command, String> {
        let mut command = std::process::Command::new("open");
        command.arg(target);
        Ok(command)
    }

    #[cfg(target_os = "windows")]
    fn open_external_url_command(target: &str) -> Result<std::process::Command, String> {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", target]);
        Ok(command)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    fn open_external_url_command(target: &str) -> Result<std::process::Command, String> {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(target);
        Ok(command)
    }

    #[cfg(not(any(unix, target_os = "windows")))]
    fn open_external_url_command(_target: &str) -> Result<std::process::Command, String> {
        Err("opening URLs is not supported on this platform".to_string())
    }

    fn markdown_file_link_target(&self, target: &str) -> Option<PathBuf> {
        let path_part = target
            .split_once('#')
            .map(|(path, _)| path)
            .unwrap_or(target);
        if path_part.is_empty() {
            return None;
        }

        let path = Path::new(path_part);
        if path.is_absolute() {
            if self.backend.is_remote() {
                return Self::remote_workdir_relative(&self.backend.workdir_path(), path);
            }
            return self.workdir_relative(path);
        }

        let preview_path = self
            .preview_content
            .as_ref()
            .map(|preview| Path::new(&preview.path))?;
        let base = preview_path.parent().unwrap_or_else(|| Path::new(""));
        Self::normalize_relative_path(base.join(path))
    }

    fn remote_workdir_relative(root: &Path, abs: &Path) -> Option<PathBuf> {
        let rel = abs.strip_prefix(root).ok()?;
        Self::normalize_relative_path(rel.to_path_buf())
    }

    fn normalize_relative_path(path: PathBuf) -> Option<PathBuf> {
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => out.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop().then_some(())?;
                }
                Component::RootDir | Component::Prefix(_) => return None,
            }
        }
        Some(out)
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
            if self.git_status.tree_mode {
                for path in git_tree::visible_file_paths(
                    &self.staged_files,
                    true,
                    &self.git_status.collapsed_dirs,
                ) {
                    items.push((path, true));
                }
            } else {
                for f in &self.staged_files {
                    items.push((f.path.clone(), true));
                }
            }
        }
        if !self.unstaged_collapsed {
            if self.git_status.tree_mode {
                for path in git_tree::visible_file_paths(
                    &self.unstaged_files,
                    false,
                    &self.git_status.collapsed_dirs,
                ) {
                    items.push((path, false));
                }
            } else {
                for f in &self.unstaged_files {
                    items.push((f.path.clone(), false));
                }
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

    /// Vim `gg` — jump the active content panel to its top. List panels
    /// (file tree, git status, commit graph) move their selection to the
    /// first row; content panels (Diff/Commit) reset the vertical scroll.
    /// SQLite preview is a no-op so the bare `g` keystroke can still open
    /// the goto-page input.
    ///
    /// **Side effects on list panels** — list-mode `gg` is not a pure
    /// cursor move:
    /// - Git Files: `navigate_files` also resets `diff_scroll` /
    ///   `diff_h_scroll` / `sbs_left/right_h_scroll` and triggers a
    ///   deferred `load_diff()` for the new selection, so the right
    ///   pane reloads to file #1's diff.
    /// - Graph Files: `move_graph_selection` clears any visual range
    ///   (when no anchor is set; when an anchor exists we delegate to
    ///   `extend_graph_selection` and preserve the range — matching
    ///   vim's behaviour) and triggers `load_commit_detail()` for the
    ///   new selection.
    pub fn scroll_active_preview_to_top(&mut self) {
        if self.is_sqlite_preview() {
            return;
        }
        // Symmetric sentinel for the two directions. `i32::MIN` would
        // overflow in the `(-delta) as usize` paths inside
        // `file_tree::navigate` / `navigate_files` / `move_graph_selection`
        // (debug-build panic); pick a value that's far larger than any
        // realistic entry count but safe to negate.
        const NAV_FAR: i32 = 1_000_000;
        match (self.active_tab, self.active_panel) {
            (Tab::Files, Panel::Files) => {
                if !self.file_tree.entries.is_empty() {
                    self.file_tree.navigate(-NAV_FAR);
                }
            }
            (Tab::Files, Panel::Diff) | (Tab::Search, Panel::Diff) => {
                self.preview_scroll = 0;
            }
            (Tab::Search, Panel::Files) => {
                // Search-tab left column owns its own list cursor via the
                // global_search overlay — leave it alone here.
            }
            (Tab::Git, Panel::Files) => {
                self.navigate_files(-NAV_FAR);
            }
            (Tab::Git, Panel::Diff) => {
                self.diff_scroll = 0;
            }
            (Tab::Graph, Panel::Files) => {
                // Preserve any Shift-extended visual range: vim's `gg`
                // in visual mode extends the selection to the top, so
                // delegate to `extend_graph_selection` (which keeps
                // `selection_anchor` intact). Without this, the chord
                // would call `move_graph_selection` and silently
                // collapse the range to a single commit.
                if self.git_graph.selection_anchor.is_some() {
                    self.extend_graph_selection(-NAV_FAR);
                } else {
                    self.move_graph_selection(-NAV_FAR);
                }
            }
            (Tab::Graph, Panel::Commit) => {
                self.commit_detail.scroll = 0;
            }
            (Tab::Graph, Panel::Diff) => {
                self.commit_detail.file_diff_scroll = 0;
            }
            _ => {}
        }
    }

    /// Vim `G` — jump the active content panel to its bottom. List panels
    /// move selection to the last row; content panels set the scroll to
    /// `usize::MAX` and rely on the render-layer clamp
    /// (ui::preview / diff_panel / commit_detail_panel all clamp
    /// against `lines.len() - viewport`).
    pub fn scroll_active_preview_to_bottom(&mut self) {
        if self.is_sqlite_preview() {
            return;
        }
        const NAV_FAR: i32 = 1_000_000;
        match (self.active_tab, self.active_panel) {
            (Tab::Files, Panel::Files) => {
                if !self.file_tree.entries.is_empty() {
                    self.file_tree.navigate(NAV_FAR);
                }
            }
            (Tab::Files, Panel::Diff) | (Tab::Search, Panel::Diff) => {
                self.preview_scroll = usize::MAX;
            }
            (Tab::Search, Panel::Files) => {}
            (Tab::Git, Panel::Files) => {
                self.navigate_files(NAV_FAR);
            }
            (Tab::Git, Panel::Diff) => {
                self.diff_scroll = usize::MAX;
            }
            (Tab::Graph, Panel::Files) => {
                // Mirror scroll_active_preview_to_top: keep visual-range
                // anchors so `G` extends rather than collapses.
                if self.git_graph.selection_anchor.is_some() {
                    self.extend_graph_selection(NAV_FAR);
                } else {
                    self.move_graph_selection(NAV_FAR);
                }
            }
            (Tab::Graph, Panel::Commit) => {
                self.commit_detail.scroll = usize::MAX;
            }
            (Tab::Graph, Panel::Diff) => {
                self.commit_detail.file_diff_scroll = usize::MAX;
            }
            _ => {}
        }
    }

    /// True when the active content panel is rendering a SQLite database
    /// preview *in a tab whose handler actually wires bare `g` to the
    /// goto-page input*. Only `Tab::Files` qualifies — `handle_key_search`
    /// has no `g` → `db_goto_input` branch, so suppressing `gg` there
    /// would silence both the chord and the per-tab fallback, leaving
    /// bare `g` as a no-op against a .db preview opened from Search.
    pub fn is_sqlite_preview(&self) -> bool {
        self.active_panel == Panel::Diff
            && self.active_tab == Tab::Files
            && self
                .preview_content
                .as_ref()
                .is_some_and(|p| p.is_database())
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
            // Workspace symbol index follows the same lazy-rebuild rule
            // as quick_open. fs_watcher is coarse — a single `()`
            // pulse with no path info — so we invalidate the entire
            // index and let the next `kick_active_tab_work` rebuild it.
            self.nav_workspace_load.mark_stale();
            // The LSP refine cache maps a click position to a resolved
            // definition location. A file edit can move that symbol, so
            // a cached entry would jump to a stale line. Drop the whole
            // cache on any fs change — it refills lazily on the next
            // `gd`. (Same coarse-invalidation rationale as above.)
            self.nav_refine_cache.clear();
            // Bump the epoch so any refine dispatched before this clear
            // (its location snapshotted from pre-edit bytes) is dropped
            // by the `LspRefineDone` handler instead of repopulating the
            // cache we just emptied.
            self.nav_refine_epoch = self.nav_refine_epoch.wrapping_add(1);
        }

        // VSCode "Reveal" fade — clear `preview_highlight` after
        // `PREVIEW_HIGHLIGHT_TTL` so the highlight doesn't linger
        // forever on the destination line. Set on the rising edge
        // (None → Some) and consumed on expiry. Cleared synchronously
        // here so the next render sees no highlight.
        self.advance_preview_highlight_fade();

        self.maybe_kick_global_search();
        // Lazy rebuild of the nav workspace index when stale and idle.
        // Same pattern as quick_open's lazy rebuild — index work is
        // wasteful to re-trigger on every fs event, but the next
        // `gd` / `gr` after the user-visible quiet period gets a
        // fresh index.
        if self.nav_workspace_load.should_request() {
            self.dispatch_nav_workspace_build();
        }
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
        crate::input::tick_drag_autoscroll(self);
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
            .get(self.global_search.core.selected_idx)
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
        if self.global_search.core.filter == self.global_search.last_searched_query {
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
        self.global_search.core.selected_idx = 0;
        self.global_search.scroll = 0;
        // New query → fresh results → start from smart-view. Leaving a
        // stale h-scroll here would mean the first chunks land already
        // offset, which looks like a bug.
        self.global_search.results_h_scroll = 0;
        self.global_search.last_searched_query = self.global_search.core.filter.clone();
        self.global_search.last_keystroke_at = None;

        if self.global_search.core.filter.is_empty() {
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
            self.global_search.core.filter.clone(),
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
                if self.preview_load.should_request() && self.preview_schedule.is_none() {
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
                // Refresh policy:
                //   - "stale w/o error" (e.g. fs-watcher mark_stale, tab
                //     activation) → immediate
                //   - "stale w/ error" → throttled by should_poll_graph
                //     (otherwise persistent worker failures would
                //     hammer at frame rate as soon as complete_err
                //     clears `loading`)
                //   - periodic poll → throttled by should_poll_graph
                //
                // The previous "rows.is_empty() && !loading" recovery
                // arm collapses into the periodic arm: bootstrap fires
                // on the first tick (next_graph_revalidate_at is in
                // the past at construction) but errors back off.
                let stale_no_error = self.graph_load.stale && self.graph_load.error.is_none();
                if !self.graph_load.loading && (stale_no_error || should_poll_graph) {
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
                    && self.preview_schedule.is_none()
                    && let Some(hit) = self
                        .global_search
                        .results
                        .get(self.global_search.core.selected_idx)
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
    let layout = crate::prefs::get("diff.layout")
        .as_deref()
        .map(DiffLayout::from_pref_str)
        .unwrap_or(DiffLayout::Unified);
    let mode = crate::prefs::get("diff.mode")
        .as_deref()
        .map(DiffMode::from_pref_str)
        .unwrap_or(DiffMode::Compact);
    (layout, mode)
}

fn save_prefs(layout: DiffLayout, mode: DiffMode) {
    crate::prefs::set("diff.layout", layout.pref_str());
    crate::prefs::set("diff.mode", mode.pref_str());
}

/// Prefs key for the Graph tab's active scope (one of `all` or
/// `branch:<full_ref>`).
pub(crate) const PREF_GRAPH_SCOPE: &str = "graph.scope";
/// Prefs key for the recent-branch MRU shown at the top of the
/// branch picker. Tab-separated full ref names.
pub(crate) const PREF_GRAPH_SCOPE_RECENT: &str = "graph.scope.recent";

/// Decode the persisted `graph.scope` value. Unknown / malformed
/// values fall back to `AllRefs` rather than erroring — we'd rather
/// silently lose a stale pref than refuse to boot.
pub(crate) fn load_graph_scope_pref() -> (GraphScope, Vec<String>) {
    let scope = match crate::prefs::get(PREF_GRAPH_SCOPE).as_deref() {
        Some("all") | None => GraphScope::AllRefs,
        Some(other) => other
            .strip_prefix("branch:")
            .filter(|r| !r.is_empty())
            .map(|r| GraphScope::Branch(r.to_string()))
            .unwrap_or(GraphScope::AllRefs),
    };
    let recent = crate::prefs::get(PREF_GRAPH_SCOPE_RECENT)
        .map(|raw| {
            raw.split('\t')
                .filter(|s| !s.is_empty())
                .take(GRAPH_RECENT_BRANCHES_MAX)
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    (scope, recent)
}

/// Persist the Graph tab's scope + recents. Called from
/// [`App::set_graph_scope`] every time the user accepts a picker
/// selection (or the stale-branch fallback fires).
pub(crate) fn persist_graph_scope(scope: &GraphScope, recent: &[String]) {
    let value = match scope {
        GraphScope::AllRefs => "all".to_string(),
        GraphScope::Branch(s) => format!("branch:{s}"),
    };
    crate::prefs::set(PREF_GRAPH_SCOPE, &value);
    crate::prefs::set(PREF_GRAPH_SCOPE_RECENT, &recent.join("\t"));
}

/// `true` when the payload's scope ref isn't present anywhere in the
/// payload's `ref_map`. Used by the stale-branch fallback to confirm
/// "ref really doesn't exist" before swapping scope back to `AllRefs`
/// — distinguishes a genuinely-deleted branch from a transient
/// `revwalk()` error (both surface as empty rows in the payload).
fn payload_scope_ref_missing(payload: &crate::tasks::GraphPayload) -> bool {
    let GraphScope::Branch(target) = &payload.scope else {
        return false;
    };
    !payload.ref_map.values().any(|labels| {
        labels.iter().any(|label| match label {
            RefLabel::Branch(name) => format!("refs/heads/{name}") == *target,
            RefLabel::RemoteBranch(name) => format!("refs/remotes/{name}") == *target,
            _ => false,
        })
    })
}

/// Strip the `refs/heads/` or `refs/remotes/` prefix off a fully-qualified
/// ref for display purposes (toasts, picker rows, panel titles).
pub(crate) fn shorthand_for_full_ref(full_ref: &str) -> &str {
    full_ref
        .strip_prefix("refs/heads/")
        .or_else(|| full_ref.strip_prefix("refs/remotes/"))
        .unwrap_or(full_ref)
}

#[cfg(test)]
mod tests {
    use super::{
        App, GRAPH_RECENT_BRANCHES_MAX, GitGraphState, PREF_GRAPH_SCOPE, PREF_GRAPH_SCOPE_RECENT,
        folder_contains, load_graph_scope_pref, persist_graph_scope,
    };
    use crate::tasks::{GraphPayload, WorkerResult};
    use crate::ui::theme::Theme;
    use reef_core::git::{FileEntry, FileStatus, GraphScope};
    use reef_core::preview::{PreviewBody, PreviewDocument as PreviewContent};
    use reef_io::LocalBackend;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use test_support::{HOME_LOCK, HomeGuard, commit_file, tempdir_repo};

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
    fn markdown_link_targets_resolve_from_preview_directory() {
        assert_eq!(
            App::normalize_relative_path(PathBuf::from("docs/../README.md")),
            Some(PathBuf::from("README.md"))
        );
        assert_eq!(
            App::normalize_relative_path(PathBuf::from("../outside.md")),
            None
        );
        assert!(App::is_url_link("https://example.com/a"));

        let mut fx = make_scope_fixture();
        fx.app.preview_content = Some(PreviewContent {
            path: "docs/guide/index.md".into(),
            body: PreviewBody::Text(reef_core::preview::TextPreview {
                lines: vec![],
                highlighted: None,
                parsed: None,
            }),
        });

        assert_eq!(
            fx.app.markdown_file_link_target("../intro.md#top"),
            Some(PathBuf::from("docs/intro.md"))
        );
        assert_eq!(fx.app.markdown_file_link_target("#local"), None);
    }

    #[test]
    fn remote_markdown_absolute_links_resolve_under_workdir() {
        let root = PathBuf::from("/home/me/repo");
        assert_eq!(
            App::remote_workdir_relative(&root, Path::new("/home/me/repo/docs/a.md")),
            Some(PathBuf::from("docs/a.md"))
        );
        assert_eq!(
            App::remote_workdir_relative(&root, Path::new("/etc/passwd")),
            None
        );
    }

    fn git_entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn navigate_files_tree_mode_follows_visible_tree_order() {
        let mut fx = make_scope_fixture();
        fx.app.git_status.tree_mode = true;
        fx.app.unstaged_files = vec![
            git_entry("z.txt"),
            git_entry("src/z.rs"),
            git_entry("README.md"),
            git_entry("src/a.rs"),
            git_entry("assets/logo.png"),
        ];
        fx.app.selected_file = Some(super::SelectedFile {
            path: "assets/logo.png".to_string(),
            is_staged: false,
        });

        fx.app.navigate_files(1);
        assert_eq!(
            fx.app.selected_file.as_ref().map(|s| s.path.as_str()),
            Some("src/a.rs")
        );

        fx.app.navigate_files(1);
        assert_eq!(
            fx.app.selected_file.as_ref().map(|s| s.path.as_str()),
            Some("src/z.rs")
        );
    }

    #[test]
    fn navigate_files_tree_mode_skips_collapsed_dirs() {
        let mut fx = make_scope_fixture();
        fx.app.git_status.tree_mode = true;
        fx.app.unstaged_files = vec![
            git_entry("src/a.rs"),
            git_entry("README.md"),
            git_entry("src/z.rs"),
            git_entry("z.txt"),
        ];
        fx.app
            .git_status
            .collapsed_dirs
            .insert(reef_core::git::tree::collapsed_key(false, "src"));
        fx.app.selected_file = Some(super::SelectedFile {
            path: "README.md".to_string(),
            is_staged: false,
        });

        fx.app.navigate_files(-1);
        assert_eq!(
            fx.app.selected_file.as_ref().map(|s| s.path.as_str()),
            Some("README.md")
        );
    }

    // ── Graph scope: set / fallback / cache ─────────────────────────────

    /// Per-test HOME isolation. `prefs::*` reads/writes
    /// `~/.config/reef/prefs`, and the scope tests both write (via
    /// `set_graph_scope` / `apply_scope_no_refresh`) and assert prefs
    /// state, so we have to redirect HOME for the duration.
    struct ScopeFixture {
        app: App,
        _home_guard: test_support::HomeGuard,
        _home: tempfile::TempDir,
        _repo: tempfile::TempDir,
        _home_lock: std::sync::MutexGuard<'static, ()>,
    }

    fn make_scope_fixture() -> ScopeFixture {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().expect("home tempdir");
        let home_guard = HomeGuard::enter(home.path());
        let (repo, raw) = tempdir_repo();
        // One commit so HEAD resolves; the worker won't deadlock on us
        // during App construction (it kicks `refresh_graph` synchronously
        // dispatched onto the channel).
        commit_file(&raw, "a.txt", "hello\n", "init");
        let backend = Arc::new(LocalBackend::open_at(repo.path().to_path_buf()));
        let mut app = App::new_with_backend(Theme::dark(), backend, None);
        app.fs_watcher_rx = None;
        ScopeFixture {
            app,
            _home_guard: home_guard,
            _home: home,
            _repo: repo,
            _home_lock: lock,
        }
    }

    #[test]
    fn set_graph_scope_pushes_recents_dedupes_and_caps() {
        // Drive set_graph_scope past the cap; the recents list should
        // stay newest-first, deduped, and clamped at GRAPH_RECENT_BRANCHES_MAX.
        let mut fx = make_scope_fixture();
        for i in 0..(GRAPH_RECENT_BRANCHES_MAX + 2) {
            fx.app
                .set_graph_scope(GraphScope::Branch(format!("refs/heads/b{i}")));
        }
        assert_eq!(
            fx.app.git_graph.recent_branches.len(),
            GRAPH_RECENT_BRANCHES_MAX
        );
        // Newest-first. The most recent push was `b{MAX+1}`.
        let newest = format!("refs/heads/b{}", GRAPH_RECENT_BRANCHES_MAX + 1);
        assert_eq!(fx.app.git_graph.recent_branches[0], newest);

        // Re-pushing an existing branch moves it to the front without
        // duplicating.
        let oldest_kept = fx.app.git_graph.recent_branches.last().cloned().unwrap();
        fx.app
            .set_graph_scope(GraphScope::Branch(oldest_kept.clone()));
        assert_eq!(fx.app.git_graph.recent_branches[0], oldest_kept);
        let dup_count = fx
            .app
            .git_graph
            .recent_branches
            .iter()
            .filter(|s| **s == oldest_kept)
            .count();
        assert_eq!(dup_count, 1, "dedupe must keep exactly one entry");
        assert_eq!(
            fx.app.git_graph.recent_branches.len(),
            GRAPH_RECENT_BRANCHES_MAX
        );
    }

    #[test]
    fn set_graph_scope_invalidates_cache_key_and_resets_selection() {
        // The cache_key is what gates "skip the revwalk"; scope changes
        // must bust it. Selection state from the previous scope is
        // meaningless under the new one and must reset.
        let mut fx = make_scope_fixture();
        fx.app.git_graph.cache_key = Some(("dead".into(), 0xCAFE, 0xBABE));
        fx.app.git_graph.selected_idx = 7;
        fx.app.git_graph.scroll = 5;
        fx.app.git_graph.selection_anchor = Some(3);
        fx.app.git_graph.selected_commit = Some("stale".into());

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert!(fx.app.git_graph.cache_key.is_none());
        assert_eq!(fx.app.git_graph.selected_idx, 0);
        assert_eq!(fx.app.git_graph.scroll, 0);
        assert!(fx.app.git_graph.selection_anchor.is_none());
        assert!(fx.app.git_graph.selected_commit.is_none());
    }

    #[test]
    fn set_graph_scope_persists_to_prefs() {
        // Round-trip through the on-disk pref file so the next session
        // really would land back on the same branch.
        let mut fx = make_scope_fixture();
        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/feature".into()));

        let (scope, recent) = load_graph_scope_pref();
        assert_eq!(scope, GraphScope::Branch("refs/heads/feature".into()));
        assert_eq!(recent, vec!["refs/heads/feature".to_string()]);
    }

    #[test]
    fn set_graph_scope_same_scope_is_noop() {
        let mut fx = make_scope_fixture();
        fx.app.git_graph.scope = GraphScope::Branch("refs/heads/main".into());
        fx.app.git_graph.recent_branches = vec!["refs/heads/main".into()];
        // Pre-seed a cache_key — the no-op path must NOT bust it (only
        // a genuine scope change should).
        fx.app.git_graph.cache_key = Some(("h".into(), 1, 2));
        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));
        assert!(fx.app.git_graph.cache_key.is_some());
    }

    #[test]
    fn stale_branch_payload_falls_back_to_all_refs() {
        // Drive a Graph WorkerResult whose payload claims the scope is a
        // branch but returned zero rows. The main thread should detect
        // the missing-branch case, drop the recent entry, swap scope
        // back to AllRefs, surface a toast, and persist the rollback.
        let mut fx = make_scope_fixture();
        // Stash the scope directly (bypassing `set_graph_scope` so we
        // don't have to babysit the worker round-trip in this test).
        fx.app.git_graph.scope = GraphScope::Branch("refs/heads/ghost".into());
        fx.app.git_graph.recent_branches = vec!["refs/heads/ghost".into()];
        persist_graph_scope(&fx.app.git_graph.scope, &fx.app.git_graph.recent_branches);

        let generation = fx.app.graph_load.begin();
        let payload = GraphPayload {
            rows: Vec::new(),
            ref_map: std::collections::HashMap::new(),
            cache_key: ("h".into(), 0, 0),
            scope: GraphScope::Branch("refs/heads/ghost".into()),
        };
        let toasts_before = fx.app.toasts.len();
        fx.app.apply_worker_result(WorkerResult::Graph {
            generation,
            result: Ok(payload),
        });

        assert_eq!(fx.app.git_graph.scope, GraphScope::AllRefs);
        assert!(
            !fx.app
                .git_graph
                .recent_branches
                .contains(&"refs/heads/ghost".to_string())
        );
        assert!(fx.app.toasts.len() > toasts_before);
        assert!(
            fx.app.toasts.last().unwrap().message.contains("ghost"),
            "toast should name the lost branch"
        );

        // Persisted state matches the fallback.
        assert_eq!(crate::prefs::get(PREF_GRAPH_SCOPE).as_deref(), Some("all"));
        assert_eq!(
            crate::prefs::get(PREF_GRAPH_SCOPE_RECENT)
                .as_deref()
                .unwrap_or(""),
            ""
        );
    }

    #[test]
    fn open_graph_branch_picker_refuses_on_cold_start() {
        // Cold-start safety: with a persisted Branch scope but no
        // graph payload yet (ref_map still empty), pressing 'b' must
        // NOT activate the picker — otherwise visible_rows would only
        // offer `[ All refs ]` and a stray Enter would overwrite the
        // user's persisted choice.
        let mut fx = make_scope_fixture();
        fx.app.git_graph.scope = GraphScope::Branch("refs/heads/feature".into());
        fx.app.git_graph.ref_map.clear();
        let toasts_before = fx.app.toasts.len();

        fx.app.open_graph_branch_picker();

        assert!(
            !fx.app.graph_branch_picker.core.active,
            "picker must not open while ref_map is empty"
        );
        assert!(
            fx.app.toasts.len() > toasts_before,
            "must surface a hint toast"
        );
        // Persisted scope untouched.
        assert_eq!(
            fx.app.git_graph.scope,
            GraphScope::Branch("refs/heads/feature".into())
        );
    }

    #[test]
    fn stale_branch_fallback_skipped_when_ref_still_present() {
        // Walk returned empty (transient revwalk error / lock
        // contention), but the payload's ref_map still lists the
        // branch. Fallback must NOT misfire — no toast, no scope flip,
        // no recents drop.
        let mut fx = make_scope_fixture();
        fx.app.git_graph.scope = GraphScope::Branch("refs/heads/feature".into());
        fx.app.git_graph.recent_branches = vec!["refs/heads/feature".into()];

        let mut ref_map: std::collections::HashMap<String, Vec<reef_core::git::RefLabel>> =
            std::collections::HashMap::new();
        ref_map.insert(
            "abc123".to_string(),
            vec![reef_core::git::RefLabel::Branch("feature".into())],
        );

        let generation = fx.app.graph_load.begin();
        let payload = GraphPayload {
            rows: Vec::new(),
            ref_map,
            cache_key: ("h".into(), 0, 0),
            scope: GraphScope::Branch("refs/heads/feature".into()),
        };
        let toasts_before = fx.app.toasts.len();
        fx.app.apply_worker_result(WorkerResult::Graph {
            generation,
            result: Ok(payload),
        });

        // Scope kept; toast not pushed; recents preserved.
        assert_eq!(
            fx.app.git_graph.scope,
            GraphScope::Branch("refs/heads/feature".into())
        );
        assert_eq!(fx.app.toasts.len(), toasts_before);
        assert!(
            fx.app
                .git_graph
                .recent_branches
                .contains(&"refs/heads/feature".to_string())
        );
    }

    #[test]
    fn apply_scope_no_refresh_invalidates_detail_loads_without_loading() {
        // Stale in-flight detail / file-diff loads must NOT repaint
        // the right panel after a scope swap. We bump generation via
        // `invalidate` (not `begin`) so the status bar doesn't get
        // stuck on a phantom "refreshing…" — verify both that the
        // generation advances AND that `loading` stays false.
        let mut fx = make_scope_fixture();
        // Pretend an old load was in flight so we can confirm `loading`
        // gets cleared, not preserved.
        fx.app.commit_detail_load.loading = true;
        fx.app.commit_file_diff_load.loading = true;
        let before_detail = fx.app.commit_detail_load.generation;
        let before_diff = fx.app.commit_file_diff_load.generation;

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert_ne!(
            fx.app.commit_detail_load.generation, before_detail,
            "commit_detail_load generation must advance"
        );
        assert_ne!(
            fx.app.commit_file_diff_load.generation, before_diff,
            "commit_file_diff_load generation must advance"
        );
        assert!(
            !fx.app.commit_detail_load.loading,
            "commit_detail_load.loading must be cleared (no follow-up dispatcher)"
        );
        assert!(
            !fx.app.commit_file_diff_load.loading,
            "commit_file_diff_load.loading must be cleared (no follow-up dispatcher)"
        );
    }

    #[test]
    fn set_graph_scope_clears_active_commit_graph_search() {
        // Search matches index into git_graph.rows; clearing rows
        // without resetting search would leave `n`/`N` jumping to
        // rows that no longer correspond to anything visible.
        let mut fx = make_scope_fixture();
        // Synthesize an active CommitGraph search via `set_matches` so the
        // `row_index` invariant holds (set_matches resets `current`, so
        // assign it on the next line).
        fx.app.search = crate::search::SearchState {
            target: Some(crate::search::SearchTarget::CommitGraph),
            query: "foo".into(),
            ..crate::search::SearchState::default()
        };
        fx.app.search.set_matches(vec![crate::search::MatchLoc {
            row: 0,
            byte_range: 0..3,
        }]);
        fx.app.search.current = Some(0);

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert!(
            fx.app.search.matches.is_empty(),
            "CommitGraph search must be cleared on scope swap"
        );
        assert!(fx.app.search.target.is_none());
    }

    #[test]
    fn set_graph_scope_preserves_search_targeting_other_panel() {
        // Searches on file preview, commit detail body, etc. don't
        // index into git_graph.rows and should survive a scope swap.
        let mut fx = make_scope_fixture();
        fx.app.search = crate::search::SearchState {
            target: Some(crate::search::SearchTarget::FilePreview),
            query: "foo".into(),
            ..crate::search::SearchState::default()
        };
        fx.app.search.set_matches(vec![crate::search::MatchLoc {
            row: 5,
            byte_range: 0..3,
        }]);
        fx.app.search.current = Some(0);

        fx.app
            .set_graph_scope(GraphScope::Branch("refs/heads/main".into()));

        assert_eq!(
            fx.app.search.target,
            Some(crate::search::SearchTarget::FilePreview),
            "unrelated search target must not be cleared"
        );
        assert_eq!(fx.app.search.matches.len(), 1);
    }

    #[test]
    fn confirm_graph_branch_picker_closes_overlay_when_filter_matches_nothing() {
        // Filter "zzz" matches no branches and is not a substring of
        // "all refs"; visible_rows is empty → confirm() returns None.
        // The handler must still close the overlay so input isn't
        // trapped.
        let mut fx = make_scope_fixture();
        // Pretend we already have a populated ref_map so open_graph_branch_picker
        // succeeds.
        fx.app.git_graph.ref_map.insert(
            "oid".into(),
            vec![reef_core::git::RefLabel::Branch("main".into())],
        );
        fx.app.open_graph_branch_picker();
        assert!(fx.app.graph_branch_picker.core.active);

        fx.app.graph_branch_picker.core.filter = "zzz".into();
        assert!(
            fx.app.graph_branch_picker.confirm().is_none(),
            "filter 'zzz' has no rows"
        );

        fx.app.confirm_graph_branch_picker();
        assert!(
            !fx.app.graph_branch_picker.core.active,
            "picker must close even when confirm() returned None"
        );
    }

    #[test]
    fn payload_with_matching_scope_and_nonempty_rows_does_not_fall_back() {
        // Sanity: a Branch-scoped payload that returned rows should be
        // applied normally (no fallback, no toast).
        let mut fx = make_scope_fixture();
        fx.app.git_graph.scope = GraphScope::Branch("refs/heads/feature".into());

        let generation = fx.app.graph_load.begin();
        let payload = GraphPayload {
            rows: Vec::new(), // emptiness is fine — the OK case is "scope didn't disappear"
            ref_map: std::collections::HashMap::new(),
            cache_key: ("h".into(), 0, 1),
            // Scope mismatched against current: that's the "stale request" path,
            // not the "ghost branch" path. The guard requires scope match.
            scope: GraphScope::Branch("refs/heads/other".into()),
        };
        let toasts_before = fx.app.toasts.len();
        fx.app.apply_worker_result(WorkerResult::Graph {
            generation,
            result: Ok(payload),
        });

        // Scope should NOT have flipped — the payload was for a different
        // branch than the user is currently looking at.
        assert!(matches!(
            fx.app.git_graph.scope,
            GraphScope::Branch(ref s) if s == "refs/heads/feature"
        ));
        assert_eq!(fx.app.toasts.len(), toasts_before);
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

    #[test]
    fn stale_preview_refresh_keeps_pending_debounce_deadline() {
        let _home_lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().expect("home tempdir");
        let _home = HomeGuard::enter(home.path());
        let (tmp, repo) = tempdir_repo();
        commit_file(&repo, "a.txt", "hello\n", "add a");
        let backend = Arc::new(LocalBackend::open_at(tmp.path().to_path_buf()));
        let mut app = App::new_with_backend(Theme::dark(), backend, None);
        app.fs_watcher_rx = None;

        let file_idx = app
            .file_tree
            .entries
            .iter()
            .position(|entry| entry.path == std::path::Path::new("a.txt"))
            .expect("a.txt entry");
        app.file_tree.selected = file_idx;
        app.load_preview();

        let (scheduled_path, scheduled_deadline) =
            app.preview_schedule.clone().expect("preview scheduled");
        app.preview_load.mark_stale();
        app.kick_active_tab_work();

        assert_eq!(
            app.preview_schedule,
            Some((scheduled_path, scheduled_deadline))
        );
        assert!(app.preview_load.stale);
        assert!(!app.preview_load.loading);
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

    // ── ConfirmModal lifecycle ───────────────────────────────────────────

    use crate::ui::confirm_modal::{ConfirmModal, ModalTone};
    use std::cell::Cell;
    use std::rc::Rc;

    /// Build a barebones `ConfirmModal` whose two callbacks each tick a
    /// shared counter so the test can assert which one fired (and how
    /// many times). `confirm_keys` is empty — the tests drive the modal
    /// through the public `App::fire_*` entry points directly, not via
    /// keyboard.
    fn modal_with_counters(primary: Rc<Cell<u32>>, cancel: Rc<Cell<u32>>) -> ConfirmModal {
        ConfirmModal {
            title: "t".into(),
            tone: ModalTone::Danger,
            body: "b".into(),
            primary_label: "P".into(),
            cancel_label: "C".into(),
            confirm_keys: vec![],
            on_confirm: Box::new(move |_app| primary.set(primary.get() + 1)),
            on_cancel: Box::new(move |_app| cancel.set(cancel.get() + 1)),
        }
    }

    /// Holds the HOME lock + tempdirs alongside the App so each test
    /// gets a clean isolated environment. Drop order matters: the App
    /// (and its tasks worker) gets cleaned up before the tempdirs.
    struct ConfirmFixture {
        app: App,
        // Listed after `app` so they outlive it (Rust drops fields top-
        // down, so these fields drop *after* `app` only because they
        // appear after it in the struct definition — fine in practice).
        _home_guard: test_support::HomeGuard,
        _home: tempfile::TempDir,
        _repo: tempfile::TempDir,
        _home_lock: std::sync::MutexGuard<'static, ()>,
    }

    fn make_fixture() -> ConfirmFixture {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().expect("home tempdir");
        let home_guard = HomeGuard::enter(home.path());
        let (repo, _) = tempdir_repo();
        let backend = Arc::new(LocalBackend::open_at(repo.path().to_path_buf()));
        let mut app = App::new_with_backend(Theme::dark(), backend, None);
        app.fs_watcher_rx = None;
        ConfirmFixture {
            app,
            _home_guard: home_guard,
            _home: home,
            _repo: repo,
            _home_lock: lock,
        }
    }

    #[test]
    fn fire_confirm_primary_runs_closure_and_clears_modal() {
        let primary = Rc::new(Cell::new(0));
        let cancel = Rc::new(Cell::new(0));
        let mut fx = make_fixture();

        fx.app
            .show_confirm(modal_with_counters(primary.clone(), cancel.clone()));
        assert!(fx.app.confirm_modal.is_some(), "modal opens");

        fx.app.fire_confirm_primary();
        assert_eq!(primary.get(), 1, "on_confirm fired exactly once");
        assert_eq!(cancel.get(), 0, "on_cancel must not fire");
        assert!(
            fx.app.confirm_modal.is_none(),
            "modal cleared after primary"
        );
    }

    #[test]
    fn fire_confirm_cancel_runs_closure_and_clears_modal() {
        let primary = Rc::new(Cell::new(0));
        let cancel = Rc::new(Cell::new(0));
        let mut fx = make_fixture();

        fx.app
            .show_confirm(modal_with_counters(primary.clone(), cancel.clone()));
        fx.app.fire_confirm_cancel();

        assert_eq!(primary.get(), 0);
        assert_eq!(cancel.get(), 1);
        assert!(fx.app.confirm_modal.is_none());
    }

    #[test]
    fn dismiss_confirm_skips_callbacks() {
        let primary = Rc::new(Cell::new(0));
        let cancel = Rc::new(Cell::new(0));
        let mut fx = make_fixture();

        fx.app
            .show_confirm(modal_with_counters(primary.clone(), cancel.clone()));
        fx.app.dismiss_confirm();

        // dismiss is the "force-close, drop callbacks" path — used by
        // competing modal opens, tab switches, mutation completion.
        assert_eq!(primary.get(), 0);
        assert_eq!(cancel.get(), 0);
        assert!(fx.app.confirm_modal.is_none());
    }

    #[test]
    fn show_confirm_over_existing_fires_cancel_on_old_modal() {
        let primary_a = Rc::new(Cell::new(0));
        let cancel_a = Rc::new(Cell::new(0));
        let primary_b = Rc::new(Cell::new(0));
        let cancel_b = Rc::new(Cell::new(0));
        let mut fx = make_fixture();

        fx.app
            .show_confirm(modal_with_counters(primary_a.clone(), cancel_a.clone()));
        // B opens while A is still up — A's on_cancel should fire so its
        // caller can clean up; A's on_confirm must NOT fire.
        fx.app
            .show_confirm(modal_with_counters(primary_b.clone(), cancel_b.clone()));

        assert_eq!(primary_a.get(), 0);
        assert_eq!(cancel_a.get(), 1, "A's cancel fires on replace");
        assert_eq!(primary_b.get(), 0);
        assert_eq!(cancel_b.get(), 0);
        assert!(fx.app.confirm_modal.is_some(), "B is now the active modal");

        // Drive B to completion to verify it's independent.
        fx.app.fire_confirm_primary();
        assert_eq!(primary_b.get(), 1);
        assert_eq!(cancel_b.get(), 0);
        assert!(fx.app.confirm_modal.is_none());
    }

    #[test]
    fn callback_can_reopen_modal_without_recursion_or_leak() {
        // A closure that calls `show_confirm` again is the documented
        // "keep modal open" pattern (mirrors `execute_tree_delete`
        // backing off on `fs_mutation_load.loading`). Verify it doesn't
        // stack-overflow and that the second modal is the one left
        // standing.
        let counter = Rc::new(Cell::new(0));
        let mut fx = make_fixture();

        let counter_inner = counter.clone();
        fx.app.show_confirm(ConfirmModal {
            title: "t".into(),
            tone: ModalTone::Danger,
            body: "b".into(),
            primary_label: "P".into(),
            cancel_label: "C".into(),
            confirm_keys: vec![],
            on_confirm: Box::new(move |app| {
                counter_inner.set(counter_inner.get() + 1);
                // Re-open with a fresh, no-op modal.
                app.show_confirm(ConfirmModal {
                    title: "t2".into(),
                    tone: ModalTone::Danger,
                    body: "b2".into(),
                    primary_label: "P".into(),
                    cancel_label: "C".into(),
                    confirm_keys: vec![],
                    on_confirm: Box::new(|_| {}),
                    on_cancel: Box::new(|_| {}),
                });
            }),
            on_cancel: Box::new(|_| {}),
        });

        fx.app.fire_confirm_primary();
        assert_eq!(counter.get(), 1, "first closure ran once");
        assert!(
            fx.app.confirm_modal.is_some(),
            "second modal is still up after the closure re-opened"
        );
    }
}
