use crate::{
    AppPanel, AppState, AppTab, ConfirmRequest, ConfirmTone, GitGraphState, MatchHit, SelectedFile,
    ViewMode, features::hosts_picker::InputMode,
};
use reef_core::git::GraphScope;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppSnapshot {
    pub active_tab: AppTab,
    pub active_panel: AppPanel,
    pub view_mode: ViewMode,
    pub workdir_name: String,
    pub branch_name: String,
    pub has_repo: bool,
    pub show_help: bool,
    pub sidebar_visible: bool,
    pub toast: Option<crate::Toast>,
    pub overlays: OverlaySnapshot,
    pub files: FilesPanelSnapshot,
    pub search: GlobalSearchPanelSnapshot,
    pub git: GitPanelSnapshot,
    pub graph: GraphPanelSnapshot,
    pub quick_open: QuickOpenSnapshot,
    pub hosts_picker: HostsPickerSnapshot,
    pub graph_branch_picker: GraphBranchPickerSnapshot,
}

#[derive(Debug, Clone, Default)]
pub struct OverlaySnapshot {
    pub db_goto: bool,
    pub quick_open: bool,
    pub global_search: bool,
    pub hosts_picker: bool,
    pub graph_branch_picker: bool,
    pub nav_candidates: bool,
    pub paste_conflict: bool,
    pub place_mode: bool,
    pub tree_drag: bool,
    pub tree_edit: bool,
    pub tree_context_menu: bool,
    pub confirm: Option<PendingConfirmSnapshot>,
    pub commit_editor: bool,
    pub search_input: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConfirmSnapshot {
    pub request: ConfirmRequest,
    pub tone: ConfirmTone,
}

#[derive(Debug, Clone, Default)]
pub struct AsyncSnapshot {
    pub loading: bool,
    pub stale: bool,
    pub error: Option<String>,
}

impl AsyncSnapshot {
    fn from_state(state: &crate::AsyncState) -> Self {
        Self {
            loading: state.loading,
            stale: state.stale,
            error: state.error.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FilesPanelSnapshot {
    pub root: PathBuf,
    pub tree_len: usize,
    pub tree_selected: usize,
    pub tree_scroll: usize,
    pub selected_path: Option<PathBuf>,
    pub preview_path: Option<String>,
    pub preview_kind: Option<PreviewKindSnapshot>,
    pub preview_scroll: usize,
    pub preview_h_scroll: usize,
    pub tree_load: AsyncSnapshot,
    pub preview_load: AsyncSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKindSnapshot {
    Text,
    Markdown,
    Image,
    Binary,
    Database,
}

#[derive(Debug, Clone)]
pub struct GlobalSearchPanelSnapshot {
    pub active: bool,
    pub query: String,
    pub query_cursor: usize,
    pub selected_idx: usize,
    pub result_count: usize,
    pub included_count: usize,
    pub truncated: bool,
    pub replace_open: bool,
    pub replace_text: String,
    pub replace_cursor: usize,
    pub focus: crate::SearchPanelFocus,
    pub scroll: usize,
    pub h_scroll: usize,
    pub load: AsyncSnapshot,
    pub replace_load: AsyncSnapshot,
    pub replace_progress: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct GlobalSearchRowSnapshot {
    pub hit: MatchHit,
    pub included: bool,
}

#[derive(Debug, Clone)]
pub struct GitPanelSnapshot {
    pub staged_count: usize,
    pub unstaged_count: usize,
    pub selected_file: Option<SelectedFile>,
    pub tree_mode: bool,
    pub status_scroll: usize,
    pub diff_scroll: usize,
    pub diff_h_scroll: usize,
    pub status_load: AsyncSnapshot,
    pub diff_load: AsyncSnapshot,
    pub commit_in_flight: bool,
    pub push_in_flight: bool,
    pub ahead_behind: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct GraphPanelSnapshot {
    pub scope: GraphScope,
    pub row_count: usize,
    pub selected_idx: usize,
    pub selected_commit: Option<String>,
    pub selection_range: (usize, usize),
    pub visual_mode: bool,
    pub scroll: usize,
    pub detail_loaded: bool,
    pub file_diff_path: Option<String>,
    pub graph_load: AsyncSnapshot,
    pub detail_load: AsyncSnapshot,
    pub file_diff_load: AsyncSnapshot,
}

#[derive(Debug, Clone)]
pub struct QuickOpenSnapshot {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub selected_idx: usize,
    pub match_count: usize,
    pub recent: bool,
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickOpenRowSnapshot {
    pub display: String,
    pub indices: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct HostsPickerSnapshot {
    pub active: bool,
    pub input_mode: InputMode,
    pub input_text: String,
    pub cursor: usize,
    pub selected_idx: usize,
    pub row_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostsPickerRowSnapshot {
    pub left: String,
    pub right: String,
    pub recent: bool,
}

#[derive(Debug, Clone)]
pub struct GraphBranchPickerSnapshot {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub selected_idx: usize,
    pub row_count: usize,
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphBranchPickerRowSnapshot {
    pub left: String,
    pub right: String,
    pub accent: bool,
}

impl AppSnapshot {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            active_tab: state.active_tab,
            active_panel: state.active_panel,
            view_mode: state.view_mode,
            workdir_name: state.workdir_name.clone(),
            branch_name: state.branch_name.clone(),
            has_repo: state.backend.has_repo(),
            show_help: state.show_help,
            sidebar_visible: state.sidebar_visible,
            toast: state.toasts.last().cloned(),
            overlays: OverlaySnapshot {
                db_goto: state.db_goto_input.is_some(),
                quick_open: state.quick_open.core.active,
                global_search: state.global_search.core.active,
                hosts_picker: state.hosts_picker.core.active,
                graph_branch_picker: state.graph_branch_picker.core.active,
                nav_candidates: state.nav_candidates.is_some(),
                paste_conflict: state.paste_conflict.is_some(),
                place_mode: state.place_mode.active,
                tree_drag: state.tree_drag.active,
                tree_edit: state.tree_edit.active,
                tree_context_menu: state.tree_context_menu.active,
                confirm: state
                    .pending_confirm
                    .as_ref()
                    .map(|request| PendingConfirmSnapshot {
                        request: request.clone(),
                        tone: request.tone(),
                    }),
                commit_editor: state.git_status.commit_editing,
                search_input: state.global_search.input_focused(),
            },
            files: FilesPanelSnapshot::from_state(state),
            search: GlobalSearchPanelSnapshot::from_state(state),
            git: GitPanelSnapshot::from_state(state),
            graph: GraphPanelSnapshot::from_state(state),
            quick_open: QuickOpenSnapshot::from_state(state),
            hosts_picker: HostsPickerSnapshot::from_state(state),
            graph_branch_picker: GraphBranchPickerSnapshot::from_state(state),
        }
    }
}

impl FilesPanelSnapshot {
    fn from_state(state: &AppState) -> Self {
        Self {
            root: state.file_tree.root.clone(),
            tree_len: state.file_tree.entries.len(),
            tree_selected: state.file_tree.selected,
            tree_scroll: state.tree_scroll,
            selected_path: state.file_tree.selected_path(),
            preview_path: state.preview_content.as_ref().map(|p| p.path.clone()),
            preview_kind: state
                .preview_content
                .as_ref()
                .map(|p| PreviewKindSnapshot::from_body(&p.body)),
            preview_scroll: state.preview_scroll,
            preview_h_scroll: state.preview_h_scroll,
            tree_load: AsyncSnapshot::from_state(&state.file_tree_load),
            preview_load: AsyncSnapshot::from_state(&state.preview_load),
        }
    }
}

impl PreviewKindSnapshot {
    fn from_body(body: &reef_core::preview::PreviewBody) -> Self {
        match body {
            reef_core::preview::PreviewBody::Text(_) => Self::Text,
            reef_core::preview::PreviewBody::Markdown(_) => Self::Markdown,
            reef_core::preview::PreviewBody::Image(_) => Self::Image,
            reef_core::preview::PreviewBody::Binary(_) => Self::Binary,
            reef_core::preview::PreviewBody::Database(_) => Self::Database,
        }
    }
}

impl GlobalSearchPanelSnapshot {
    fn from_state(state: &AppState) -> Self {
        Self {
            active: state.global_search.core.active,
            query: state.global_search.core.filter.clone(),
            query_cursor: state.global_search.core.cursor,
            selected_idx: state.global_search.core.selected_idx,
            result_count: state.global_search.results.len(),
            included_count: state.global_search.included_count(),
            truncated: state.global_search.truncated,
            replace_open: state.global_search.replace_open,
            replace_text: state.global_search.replace_text.clone(),
            replace_cursor: state.global_search.replace_cursor,
            focus: state.global_search.focus,
            scroll: state.global_search.scroll,
            h_scroll: state.global_search.results_h_scroll,
            load: AsyncSnapshot::from_state(&state.global_search_load),
            replace_load: AsyncSnapshot::from_state(&state.replace_load),
            replace_progress: state.global_search.replace_progress,
        }
    }
}

impl GitPanelSnapshot {
    fn from_state(state: &AppState) -> Self {
        Self {
            staged_count: state.staged_files.len(),
            unstaged_count: state.unstaged_files.len(),
            selected_file: state.selected_file.clone(),
            tree_mode: state.git_status.tree_mode,
            status_scroll: state.git_status.scroll,
            diff_scroll: state.diff_scroll,
            diff_h_scroll: state.diff_h_scroll,
            status_load: AsyncSnapshot::from_state(&state.git_status_load),
            diff_load: AsyncSnapshot::from_state(&state.diff_load),
            commit_in_flight: state.commit_load.loading,
            push_in_flight: state.push_load.loading,
            ahead_behind: state.git_status.ahead_behind,
        }
    }
}

impl GraphPanelSnapshot {
    fn from_state(state: &AppState) -> Self {
        Self {
            scope: state.git_graph.scope.clone(),
            row_count: state.git_graph.rows.len(),
            selected_idx: state.git_graph.selected_idx,
            selected_commit: state.git_graph.selected_commit.clone(),
            selection_range: GitGraphState::selected_range(&state.git_graph),
            visual_mode: state.git_graph.in_visual_mode(),
            scroll: state.git_graph.scroll,
            detail_loaded: state.commit_detail.detail.is_some()
                || state.commit_detail.range_detail.is_some(),
            file_diff_path: state
                .commit_detail
                .file_diff
                .as_ref()
                .map(|diff| diff.path.clone()),
            graph_load: AsyncSnapshot::from_state(&state.graph_load),
            detail_load: AsyncSnapshot::from_state(&state.commit_detail_load),
            file_diff_load: AsyncSnapshot::from_state(&state.commit_file_diff_load),
        }
    }
}

impl QuickOpenSnapshot {
    fn from_state(state: &AppState) -> Self {
        Self {
            active: state.quick_open.core.active,
            query: state.quick_open.core.filter.clone(),
            cursor: state.quick_open.core.cursor,
            selected_idx: state.quick_open.core.selected_idx,
            match_count: state.quick_open.matches.len(),
            recent: state.quick_open.core.filter.is_empty() && !state.quick_open.mru.is_empty(),
            scroll: state.quick_open.scroll,
        }
    }
}

impl HostsPickerSnapshot {
    fn from_state(state: &AppState) -> Self {
        let (input_text, cursor) = match state.hosts_picker.input_mode {
            InputMode::Search => (
                state.hosts_picker.core.filter.clone(),
                state.hosts_picker.core.cursor,
            ),
            InputMode::Path => (
                state.hosts_picker.path_buffer.clone(),
                state.hosts_picker.path_cursor,
            ),
        };
        Self {
            active: state.hosts_picker.core.active,
            input_mode: state.hosts_picker.input_mode,
            input_text,
            cursor,
            selected_idx: state.hosts_picker.core.selected_idx,
            row_count: state.hosts_picker.visible_rows().len(),
        }
    }
}

impl GraphBranchPickerSnapshot {
    fn from_state(state: &AppState) -> Self {
        Self {
            active: state.graph_branch_picker.core.active,
            query: state.graph_branch_picker.core.filter.clone(),
            cursor: state.graph_branch_picker.core.cursor,
            selected_idx: state.graph_branch_picker.core.selected_idx,
            row_count: state.graph_branch_picker.visible_rows().len(),
            scroll: state.graph_branch_picker.scroll,
        }
    }
}
