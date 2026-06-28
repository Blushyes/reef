use super::*;

impl AppState {
    pub fn rebuild_quick_open_index(&mut self, index: Vec<crate::features::quick_open::Candidate>) {
        self.quick_open.index = index;
        self.quick_open.index_stale = false;
        crate::features::quick_open::filter(&mut self.quick_open);
    }

    fn request_quick_open_index_if_needed(&mut self) {
        if self.quick_open_load.loading {
            return;
        }
        if !self.quick_open.index_stale && !self.quick_open.index.is_empty() {
            return;
        }
        let generation = self.quick_open_load.begin();
        self.tasks
            .build_quick_open_index(generation, Arc::clone(&self.backend));
    }

    pub fn filter_quick_open(&mut self) {
        crate::features::quick_open::filter(&mut self.quick_open);
    }

    pub fn apply_quick_open_picker_input(
        &mut self,
        input: crate::PickerInput,
    ) -> crate::PickerInputOutcome {
        let visible = self.quick_open.matches.len();
        let outcome =
            crate::features::picker::apply_picker_input(&mut self.quick_open.core, input, visible);
        if outcome == crate::PickerInputOutcome::Edited {
            self.filter_quick_open();
        }
        outcome
    }

    pub fn paste_quick_open_filter(&mut self, s: &str) {
        if crate::text_input::paste_single_line(
            s,
            &mut self.quick_open.core.filter,
            &mut self.quick_open.core.cursor,
        ) {
            self.filter_quick_open();
        }
    }

    pub fn move_quick_open_selection(&mut self, delta: i32) {
        crate::features::quick_open::move_selection(&mut self.quick_open, delta);
    }

    pub fn select_quick_open_match(&mut self, idx: usize) {
        self.quick_open.core.selected_idx = idx;
    }

    pub fn ensure_quick_open_selection_visible(&mut self, visible_rows: usize) -> usize {
        let sel = self.quick_open.core.selected_idx;
        if sel < self.quick_open.scroll {
            self.quick_open.scroll = sel;
        } else if visible_rows > 0 && sel >= self.quick_open.scroll + visible_rows {
            self.quick_open.scroll = sel + 1 - visible_rows;
        }
        self.quick_open.scroll
    }

    pub fn quick_open_selected_path(&self) -> Option<PathBuf> {
        let m = self
            .quick_open
            .matches
            .get(self.quick_open.core.selected_idx)?;
        self.quick_open
            .index
            .get(m.idx)
            .map(|candidate| candidate.rel_path.clone())
    }

    pub fn bump_quick_open_mru(&mut self, rel: PathBuf, cap: usize) {
        reef_core::quick_open::bump_mru(&mut self.quick_open.mru, rel, cap);
    }

    pub fn accept_quick_open_path(&mut self, rel: PathBuf) -> TabChangeOutcome {
        self.close_quick_open();
        let outcome = self.set_active_tab(AppTab::Files);
        self.file_tree.reveal(&rel);
        self.refresh_file_tree_with_target(Some(rel.clone()));
        self.load_preview_for_path(rel);
        outcome
    }

    pub fn open_quick_open(&mut self) {
        self.quick_open.core.active = true;
        self.quick_open.core.selected_idx = 0;
        self.quick_open.scroll = 0;
        self.quick_open.core.cursor = self.quick_open.core.filter.len();
        self.filter_quick_open();
        self.request_quick_open_index_if_needed();
    }

    pub fn close_quick_open(&mut self) {
        self.quick_open.core.active = false;
    }
}
