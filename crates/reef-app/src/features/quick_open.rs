use crate::PickerState;
use reef_core::quick_open::{QuickOpenCandidate, QuickOpenMatch};
use std::collections::VecDeque;
use std::path::PathBuf;

pub type Candidate = QuickOpenCandidate;
pub type MatchEntry = QuickOpenMatch;

pub struct QuickOpenState {
    pub core: PickerState,
    pub scroll: usize,
    pub index: Vec<Candidate>,
    pub index_stale: bool,
    pub matches: Vec<MatchEntry>,
    pub mru: VecDeque<PathBuf>,
}

impl Default for QuickOpenState {
    fn default() -> Self {
        Self {
            core: PickerState::default(),
            scroll: 0,
            index: Vec::new(),
            index_stale: true,
            matches: Vec::new(),
            mru: VecDeque::new(),
        }
    }
}

pub fn filter(state: &mut QuickOpenState) {
    state.matches =
        reef_core::quick_open::filter_candidates(&state.index, &state.core.filter, &state.mru);
    state.core.selected_idx = 0;
    state.scroll = 0;
}

pub fn mark_stale(state: &mut QuickOpenState) {
    state.index_stale = true;
}

pub fn move_selection(state: &mut QuickOpenState, delta: i32) {
    if state.matches.is_empty() {
        state.core.selected_idx = 0;
        return;
    }
    let last = state.matches.len() - 1;
    let cur = state.core.selected_idx as i32;
    let next = (cur + delta).clamp(0, last as i32) as usize;
    state.core.selected_idx = next;
}
