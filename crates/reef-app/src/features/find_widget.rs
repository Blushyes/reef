use crate::features::search::{MatchLoc, SearchSnapshot};
use reef_core::diff::{DiffLayout, DiffSide};
use reef_core::search::{build_row_index, ranges_on_row};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindTarget {
    FilePreview,
    DiffUnified,
    DiffSbsLeft,
    DiffSbsRight,
    GraphDiffUnified,
    GraphDiffSbsLeft,
    GraphDiffSbsRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindWidgetToggle {
    MatchCase,
    WholeWord,
    Regex,
}

#[derive(Debug, Default)]
pub struct FindWidgetState {
    pub active: bool,
    pub query: String,
    pub cursor: usize,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub target: Option<FindTarget>,
    pub matches: Vec<MatchLoc>,
    pub current: Option<usize>,
    pub snapshot: Option<SearchSnapshot>,
    pub row_index: HashMap<usize, Vec<usize>>,
    pub regex_error: Option<String>,
}

impl FindWidgetState {
    pub fn set_matches(&mut self, matches: Vec<MatchLoc>) {
        self.matches = matches;
        self.current = None;
        self.row_index = build_row_index(&self.matches);
    }

    pub fn clear_matches(&mut self) {
        self.matches.clear();
        self.row_index.clear();
        self.current = None;
        self.regex_error = None;
    }

    pub fn ranges_on_row(
        &self,
        target: FindTarget,
        row: usize,
    ) -> (Vec<Range<usize>>, Option<Range<usize>>) {
        if self.target != Some(target) || self.matches.is_empty() {
            return (Vec::new(), None);
        }
        ranges_on_row(&self.matches, &self.row_index, self.current, row)
    }

    pub fn clear(&mut self) {
        *self = FindWidgetState::default();
    }
}

pub fn diff_target_from_layout(
    layout: DiffLayout,
    selection_side: Option<DiffSide>,
    graph: bool,
) -> FindTarget {
    match layout {
        DiffLayout::Unified => {
            if graph {
                FindTarget::GraphDiffUnified
            } else {
                FindTarget::DiffUnified
            }
        }
        DiffLayout::SideBySide => match selection_side {
            Some(DiffSide::SbsLeft) => {
                if graph {
                    FindTarget::GraphDiffSbsLeft
                } else {
                    FindTarget::DiffSbsLeft
                }
            }
            Some(DiffSide::SbsRight) | Some(DiffSide::Unified) | None => {
                if graph {
                    FindTarget::GraphDiffSbsRight
                } else {
                    FindTarget::DiffSbsRight
                }
            }
        },
    }
}

pub fn baseline_row(target: FindTarget, snap: &SearchSnapshot) -> usize {
    match target {
        FindTarget::FilePreview => snap.preview_scroll,
        FindTarget::DiffUnified | FindTarget::DiffSbsLeft | FindTarget::DiffSbsRight => {
            snap.diff_scroll
        }
        FindTarget::GraphDiffUnified
        | FindTarget::GraphDiffSbsLeft
        | FindTarget::GraphDiffSbsRight => snap.graph_diff_scroll,
    }
}
