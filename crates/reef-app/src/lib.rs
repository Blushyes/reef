//! Shared Reef application primitives.
//!
//! Keep only renderer-neutral state that Reef frontends actually share.

mod app;
mod command;
mod effect;
mod engine;
mod features;
mod location;
mod runtime;
mod snapshot;
mod tab;
mod tasks;
mod text_input;

pub use app::{
    AppPanel, AppPrefs, AppState, AppStateConfig, CommitDetailState, CommitError, CommitFileDiff,
    CommitFileDiffLoadOutcome, DiffHighlighted, DiffMode, DiscardTarget, FocusedPreviewFileRow,
    FocusedPreviewFileSource, GLOBAL_SEARCH_DEBOUNCE, GLOBAL_SEARCH_MAX_H_SCROLL,
    GLOBAL_SEARCH_MAX_LINE_CHARS, GLOBAL_SEARCH_MAX_RESULTS, GLOBAL_SEARCH_PREVIEW_SYNC_DEBOUNCE,
    GRAPH_RECENT_BRANCHES_MAX, GitGraphState, GitStatusState, GraphBranchPickerOpenOutcome,
    GraphScopeChangeOutcome, HighlightFade, HighlightedDiff, LineTokens, LspRefineOutcome,
    MatchHit, NavAnchor, NavCandidatesPopup, NavPendingJump, NormalizeActivePanelOutcome,
    PREFETCH_DELAY, PREVIEW_DEBOUNCE, PreviewHighlight, PreviewMergeOutcome, PushError,
    RangeDetail, SearchPanelFocus, SelectedFile, TabChangeOutcome, TickOptions,
    ToggleSidebarOutcome, ViewMode, center_scroll, compute_sidebar_width, compute_three_col_widths,
    compute_uses_three_col,
};
pub use command::AppCommand;
pub use effect::{AppEffect, Toast, ToastLevel};
pub use engine::{AppCommandOutcome, AppConfig, ReefApp};
pub use features::confirm::{ConfirmRequest, ConfirmTone, TreeDeleteConfirm};
pub use features::db_preview::{DbNav, DbPreviewState, max_page_for_object};
pub use features::file_clipboard::FileClipboard;
pub use features::file_selection::SelectionSet;
pub use features::file_tree::{FileTree, FileTreeState, TreeEntry};
pub use features::find_widget::{
    FindTarget, FindWidgetState, FindWidgetToggle, diff_target_from_layout,
};
pub use features::global_search::{
    GlobalSearchState, mark_query_edited_at as mark_global_search_query_edited_at,
    move_selection as move_global_search_selection,
};
pub use features::graph_branch_picker::GraphBranchPickerState;
pub use features::hosts_picker::{
    HostsPickerState, InputMode, MAX_RECENT, RECENT_PREF_KEY, SshTarget,
};
pub use features::picker::{PickerInput, PickerInputOutcome, PickerState, apply_picker_input};
pub use features::place_mode::{HoverTarget, PlaceModeState, resolve_hover_target};
pub use features::quick_open::{
    Candidate as QuickOpenCandidate, QuickOpenState, filter as filter_quick_open,
    mark_stale as mark_quick_open_stale, move_selection as move_quick_open_selection,
};
pub use features::search::{
    MatchLoc, SearchSnapshot, SearchState, SearchTarget, SearchViewport, WrapMsg,
};
pub use features::settings::{EditorEdit, SettingItem, SettingSection, SettingsState};
pub use features::tree_context_menu::{ContextMenuItem, ContextMenuState};
pub use features::tree_drag::{DragPress, InputModifiers, TreeDragState};
pub use features::tree_edit::{TreeEditMode, TreeEditState};
pub use location::{CursorPosition, LocationSnapshot, LocationSurface, ScrollPosition};
pub use runtime::{AppRuntimeEvent, AsyncState, FileActionNotice};
pub use snapshot::{
    AppSnapshot, AsyncSnapshot, FilesPanelSnapshot, GitPanelSnapshot, GlobalSearchPanelSnapshot,
    GlobalSearchRowSnapshot, GraphBranchPickerRowSnapshot, GraphBranchPickerSnapshot,
    GraphPanelSnapshot, HostsPickerRowSnapshot, HostsPickerSnapshot, OverlaySnapshot,
    PendingConfirmSnapshot, PreviewKindSnapshot, QuickOpenRowSnapshot, QuickOpenSnapshot,
};
pub use tab::AppTab;
pub use tasks::{
    _reset_highlight_cache, DbDetailPayload, DbPagePayload, DbPageRequest, FileTreePayload,
    FsMutationKind, GitMutation, GitMutationPayload, GitRevertPath, GitStatusPayload, GraphPayload,
    MAX_REPLACE_FILE_SIZE, PasteItem, ReplaceItem, ReplaceLine, ReplaceSummary, TaskCoordinator,
    WorkerResult, highlight_diff,
};
pub use text_input::{
    TextEditOp, TextEditOutcome, apply_multi_line_op, apply_single_line_op,
    apply_single_line_op_filtered,
};
