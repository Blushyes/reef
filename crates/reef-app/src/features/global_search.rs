use crate::PickerState;
use crate::app::{MatchHit, SearchPanelFocus};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

pub struct GlobalSearchState {
    pub core: PickerState,
    pub scroll: usize,
    pub results: Vec<MatchHit>,
    pub results_generation: u64,
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
            results_generation: 0,
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

fn compare_hits(left: &MatchHit, right: &MatchHit) -> Ordering {
    left.path.cmp(&right.path).then(left.line.cmp(&right.line))
}

/// Merge a newly streamed batch into the already sorted result set.
///
/// Search backends are free to emit files in walker order, so each batch is
/// sorted locally before a linear merge. This preserves the stable path/line
/// ordering without repeatedly sorting every result received so far.
pub fn merge_hits(results: &mut Vec<MatchHit>, mut incoming: Vec<MatchHit>) {
    if incoming.is_empty() {
        return;
    }
    incoming.sort_by(compare_hits);
    if results.is_empty() {
        *results = incoming;
        return;
    }
    if compare_hits(results.last().expect("non-empty results"), &incoming[0]) != Ordering::Greater {
        results.append(&mut incoming);
        return;
    }

    let existing = std::mem::take(results);
    let mut existing = existing.into_iter().peekable();
    let mut incoming = incoming.into_iter().peekable();
    results.reserve(existing.len() + incoming.len());
    while let (Some(left), Some(right)) = (existing.peek(), incoming.peek()) {
        if compare_hits(left, right) != Ordering::Greater {
            results.push(existing.next().expect("peeked existing hit"));
        } else {
            results.push(incoming.next().expect("peeked incoming hit"));
        }
    }
    results.extend(existing);
    results.extend(incoming);
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

#[cfg(test)]
mod tests {
    use super::merge_hits;
    use crate::app::MatchHit;
    use std::path::PathBuf;

    fn hit(path: &str, line: usize) -> MatchHit {
        MatchHit {
            path: PathBuf::from(path),
            display: path.to_string(),
            line,
            line_text: String::new(),
            line_revision: 0,
            byte_range: 0..0,
        }
    }

    #[test]
    fn streamed_hits_merge_in_path_and_line_order() {
        let mut results = vec![hit("b.rs", 2), hit("d.rs", 1)];
        merge_hits(
            &mut results,
            vec![hit("c.rs", 4), hit("a.rs", 3), hit("b.rs", 1)],
        );
        let order = results
            .iter()
            .map(|hit| (hit.path.to_string_lossy().into_owned(), hit.line))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                ("a.rs".to_string(), 3),
                ("b.rs".to_string(), 1),
                ("b.rs".to_string(), 2),
                ("c.rs".to_string(), 4),
                ("d.rs".to_string(), 1),
            ]
        );
    }
}
