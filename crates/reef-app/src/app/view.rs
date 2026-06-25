use super::*;

impl AppState {
    pub fn push_toast(&mut self, toast: Toast) {
        self.toasts.push(toast);
    }

    pub fn open_help(&mut self) {
        self.show_help = true;
    }

    pub fn close_help(&mut self) {
        self.show_help = false;
    }

    pub fn request_tree_delete_confirm(&mut self, path: PathBuf, is_dir: bool, hard: bool) {
        let display_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(reef_core::file_ops::sanitize_filename)
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        self.pending_confirm = Some(ConfirmRequest::TreeDelete(TreeDeleteConfirm {
            path,
            display_name,
            is_dir,
            hard,
        }));
    }

    pub fn request_confirm(&mut self, request: ConfirmRequest) {
        self.pending_confirm = Some(request);
    }

    pub fn dismiss_confirm(&mut self) {
        self.pending_confirm = None;
    }

    pub fn set_active_panel(&mut self, panel: AppPanel) {
        self.active_panel = panel;
    }

    pub fn cycle_active_panel(&mut self, reverse: bool, uses_three_col: bool) {
        self.active_panel = next_panel(self.active_panel, uses_three_col, reverse);
    }

    pub fn open_global_replace_tab(&mut self) -> TabChangeOutcome {
        let outcome = self.set_active_tab(AppTab::Search);
        self.active_panel = AppPanel::Files;
        self.global_search.replace_open = true;
        self.global_search.focus = SearchPanelFocus::ReplaceInput;
        outcome
    }

    pub fn close_active_palettes(&mut self) {
        self.quick_open.core.active = false;
        self.global_search.core.active = false;
    }

    pub fn set_split_percent(&mut self, percent: u16) {
        self.split_percent = percent.clamp(10, 80);
    }

    pub fn set_graph_diff_split_percent(&mut self, percent: u16) {
        self.graph_diff_split_percent = percent.clamp(20, 80);
    }

    pub fn set_ctrl_hover_target(&mut self, target: Option<(usize, Range<usize>)>) {
        self.ctrl_hover_target = target;
    }

    pub fn set_focused_preview_files_selection(&mut self, idx: usize) {
        self.focused_preview_files_selected = idx;
    }

    pub fn graph_sidebar_width(&self, total_width: u16) -> u16 {
        if !self.sidebar_visible {
            return 0;
        }
        compute_sidebar_width(total_width, self.split_percent)
    }

    pub fn graph_three_col_widths(&self, total_width: u16) -> (u16, u16, u16) {
        compute_three_col_widths(
            total_width,
            self.graph_sidebar_width(total_width),
            self.graph_diff_split_percent,
        )
    }

    pub fn graph_diff_column_start(&self, total_width: u16) -> Option<u16> {
        if !self.graph_uses_three_col_for_width(total_width) {
            return None;
        }
        let (_, _, diff_w) = self.graph_three_col_widths(total_width);
        Some(total_width.saturating_sub(diff_w))
    }

    pub fn graph_uses_three_col_for_width(&self, total_width: u16) -> bool {
        compute_uses_three_col(
            self.active_tab,
            total_width,
            self.commit_detail.file_diff.is_some(),
            self.commit_file_diff_load.loading,
        )
    }

    pub fn set_active_tab(&mut self, tab: AppTab) -> TabChangeOutcome {
        if self.active_tab == tab {
            return TabChangeOutcome::default();
        }
        let was_files = self.active_tab == AppTab::Files;
        self.active_tab = tab;
        let mut outcome = TabChangeOutcome {
            changed: true,
            clear_preview_selection: true,
            clear_commit_detail_selection: true,
            clear_diff_selection: true,
            close_find_widget: true,
            dismiss_confirm: false,
            sync_search_preview: false,
        };
        if was_files {
            self.tree_edit.clear();
            self.tree_context_menu.close();
            outcome.dismiss_confirm = true;
        }
        self.focused_preview_files_open = false;
        self.focused_preview_files_selected = 0;
        match tab {
            AppTab::Git => self.git_status_load.mark_stale(),
            AppTab::Graph => self.graph_load.mark_stale(),
            AppTab::Files => {
                if self.file_tree.entries.is_empty() {
                    self.file_tree_load.mark_stale();
                }
            }
            AppTab::Search => {
                outcome.sync_search_preview = true;
            }
        }
        outcome
    }

    pub fn open_settings(&mut self) -> bool {
        if self.view_mode == ViewMode::Settings {
            return false;
        }
        self.view_mode = ViewMode::Settings;
        true
    }

    pub fn close_settings(&mut self) {
        self.view_mode = ViewMode::Main;
    }

    pub fn enter_focused_preview(&mut self, uses_three_col: bool) {
        if self.view_mode != ViewMode::Main {
            return;
        }
        self.active_panel = match self.active_tab {
            AppTab::Graph if !uses_three_col => AppPanel::Commit,
            _ => AppPanel::Diff,
        };
        self.view_mode = ViewMode::FocusedPreview;
    }

    pub fn close_focused_preview(&mut self) {
        if self.view_mode != ViewMode::FocusedPreview {
            return;
        }
        self.view_mode = ViewMode::Main;
        self.focused_preview_files_open = false;
        self.focused_preview_files_selected = 0;
    }

    pub fn toggle_focused_preview(&mut self, uses_three_col: bool) {
        match self.view_mode {
            ViewMode::Main => self.enter_focused_preview(uses_three_col),
            ViewMode::FocusedPreview => self.close_focused_preview(),
            ViewMode::Settings => {}
        }
    }

    pub fn enter_focused_preview_with_file(
        &mut self,
        rel: PathBuf,
        dark: bool,
        wants_decoded_image: bool,
    ) -> TabChangeOutcome {
        let outcome = self.set_active_tab(AppTab::Files);
        self.file_tree.reveal(&rel);
        self.refresh_file_tree_with_target(Some(rel.clone()));
        self.preview_schedule = None;
        self.prefetch_schedule = None;
        self.dispatch_preview_load(rel, dark, wants_decoded_image);
        self.enter_focused_preview(false);
        outcome
    }

    pub fn focused_preview_chip_visible(&self, uses_three_col: bool) -> bool {
        if !self.backend.has_repo() {
            return false;
        }
        match self.active_tab {
            AppTab::Git => true,
            AppTab::Graph => uses_three_col,
            _ => false,
        }
    }

    pub fn focused_preview_file_entries(&self) -> Vec<FocusedPreviewFileRow> {
        match self.active_tab {
            AppTab::Git => {
                let mut out = Vec::new();
                for file in &self.staged_files {
                    out.push(FocusedPreviewFileRow {
                        path: file.path.clone(),
                        status: file.status.label().chars().next().unwrap_or(' '),
                        source: FocusedPreviewFileSource::GitStaged,
                    });
                }
                for file in &self.unstaged_files {
                    out.push(FocusedPreviewFileRow {
                        path: file.path.clone(),
                        status: file.status.label().chars().next().unwrap_or(' '),
                        source: FocusedPreviewFileSource::GitUnstaged,
                    });
                }
                out.sort_by(|a, b| a.path.cmp(&b.path));
                out
            }
            AppTab::Graph => {
                let Some(detail) = self.commit_detail.detail.as_ref() else {
                    return Vec::new();
                };
                let mut out: Vec<FocusedPreviewFileRow> = detail
                    .files
                    .iter()
                    .map(|file| FocusedPreviewFileRow {
                        path: file.path.clone(),
                        status: file.status.label().chars().next().unwrap_or(' '),
                        source: FocusedPreviewFileSource::GraphCommit,
                    })
                    .collect();
                out.sort_by(|a, b| a.path.cmp(&b.path));
                out
            }
            _ => Vec::new(),
        }
    }

    pub fn open_focused_preview_files(&mut self) {
        let entries = self.focused_preview_file_entries();
        if entries.is_empty() {
            return;
        }
        let idx = match self.active_tab {
            AppTab::Git => {
                let sel = self.selected_file.as_ref();
                sel.and_then(|selected| {
                    let target_source = if selected.is_staged {
                        FocusedPreviewFileSource::GitStaged
                    } else {
                        FocusedPreviewFileSource::GitUnstaged
                    };
                    entries.iter().position(|entry| {
                        entry.path == selected.path && entry.source == target_source
                    })
                })
                .unwrap_or(0)
            }
            AppTab::Graph => self
                .commit_detail
                .file_diff
                .as_ref()
                .and_then(|diff| entries.iter().position(|entry| entry.path == diff.path))
                .unwrap_or(0),
            _ => 0,
        };
        self.focused_preview_files_selected = idx;
        self.focused_preview_files_open = true;
    }

    pub fn close_focused_preview_files(&mut self) {
        self.focused_preview_files_open = false;
    }

    pub fn toggle_focused_preview_files(&mut self) {
        if self.focused_preview_files_open {
            self.close_focused_preview_files();
        } else {
            self.open_focused_preview_files();
        }
    }

    pub fn move_focused_preview_files_selection(&mut self, delta: i32) {
        let len = self.focused_preview_file_entries().len();
        if len == 0 {
            return;
        }
        let current = self.focused_preview_files_selected as i32;
        let next = (current + delta).rem_euclid(len as i32);
        self.focused_preview_files_selected = next as usize;
    }

    pub fn focused_preview_pick_row(&mut self, idx: usize) -> Option<FocusedPreviewFileRow> {
        let entries = self.focused_preview_file_entries();
        let row = entries.get(idx).cloned()?;
        self.focused_preview_files_selected = idx;
        self.focused_preview_files_open = false;
        Some(row)
    }

    pub fn confirm_focused_preview_files_selection(&mut self) -> Option<FocusedPreviewFileRow> {
        self.focused_preview_pick_row(self.focused_preview_files_selected)
    }

    pub fn jump_to_location(
        &mut self,
        target: LocationSnapshot,
        dark: bool,
        uses_three_col: bool,
    ) -> JumpToLocationOutcome {
        let mut outcome = JumpToLocationOutcome::default();
        match target.surface.clone() {
            LocationSurface::FilePreview => {
                self.set_active_tab(AppTab::Files);
                self.active_panel = AppPanel::Diff;
                self.file_tree.reveal(&target.path);
                self.refresh_file_tree_with_target(Some(target.path.clone()));
                self.load_preview_for_path(target.path.clone());
                outcome.restore_preview_cursor = Some(target);
            }
            LocationSurface::SearchPreview => {
                self.set_active_tab(AppTab::Search);
                self.active_panel = AppPanel::Diff;
                self.load_preview_for_path(target.path.clone());
                outcome.restore_preview_cursor = Some(target);
            }
            LocationSurface::GitDiff {
                file_path,
                is_staged,
            } => {
                self.set_active_tab(AppTab::Git);
                self.active_panel = AppPanel::Diff;
                self.select_file(file_path, is_staged, dark);
                self.diff_scroll = target.scroll.vertical;
                self.diff_h_scroll = target.scroll.horizontal;
                outcome.clear_diff_selection = true;
            }
            LocationSurface::GraphDiff {
                commit_oid,
                file_path,
            } => {
                self.set_active_tab(AppTab::Graph);
                self.active_panel = AppPanel::Diff;
                if let Some(idx) = self.git_graph.find_row_by_oid(&commit_oid) {
                    self.git_graph.selected_idx = idx;
                    self.git_graph.selected_commit = Some(commit_oid);
                    self.git_graph.selection_anchor = None;
                    self.commit_detail.range_detail = None;
                }
                self.load_commit_file_diff(&file_path, dark, uses_three_col);
                self.commit_detail.file_diff_scroll = target.scroll.vertical;
                self.commit_detail.file_diff_h_scroll = target.scroll.horizontal;
                outcome.clear_commit_detail_selection = true;
                outcome.clear_diff_selection = true;
            }
        }
        outcome
    }

    pub fn normalize_active_panel(&mut self, uses_three_col: bool) -> NormalizeActivePanelOutcome {
        if self.active_panel == AppPanel::Commit && !uses_three_col {
            self.active_panel = AppPanel::Diff;
        }
        if !self.sidebar_visible && self.active_panel == AppPanel::Files {
            self.active_panel = AppPanel::Diff;
        }
        let clear_diff_selection = self.active_tab == AppTab::Graph && !uses_three_col;
        NormalizeActivePanelOutcome {
            clear_diff_selection,
        }
    }

    pub fn toggle_sidebar(&mut self) -> ToggleSidebarOutcome {
        self.sidebar_visible = !self.sidebar_visible;
        let hidden = !self.sidebar_visible;
        let mut outcome = ToggleSidebarOutcome {
            hidden,
            cancel_split_drags: hidden,
            show_hidden_hint: false,
        };
        if hidden {
            if self.active_panel == AppPanel::Files {
                self.active_panel = AppPanel::Diff;
            }
            if !self.sidebar_hide_hint_shown {
                self.sidebar_hide_hint_shown = true;
                outcome.show_hidden_hint = true;
            }
        }
        outcome
    }
}
