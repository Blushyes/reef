use super::*;

impl AppState {
    pub fn scroll_preview_vertical(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.preview_scroll, delta);
    }

    pub fn set_preview_vertical_scroll(&mut self, value: usize) {
        self.preview_scroll = value;
    }

    pub fn clamp_preview_vertical_scroll(&mut self, max_scroll: usize) -> usize {
        self.preview_scroll = self.preview_scroll.min(max_scroll);
        self.preview_scroll
    }

    pub fn reset_preview_scroll(&mut self, reset_horizontal: bool) {
        self.preview_scroll = 0;
        if reset_horizontal {
            self.preview_h_scroll = 0;
        }
    }

    pub fn scroll_preview_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.preview_h_scroll, delta);
    }

    pub fn set_preview_horizontal_scroll(&mut self, value: usize) {
        self.preview_h_scroll = value;
    }

    pub fn clamp_preview_horizontal_scroll(&mut self, max_scroll: usize) -> usize {
        self.preview_h_scroll = self.preview_h_scroll.min(max_scroll);
        self.preview_h_scroll
    }

    pub fn scroll_diff_vertical(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.diff_scroll, delta);
    }

    pub fn set_diff_vertical_scroll(&mut self, value: usize) {
        self.diff_scroll = value;
    }

    pub fn clamp_diff_vertical_scroll(&mut self, max_scroll: usize) -> usize {
        self.diff_scroll = self.diff_scroll.min(max_scroll);
        self.diff_scroll
    }

    pub fn scroll_diff_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.diff_h_scroll, delta);
        apply_scroll_delta(&mut self.sbs_left_h_scroll, delta);
        apply_scroll_delta(&mut self.sbs_right_h_scroll, delta);
    }

    pub fn set_diff_horizontal_scroll(&mut self, value: usize) {
        self.diff_h_scroll = value;
        self.sbs_left_h_scroll = value;
        self.sbs_right_h_scroll = value;
    }

    pub fn clamp_diff_horizontal_scroll(&mut self, max_scroll: usize) -> usize {
        self.diff_h_scroll = self.diff_h_scroll.min(max_scroll);
        self.diff_h_scroll
    }

    pub fn diff_scroll_state(&self) -> (usize, usize, usize, usize) {
        (
            self.diff_scroll,
            self.diff_h_scroll,
            self.sbs_left_h_scroll,
            self.sbs_right_h_scroll,
        )
    }

    pub fn set_diff_scroll_state(
        &mut self,
        scroll: usize,
        h_scroll: usize,
        sbs_left_h_scroll: usize,
        sbs_right_h_scroll: usize,
    ) {
        self.diff_scroll = scroll;
        self.diff_h_scroll = h_scroll;
        self.sbs_left_h_scroll = sbs_left_h_scroll;
        self.sbs_right_h_scroll = sbs_right_h_scroll;
    }

    pub fn scroll_commit_detail_file_diff_vertical(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.file_diff_scroll, delta);
    }

    pub fn set_commit_detail_file_diff_vertical_scroll(&mut self, value: usize) {
        self.commit_detail.file_diff_scroll = value;
    }

    pub fn clamp_commit_detail_file_diff_vertical_scroll(&mut self, max_scroll: usize) -> usize {
        self.commit_detail.file_diff_scroll = self.commit_detail.file_diff_scroll.min(max_scroll);
        self.commit_detail.file_diff_scroll
    }

    pub fn commit_file_diff_scroll_state(&self) -> (usize, usize, usize, usize) {
        (
            self.commit_detail.file_diff_scroll,
            self.commit_detail.file_diff_h_scroll,
            self.commit_detail.file_diff_sbs_left_h_scroll,
            self.commit_detail.file_diff_sbs_right_h_scroll,
        )
    }

    pub fn set_commit_file_diff_scroll_state(
        &mut self,
        scroll: usize,
        h_scroll: usize,
        sbs_left_h_scroll: usize,
        sbs_right_h_scroll: usize,
    ) {
        self.commit_detail.file_diff_scroll = scroll;
        self.commit_detail.file_diff_h_scroll = h_scroll;
        self.commit_detail.file_diff_sbs_left_h_scroll = sbs_left_h_scroll;
        self.commit_detail.file_diff_sbs_right_h_scroll = sbs_right_h_scroll;
    }

    pub fn active_diff_vertical_scroll(&self) -> Option<usize> {
        match self.active_tab {
            AppTab::Git => Some(self.diff_scroll),
            AppTab::Graph => Some(self.commit_detail.file_diff_scroll),
            _ => None,
        }
    }

    pub fn set_active_diff_vertical_scroll(&mut self, value: usize) -> bool {
        match self.active_tab {
            AppTab::Git => {
                self.diff_scroll = value;
                true
            }
            AppTab::Graph => {
                self.commit_detail.file_diff_scroll = value;
                true
            }
            _ => false,
        }
    }

    pub fn scroll_commit_detail_vertical(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.scroll, delta);
    }

    pub fn set_commit_detail_vertical_scroll(&mut self, value: usize) {
        self.commit_detail.scroll = value;
    }

    pub fn clamp_commit_detail_vertical_scroll(&mut self, max_scroll: usize) {
        self.commit_detail.scroll = self.commit_detail.scroll.min(max_scroll);
    }

    pub fn scroll_tree_vertical(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.tree_scroll, delta);
    }

    pub fn clamp_git_status_scroll(&mut self, max_scroll: usize) -> usize {
        self.git_status.scroll = self.git_status.scroll.min(max_scroll);
        self.git_status.scroll
    }

    pub fn scroll_git_status_vertical(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.git_status.scroll, delta);
    }

    pub fn scroll_commit_detail_file_diff_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.file_diff_h_scroll, delta);
    }

    pub fn scroll_commit_detail_file_diff_sbs_left_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.file_diff_sbs_left_h_scroll, delta);
    }

    pub fn scroll_commit_detail_file_diff_sbs_right_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.file_diff_sbs_right_h_scroll, delta);
    }

    pub fn scroll_commit_detail_diff_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.diff_h_scroll, delta);
    }

    pub fn clamp_commit_detail_diff_horizontal_scroll(&mut self, max_scroll: usize) {
        self.commit_detail.diff_h_scroll = self.commit_detail.diff_h_scroll.min(max_scroll);
    }

    pub fn scroll_commit_detail_sbs_left_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.sbs_left_h_scroll, delta);
    }

    pub fn scroll_commit_detail_sbs_right_horizontal(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.commit_detail.sbs_right_h_scroll, delta);
    }

    pub fn clamp_commit_detail_sbs_horizontal_scrolls(
        &mut self,
        max_left: usize,
        max_right: usize,
    ) {
        self.commit_detail.sbs_left_h_scroll = self.commit_detail.sbs_left_h_scroll.min(max_left);
        self.commit_detail.sbs_right_h_scroll =
            self.commit_detail.sbs_right_h_scroll.min(max_right);
    }

    pub fn scroll_horizontal_at_column(&mut self, column: u16, total_width: u16, delta: i32) {
        if self.view_mode == ViewMode::FocusedPreview {
            self.scroll_focused_preview_horizontal_at_column(column, total_width, delta);
            return;
        }

        let split_x = self.graph_sidebar_width(total_width);
        let is_left = column < split_x;
        match (self.active_tab, is_left) {
            (AppTab::Search, true) => self.scroll_global_search_results_horizontal(delta),
            (_, true) => {}
            (AppTab::Files, false) | (AppTab::Search, false) => {
                self.scroll_preview_horizontal(delta);
            }
            (AppTab::Git, false) => match self.diff_layout {
                DiffLayout::Unified => apply_scroll_delta(&mut self.diff_h_scroll, delta),
                DiffLayout::SideBySide => {
                    let panel_w = total_width.saturating_sub(split_x);
                    if sbs_cursor_on_left(split_x, panel_w, column) {
                        apply_scroll_delta(&mut self.sbs_left_h_scroll, delta);
                    } else {
                        apply_scroll_delta(&mut self.sbs_right_h_scroll, delta);
                    }
                }
            },
            (AppTab::Graph, false) => {
                let diff_start = self
                    .graph_diff_column_start(total_width)
                    .unwrap_or(total_width);
                let in_diff_column = diff_start < total_width && column >= diff_start;
                match self.commit_detail.diff_layout {
                    DiffLayout::Unified => {
                        if in_diff_column {
                            self.scroll_commit_detail_file_diff_horizontal(delta);
                        } else {
                            self.scroll_commit_detail_diff_horizontal(delta);
                        }
                    }
                    DiffLayout::SideBySide => {
                        let (panel_start, panel_w, use_file_diff) = if in_diff_column {
                            (diff_start, total_width.saturating_sub(diff_start), true)
                        } else {
                            (split_x, diff_start.saturating_sub(split_x), false)
                        };
                        let left = sbs_cursor_on_left(panel_start, panel_w, column);
                        match (use_file_diff, left) {
                            (true, true) => {
                                self.scroll_commit_detail_file_diff_sbs_left_horizontal(delta);
                            }
                            (true, false) => {
                                self.scroll_commit_detail_file_diff_sbs_right_horizontal(delta);
                            }
                            (false, true) => self.scroll_commit_detail_sbs_left_horizontal(delta),
                            (false, false) => {
                                self.scroll_commit_detail_sbs_right_horizontal(delta);
                            }
                        }
                    }
                }
            }
        }
    }

    fn scroll_focused_preview_horizontal_at_column(
        &mut self,
        column: u16,
        total_width: u16,
        delta: i32,
    ) {
        match self.active_tab {
            AppTab::Files | AppTab::Search => self.scroll_preview_horizontal(delta),
            AppTab::Git => match self.diff_layout {
                DiffLayout::Unified => apply_scroll_delta(&mut self.diff_h_scroll, delta),
                DiffLayout::SideBySide => {
                    if sbs_cursor_on_left(0, total_width, column) {
                        apply_scroll_delta(&mut self.sbs_left_h_scroll, delta);
                    } else {
                        apply_scroll_delta(&mut self.sbs_right_h_scroll, delta);
                    }
                }
            },
            AppTab::Graph => {
                if self.graph_uses_three_col_for_width(total_width) {
                    match self.commit_detail.diff_layout {
                        DiffLayout::Unified => {
                            self.scroll_commit_detail_file_diff_horizontal(delta)
                        }
                        DiffLayout::SideBySide => {
                            if sbs_cursor_on_left(0, total_width, column) {
                                self.scroll_commit_detail_file_diff_sbs_left_horizontal(delta);
                            } else {
                                self.scroll_commit_detail_file_diff_sbs_right_horizontal(delta);
                            }
                        }
                    }
                } else {
                    match self.commit_detail.diff_layout {
                        DiffLayout::Unified => self.scroll_commit_detail_diff_horizontal(delta),
                        DiffLayout::SideBySide => {
                            if sbs_cursor_on_left(0, total_width, column) {
                                self.scroll_commit_detail_sbs_left_horizontal(delta);
                            } else {
                                self.scroll_commit_detail_sbs_right_horizontal(delta);
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn reconcile_graph_scroll(
        &mut self,
        visible_rows: usize,
        selection_changed: bool,
    ) -> usize {
        let total = self.git_graph.rows.len();
        let max_scroll = total.saturating_sub(visible_rows);
        self.git_graph.scroll = self.git_graph.scroll.min(max_scroll);
        if selection_changed {
            let sel = self.git_graph.selected_idx;
            if sel < self.git_graph.scroll {
                self.git_graph.scroll = sel;
            } else if visible_rows > 0 && sel >= self.git_graph.scroll + visible_rows {
                self.git_graph.scroll = sel + 1 - visible_rows;
            }
            self.git_graph.scroll = self.git_graph.scroll.min(max_scroll);
        }
        self.git_graph.scroll
    }

    pub fn scroll_graph_vertical(&mut self, delta: i32) {
        apply_scroll_delta(&mut self.git_graph.scroll, delta);
    }
}
