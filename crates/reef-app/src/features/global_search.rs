use crate::PickerState;
use crate::app::{MatchHit, SearchPanelFocus};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

pub struct GlobalSearchState {
    pub core: PickerState,
    pub scroll: usize,
    pub results: Vec<MatchHit>,
    pub truncated: bool,
    pub cancel: Arc<AtomicBool>,
    pub last_keystroke_at: Option<Instant>,
    pub last_searched_query: String,
    pub focus: SearchPanelFocus,
    pub replace_open: bool,
    pub replace_text: String,
    pub replace_cursor: usize,
    pub excluded: HashSet<(PathBuf, usize)>,
    pub replace_progress: Option<(usize, usize)>,
    pub results_h_scroll: usize,
    pub preview_sync_at: Option<Instant>,
}

impl Default for GlobalSearchState {
    fn default() -> Self {
        Self {
            core: PickerState::default(),
            scroll: 0,
            results: Vec::new(),
            truncated: false,
            cancel: Arc::new(AtomicBool::new(false)),
            last_keystroke_at: None,
            last_searched_query: String::new(),
            focus: SearchPanelFocus::List,
            replace_open: false,
            replace_text: String::new(),
            replace_cursor: 0,
            excluded: HashSet::new(),
            replace_progress: None,
            results_h_scroll: 0,
            preview_sync_at: None,
        }
    }
}

impl GlobalSearchState {
    pub fn input_focused(&self) -> bool {
        matches!(
            self.focus,
            SearchPanelFocus::FindInput | SearchPanelFocus::ReplaceInput
        )
    }

    pub fn is_match_included(&self, idx: usize) -> bool {
        let Some(hit) = self.results.get(idx) else {
            return false;
        };
        !self.excluded.contains(&(hit.path.clone(), hit.line))
    }

    pub fn toggle_match_excluded(&mut self, idx: usize) {
        let Some(hit) = self.results.get(idx).cloned() else {
            return;
        };
        let key = (hit.path.clone(), hit.line);
        if !self.excluded.remove(&key) {
            self.excluded.insert(key);
        }
    }

    pub fn included_count(&self) -> usize {
        if self.excluded.is_empty() {
            return self.results.len();
        }
        self.results
            .iter()
            .filter(|h| !self.excluded.contains(&(h.path.clone(), h.line)))
            .count()
    }

    pub fn cycle_focus_forward(&mut self) {
        self.focus = match (self.focus, self.replace_open) {
            (SearchPanelFocus::FindInput, true) => SearchPanelFocus::ReplaceInput,
            (SearchPanelFocus::FindInput, false) => SearchPanelFocus::List,
            (SearchPanelFocus::ReplaceInput, _) => SearchPanelFocus::List,
            (SearchPanelFocus::List, _) => SearchPanelFocus::FindInput,
        };
    }

    pub fn cycle_focus_backward(&mut self) {
        self.focus = match (self.focus, self.replace_open) {
            (SearchPanelFocus::FindInput, true) => SearchPanelFocus::List,
            (SearchPanelFocus::FindInput, false) => SearchPanelFocus::List,
            (SearchPanelFocus::ReplaceInput, _) => SearchPanelFocus::FindInput,
            (SearchPanelFocus::List, true) => SearchPanelFocus::ReplaceInput,
            (SearchPanelFocus::List, false) => SearchPanelFocus::FindInput,
        };
    }
}

pub fn mark_query_edited_at(state: &mut GlobalSearchState, now: Instant) {
    state.last_keystroke_at = Some(now);
    state.excluded.clear();
}

pub fn move_selection(state: &mut GlobalSearchState, delta: i32) {
    if state.results.is_empty() {
        state.core.selected_idx = 0;
        return;
    }
    let last = state.results.len() - 1;
    let cur = state.core.selected_idx as i32;
    let next = (cur + delta).clamp(0, last as i32) as usize;
    state.core.selected_idx = next;
}
