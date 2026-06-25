use super::*;

impl AppState {
    pub fn search_snapshot(&self) -> crate::features::search::SearchSnapshot {
        crate::features::search::SearchSnapshot {
            preview_scroll: self.preview_scroll,
            preview_h_scroll: self.preview_h_scroll,
            diff_scroll: self.diff_scroll,
            diff_h_scroll: self.diff_h_scroll,
            commit_detail_scroll: self.commit_detail.scroll,
            graph_diff_scroll: self.commit_detail.file_diff_scroll,
            file_tree_selected: self.file_tree.selected,
            tree_scroll: self.tree_scroll,
            git_status_scroll: self.git_status.scroll,
            git_status_selected_file: self.selected_file.clone(),
            git_graph_selected_idx: self.git_graph.selected_idx,
            git_graph_scroll: self.git_graph.scroll,
        }
    }

    pub fn begin_vim_search(
        &mut self,
        target: crate::features::search::SearchTarget,
        backwards: bool,
    ) {
        let snapshot = self.search_snapshot();
        self.search = crate::features::search::SearchState {
            active: true,
            backwards,
            query: String::new(),
            cursor: 0,
            target: Some(target),
            matches: Vec::new(),
            current: None,
            snapshot: Some(snapshot),
            wrap_msg: None,
            row_index: Default::default(),
        };
    }

    pub fn confirm_vim_search(&mut self) {
        self.search.active = false;
        self.search.wrap_msg = None;
        if self.search.query.is_empty() || self.search.matches.is_empty() {
            self.search.clear();
        }
    }

    pub fn cancel_vim_search(&mut self, dark: bool) {
        if let Some(snap) = self.search.snapshot.clone() {
            self.restore_search_snapshot(&snap, dark);
        }
        self.search.clear();
    }

    pub fn edit_vim_search_input(&mut self, op: crate::TextEditOp) -> crate::TextEditOutcome {
        let outcome = crate::text_input::apply_single_line_op(
            op,
            &mut self.search.query,
            &mut self.search.cursor,
        );
        if outcome == crate::TextEditOutcome::Edited {
            self.search.wrap_msg = None;
        }
        outcome
    }

    pub fn paste_vim_search_input(&mut self, text: &str) -> bool {
        let edited = crate::text_input::paste_single_line(
            text,
            &mut self.search.query,
            &mut self.search.cursor,
        );
        if edited {
            self.search.wrap_msg = None;
        }
        edited
    }

    pub fn restore_search_snapshot(
        &mut self,
        snap: &crate::features::search::SearchSnapshot,
        dark: bool,
    ) {
        self.preview_scroll = snap.preview_scroll;
        self.preview_h_scroll = snap.preview_h_scroll;
        self.diff_scroll = snap.diff_scroll;
        self.diff_h_scroll = snap.diff_h_scroll;
        self.commit_detail.scroll = snap.commit_detail_scroll;
        self.commit_detail.file_diff_scroll = snap.graph_diff_scroll;
        self.file_tree.selected = snap.file_tree_selected;
        self.tree_scroll = snap.tree_scroll;
        self.git_status.scroll = snap.git_status_scroll;
        if self.selected_file != snap.git_status_selected_file {
            self.selected_file = snap.git_status_selected_file.clone();
            self.diff_scroll = snap.diff_scroll;
            self.diff_h_scroll = snap.diff_h_scroll;
            self.load_diff(dark);
        }
        if self.git_graph.selected_idx != snap.git_graph_selected_idx {
            self.git_graph.selected_idx = snap.git_graph_selected_idx;
            self.git_graph.selected_commit = self
                .git_graph
                .rows
                .get(snap.git_graph_selected_idx)
                .map(|r| r.commit.oid.clone());
            self.git_graph.selection_anchor = None;
            self.commit_detail.range_detail = None;
            self.commit_detail.scroll = snap.commit_detail_scroll;
            self.load_commit_detail();
        }
        self.git_graph.scroll = snap.git_graph_scroll;
    }

    pub fn jump_to_search_match(
        &mut self,
        target: crate::features::search::SearchTarget,
        row: usize,
        dark: bool,
        preview_view_h: usize,
        diff_view_h: usize,
        commit_detail_view_h: usize,
    ) {
        use crate::features::search::SearchTarget;
        match target {
            SearchTarget::FileTree => {
                if row < self.file_tree.entries.len() {
                    self.file_tree.selected = row;
                    self.load_preview();
                }
            }
            SearchTarget::GitStatus => {
                let staged_len = self.staged_files.len();
                if row < staged_len {
                    let file = self.staged_files[row].clone();
                    self.select_file(file.path, true, dark);
                } else {
                    let idx = row - staged_len;
                    if let Some(file) = self.unstaged_files.get(idx).cloned() {
                        self.select_file(file.path, false, dark);
                    }
                }
            }
            SearchTarget::CommitGraph => {
                if row < self.git_graph.rows.len() {
                    self.git_graph.selected_idx = row;
                    self.git_graph.selected_commit =
                        self.git_graph.rows.get(row).map(|r| r.commit.oid.clone());
                    self.git_graph.selection_anchor = None;
                    self.commit_detail.range_detail = None;
                    self.commit_detail.scroll = 0;
                    self.load_commit_detail();
                }
            }
            SearchTarget::FilePreview => {
                self.preview_scroll = center_scroll(row, preview_view_h);
            }
            SearchTarget::Diff => {
                self.diff_scroll = center_scroll(row, diff_view_h);
            }
            SearchTarget::GraphDiff => {
                self.commit_detail.file_diff_scroll = center_scroll(row, diff_view_h);
            }
            SearchTarget::CommitDetail => {
                self.commit_detail.scroll = center_scroll(row, commit_detail_view_h);
            }
        }
    }

    pub fn recompute_vim_search<I, S>(
        &mut self,
        rows: I,
        dark: bool,
        viewport: crate::features::search::SearchViewport,
    ) where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let Some(target) = self.search.target else {
            return;
        };
        let query = self.search.query.clone();
        if query.is_empty() {
            self.search.clear_matches();
            if let Some(snap) = self.search.snapshot.clone() {
                self.restore_search_snapshot(&snap, dark);
            }
            return;
        }

        let case_insensitive = reef_core::search::smart_case_insensitive(&query);
        let mut matches = Vec::new();
        for (idx, text) in rows.into_iter().enumerate() {
            for byte_range in
                reef_core::search::find_literal_all(text.as_ref(), &query, case_insensitive)
            {
                matches.push(crate::features::search::MatchLoc {
                    row: idx,
                    byte_range,
                });
            }
        }

        self.search.set_matches(matches);
        self.search.current = self.pick_current_vim_search(target);
        self.jump_to_current_vim_search(dark, viewport);
    }

    pub fn step_vim_search(
        &mut self,
        reverse: bool,
        dark: bool,
        viewport: crate::features::search::SearchViewport,
    ) {
        if self.search.matches.is_empty() {
            return;
        }
        let n = self.search.matches.len();
        let go_back = self.search.backwards ^ reverse;
        let cur = self.search.current.unwrap_or(0);
        let (next, wrapped) = if go_back {
            if cur == 0 {
                (n - 1, true)
            } else {
                (cur - 1, false)
            }
        } else if cur + 1 >= n {
            (0, true)
        } else {
            (cur + 1, false)
        };
        self.search.current = Some(next);
        self.search.wrap_msg = if wrapped {
            Some(if go_back {
                crate::features::search::WrapMsg::Top
            } else {
                crate::features::search::WrapMsg::Bottom
            })
        } else {
            None
        };
        self.jump_to_current_vim_search(dark, viewport);
    }

    fn pick_current_vim_search(
        &self,
        target: crate::features::search::SearchTarget,
    ) -> Option<usize> {
        if self.search.matches.is_empty() {
            return None;
        }
        let snap = self.search.snapshot.clone().unwrap_or_default();
        let baseline_row = crate::features::search::baseline_row(target, &snap);
        if self.search.backwards {
            let mut chosen: Option<usize> = None;
            for (idx, search_match) in self.search.matches.iter().enumerate() {
                if search_match.row <= baseline_row {
                    chosen = Some(idx);
                } else {
                    break;
                }
            }
            chosen.or_else(|| Some(self.search.matches.len() - 1))
        } else {
            self.search
                .matches
                .iter()
                .position(|search_match| search_match.row >= baseline_row)
                .or(Some(0))
        }
    }

    fn jump_to_current_vim_search(
        &mut self,
        dark: bool,
        viewport: crate::features::search::SearchViewport,
    ) {
        let Some(current) = self.search.current else {
            return;
        };
        let Some(search_match) = self.search.matches.get(current).cloned() else {
            return;
        };
        let Some(target) = self.search.target else {
            return;
        };
        self.jump_to_search_match(
            target,
            search_match.row,
            dark,
            viewport.preview_view_h,
            viewport.diff_view_h,
            viewport.commit_detail_view_h,
        );
    }

    pub fn clear_vim_search(&mut self) {
        self.search.clear();
    }

    pub fn clear_vim_search_if_target(&mut self, target: crate::features::search::SearchTarget) {
        if self.search.target == Some(target) {
            self.search.clear();
        }
    }

    pub fn begin_find_widget(
        &mut self,
        target: crate::features::find_widget::FindTarget,
        query: String,
    ) {
        self.search.clear();
        let snapshot = self.search_snapshot();
        let match_case = self.find_widget.match_case;
        let whole_word = self.find_widget.whole_word;
        let regex = self.find_widget.regex;
        let cursor = query.len();
        self.find_widget = crate::features::find_widget::FindWidgetState {
            active: true,
            query,
            cursor,
            match_case,
            whole_word,
            regex,
            target: Some(target),
            snapshot: Some(snapshot),
            ..Default::default()
        };
    }

    pub fn close_find_widget(&mut self, dark: bool) {
        if let Some(snap) = self.find_widget.snapshot.clone() {
            self.restore_search_snapshot(&snap, dark);
        }
        self.find_widget.clear();
    }

    pub fn edit_find_widget_input(&mut self, op: crate::TextEditOp) -> crate::TextEditOutcome {
        crate::text_input::apply_single_line_op(
            op,
            &mut self.find_widget.query,
            &mut self.find_widget.cursor,
        )
    }

    pub fn paste_find_widget_input(&mut self, text: &str) -> bool {
        crate::text_input::paste_single_line(
            text,
            &mut self.find_widget.query,
            &mut self.find_widget.cursor,
        )
    }

    pub fn toggle_find_widget_option(
        &mut self,
        option: crate::features::find_widget::FindWidgetToggle,
    ) {
        match option {
            crate::features::find_widget::FindWidgetToggle::MatchCase => {
                self.find_widget.match_case = !self.find_widget.match_case;
            }
            crate::features::find_widget::FindWidgetToggle::WholeWord => {
                self.find_widget.whole_word = !self.find_widget.whole_word;
            }
            crate::features::find_widget::FindWidgetToggle::Regex => {
                self.find_widget.regex = !self.find_widget.regex;
            }
        }
    }

    pub fn recompute_find_widget<I, S>(
        &mut self,
        rows: I,
        dark: bool,
        viewport: crate::features::search::SearchViewport,
    ) where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let Some(target) = self.find_widget.target else {
            return;
        };
        let query = self.find_widget.query.clone();
        if query.is_empty() {
            self.find_widget.clear_matches();
            if let Some(snap) = self.find_widget.snapshot.clone() {
                self.restore_search_snapshot(&snap, dark);
            }
            return;
        }

        let options = reef_core::search::SearchOptions {
            case_sensitive: self.find_widget.match_case,
            whole_word: self.find_widget.whole_word,
            regex: self.find_widget.regex,
        };
        let mut matches = Vec::new();
        let mut regex_error = None;
        for (idx, text) in rows.into_iter().enumerate() {
            match reef_core::search::find_all_with_options(text.as_ref(), &query, &options) {
                Ok(ranges) => {
                    for byte_range in ranges {
                        matches.push(crate::features::search::MatchLoc {
                            row: idx,
                            byte_range,
                        });
                    }
                }
                Err(err) => {
                    regex_error = Some(err.to_string());
                    matches.clear();
                    break;
                }
            }
        }

        self.find_widget.set_matches(matches);
        self.find_widget.regex_error = regex_error;
        self.find_widget.current = self.pick_current_find_widget(target);
        self.jump_to_current_find_widget(viewport);
    }

    pub fn step_find_widget(
        &mut self,
        reverse: bool,
        viewport: crate::features::search::SearchViewport,
    ) {
        if self.find_widget.matches.is_empty() {
            return;
        }
        let len = self.find_widget.matches.len();
        let current = self.find_widget.current.unwrap_or(0);
        let next = if reverse {
            if current == 0 { len - 1 } else { current - 1 }
        } else if current + 1 >= len {
            0
        } else {
            current + 1
        };
        self.find_widget.current = Some(next);
        self.jump_to_current_find_widget(viewport);
    }

    fn pick_current_find_widget(
        &self,
        target: crate::features::find_widget::FindTarget,
    ) -> Option<usize> {
        if self.find_widget.matches.is_empty() {
            return None;
        }
        let snap = self.find_widget.snapshot.clone().unwrap_or_default();
        let baseline = crate::features::find_widget::baseline_row(target, &snap);
        self.find_widget
            .matches
            .iter()
            .position(|search_match| search_match.row >= baseline)
            .or(Some(0))
    }

    fn jump_to_current_find_widget(&mut self, viewport: crate::features::search::SearchViewport) {
        let Some(current) = self.find_widget.current else {
            return;
        };
        let Some(search_match) = self.find_widget.matches.get(current).cloned() else {
            return;
        };
        let Some(target) = self.find_widget.target else {
            return;
        };
        match target {
            crate::features::find_widget::FindTarget::FilePreview => {
                self.preview_scroll = center_scroll(search_match.row, viewport.preview_view_h);
            }
            crate::features::find_widget::FindTarget::DiffUnified
            | crate::features::find_widget::FindTarget::DiffSbsLeft
            | crate::features::find_widget::FindTarget::DiffSbsRight => {
                self.diff_scroll = center_scroll(search_match.row, viewport.diff_view_h);
            }
            crate::features::find_widget::FindTarget::GraphDiffUnified
            | crate::features::find_widget::FindTarget::GraphDiffSbsLeft
            | crate::features::find_widget::FindTarget::GraphDiffSbsRight => {
                self.commit_detail.file_diff_scroll =
                    center_scroll(search_match.row, viewport.diff_view_h);
            }
        }
    }
}
