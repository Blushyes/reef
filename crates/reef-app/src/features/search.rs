use crate::app::{AppPanel, AppState};
use crate::{AppTab, SelectedFile};
use reef_core::search::{SearchMatch, build_row_index, ranges_on_row};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    FileTree,
    GitStatus,
    CommitGraph,
    FilePreview,
    Diff,
    CommitDetail,
    GraphDiff,
}

pub type MatchLoc = SearchMatch;

#[derive(Debug, Clone, Default)]
pub struct SearchSnapshot {
    pub preview_scroll: usize,
    pub preview_h_scroll: usize,
    pub diff_scroll: usize,
    pub diff_h_scroll: usize,
    pub commit_detail_scroll: usize,
    pub graph_diff_scroll: usize,
    pub file_tree_selected: usize,
    pub tree_scroll: usize,
    pub git_status_scroll: usize,
    pub git_status_selected_file: Option<SelectedFile>,
    pub git_graph_selected_idx: usize,
    pub git_graph_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMsg {
    Top,
    Bottom,
    NoMatch,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchViewport {
    pub preview_view_h: usize,
    pub diff_view_h: usize,
    pub commit_detail_view_h: usize,
}

#[derive(Debug, Default)]
pub struct SearchState {
    pub active: bool,
    pub backwards: bool,
    pub query: String,
    pub cursor: usize,
    pub target: Option<SearchTarget>,
    pub matches: Vec<MatchLoc>,
    pub current: Option<usize>,
    pub snapshot: Option<SearchSnapshot>,
    pub wrap_msg: Option<WrapMsg>,
    pub row_index: HashMap<usize, Vec<usize>>,
}

impl SearchState {
    pub fn can_step(&self) -> bool {
        !self.active && !self.matches.is_empty() && self.target.is_some()
    }

    pub fn set_matches(&mut self, matches: Vec<MatchLoc>) {
        self.matches = matches;
        self.current = None;
        self.row_index = build_row_index(&self.matches);
    }

    pub fn clear_matches(&mut self) {
        self.matches.clear();
        self.row_index.clear();
        self.current = None;
    }

    pub fn ranges_on_row(
        &self,
        target: SearchTarget,
        row: usize,
    ) -> (Vec<Range<usize>>, Option<Range<usize>>) {
        if self.target != Some(target) || self.matches.is_empty() {
            return (Vec::new(), None);
        }
        ranges_on_row(&self.matches, &self.row_index, self.current, row)
    }

    pub fn clear(&mut self) {
        *self = SearchState::default();
    }
}

pub fn resolve_search_target(state: &AppState, graph_uses_three_col: bool) -> Option<SearchTarget> {
    match (state.active_tab, state.active_panel) {
        (AppTab::Files, AppPanel::Files) => Some(SearchTarget::FileTree),
        (AppTab::Files, AppPanel::Diff) => Some(SearchTarget::FilePreview),
        (AppTab::Git, AppPanel::Files) => Some(SearchTarget::GitStatus),
        (AppTab::Git, AppPanel::Diff) => Some(SearchTarget::Diff),
        (AppTab::Graph, AppPanel::Files) => Some(SearchTarget::CommitGraph),
        (AppTab::Graph, AppPanel::Commit) => Some(SearchTarget::CommitDetail),
        (AppTab::Graph, AppPanel::Diff) => {
            if graph_uses_three_col {
                Some(SearchTarget::GraphDiff)
            } else {
                Some(SearchTarget::CommitDetail)
            }
        }
        (_, AppPanel::Commit) => None,
        (AppTab::Search, AppPanel::Files) => None,
        (AppTab::Search, AppPanel::Diff) => Some(SearchTarget::FilePreview),
    }
}

pub fn baseline_row(target: SearchTarget, snap: &SearchSnapshot) -> usize {
    match target {
        SearchTarget::FileTree => snap.file_tree_selected,
        SearchTarget::GitStatus => snap.git_status_scroll,
        SearchTarget::CommitGraph => snap.git_graph_selected_idx,
        SearchTarget::FilePreview => snap.preview_scroll,
        SearchTarget::Diff => snap.diff_scroll,
        SearchTarget::GraphDiff => snap.graph_diff_scroll,
        SearchTarget::CommitDetail => snap.commit_detail_scroll,
    }
}
