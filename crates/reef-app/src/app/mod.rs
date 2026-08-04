use reef_core::diff::{DiffContent, DiffDisplay, DiffLayout};
use reef_core::git::graph::GraphRow;
use reef_core::git::{CommitDetail, CommitInfo, FileEntry, GraphScope, RefLabel};
use reef_core::preview::PreviewDocument as PreviewContent;
use reef_io::Backend;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod db;
mod files;
mod git;
mod global_search;
mod nav;
mod pickers;
mod preview;
mod quick_open;
mod runtime;
mod scroll;
mod search;
mod view;

use crate::features::file_tree::FileTree;
use crate::features::{
    confirm::{ConfirmRequest, TreeDeleteConfirm},
    db_preview::{DbNav, DbPreviewState, max_page_for_object},
    file_clipboard::FileClipboard,
    file_selection::SelectionSet,
    find_widget::FindWidgetState,
    global_search::GlobalSearchState,
    graph_branch_picker::GraphBranchPickerState,
    hosts_picker::HostsPickerState,
    place_mode::PlaceModeState,
    quick_open::QuickOpenState,
    search::SearchState,
    settings::SettingsState,
    tree_context_menu::ContextMenuState,
    tree_drag::TreeDragState,
    tree_edit::TreeEditState,
};
use crate::tasks::{
    DbCellRequest, DbPageRequest, GitMutation, GitMutationPayload, GitRevertPath, GraphPayload,
    PasteItem, PastePlanError, PastePlanPayload, TaskCoordinator, TreeEditMutation, TreeEditPlan,
    TreeEditPlanError, WorkerResult,
};
use crate::{
    AppRuntimeEvent, AppTab, AsyncState, FileActionNotice, FsMutationKind, LocationSnapshot,
    LocationSurface, Toast,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Main,
    Settings,
    FocusedPreview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppPanel {
    Files,
    Commit,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Compact,
    FullFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StructuredPreviewMode {
    #[default]
    Tree,
    Raw,
}

impl StructuredPreviewMode {
    pub fn pref_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Raw => "raw",
        }
    }

    pub fn from_pref_str(value: &str) -> Self {
        match value {
            "raw" => Self::Raw,
            _ => Self::Tree,
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardTarget {
    File { is_staged: bool, path: String },
    Folder { is_staged: bool, path: String },
    Section { is_staged: bool },
}

#[derive(Debug, Default)]
pub struct GitStatusState {
    pub tree_mode: bool,
    pub collapsed_dirs: HashSet<String>,
    pub(crate) staged_tree_rows: Vec<reef_core::git::tree::TreeRow>,
    pub(crate) unstaged_tree_rows: Vec<reef_core::git::tree::TreeRow>,
    pub confirm_discard: Option<DiscardTarget>,
    pub confirm_push: bool,
    pub confirm_force_push: bool,
    pub push_error: Option<PushError>,
    pub scroll: usize,
    pub ahead_behind: Option<(usize, usize)>,
    pub commit_message: String,
    pub commit_cursor: usize,
    pub commit_editing: bool,
    pub commit_error: Option<CommitError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitError {
    NothingStaged,
    Failed(String),
}

pub const GRAPH_RECENT_BRANCHES_MAX: usize = 5;

#[derive(Debug, Default)]
pub struct GitGraphState {
    pub rows: Vec<GraphRow>,
    pub ref_map: HashMap<String, Vec<RefLabel>>,
    pub cache_key: Option<(String, u64, u64)>,
    pub scope: GraphScope,
    pub recent_branches: Vec<String>,
    pub selected_idx: usize,
    pub selected_commit: Option<String>,
    pub selection_anchor: Option<usize>,
    pub scroll: usize,
}

impl GitGraphState {
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

    pub fn in_visual_mode(&self) -> bool {
        self.selection_anchor.is_some()
    }

    pub fn find_row_by_oid(&self, oid: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.commit.oid == oid)
    }
}

pub type LineTokens = Arc<Vec<reef_core::text::StyledToken>>;
pub type DiffHighlighted = reef_core::diff::DiffHighlighted<LineTokens>;

#[derive(Debug, Clone)]
pub struct HighlightedDiff {
    pub diff: DiffContent,
    pub highlighted: Option<Arc<DiffHighlighted>>,
    pub display: Arc<DiffDisplay<LineTokens>>,
}

impl HighlightedDiff {
    pub fn new(diff: DiffContent, highlighted: Option<Arc<DiffHighlighted>>) -> Self {
        let display = Arc::new(DiffDisplay::build(&diff, highlighted.as_deref()));
        Self {
            diff,
            highlighted,
            display,
        }
    }
}

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

#[derive(Debug, Clone)]
pub struct RangeDetail {
    pub oldest_oid: String,
    pub newest_oid: String,
    pub commit_count: usize,
    pub commits: Vec<CommitInfo>,
    pub files: Vec<FileEntry>,
}

#[derive(Debug)]
pub struct CommitDetailState {
    pub detail: Option<CommitDetail>,
    pub range_detail: Option<RangeDetail>,
    pub file_diff: Option<CommitFileDiff>,
    pub diff_layout: DiffLayout,
    pub diff_mode: DiffMode,
    pub files_tree_mode: bool,
    pub files_collapsed: HashSet<String>,
    pub scroll: usize,
    pub file_diff_scroll: usize,
    pub diff_h_scroll: usize,
    pub sbs_left_h_scroll: usize,
    pub sbs_right_h_scroll: usize,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedFile {
    pub path: String,
    pub is_staged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct PreviewHighlight {
    pub path: PathBuf,
    pub row: usize,
    pub byte_range: Range<usize>,
    pub fade: HighlightFade,
    pub pending_utf16: Option<Range<u32>>,
}

#[derive(Debug, Clone, Copy)]
pub enum HighlightFade {
    Persistent,
    Pending { armed_at: Instant },
    Counting { since: Instant },
}

#[derive(Debug, Clone, Copy)]
pub enum NavAnchor {
    Keyboard,
    Mouse { col: u16, row: u16 },
}

#[derive(Debug, Clone)]
pub struct NavPendingJump {
    pub lang: reef_core::nav::NavLang,
    pub cache_key: String,
    pub origin: crate::LocationSnapshot,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct LspRefineOutcome {
    pub pending_jump: NavPendingJump,
    pub location: reef_core::nav::LspLocation,
}

#[derive(Debug, Clone)]
pub struct NavCandidatesPopup {
    pub anchor_col: u16,
    pub anchor_row: u16,
    pub candidates: Vec<reef_core::nav::Location>,
    pub selected: usize,
    pub scroll: usize,
    pub current_path: PathBuf,
    pub origin: crate::LocationSnapshot,
    pub opened_by_ctrl_click: bool,
    pub max_row_width: u16,
}

impl NavCandidatesPopup {
    pub const MAX_VISIBLE_ROWS: usize = 8;

    pub fn visible_rows(&self) -> usize {
        self.candidates.len().min(Self::MAX_VISIBLE_ROWS)
    }

    pub fn clamp_scroll(&mut self) {
        let visible = self.visible_rows();
        if visible == 0 {
            self.scroll = 0;
            return;
        }
        let max_scroll = self.candidates.len().saturating_sub(visible);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        self.scroll = self.scroll.min(max_scroll);
    }
}

pub fn compute_uses_three_col(
    active_tab: AppTab,
    total_width: u16,
    has_file_diff: bool,
    load_in_flight: bool,
) -> bool {
    active_tab == AppTab::Graph
        && total_width >= GRAPH_THREE_COL_MIN_WIDTH
        && (has_file_diff || load_in_flight)
}

pub fn compute_sidebar_width(total_width: u16, split_percent: u16) -> u16 {
    let raw = (total_width as u32 * split_percent as u32 / 100) as u16;
    raw.max(10).min(total_width.saturating_sub(20))
}

pub fn compute_three_col_widths(
    total_width: u16,
    sidebar_w: u16,
    graph_diff_split_percent: u16,
) -> (u16, u16, u16) {
    let remainder = total_width.saturating_sub(sidebar_w);
    let diff_w_raw = (remainder as u32 * graph_diff_split_percent as u32 / 100) as u16;
    let diff_w = diff_w_raw.max(20).min(remainder.saturating_sub(20));
    let commit_w = remainder.saturating_sub(diff_w);
    (sidebar_w, commit_w, diff_w)
}

fn sbs_cursor_on_left(panel_start: u16, panel_w: u16, column: u16) -> bool {
    let panel_mid = panel_start.saturating_add(panel_w / 2);
    column < panel_mid
}

pub const GLOBAL_SEARCH_MAX_RESULTS: usize = 1000;
pub const GLOBAL_SEARCH_MAX_LINE_CHARS: usize = 250;
pub const GLOBAL_SEARCH_MAX_H_SCROLL: usize = GLOBAL_SEARCH_MAX_LINE_CHARS;
pub const GLOBAL_SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);
pub const GLOBAL_SEARCH_PREVIEW_SYNC_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_millis(100);
pub const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(80);
pub const PREFETCH_DELAY: Duration = Duration::from_millis(300);
pub const GRAPH_THREE_COL_MIN_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPanelFocus {
    FindInput,
    ReplaceInput,
    List,
}

#[derive(Debug, Clone)]
pub struct MatchHit {
    pub path: PathBuf,
    pub display: String,
    pub line: usize,
    pub line_text: String,
    pub line_revision: u64,
    pub byte_range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPreviewEnrichment {
    generation: u64,
    path: String,
}

#[derive(Debug, Clone)]
struct StructuredPreviewState {
    path: String,
    document: Arc<reef_core::structured_data::StructuredDataDocument>,
}

#[derive(Debug, Clone)]
struct PendingGlobalSearchAccept {
    hit: MatchHit,
    origin: Option<LocationSnapshot>,
    generation: Option<u64>,
}

pub struct AppState {
    pub backend: Arc<dyn Backend>,
    pub workdir_name: String,
    pub branch_name: String,

    pub active_tab: AppTab,
    pub active_panel: AppPanel,
    pub view_mode: ViewMode,

    pub focused_preview_files_open: bool,
    pub focused_preview_files_selected: usize,

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
    pub sbs_left_h_scroll: usize,
    pub sbs_right_h_scroll: usize,

    pub file_tree: FileTree,
    pub preview_content: Option<Arc<PreviewContent>>,
    pub preview_content_revision: u64,
    pub preview_source_revision: u64,
    pub preview_snapshot: Option<Arc<crate::PreviewDocumentSnapshot>>,
    pub preview_enrichment_dark: bool,
    preview_enrichment_pending: Option<PendingPreviewEnrichment>,
    structured_preview: Option<StructuredPreviewState>,
    pub structured_preview_mode: StructuredPreviewMode,
    pub preview_schedule: Option<(PathBuf, Instant)>,
    pub prefetch_schedule: Option<Instant>,
    pub preview_in_flight_path: Option<PathBuf>,
    pub tree_scroll: usize,
    pub preview_scroll: usize,
    pub preview_h_scroll: usize,
    pub db_goto_input: Option<String>,
    pub db_goto_cursor: usize,
    pub db_preview: Option<DbPreviewState>,

    pub split_percent: u16,
    pub sidebar_visible: bool,
    pub sidebar_hide_hint_shown: bool,
    pub graph_diff_split_percent: u16,

    pub git_status: GitStatusState,
    pub git_graph: GitGraphState,
    pub commit_detail: CommitDetailState,
    pub toasts: Vec<Toast>,

    pub fs_watcher_rx: Option<crossbeam_channel::Receiver<reef_io::FsChange>>,

    pub show_help: bool,
    pub pending_edit: Option<PathBuf>,
    pub settings: SettingsState,

    pub quick_open: QuickOpenState,
    pub global_search: GlobalSearchState,
    pending_global_search_accept: Option<PendingGlobalSearchAccept>,
    pub search: SearchState,
    pub find_widget: FindWidgetState,
    pub hosts_picker: HostsPickerState,
    pub graph_branch_picker: GraphBranchPickerState,
    pub pending_ssh_target: Option<crate::features::hosts_picker::SshTarget>,

    pub preview_highlight: Option<PreviewHighlight>,
    pub location_history: reef_core::history::History<LocationSnapshot>,
    pub nav_candidates: Option<NavCandidatesPopup>,
    pub ctrl_hover_target: Option<(usize, Range<usize>)>,
    pub nav_workspace: Option<Arc<reef_core::nav::WorkspaceIndex>>,
    pub nav_workspace_load: AsyncState,
    pub lsp_states: HashMap<reef_core::nav::NavLang, reef_core::nav::LspBadge>,
    pub lsp_installed: HashMap<reef_core::nav::NavLang, bool>,
    pub nav_refine_cache: HashMap<(reef_core::nav::NavLang, String), reef_core::nav::LspLocation>,
    pub nav_refine_gen: u64,
    pub nav_refine_epoch: u64,
    pub nav_pending_lsp_jump: Option<NavPendingJump>,

    pub place_mode: PlaceModeState,
    pub tree_edit: TreeEditState,
    pub tree_context_menu: ContextMenuState,
    pub file_clipboard: FileClipboard,
    pub file_selection: SelectionSet,
    pub tree_drag: TreeDragState,
    pub paste_conflict: Option<reef_core::file_ops::PasteConflictPrompt>,
    pub pending_confirm: Option<ConfirmRequest>,

    pub tasks: TaskCoordinator,
    pub file_tree_load: AsyncState,
    file_tree_revision: u64,
    next_file_tree_subtree_request_id: u64,
    file_tree_subtree_requests: HashMap<PathBuf, u64>,
    pub preview_load: AsyncState,
    pub db_page_load: AsyncState,
    pub db_detail_load: AsyncState,
    pub db_cell_load: AsyncState,
    pub db_cell_cancellation: Option<reef_io::CancellationToken>,
    pub git_status_load: AsyncState,
    pub git_status_stats_load: AsyncState,
    pub git_mutation_load: AsyncState,
    pub commit_load: AsyncState,
    pub push_load: AsyncState,
    pub diff_load: AsyncState,
    pub graph_load: AsyncState,
    pub commit_detail_load: AsyncState,
    pub commit_file_diff_load: AsyncState,
    pub global_search_load: AsyncState,
    pub quick_open_load: AsyncState,
    pub file_copy_load: AsyncState,
    pub fs_mutation_load: AsyncState,
    pub fs_mutation_select_on_done: Option<PathBuf>,
    pub replace_load: AsyncState,
    pub next_graph_revalidate_at: Instant,
}

pub struct AppPrefs {
    pub diff_layout: DiffLayout,
    pub diff_mode: DiffMode,
    pub status_tree_mode: bool,
    pub graph_scope: GraphScope,
    pub graph_recent_branches: Vec<String>,
    pub commit_diff_layout: DiffLayout,
    pub commit_diff_mode: DiffMode,
    pub commit_files_tree_mode: bool,
    pub structured_preview_mode: StructuredPreviewMode,
    pub quick_open: QuickOpenState,
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self {
            diff_layout: DiffLayout::Unified,
            diff_mode: DiffMode::Compact,
            status_tree_mode: false,
            graph_scope: GraphScope::AllRefs,
            graph_recent_branches: Vec::new(),
            commit_diff_layout: DiffLayout::Unified,
            commit_diff_mode: DiffMode::Compact,
            commit_files_tree_mode: false,
            structured_preview_mode: StructuredPreviewMode::Tree,
            quick_open: QuickOpenState::default(),
        }
    }
}

pub struct AppStateConfig {
    pub backend: Arc<dyn Backend>,
    pub prefs: AppPrefs,
    pub now: Instant,
    pub subscribe_fs_events: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickOptions {
    pub dark: bool,
    pub wants_decoded_image: bool,
    pub uses_three_col: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NormalizeActivePanelOutcome {
    pub clear_diff_selection: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToggleSidebarOutcome {
    pub hidden: bool,
    pub cancel_split_drags: bool,
    pub show_hidden_hint: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphScopeChangeOutcome {
    pub changed: bool,
    pub clear_commit_graph_search: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphBranchPickerOpenOutcome {
    pub opened: bool,
    pub stale_branch_short_ref: Option<String>,
    pub scope_change: GraphScopeChangeOutcome,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitFileDiffLoadOutcome {
    pub clear_commit_detail_selection: bool,
    pub clear_diff_selection: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TabChangeOutcome {
    pub changed: bool,
    pub clear_preview_selection: bool,
    pub clear_commit_detail_selection: bool,
    pub clear_diff_selection: bool,
    pub close_find_widget: bool,
    pub dismiss_confirm: bool,
    pub sync_search_preview: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewMergeOutcome {
    pub accepted: bool,
    pub same_file: bool,
    pub clear_preview_selection: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JumpToLocationOutcome {
    pub restore_preview_cursor: Option<LocationSnapshot>,
    pub clear_commit_detail_selection: bool,
    pub clear_diff_selection: bool,
}

impl AppState {
    pub const GRAPH_THREE_COL_MIN_WIDTH: u16 = crate::app::GRAPH_THREE_COL_MIN_WIDTH;
    pub const NAV_HISTORY_CAP: usize = 128;

    pub fn new(config: AppStateConfig) -> Self {
        let AppStateConfig {
            backend,
            prefs,
            now,
            subscribe_fs_events,
        } = config;
        let workdir = backend.workdir_path();
        let workdir_name = backend.workdir_name();
        let branch_name = backend.branch_name();
        let file_tree = FileTree::new(&workdir);
        let fs_watcher_rx = subscribe_fs_events.then(|| backend.subscribe_fs_events());
        Self {
            backend,
            workdir_name,
            branch_name,
            active_tab: AppTab::Files,
            active_panel: AppPanel::Files,
            view_mode: ViewMode::Main,
            focused_preview_files_open: false,
            focused_preview_files_selected: 0,
            staged_files: Vec::new(),
            unstaged_files: Vec::new(),
            selected_file: None,
            diff_content: None,
            diff_layout: prefs.diff_layout,
            diff_mode: prefs.diff_mode,
            staged_collapsed: false,
            unstaged_collapsed: false,
            file_scroll: 0,
            diff_scroll: 0,
            diff_h_scroll: 0,
            sbs_left_h_scroll: 0,
            sbs_right_h_scroll: 0,
            file_tree,
            preview_content: None,
            preview_content_revision: 0,
            preview_source_revision: 0,
            preview_snapshot: None,
            preview_enrichment_dark: false,
            preview_enrichment_pending: None,
            structured_preview: None,
            structured_preview_mode: prefs.structured_preview_mode,
            preview_schedule: None,
            prefetch_schedule: None,
            preview_in_flight_path: None,
            tree_scroll: 0,
            preview_scroll: 0,
            preview_h_scroll: 0,
            db_goto_input: None,
            db_goto_cursor: 0,
            db_preview: None,
            split_percent: 30,
            sidebar_visible: true,
            sidebar_hide_hint_shown: false,
            graph_diff_split_percent: 60,
            git_status: GitStatusState {
                tree_mode: prefs.status_tree_mode,
                ..GitStatusState::default()
            },
            git_graph: GitGraphState {
                scope: prefs.graph_scope,
                recent_branches: prefs.graph_recent_branches,
                ..GitGraphState::default()
            },
            commit_detail: CommitDetailState {
                diff_layout: prefs.commit_diff_layout,
                diff_mode: prefs.commit_diff_mode,
                files_tree_mode: prefs.commit_files_tree_mode,
                ..CommitDetailState::default()
            },
            toasts: Vec::new(),
            fs_watcher_rx,
            show_help: false,
            pending_edit: None,
            settings: SettingsState::default(),
            quick_open: prefs.quick_open,
            global_search: GlobalSearchState::default(),
            pending_global_search_accept: None,
            search: SearchState::default(),
            find_widget: FindWidgetState::default(),
            hosts_picker: HostsPickerState::default(),
            graph_branch_picker: GraphBranchPickerState::default(),
            pending_ssh_target: None,
            preview_highlight: None,
            location_history: reef_core::history::History::new(Self::NAV_HISTORY_CAP),
            nav_candidates: None,
            ctrl_hover_target: None,
            nav_workspace: None,
            nav_workspace_load: AsyncState::default(),
            lsp_states: HashMap::new(),
            lsp_installed: HashMap::new(),
            nav_refine_cache: HashMap::new(),
            nav_refine_gen: 0,
            nav_refine_epoch: 0,
            nav_pending_lsp_jump: None,
            place_mode: PlaceModeState::default(),
            tree_edit: TreeEditState::default(),
            tree_context_menu: ContextMenuState::default(),
            file_clipboard: FileClipboard::default(),
            file_selection: SelectionSet::default(),
            tree_drag: TreeDragState::default(),
            paste_conflict: None,
            pending_confirm: None,
            tasks: TaskCoordinator::new(),
            file_tree_load: AsyncState::default(),
            file_tree_revision: 0,
            next_file_tree_subtree_request_id: 0,
            file_tree_subtree_requests: HashMap::new(),
            preview_load: AsyncState::default(),
            db_page_load: AsyncState::default(),
            db_detail_load: AsyncState::default(),
            db_cell_load: AsyncState::default(),
            db_cell_cancellation: None,
            git_status_load: AsyncState::default(),
            git_status_stats_load: AsyncState::default(),
            git_mutation_load: AsyncState::default(),
            commit_load: AsyncState::default(),
            push_load: AsyncState::default(),
            diff_load: AsyncState::default(),
            graph_load: AsyncState::default(),
            commit_detail_load: AsyncState::default(),
            commit_file_diff_load: AsyncState::default(),
            global_search_load: AsyncState::default(),
            quick_open_load: AsyncState::default(),
            file_copy_load: AsyncState::default(),
            fs_mutation_load: AsyncState::default(),
            fs_mutation_select_on_done: None,
            replace_load: AsyncState::default(),
            next_graph_revalidate_at: now + Duration::from_millis(1200),
        }
    }
}

fn payload_scope_ref_missing(payload: &GraphPayload) -> bool {
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

fn shorthand_for_full_ref(full_ref: &str) -> &str {
    full_ref
        .strip_prefix("refs/heads/")
        .or_else(|| full_ref.strip_prefix("refs/remotes/"))
        .unwrap_or(full_ref)
}

fn folder_contains(folder_path: &str, file_path: &str) -> bool {
    if folder_path.is_empty() {
        return false;
    }
    let folder_path = folder_path.trim_end_matches('/');
    file_path == folder_path
        || file_path
            .strip_prefix(folder_path)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn navigable_git_files(
    staged_files: &[FileEntry],
    unstaged_files: &[FileEntry],
    staged_tree_rows: &[reef_core::git::tree::TreeRow],
    unstaged_tree_rows: &[reef_core::git::tree::TreeRow],
    staged_collapsed: bool,
    unstaged_collapsed: bool,
    tree_mode: bool,
) -> Vec<(String, bool)> {
    let mut items = Vec::new();
    if !staged_files.is_empty() && !staged_collapsed {
        if tree_mode {
            for row in staged_tree_rows {
                if let reef_core::git::tree::TreeRow::File { source_index, .. } = row
                    && let Some(file) = staged_files.get(*source_index)
                {
                    items.push((file.path.clone(), true));
                }
            }
        } else {
            for file in staged_files {
                items.push((file.path.clone(), true));
            }
        }
    }
    if !unstaged_collapsed {
        if tree_mode {
            for row in unstaged_tree_rows {
                if let reef_core::git::tree::TreeRow::File { source_index, .. } = row
                    && let Some(file) = unstaged_files.get(*source_index)
                {
                    items.push((file.path.clone(), false));
                }
            }
        } else {
            for file in unstaged_files {
                items.push((file.path.clone(), false));
            }
        }
    }
    items
}

fn next_panel(current: AppPanel, three_col: bool, reverse: bool) -> AppPanel {
    if three_col {
        match (current, reverse) {
            (AppPanel::Files, false) => AppPanel::Commit,
            (AppPanel::Commit, false) => AppPanel::Diff,
            (AppPanel::Diff, false) => AppPanel::Files,
            (AppPanel::Files, true) => AppPanel::Diff,
            (AppPanel::Commit, true) => AppPanel::Files,
            (AppPanel::Diff, true) => AppPanel::Commit,
        }
    } else {
        match current {
            AppPanel::Files | AppPanel::Commit => AppPanel::Diff,
            AppPanel::Diff => AppPanel::Files,
        }
    }
}

fn apply_scroll_delta(value: &mut usize, delta: i32) {
    if delta < 0 {
        *value = value.saturating_sub((-delta) as usize);
    } else {
        *value = value.saturating_add(delta as usize);
    }
}

pub fn center_scroll(row: usize, view_h: usize) -> usize {
    if view_h <= 1 {
        return row;
    }
    row.saturating_sub(view_h / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_core::git::{FileEntry, FileStatus};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn git_entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
        }
    }

    fn tree_entry(path: &str, depth: usize, is_dir: bool) -> crate::TreeEntry {
        crate::TreeEntry {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            depth,
            is_dir,
            has_children: is_dir,
            is_expanded: false,
            git_status: None,
        }
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
        assert!(!folder_contains("src/ui", "src/ui-helper.rs"));
    }

    #[test]
    fn folder_contains_exact_path_match() {
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
        assert!(!folder_contains("", "anything.rs"));
    }

    #[test]
    fn discard_file_target_preserves_staged_flag() {
        let app = minimal_app_state();

        let paths = app.discard_paths_for_target(&DiscardTarget::File {
            is_staged: true,
            path: "src/main.rs".to_string(),
        });

        assert_eq!(
            paths,
            vec![GitRevertPath {
                path: "src/main.rs".to_string(),
                is_staged: true,
            }]
        );
    }

    #[test]
    fn discard_folder_target_marks_all_paths_with_source_stage() {
        let app = AppState {
            staged_files: vec![git_entry("src/a.rs"), git_entry("src/nested/b.rs")],
            unstaged_files: vec![git_entry("src/c.rs"), git_entry("README.md")],
            ..minimal_app_state()
        };

        let paths = app.discard_paths_for_target(&DiscardTarget::Folder {
            is_staged: true,
            path: "src".to_string(),
        });

        assert_eq!(
            paths,
            vec![
                GitRevertPath {
                    path: "src/a.rs".to_string(),
                    is_staged: true,
                },
                GitRevertPath {
                    path: "src/nested/b.rs".to_string(),
                    is_staged: true,
                },
            ]
        );
    }

    #[test]
    fn navigable_git_files_tree_mode_follows_visible_tree_order() {
        let unstaged = vec![
            git_entry("z.txt"),
            git_entry("src/z.rs"),
            git_entry("README.md"),
            git_entry("src/a.rs"),
            git_entry("assets/logo.png"),
        ];
        let rows = reef_core::git::tree::visible_rows(&unstaged, false, &HashSet::new());

        assert_eq!(
            navigable_git_files(&[], &unstaged, &[], &rows, false, false, true),
            vec![
                ("assets/logo.png".to_string(), false),
                ("src/a.rs".to_string(), false),
                ("src/z.rs".to_string(), false),
                ("README.md".to_string(), false),
                ("z.txt".to_string(), false),
            ]
        );
    }

    #[test]
    fn navigable_git_files_tree_mode_skips_collapsed_dirs() {
        let unstaged = vec![
            git_entry("src/a.rs"),
            git_entry("README.md"),
            git_entry("src/z.rs"),
            git_entry("z.txt"),
        ];
        let collapsed = HashSet::from([reef_core::git::tree::collapsed_key(false, "src")]);
        let rows = reef_core::git::tree::visible_rows(&unstaged, false, &collapsed);

        assert_eq!(
            navigable_git_files(&[], &unstaged, &[], &rows, false, false, true),
            vec![
                ("README.md".to_string(), false),
                ("z.txt".to_string(), false)
            ]
        );
    }

    #[test]
    fn list_mode_does_not_request_git_tree_rebuilds() {
        let app = minimal_app_state();

        assert!(!app.git_status_tree_needs_rebuild(&[git_entry("src/a.rs")], &[]));
    }

    #[test]
    fn unchanged_git_paths_keep_cached_tree_rows() {
        let app = AppState {
            staged_files: vec![git_entry("src/a.rs")],
            git_status: GitStatusState {
                tree_mode: true,
                ..GitStatusState::default()
            },
            ..minimal_app_state()
        };
        let mut updated = git_entry("src/a.rs");
        updated.status = FileStatus::Added;
        updated.additions = 42;

        assert!(!app.git_status_tree_needs_rebuild(&[updated], &[]));
    }

    #[test]
    fn status_metadata_refresh_retains_visible_counts_until_stats_arrive() {
        let mut previous = git_entry("src/a.rs");
        previous.additions = 7;
        previous.deletions = 3;
        let mut next = vec![git_entry("src/a.rs"), git_entry("src/b.rs")];

        AppState::retain_cached_git_status_stats(&mut next, &[previous]);

        assert_eq!((next[0].additions, next[0].deletions), (7, 3));
        assert_eq!((next[1].additions, next[1].deletions), (0, 0));
    }

    #[test]
    fn completed_status_stats_replace_cached_counts() {
        let mut staged = git_entry("src/a.rs");
        staged.additions = 7;
        staged.deletions = 3;
        let mut app = AppState {
            staged_files: vec![staged],
            unstaged_files: vec![git_entry("src/b.rs")],
            ..minimal_app_state()
        };

        app.apply_git_status_stats(reef_core::git::GitStatusStats {
            staged: HashMap::from([("src/a.rs".to_string(), (2, 1))]),
            unstaged: HashMap::new(),
        });

        assert_eq!(
            (app.staged_files[0].additions, app.staged_files[0].deletions),
            (2, 1)
        );
        assert_eq!(
            (
                app.unstaged_files[0].additions,
                app.unstaged_files[0].deletions
            ),
            (0, 0)
        );
    }

    #[test]
    fn completed_status_metadata_defers_stats_when_git_tab_is_inactive() {
        let mut app = minimal_app_state();
        app.active_tab = AppTab::Files;
        let generation = app.git_status_load.begin();

        app.apply_worker_result_core(
            WorkerResult::GitStatus {
                generation,
                result: Ok(crate::tasks::GitStatusPayload {
                    staged: Vec::new(),
                    unstaged: vec![git_entry("src/a.rs")],
                    ahead_behind: None,
                    branch_name: "main".to_string(),
                }),
            },
            Instant::now(),
        );

        assert!(!app.git_status_stats_load.loading);
        assert!(app.git_status_stats_load.stale);
    }

    #[test]
    fn failed_status_stats_wait_for_a_new_invalidation_before_retrying() {
        let mut app = minimal_app_state();
        app.active_tab = AppTab::Git;
        let generation = app.git_status_stats_load.begin();

        app.apply_worker_result_core(
            WorkerResult::GitStatusStats {
                generation,
                result: Err("stats failed".to_string()),
            },
            Instant::now(),
        );

        assert!(!app.has_step_work_due(Instant::now()));

        app.git_status_stats_load.invalidate();
        app.git_status_stats_load.mark_stale();

        assert!(app.has_step_work_due(Instant::now()));
    }

    #[test]
    fn stale_git_mutation_success_does_not_change_current_selection() {
        let mut app = AppState {
            selected_file: Some(SelectedFile {
                path: "src/a.rs".to_string(),
                is_staged: false,
            }),
            ..minimal_app_state()
        };
        let generation = app.git_mutation_load.begin();
        app.git_mutation_load.invalidate();

        let events = app.apply_worker_result_core(
            WorkerResult::GitMutation {
                generation,
                result: Ok(GitMutationPayload {
                    mutation: GitMutation::Stage(vec!["src/a.rs".to_string()]),
                    touched: vec!["src/a.rs".to_string()],
                    errors: Vec::new(),
                }),
            },
            Instant::now(),
        );

        assert!(
            events.is_empty()
                && !app.git_status_load.loading
                && app
                    .selected_file
                    .as_ref()
                    .is_some_and(|selected| !selected.is_staged)
        );
    }

    #[test]
    fn stale_git_mutation_error_does_not_emit_side_effects() {
        let mut app = minimal_app_state();
        let generation = app.git_mutation_load.begin();
        app.git_mutation_load.invalidate();

        let events = app.apply_worker_result_core(
            WorkerResult::GitMutation {
                generation,
                result: Err("stale failure".to_string()),
            },
            Instant::now(),
        );

        assert!(events.is_empty() && app.toasts.is_empty() && !app.git_status_load.loading);
    }

    #[test]
    fn changed_git_paths_request_tree_rebuild() {
        let app = AppState {
            staged_files: vec![git_entry("src/a.rs")],
            git_status: GitStatusState {
                tree_mode: true,
                ..GitStatusState::default()
            },
            ..minimal_app_state()
        };

        assert!(app.git_status_tree_needs_rebuild(&[git_entry("src/b.rs")], &[]));
    }

    #[test]
    fn entering_git_tree_mode_builds_cached_rows() {
        let mut app = AppState {
            staged_files: vec![git_entry("src/a.rs")],
            ..minimal_app_state()
        };

        app.toggle_status_tree_mode();

        assert!(!app.git_status.staged_tree_rows.is_empty());
    }

    #[test]
    fn leaving_git_tree_mode_releases_cached_rows() {
        let mut app = AppState {
            staged_files: vec![git_entry("src/a.rs")],
            git_status: GitStatusState {
                tree_mode: true,
                ..GitStatusState::default()
            },
            ..minimal_app_state()
        };
        app.rebuild_git_status_tree_rows();

        app.toggle_status_tree_mode();

        assert!(app.git_status.staged_tree_rows.is_empty());
    }

    #[test]
    fn repo_presence_change_cancels_git_confirmations() {
        let mut app = minimal_app_state();
        app.git_status.confirm_discard = Some(DiscardTarget::File {
            is_staged: false,
            path: "src/a.rs".to_string(),
        });
        app.git_status.confirm_push = true;
        app.git_status.confirm_force_push = true;
        let (tx, rx) = crossbeam_channel::bounded(1);
        app.fs_watcher_rx = Some(rx);
        tx.send(reef_io::FsChange {
            workspace_changed: false,
            workspace_paths: Vec::new(),
            git_metadata_changed: false,
            repo_presence_changed: true,
        })
        .unwrap();

        app.drain_fs_watcher_events();

        assert_eq!(
            (
                app.git_status.confirm_discard.as_ref(),
                app.git_status.confirm_push,
                app.git_status.confirm_force_push,
            ),
            (None, false, false),
        );
    }

    #[test]
    fn git_metadata_change_refreshes_git_without_reloading_workspace_content() {
        let mut app = minimal_app_state();

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: false,
            workspace_paths: Vec::new(),
            git_metadata_changed: true,
            repo_presence_changed: false,
        });

        assert!(app.git_status_load.stale);
        assert!(app.graph_load.stale);
        assert!(!app.file_tree_load.stale);
        assert!(!app.preview_load.stale);
        assert!(!app.nav_workspace_load.stale);
    }

    #[test]
    fn unrelated_workspace_change_refreshes_tree_without_reloading_preview() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("script.json")));

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: true,
            workspace_paths: vec![PathBuf::from(".DS_Store")],
            git_metadata_changed: false,
            repo_presence_changed: false,
        });

        assert!(app.file_tree_load.stale);
        assert!(!app.preview_load.stale);
    }

    #[test]
    fn selected_preview_workspace_change_reloads_preview() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("script.json")));

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: true,
            workspace_paths: vec![PathBuf::from("script.json")],
            git_metadata_changed: false,
            repo_presence_changed: false,
        });

        assert!(app.preview_load.stale);
    }

    #[test]
    fn scheduled_preview_target_controls_workspace_invalidation() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("old.json")));
        app.preview_schedule = Some((PathBuf::from("next.json"), Instant::now()));

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: true,
            workspace_paths: vec![PathBuf::from("old.json")],
            git_metadata_changed: false,
            repo_presence_changed: false,
        });

        assert!(!app.preview_load.stale);

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: true,
            workspace_paths: vec![PathBuf::from("next.json")],
            git_metadata_changed: false,
            repo_presence_changed: false,
        });

        assert!(app.preview_load.stale);
    }

    #[test]
    fn selecting_displayed_preview_again_does_not_schedule_a_reload() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("script.json")));

        app.load_preview_for_path(PathBuf::from("script.json"));

        assert!(app.preview_schedule.is_none());
        assert!(app.preview_in_flight_path.is_none());
    }

    #[test]
    fn stale_displayed_preview_still_schedules_a_reload() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("script.json")));
        app.preview_load.mark_stale();

        app.load_preview_for_path(PathBuf::from("script.json"));

        assert_eq!(
            app.preview_schedule
                .as_ref()
                .map(|(path, _)| path.as_path()),
            Some(Path::new("script.json"))
        );
    }

    #[test]
    fn selecting_pending_preview_again_preserves_the_original_request() {
        let mut app = minimal_app_state();
        let deadline = Instant::now();
        app.preview_schedule = Some((PathBuf::from("script.json"), deadline));

        app.load_preview_for_path(PathBuf::from("script.json"));

        assert_eq!(
            app.preview_schedule,
            Some((PathBuf::from("script.json"), deadline))
        );
    }

    #[test]
    fn reselecting_displayed_preview_cancels_a_pending_other_path() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("script.json")));
        app.preview_schedule = Some((PathBuf::from("other.json"), Instant::now()));

        app.load_preview_for_path(PathBuf::from("script.json"));

        assert!(app.preview_schedule.is_none());
        assert!(app.preview_is_for(Path::new("script.json")));
    }

    #[test]
    fn in_flight_preview_target_controls_workspace_invalidation() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("old.json")));
        app.preview_in_flight_path = Some(PathBuf::from("next.json"));

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: true,
            workspace_paths: vec![PathBuf::from("next.json")],
            git_metadata_changed: false,
            repo_presence_changed: false,
        });

        assert!(app.preview_load.stale);
    }

    #[test]
    fn selected_preview_parent_directory_change_reloads_preview() {
        let mut app = minimal_app_state();
        app.preview_content = Some(Arc::new(global_search_text_preview("docs/guide/index.md")));

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: true,
            workspace_paths: vec![PathBuf::from("docs/guide")],
            git_metadata_changed: false,
            repo_presence_changed: false,
        });

        assert!(app.preview_load.stale);
    }

    #[test]
    fn git_metadata_change_requests_status_refresh_on_git_tab() {
        let mut app = minimal_app_state();
        app.active_tab = AppTab::Git;

        app.apply_fs_change(reef_io::FsChange {
            workspace_changed: false,
            workspace_paths: Vec::new(),
            git_metadata_changed: true,
            repo_presence_changed: false,
        });

        assert!(app.has_step_work_due(Instant::now()));
    }

    #[test]
    fn navigate_files_moves_selection_and_resets_diff_scrolls() {
        let mut app = AppState {
            staged_files: Vec::new(),
            unstaged_files: vec![
                git_entry("z.txt"),
                git_entry("src/z.rs"),
                git_entry("README.md"),
                git_entry("src/a.rs"),
                git_entry("assets/logo.png"),
            ],
            selected_file: Some(SelectedFile {
                path: "assets/logo.png".to_string(),
                is_staged: false,
            }),
            diff_scroll: 9,
            diff_h_scroll: 8,
            sbs_left_h_scroll: 7,
            sbs_right_h_scroll: 6,
            git_status: GitStatusState {
                tree_mode: true,
                ..GitStatusState::default()
            },
            ..minimal_app_state()
        };
        app.rebuild_git_status_tree_rows();

        app.navigate_files(2);

        assert_eq!(
            app.selected_file
                .as_ref()
                .map(|selected| selected.path.as_str()),
            Some("src/z.rs")
        );
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.diff_h_scroll, 0);
        assert_eq!(app.sbs_left_h_scroll, 0);
        assert_eq!(app.sbs_right_h_scroll, 0);
    }

    #[test]
    fn collapse_during_tree_load_rejects_pre_collapse_payload() {
        use crate::FileTreeState;
        use crate::tasks::FileTreePayload;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let backend = Arc::new(reef_io::LocalBackend::open_at(tmp.path().to_path_buf()));
        let mut app = AppState::new(AppStateConfig {
            backend,
            prefs: AppPrefs::default(),
            now: Instant::now(),
            subscribe_fs_events: false,
        });
        let mut parent = tree_entry("src", 0, true);
        parent.is_expanded = true;
        app.file_tree.state =
            FileTreeState::with_entries(vec![parent.clone(), tree_entry("src/a.rs", 1, false)]);
        app.file_tree.toggle_expand(0);
        let stale_generation = app.file_tree_load.begin();
        let stale_tree_revision = app.file_tree_revision;

        app.toggle_file_tree_expand_and_refresh(0);
        app.apply_worker_result_core(
            WorkerResult::FileTree {
                generation: stale_generation,
                tree_revision: stale_tree_revision,
                result: Ok(FileTreePayload {
                    entries: vec![parent, tree_entry("src/a.rs", 1, false)],
                    selected_idx: 0,
                }),
            },
            Instant::now(),
        );

        assert_eq!(stale_generation, app.file_tree_load.generation);
        assert!(!app.file_tree_load.loading);
        assert!(app.file_tree_load.stale);
        assert_eq!(
            app.file_tree
                .entries
                .iter()
                .map(|entry| entry.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("src")]
        );
        assert!(!app.file_tree.entries[0].is_expanded);
    }

    #[test]
    fn collapse_all_during_tree_load_invalidates_the_pending_tree_shape() {
        use crate::FileTreeState;

        let mut app = minimal_app_state();
        app.file_tree.state = FileTreeState::with_entries(vec![
            tree_entry("src", 0, true),
            tree_entry("src/a.rs", 1, false),
        ]);
        app.file_tree.toggle_expand(0);
        let tree_revision = app.file_tree_revision;
        let generation = app.file_tree_load.begin();

        app.collapse_all_file_tree_entries();

        assert!(app.file_tree_load.loading);
        assert_eq!(app.file_tree_load.generation, generation);
        assert_ne!(app.file_tree_revision, tree_revision);
        assert_eq!(
            app.file_tree
                .entries
                .iter()
                .map(|entry| entry.path.as_path())
                .collect::<Vec<_>>(),
            vec![Path::new("src")],
        );
    }

    #[test]
    fn subtree_result_uses_current_git_status_decorations() {
        use crate::FileTreeState;
        use crate::tasks::FileTreeSubtreePayload;

        let mut app = minimal_app_state();
        app.file_tree.state = FileTreeState::with_entries(vec![tree_entry("src", 0, true)]);
        app.file_tree.toggle_expand(0);
        app.unstaged_files = vec![FileEntry {
            path: "src/a.rs".to_string(),
            status: FileStatus::Added,
            additions: 1,
            deletions: 0,
        }];
        app.file_tree
            .refresh_git_statuses(&[], &app.unstaged_files);
        let request_id = 1;
        app.file_tree_subtree_requests
            .insert(PathBuf::from("src"), request_id);
        let mut stale_child = tree_entry("src/a.rs", 1, false);
        stale_child.git_status = Some('M');

        app.apply_worker_result_core(
            WorkerResult::FileTreeSubtree {
                request_id,
                parent_path: PathBuf::from("src"),
                result: Ok(FileTreeSubtreePayload {
                    parent_path: PathBuf::from("src"),
                    entries: vec![stale_child],
                }),
            },
            Instant::now(),
        );

        assert_eq!(app.file_tree.entries[1].git_status, Some('A'));
    }

    #[test]
    fn sibling_subtree_results_apply_independently_out_of_order() {
        use crate::FileTreeState;
        use crate::tasks::FileTreeSubtreePayload;

        let mut app = minimal_app_state();
        app.file_tree.state = FileTreeState::with_entries(vec![
            tree_entry("alpha", 0, true),
            tree_entry("beta", 0, true),
        ]);
        app.file_tree.toggle_expand(0);
        app.file_tree.toggle_expand(1);
        app.file_tree_subtree_requests
            .insert(PathBuf::from("alpha"), 1);
        app.file_tree_subtree_requests
            .insert(PathBuf::from("beta"), 2);

        app.apply_worker_result_core(
            WorkerResult::FileTreeSubtree {
                request_id: 2,
                parent_path: PathBuf::from("beta"),
                result: Ok(FileTreeSubtreePayload {
                    parent_path: PathBuf::from("beta"),
                    entries: vec![tree_entry("beta/b.rs", 1, false)],
                }),
            },
            Instant::now(),
        );
        app.apply_worker_result_core(
            WorkerResult::FileTreeSubtree {
                request_id: 1,
                parent_path: PathBuf::from("alpha"),
                result: Ok(FileTreeSubtreePayload {
                    parent_path: PathBuf::from("alpha"),
                    entries: vec![tree_entry("alpha/a.rs", 1, false)],
                }),
            },
            Instant::now(),
        );

        assert_eq!(
            app.file_tree
                .entries
                .iter()
                .map(|entry| entry.path.as_path())
                .collect::<Vec<_>>(),
            vec![
                Path::new("alpha"),
                Path::new("alpha/a.rs"),
                Path::new("beta"),
                Path::new("beta/b.rs"),
            ]
        );
        assert!(app.file_tree_subtree_requests.is_empty());
    }

    #[test]
    fn collapsed_parent_rejects_its_in_flight_subtree_result() {
        use crate::FileTreeState;
        use crate::tasks::FileTreeSubtreePayload;

        let mut app = minimal_app_state();
        app.file_tree.state = FileTreeState::with_entries(vec![tree_entry("src", 0, true)]);
        app.file_tree.toggle_expand(0);
        app.file_tree_subtree_requests
            .insert(PathBuf::from("src"), 7);
        app.toggle_file_tree_expand_and_refresh(0);

        app.apply_worker_result_core(
            WorkerResult::FileTreeSubtree {
                request_id: 7,
                parent_path: PathBuf::from("src"),
                result: Ok(FileTreeSubtreePayload {
                    parent_path: PathBuf::from("src"),
                    entries: vec![tree_entry("src/a.rs", 1, false)],
                }),
            },
            Instant::now(),
        );

        assert_eq!(app.file_tree.entries.len(), 1);
        assert!(!app.file_tree.entries[0].is_expanded);
    }

    #[test]
    fn app_state_new_applies_initial_prefs() {
        use reef_core::diff::DiffLayout;
        use reef_core::git::GraphScope;
        use reef_io::LocalBackend;
        use std::path::PathBuf;
        use std::sync::Arc;

        let app = AppState::new(AppStateConfig {
            backend: Arc::new(LocalBackend::open_at(PathBuf::from("."))),
            prefs: AppPrefs {
                diff_layout: DiffLayout::SideBySide,
                diff_mode: DiffMode::FullFile,
                status_tree_mode: true,
                graph_scope: GraphScope::Branch("refs/heads/main".into()),
                graph_recent_branches: vec!["refs/heads/main".into()],
                commit_diff_layout: DiffLayout::SideBySide,
                commit_diff_mode: DiffMode::FullFile,
                commit_files_tree_mode: true,
                structured_preview_mode: StructuredPreviewMode::Raw,
                quick_open: crate::QuickOpenState::default(),
            },
            now: Instant::now(),
            subscribe_fs_events: false,
        });

        assert_eq!(app.diff_layout, DiffLayout::SideBySide);
        assert_eq!(app.diff_mode, DiffMode::FullFile);
        assert!(app.git_status.tree_mode);
        assert_eq!(
            app.git_graph.scope,
            GraphScope::Branch("refs/heads/main".into())
        );
        assert_eq!(app.git_graph.recent_branches, ["refs/heads/main"]);
        assert_eq!(app.commit_detail.diff_layout, DiffLayout::SideBySide);
        assert_eq!(app.commit_detail.diff_mode, DiffMode::FullFile);
        assert!(app.commit_detail.files_tree_mode);
        assert_eq!(app.structured_preview_mode, StructuredPreviewMode::Raw);
        assert!(app.fs_watcher_rx.is_none());
    }

    #[test]
    fn paste_global_search_overlay_inserts_and_marks_edited() {
        let mut app = minimal_app_state();
        assert!(app.global_search.last_keystroke_at.is_none());

        app.paste_global_search_overlay("hello", Instant::now());

        assert_eq!(app.global_search.core.filter, "hello");
        assert_eq!(app.global_search.core.cursor, 5);
        assert!(app.global_search.last_keystroke_at.is_some());
    }

    #[test]
    fn paste_global_search_overlay_pure_newlines_do_not_trigger_rerun() {
        let mut app = minimal_app_state();

        app.paste_global_search_overlay("\n\r\n", Instant::now());

        assert!(app.global_search.core.filter.is_empty());
        assert!(app.global_search.last_keystroke_at.is_none());
    }

    #[test]
    fn paste_global_search_overlay_preserves_unrelated_state() {
        let mut app = minimal_app_state();
        app.global_search.results = vec![dummy_hit("a"), dummy_hit("b"), dummy_hit("c")];
        app.global_search.core.selected_idx = 2;
        app.global_search.replace_text = "kept".to_string();
        app.global_search.replace_cursor = 4;

        app.paste_global_search_overlay("foo", Instant::now());

        assert_eq!(app.global_search.core.selected_idx, 2);
        assert_eq!(app.global_search.replace_text, "kept");
        assert_eq!(app.global_search.replace_cursor, 4);
    }

    #[test]
    fn set_global_search_query_atomically_updates_renderer_input_state() {
        let mut app = minimal_app_state();
        app.global_search.results = vec![dummy_hit("a"), dummy_hit("b")];
        app.global_search.core.selected_idx = 1;
        app.global_search.excluded.insert((PathBuf::from("a"), 0));
        let now = Instant::now();

        app.set_global_search_query("needle".to_string(), now);

        assert_eq!(app.global_search.core.filter, "needle");
        assert_eq!(app.global_search.core.cursor, "needle".len());
        assert_eq!(app.global_search.core.selected_idx, 0);
        assert!(app.global_search.excluded.is_empty());
        assert_eq!(app.global_search.last_keystroke_at, Some(now));
    }

    #[test]
    fn set_global_search_query_does_not_reschedule_unchanged_query() {
        let mut app = minimal_app_state();
        app.global_search.core.filter = "needle".to_string();
        app.global_search.core.cursor = 0;
        let excluded = (PathBuf::from("a"), 0);
        app.global_search.excluded.insert(excluded.clone());

        app.set_global_search_query("needle".to_string(), Instant::now());

        assert_eq!(app.global_search.core.cursor, "needle".len());
        assert_eq!(app.global_search.excluded.len(), 1);
        assert!(app.global_search.excluded.contains(&excluded));
        assert!(app.global_search.last_keystroke_at.is_none());
    }

    #[test]
    fn set_global_search_replacement_updates_text_and_cursor_together() {
        let mut app = minimal_app_state();

        app.set_global_search_replacement("replacement".to_string());

        assert_eq!(app.global_search.replace_text, "replacement");
        assert_eq!(app.global_search.replace_cursor, "replacement".len());
        assert!(app.global_search.last_keystroke_at.is_none());
    }

    #[test]
    fn sync_global_search_preview_updates_a_different_match_on_the_same_line() {
        use reef_core::preview::{PreviewBody, TextPreview};

        let mut app = minimal_app_state();
        let mut hit = dummy_hit("Cargo.toml");
        hit.line_text = "needle and needle".to_string();
        hit.byte_range = 11..17;
        app.active_tab = AppTab::Search;
        app.global_search.results = vec![hit];
        app.preview_content = Some(Arc::new(PreviewContent {
            path: "Cargo.toml".to_string(),
            resolved_path: None,
            local_path: None,
            bytes_on_disk: 0,
            mime: Some("text/plain".to_string()),
            body: PreviewBody::Text(TextPreview {
                lines: vec!["needle and needle".to_string()],
                source: None,
                highlighted: None,
                parsed: None,
            }),
        }));
        app.set_preview_highlight_persistent(PathBuf::from("Cargo.toml"), 0, 0..6);

        assert!(app.sync_global_search_preview_if_stale(20));
        assert_eq!(
            app.preview_highlight
                .as_ref()
                .map(|highlight| highlight.byte_range.clone()),
            Some(11..17)
        );
        assert!(app.preview_schedule.is_none());
        assert!(!app.sync_global_search_preview_if_stale(20));
    }

    #[test]
    fn sync_global_search_preview_recenters_reused_content() {
        let mut app = minimal_app_state();
        let mut hit = dummy_hit("Cargo.toml");
        hit.line = 50;
        hit.byte_range = 4..10;
        app.active_tab = AppTab::Search;
        app.global_search.results = vec![hit];
        app.preview_content = Some(Arc::new(global_search_text_preview("Cargo.toml")));

        assert!(app.sync_global_search_preview_to_selected(10));

        assert_eq!(app.preview_scroll, 45);
        assert!(app.preview_schedule.is_none());
    }

    #[test]
    fn sync_global_search_preview_cancels_scheduled_other_path() {
        let mut app = minimal_app_state();
        let mut hit = dummy_hit("Cargo.toml");
        hit.line = 50;
        hit.byte_range = 4..10;
        app.active_tab = AppTab::Search;
        app.global_search.results = vec![hit.clone()];
        app.preview_content = Some(Arc::new(global_search_text_preview("Cargo.toml")));
        app.set_preview_highlight_persistent(hit.path, hit.line, hit.byte_range);
        app.preview_schedule = Some((PathBuf::from("README.md"), Instant::now()));

        assert!(app.sync_global_search_preview_if_stale(10));

        assert!(app.preview_schedule.is_none());
        assert_eq!(app.preview_scroll, 45);
    }

    #[test]
    fn sync_global_search_preview_invalidates_in_flight_other_path() {
        let mut app = minimal_app_state();
        let mut hit = dummy_hit("Cargo.toml");
        hit.line = 50;
        hit.byte_range = 4..10;
        app.active_tab = AppTab::Search;
        app.global_search.results = vec![hit.clone()];
        app.preview_content = Some(Arc::new(global_search_text_preview("Cargo.toml")));
        app.set_preview_highlight_persistent(hit.path, hit.line, hit.byte_range);
        let stale_generation = app.preview_load.begin();
        app.preview_in_flight_path = Some(PathBuf::from("README.md"));

        assert!(app.sync_global_search_preview_if_stale(10));
        assert_ne!(app.preview_load.generation, stale_generation);
        assert!(app.preview_in_flight_path.is_none());

        let outcome = app.apply_preview_content(
            stale_generation,
            Some(global_search_text_preview("README.md")),
            10,
        );
        assert!(!outcome.accepted);
        assert!(app.preview_is_for(Path::new("Cargo.toml")));
    }

    #[test]
    fn sync_global_search_preview_is_inactive_outside_search_tab() {
        let mut app = minimal_app_state();
        app.global_search.results = vec![dummy_hit("Cargo.toml")];

        assert!(!app.sync_global_search_preview_if_stale(20));
        assert!(app.preview_highlight.is_none());
    }

    #[test]
    fn failed_global_search_accept_does_not_retry_preview() {
        let mut app = minimal_app_state();
        let hit = dummy_hit("Cargo.toml");
        app.active_tab = AppTab::Search;
        app.global_search.results = vec![hit.clone()];
        let generation = app.preview_load.begin();
        app.preview_in_flight_path = Some(hit.path.clone());
        app.begin_global_search_hit_accept(hit, None);

        assert!(
            app.reject_global_search_hit_accept(generation, Some("preview failed".to_string()))
        );

        assert!(!app.preview_load.stale);
        assert!(!app.has_step_work_due(Instant::now()));
    }

    #[test]
    fn paste_global_search_tab_routes_find_input_to_query() {
        let mut app = minimal_app_state();
        app.global_search.focus = SearchPanelFocus::FindInput;

        app.paste_global_search_tab("abc", Instant::now());

        assert_eq!(app.global_search.core.filter, "abc");
        assert_eq!(app.global_search.core.cursor, 3);
        assert!(app.global_search.last_keystroke_at.is_some());
        assert!(app.global_search.replace_text.is_empty());
    }

    #[test]
    fn paste_global_search_tab_routes_replace_input_to_replace_text() {
        let mut app = minimal_app_state();
        app.global_search.focus = SearchPanelFocus::ReplaceInput;
        app.global_search.replace_open = true;

        app.paste_global_search_tab("xyz", Instant::now());

        assert_eq!(app.global_search.replace_text, "xyz");
        assert_eq!(app.global_search.replace_cursor, 3);
        assert!(app.global_search.core.filter.is_empty());
        assert!(app.global_search.last_keystroke_at.is_none());
    }

    #[test]
    fn paste_global_search_tab_list_focus_is_noop() {
        let mut app = minimal_app_state();
        app.global_search.focus = SearchPanelFocus::List;
        app.global_search.core.filter = "kept-query".to_string();
        app.global_search.core.cursor = 10;
        app.global_search.replace_text = "kept-replace".to_string();
        app.global_search.replace_cursor = 12;

        app.paste_global_search_tab("LEAK", Instant::now());

        assert_eq!(app.global_search.core.filter, "kept-query");
        assert_eq!(app.global_search.replace_text, "kept-replace");
        assert!(app.global_search.last_keystroke_at.is_none());
    }

    #[test]
    fn app_snapshot_exposes_panel_summaries_without_rows() {
        let mut app = minimal_app_state();
        app.global_search.results = vec![dummy_hit("a"), dummy_hit("b")];
        app.global_search.core.selected_idx = 1;
        app.staged_files = vec![git_entry("staged.rs")];
        app.unstaged_files = vec![git_entry("unstaged.rs")];
        app.git_graph.rows = vec![reef_core::git::graph::GraphRow {
            commit: reef_core::git::CommitInfo {
                oid: "abc".to_string(),
                short_oid: "abc".to_string(),
                parents: Vec::new(),
                author_name: "a".to_string(),
                author_email: "a@example.com".to_string(),
                time: 0,
                subject: "init".to_string(),
            },
            cells: Vec::new(),
            node_col: 0,
        }];
        app.git_graph.selected_idx = 0;
        app.git_graph.selected_commit = Some("abc".to_string());

        let snapshot = crate::AppSnapshot::from_state(&app);

        assert_eq!(snapshot.search.result_count, 2);
        assert_eq!(snapshot.search.selected_idx, 1);
        assert_eq!(snapshot.git.staged_count, 1);
        assert_eq!(snapshot.git.unstaged_count, 1);
        assert_eq!(snapshot.graph.row_count, 1);
        assert_eq!(snapshot.graph.selected_commit.as_deref(), Some("abc"));
    }

    #[test]
    fn next_panel_three_col_forward_cycles_through_all_three() {
        assert_eq!(next_panel(AppPanel::Files, true, false), AppPanel::Commit);
        assert_eq!(next_panel(AppPanel::Commit, true, false), AppPanel::Diff);
        assert_eq!(next_panel(AppPanel::Diff, true, false), AppPanel::Files);
    }

    #[test]
    fn next_panel_three_col_reverse_cycles_in_opposite_order() {
        assert_eq!(next_panel(AppPanel::Files, true, true), AppPanel::Diff);
        assert_eq!(next_panel(AppPanel::Diff, true, true), AppPanel::Commit);
        assert_eq!(next_panel(AppPanel::Commit, true, true), AppPanel::Files);
    }

    #[test]
    fn next_panel_three_col_round_trip_returns_origin() {
        for panel in [AppPanel::Files, AppPanel::Commit, AppPanel::Diff] {
            let one_step = next_panel(panel, true, false);
            let back = next_panel(one_step, true, true);
            assert_eq!(back, panel);
        }
    }

    #[test]
    fn next_panel_two_col_toggles_files_and_diff() {
        assert_eq!(next_panel(AppPanel::Files, false, false), AppPanel::Diff);
        assert_eq!(next_panel(AppPanel::Diff, false, false), AppPanel::Files);
        assert_eq!(next_panel(AppPanel::Commit, false, false), AppPanel::Diff);
    }

    #[test]
    fn next_panel_two_col_reverse_equals_forward() {
        for panel in [AppPanel::Files, AppPanel::Commit, AppPanel::Diff] {
            assert_eq!(
                next_panel(panel, false, false),
                next_panel(panel, false, true),
            );
        }
    }

    #[test]
    fn idle_git_tab_does_not_schedule_periodic_status_refresh() {
        let mut app = minimal_app_state();
        app.active_tab = AppTab::Git;

        assert!(app.next_deadline().is_none());
    }

    fn minimal_app_state() -> AppState {
        use reef_io::LocalBackend;
        use std::path::PathBuf;
        use std::sync::Arc;
        use std::time::Instant;

        let backend = Arc::new(LocalBackend::open_at(PathBuf::from(".")));
        AppState::new(AppStateConfig {
            backend,
            prefs: AppPrefs::default(),
            now: Instant::now(),
            subscribe_fs_events: false,
        })
    }

    fn dummy_hit(name: &str) -> MatchHit {
        MatchHit {
            path: PathBuf::from(name),
            display: name.to_string(),
            line: 0,
            line_text: String::new(),
            line_revision: reef_io::content_line_revision(b""),
            byte_range: 0..0,
        }
    }

    fn global_search_text_preview(path: &str) -> PreviewContent {
        use reef_core::preview::{PreviewBody, TextPreview};

        PreviewContent {
            path: path.to_string(),
            resolved_path: None,
            local_path: None,
            bytes_on_disk: 0,
            mime: Some("text/plain".to_string()),
            body: PreviewBody::Text(TextPreview {
                lines: vec!["needle".to_string()],
                source: None,
                highlighted: None,
                parsed: None,
            }),
        }
    }
}
