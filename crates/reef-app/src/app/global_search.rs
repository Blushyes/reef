use super::*;

impl AppState {
    pub fn focus_global_search_find_input(&mut self) {
        self.global_search.focus = SearchPanelFocus::FindInput;
    }

    pub fn focus_global_search_replace_input(&mut self) {
        if self.global_search.replace_open {
            self.global_search.focus = SearchPanelFocus::ReplaceInput;
        }
    }

    pub fn focus_global_search_list(&mut self) {
        self.global_search.focus = SearchPanelFocus::List;
    }

    pub fn scroll_global_search_results_horizontal(&mut self, delta: i32) {
        if delta < 0 {
            self.global_search.results_h_scroll = self
                .global_search
                .results_h_scroll
                .saturating_sub((-delta) as usize);
        } else {
            self.global_search.results_h_scroll = self
                .global_search
                .results_h_scroll
                .saturating_add(delta as usize)
                .min(GLOBAL_SEARCH_MAX_H_SCROLL);
        }
    }

    pub fn set_global_search_results_horizontal_scroll(&mut self, value: usize) {
        self.global_search.results_h_scroll = value.min(GLOBAL_SEARCH_MAX_H_SCROLL);
    }

    pub fn clamp_global_search_results_horizontal_scroll(&mut self) -> usize {
        self.global_search.results_h_scroll = self
            .global_search
            .results_h_scroll
            .min(GLOBAL_SEARCH_MAX_H_SCROLL);
        self.global_search.results_h_scroll
    }

    pub fn select_global_search_result(&mut self, idx: usize) {
        self.cancel_global_search_hit_accept();
        self.global_search.core.selected_idx = idx;
    }

    pub fn ensure_global_search_selection_visible(&mut self, visible_rows: usize) -> usize {
        let sel = self.global_search.core.selected_idx;
        if sel < self.global_search.scroll {
            self.global_search.scroll = sel;
        } else if visible_rows > 0 && sel >= self.global_search.scroll + visible_rows {
            self.global_search.scroll = sel + 1 - visible_rows;
        }
        self.global_search.scroll
    }

    pub fn selected_global_search_hit(&self) -> Option<MatchHit> {
        self.global_search
            .results
            .get(self.global_search.core.selected_idx)
            .cloned()
    }

    pub fn sync_global_search_preview_to_selected(&mut self, preview_view_h: usize) -> bool {
        self.clear_global_search_preview_sync();
        let Some(hit) = self.selected_global_search_hit() else {
            return false;
        };
        self.cancel_preview_work_for_other_path(&hit.path);
        self.set_preview_highlight_persistent(hit.path.clone(), hit.line, hit.byte_range.clone());
        if self.preview_is_for(&hit.path) {
            self.center_preview_on_line(hit.line, preview_view_h);
        } else if !self.preview_target_matches(&hit.path) {
            self.load_preview_for_path(hit.path);
        }
        true
    }

    pub fn sync_global_search_preview_if_stale(&mut self, preview_view_h: usize) -> bool {
        if self.active_tab != AppTab::Search {
            return false;
        }
        let Some(hit) = self.selected_global_search_hit() else {
            return false;
        };
        if self.preview_highlight.as_ref().is_some_and(|highlight| {
            highlight.path == hit.path
                && highlight.row == hit.line
                && highlight.byte_range == hit.byte_range
        }) {
            if self.preview_target_matches(&hit.path) {
                return false;
            }
        }
        self.sync_global_search_preview_to_selected(preview_view_h)
    }

    pub fn begin_global_search_hit_accept(
        &mut self,
        hit: MatchHit,
        origin: Option<LocationSnapshot>,
    ) {
        if self.pending_global_search_accept.is_some() {
            return;
        }
        self.cancel_preview_work_for_other_path(&hit.path);
        let generation = (self.preview_load.loading
            && self
                .preview_in_flight_path
                .as_ref()
                .is_some_and(|path| path == &hit.path))
        .then_some(self.preview_load.generation);
        self.pending_global_search_accept = Some(PendingGlobalSearchAccept {
            hit: hit.clone(),
            origin,
            generation,
        });
        if generation.is_none() {
            self.preview_schedule = Some((hit.path, Instant::now()));
            self.prefetch_schedule = None;
        }
    }

    pub fn global_search_hit_accepting_generation(&self, generation: u64) -> bool {
        self.pending_global_search_accept
            .as_ref()
            .is_some_and(|pending| pending.generation == Some(generation))
    }

    pub fn complete_global_search_hit_accept(
        &mut self,
        generation: u64,
        preview_view_h: usize,
    ) -> Option<TabChangeOutcome> {
        let pending = self.take_global_search_hit_accept(generation)?;
        if let Some(origin) = pending.origin {
            self.push_location_history(origin);
        }
        self.close_global_search();
        let outcome = self.set_active_tab(AppTab::Files);
        self.file_tree.reveal(&pending.hit.path);
        self.refresh_file_tree_with_target(Some(pending.hit.path.clone()));
        self.set_preview_highlight_persistent(
            pending.hit.path,
            pending.hit.line,
            pending.hit.byte_range,
        );
        self.center_preview_on_line(pending.hit.line, preview_view_h);
        Some(outcome)
    }

    pub fn reject_global_search_hit_accept(
        &mut self,
        generation: u64,
        error: Option<String>,
    ) -> bool {
        let Some(pending) = self.take_global_search_hit_accept(generation) else {
            return false;
        };
        let missing = error.is_none();
        if let Some(error) = error {
            self.preview_load.complete_terminal_err(generation, error);
        } else {
            self.preview_load.complete_ok(generation);
        }
        self.preview_enrichment_pending = None;
        self.preview_in_flight_path = None;
        if missing {
            self.global_search
                .results
                .retain(|hit| hit.path != pending.hit.path);
            self.clamp_global_search_selection_to_results();
            if self
                .preview_highlight
                .as_ref()
                .is_some_and(|highlight| highlight.path == pending.hit.path)
            {
                self.preview_highlight = None;
            }
        }
        true
    }

    pub fn reset_global_search_selection(&mut self) {
        self.global_search.core.selected_idx = 0;
    }

    pub fn move_global_search_selection(&mut self, delta: i32) {
        self.cancel_global_search_hit_accept();
        crate::features::global_search::move_selection(&mut self.global_search, delta);
    }

    pub fn apply_global_search_picker_input(
        &mut self,
        input: crate::PickerInput,
        now: Instant,
    ) -> crate::PickerInputOutcome {
        let visible = self.global_search.results.len();
        let outcome = crate::features::picker::apply_picker_input(
            &mut self.global_search.core,
            input,
            visible,
        );
        if outcome == crate::PickerInputOutcome::Edited {
            self.mark_global_search_query_edited(now);
        }
        outcome
    }

    pub fn toggle_global_search_match_excluded(&mut self, idx: usize) {
        self.global_search.toggle_match_excluded(idx);
    }

    pub fn clamp_global_search_selection_to_results(&mut self) {
        let len = self.global_search.results.len();
        if self.global_search.core.selected_idx >= len {
            self.global_search.core.selected_idx = len.saturating_sub(1);
        }
    }

    pub fn schedule_global_search_preview_sync(&mut self, now: Instant) {
        self.global_search.preview_sync_at = Some(now + GLOBAL_SEARCH_PREVIEW_SYNC_DEBOUNCE);
    }

    pub fn clear_global_search_preview_sync(&mut self) {
        self.global_search.preview_sync_at = None;
    }

    pub fn consume_global_search_preview_sync_due(&mut self, now: Instant) -> bool {
        let Some(t) = self.global_search.preview_sync_at else {
            return false;
        };
        if now < t {
            return false;
        }
        self.global_search.preview_sync_at = None;
        true
    }

    pub fn reload_global_search(&mut self, now: Instant) {
        if self.global_search.core.filter.is_empty() {
            return;
        }
        self.global_search.last_searched_query.clear();
        self.global_search.last_keystroke_at = Some(now);
    }

    pub fn mark_global_search_query_edited(&mut self, now: Instant) {
        self.cancel_global_search_hit_accept();
        crate::features::global_search::mark_query_edited_at(&mut self.global_search, now);
    }

    pub fn set_global_search_query(&mut self, query: String, now: Instant) {
        if self.global_search.core.filter == query {
            self.global_search.core.cursor = query.len();
            return;
        }

        self.global_search.core.filter = query;
        self.global_search.core.cursor = self.global_search.core.filter.len();
        self.reset_global_search_selection();
        self.mark_global_search_query_edited(now);
    }

    pub fn set_global_search_replacement(&mut self, replacement: String) {
        self.global_search.replace_text = replacement;
        self.global_search.replace_cursor = self.global_search.replace_text.len();
    }

    pub fn edit_global_search_find_input(
        &mut self,
        op: crate::TextEditOp,
        now: Instant,
    ) -> crate::TextEditOutcome {
        let outcome = crate::text_input::apply_single_line_op(
            op,
            &mut self.global_search.core.filter,
            &mut self.global_search.core.cursor,
        );
        if outcome == crate::TextEditOutcome::Edited {
            self.reset_global_search_selection();
            self.mark_global_search_query_edited(now);
        }
        outcome
    }

    pub fn edit_global_search_replace_input(
        &mut self,
        op: crate::TextEditOp,
    ) -> crate::TextEditOutcome {
        crate::text_input::apply_single_line_op(
            op,
            &mut self.global_search.replace_text,
            &mut self.global_search.replace_cursor,
        )
    }

    pub fn paste_global_search_overlay(&mut self, s: &str, now: Instant) {
        if crate::text_input::paste_single_line(
            s,
            &mut self.global_search.core.filter,
            &mut self.global_search.core.cursor,
        ) {
            self.mark_global_search_query_edited(now);
        }
    }

    pub fn paste_global_search_tab(&mut self, s: &str, now: Instant) {
        match self.global_search.focus {
            SearchPanelFocus::FindInput => {
                if crate::text_input::paste_single_line(
                    s,
                    &mut self.global_search.core.filter,
                    &mut self.global_search.core.cursor,
                ) {
                    self.mark_global_search_query_edited(now);
                }
            }
            SearchPanelFocus::ReplaceInput => {
                let _ = crate::text_input::paste_single_line(
                    s,
                    &mut self.global_search.replace_text,
                    &mut self.global_search.replace_cursor,
                );
            }
            SearchPanelFocus::List => {}
        }
    }

    pub fn set_global_search_focus(&mut self, focus: SearchPanelFocus) {
        self.global_search.focus = focus;
    }

    pub fn set_global_search_replace_open(&mut self, open: bool) {
        self.global_search.replace_open = open;
        if !open && matches!(self.global_search.focus, SearchPanelFocus::ReplaceInput) {
            self.global_search.focus = SearchPanelFocus::List;
        }
    }

    pub fn toggle_global_search_replace_for_search_tab(&mut self) {
        self.global_search.replace_open = !self.global_search.replace_open;
        if !self.global_search.replace_open
            && matches!(self.global_search.focus, SearchPanelFocus::ReplaceInput)
        {
            self.global_search.focus = SearchPanelFocus::FindInput;
        }
    }

    pub fn toggle_global_search_replace(&mut self) {
        self.set_global_search_replace_open(!self.global_search.replace_open);
    }

    pub fn cycle_global_search_focus_forward(&mut self) {
        self.global_search.cycle_focus_forward();
    }

    pub fn cycle_global_search_focus_backward(&mut self) {
        self.global_search.cycle_focus_backward();
    }

    pub fn maybe_kick_global_search(&mut self, now: Instant) {
        let Some(t) = self.global_search.last_keystroke_at else {
            return;
        };
        if now.duration_since(t) < GLOBAL_SEARCH_DEBOUNCE {
            return;
        }
        if self.global_search.core.filter == self.global_search.last_searched_query {
            self.global_search.last_keystroke_at = None;
            return;
        }

        self.global_search.cancel.store(true, Ordering::Relaxed);
        let new_cancel = Arc::new(AtomicBool::new(false));
        self.global_search.cancel = new_cancel.clone();

        self.global_search.results.clear();
        self.global_search.truncated = false;
        self.global_search.core.selected_idx = 0;
        self.global_search.scroll = 0;
        self.global_search.results_h_scroll = 0;
        self.global_search.last_searched_query = self.global_search.core.filter.clone();
        self.global_search.last_keystroke_at = None;

        if self.global_search.core.filter.is_empty() {
            let generation = self.global_search_load.begin();
            self.global_search_load.complete_ok(generation);
            self.preview_highlight = None;
            return;
        }

        let generation = self.global_search_load.begin();
        self.tasks.search_all(
            generation,
            new_cancel,
            Arc::clone(&self.backend),
            self.global_search.core.filter.clone(),
        );
    }

    pub fn commit_replace_in_files(&mut self) {
        if !self.global_search.replace_open || self.replace_load.loading {
            return;
        }
        if self.global_search.results.is_empty() || self.global_search.included_count() == 0 {
            return;
        }

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
        let generation = self.replace_load.begin();
        self.tasks.replace_in_files(
            generation,
            self.backend.clone(),
            self.global_search.core.filter.clone(),
            self.global_search.replace_text.clone(),
            items,
        );
    }

    pub fn open_global_search(&mut self, seed: Option<String>) {
        if let Some(seed) = seed
            && seed != self.global_search.core.filter
        {
            self.global_search.core.filter = seed;
            crate::features::global_search::mark_query_edited_at(
                &mut self.global_search,
                Instant::now(),
            );
        }
        self.global_search.core.cursor = self.global_search.core.filter.len();
        self.global_search.core.active = true;
    }

    pub fn close_global_search(&mut self) {
        self.cancel_global_search_hit_accept();
        self.global_search.core.active = false;
    }

    pub fn pin_global_search_to_tab(&mut self) -> TabChangeOutcome {
        self.close_global_search();
        self.global_search.focus = SearchPanelFocus::FindInput;
        self.set_active_tab(AppTab::Search)
    }

    pub(crate) fn bind_global_search_hit_accept_to_preview_generation(
        &mut self,
        path: &Path,
        generation: u64,
    ) {
        if let Some(pending) = self.pending_global_search_accept.as_mut()
            && pending.hit.path == path
        {
            pending.generation = Some(generation);
        }
    }

    fn take_global_search_hit_accept(
        &mut self,
        generation: u64,
    ) -> Option<PendingGlobalSearchAccept> {
        if !self.global_search_hit_accepting_generation(generation) {
            return None;
        }
        self.pending_global_search_accept.take()
    }

    fn cancel_global_search_hit_accept(&mut self) {
        let Some(pending) = self.pending_global_search_accept.take() else {
            return;
        };
        if self
            .preview_schedule
            .as_ref()
            .is_some_and(|(path, _)| path == &pending.hit.path)
        {
            self.preview_schedule = None;
        }
        if self
            .preview_in_flight_path
            .as_ref()
            .is_some_and(|path| path == &pending.hit.path)
        {
            self.preview_load.invalidate();
            self.preview_in_flight_path = None;
            self.preview_enrichment_pending = None;
        }
    }
}
