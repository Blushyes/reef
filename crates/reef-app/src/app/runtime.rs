use super::*;

impl AppState {
    pub fn kick_active_tab_work(&mut self, now: Instant, options: TickOptions) {
        if self.file_tree_load.should_request() {
            self.refresh_file_tree();
        }

        match self.active_tab {
            AppTab::Files => {
                if self.preview_load.should_request() && self.preview_schedule.is_none() {
                    self.load_preview();
                }
            }
            AppTab::Git => {
                let has_repo = self.backend.has_repo();
                let should_poll_git = has_repo && now >= self.next_git_revalidate_at;
                if self.git_status_load.should_request()
                    || (should_poll_git && !self.git_status_load.loading)
                {
                    self.refresh_status();
                    self.next_git_revalidate_at = now + Duration::from_secs(2);
                }
                if self.diff_load.should_request() {
                    self.load_diff(options.dark);
                }
            }
            AppTab::Graph => {
                let has_repo = self.backend.has_repo();
                let should_poll_graph = has_repo && now >= self.next_graph_revalidate_at;
                let stale_no_error = self.graph_load.stale && self.graph_load.error.is_none();
                if !self.graph_load.loading && (stale_no_error || should_poll_graph) {
                    self.refresh_graph();
                    self.next_graph_revalidate_at = now + Duration::from_secs(5);
                }
                if self.commit_detail_load.should_request() {
                    self.load_commit_detail();
                }
                if self.commit_file_diff_load.should_request() {
                    self.reload_commit_file_diff(options.dark, options.uses_three_col);
                }
            }
            AppTab::Search => {
                if self.preview_load.should_request()
                    && self.preview_schedule.is_none()
                    && let Some(hit) = self
                        .global_search
                        .results
                        .get(self.global_search.core.selected_idx)
                        .cloned()
                {
                    self.load_preview_for_path(hit.path);
                }
            }
        }
    }

    pub fn drain_fs_watcher_events(&mut self) {
        let mut fs_dirty = false;
        if let Some(rx) = self.fs_watcher_rx.as_ref() {
            while rx.try_recv().is_ok() {
                fs_dirty = true;
            }
        }
        if !fs_dirty {
            return;
        }

        self.file_tree_load.mark_stale();
        self.preview_load.mark_stale();
        self.diff_load.mark_stale();
        self.git_status_load.mark_stale();
        crate::features::quick_open::mark_stale(&mut self.quick_open);
        self.nav_workspace_load.mark_stale();
        self.nav_refine_cache.clear();
        self.nav_refine_epoch = self.nav_refine_epoch.wrapping_add(1);
    }

    pub fn apply_worker_result_core(
        &mut self,
        result: WorkerResult,
        now: Instant,
    ) -> Vec<AppRuntimeEvent> {
        let mut events = Vec::new();
        match result {
            WorkerResult::FileTree { generation, result } => match result {
                Ok(payload) => {
                    if self.file_tree_load.complete_ok(generation) {
                        let before = self.file_tree.selected_path();
                        self.file_tree
                            .replace_entries(payload.entries, payload.selected_idx);
                        let staged = self.staged_files.clone();
                        let unstaged = self.unstaged_files.clone();
                        self.file_tree.refresh_git_statuses(&staged, &unstaged);
                        self.revalidate_tree_edit_anchor();
                        if before != self.file_tree.selected_path() {
                            events.push(AppRuntimeEvent::LoadPreviewSelected);
                        }
                    }
                }
                Err(error) => {
                    self.file_tree_load.complete_err(generation, error);
                }
            },
            WorkerResult::GitStatus { generation, result } => match result {
                Ok(payload) => {
                    if self.git_status_load.complete_ok(generation) {
                        let before = self.selected_file.clone();
                        self.staged_files = payload.staged;
                        self.unstaged_files = payload.unstaged;
                        self.git_status.ahead_behind = payload.ahead_behind;
                        self.branch_name = payload.branch_name;

                        let staged = self.staged_files.clone();
                        let unstaged = self.unstaged_files.clone();
                        self.file_tree.refresh_git_statuses(&staged, &unstaged);

                        if let Some(ref mut sel) = self.selected_file {
                            let in_staged = staged.iter().any(|f| f.path == sel.path);
                            let in_unstaged = unstaged.iter().any(|f| f.path == sel.path);
                            let still_in_current = if sel.is_staged {
                                in_staged
                            } else {
                                in_unstaged
                            };
                            if !still_in_current {
                                if in_staged {
                                    sel.is_staged = true;
                                } else if in_unstaged {
                                    sel.is_staged = false;
                                } else {
                                    self.selected_file = None;
                                    self.diff_content = None;
                                }
                            }
                        }
                        if before != self.selected_file {
                            events.push(AppRuntimeEvent::LoadDiffRequested);
                        }
                    }
                }
                Err(error) => {
                    self.git_status_load.complete_err(generation, error);
                }
            },
            WorkerResult::GitMutation { generation, result } => match result {
                Ok(payload) => {
                    self.git_mutation_load.complete_ok(generation);
                    self.apply_git_mutation_payload(payload, &mut events);
                }
                Err(error) => {
                    self.git_mutation_load
                        .complete_err(generation, error.clone());
                    self.push_toast(Toast::warn(format!("git update failed: {error}")));
                    self.refresh_status();
                    events.push(AppRuntimeEvent::LoadDiffRequested);
                }
            },
            WorkerResult::Commit { generation, result } => {
                self.apply_commit_result(generation, result, &mut events);
            }
            WorkerResult::Push {
                generation,
                force,
                result,
            } => {
                self.apply_push_result(generation, force, result, &mut events);
            }
            WorkerResult::Diff { generation, result } => match result {
                Ok(diff) => {
                    if self.diff_load.complete_ok(generation) {
                        self.diff_content = diff;
                    }
                }
                Err(error) => {
                    self.diff_load.complete_err(generation, error);
                }
            },
            WorkerResult::DbPage { generation, result } => match result {
                Ok(payload) => {
                    if !self.db_page_load.complete_ok(generation) {
                        return events;
                    }
                    let Some(state) = self.db_preview.as_mut() else {
                        return events;
                    };
                    if Path::new(&state.path) != payload.path.as_path() {
                        return events;
                    }
                    state.selection = payload.key;
                    state.page = payload.page;
                    state.current_rows = payload.rows;
                    state.detail = None;
                    self.reset_preview_scroll(payload.reset_h_scroll);
                }
                Err(error) => {
                    if self.db_page_load.complete_err(generation, error.clone()) {
                        self.db_page_load.stale = false;
                        self.push_toast(Toast::warn(format!("sqlite page load failed: {error}")));
                    }
                }
            },
            WorkerResult::DbDetail { generation, result } => match result {
                Ok(payload) => {
                    if !self.db_detail_load.complete_ok(generation) {
                        return events;
                    }
                    let Some(state) = self.db_preview.as_mut() else {
                        return events;
                    };
                    if Path::new(&state.path) != payload.path.as_path() {
                        return events;
                    }
                    state.selection = payload.key;
                    state.detail = Some(payload.detail);
                    state.current_rows.clear();
                    self.reset_preview_scroll(true);
                }
                Err(error) => {
                    if self.db_detail_load.complete_err(generation, error.clone()) {
                        self.db_detail_load.stale = false;
                        self.push_toast(Toast::warn(format!("sqlite detail load failed: {error}")));
                    }
                }
            },
            WorkerResult::QuickOpenIndex { generation, result } => match result {
                Ok(index) => {
                    if self.quick_open_load.complete_ok(generation) {
                        self.rebuild_quick_open_index(index);
                    }
                }
                Err(error) => {
                    if self.quick_open_load.complete_err(generation, error.clone()) {
                        self.quick_open_load.stale = false;
                        self.push_toast(Toast::warn(format!("quick open index failed: {error}")));
                    }
                }
            },
            WorkerResult::TreeEditPlan { generation, result } => {
                self.apply_tree_edit_plan_result(generation, result, &mut events);
            }
            WorkerResult::PastePlan { generation, result } => {
                self.apply_paste_plan_result(generation, result, &mut events);
            }
            WorkerResult::Graph { generation, result } => {
                self.apply_graph_result(generation, result, &mut events);
            }
            WorkerResult::CommitDetail { generation, result } => match result {
                Ok(detail) => {
                    if self.commit_detail_load.complete_ok(generation) {
                        self.commit_detail.detail = detail;
                    }
                }
                Err(error) => {
                    self.commit_detail_load.complete_err(generation, error);
                }
            },
            WorkerResult::CommitFileDiff { generation, result }
            | WorkerResult::RangeFileDiff { generation, result } => match result {
                Ok(file_diff) => {
                    if self.commit_file_diff_load.complete_ok(generation) {
                        self.commit_detail.file_diff = file_diff;
                    }
                }
                Err(error) => {
                    self.commit_file_diff_load.complete_err(generation, error);
                }
            },
            WorkerResult::RangeDetail { generation, result } => match result {
                Ok(files) => {
                    if self.commit_detail_load.complete_ok(generation)
                        && let Some(rd) = self.commit_detail.range_detail.as_mut()
                    {
                        rd.files = files;
                    }
                }
                Err(error) => {
                    self.commit_detail_load.complete_err(generation, error);
                }
            },
            WorkerResult::GlobalSearchChunk { generation, hits } => {
                if generation == self.global_search_load.generation {
                    self.global_search.results.extend(hits);
                    self.global_search
                        .results
                        .sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
                    events.push(AppRuntimeEvent::SyncSearchPreviewIfStale);
                }
            }
            WorkerResult::GlobalSearchDone {
                generation,
                truncated,
            } => {
                if self.global_search_load.complete_ok(generation) {
                    self.global_search.truncated = truncated;
                    if self.global_search.results.is_empty() && self.active_tab == AppTab::Search {
                        self.preview_highlight = None;
                    }
                }
            }
            WorkerResult::FileCopy { generation, result } => match result {
                Ok(count) => {
                    if self.file_copy_load.complete_ok(generation) {
                        self.place_mode.active = false;
                        self.place_mode.sources.clear();
                        self.refresh_file_tree();
                        events.push(AppRuntimeEvent::FileCopyDone { result: Ok(count) });
                    }
                }
                Err(error) => {
                    if self.file_copy_load.complete_err(generation, error.clone()) {
                        self.place_mode.active = false;
                        self.place_mode.sources.clear();
                        self.file_copy_load.stale = false;
                        self.file_copy_load.error = None;
                        events.push(AppRuntimeEvent::FileCopyDone { result: Err(error) });
                    }
                }
            },
            WorkerResult::FsMutation {
                generation,
                kind,
                result,
            } => match result {
                Ok(()) => {
                    if self.fs_mutation_load.complete_ok(generation) {
                        self.tree_edit.clear();
                        events.push(AppRuntimeEvent::DismissConfirm);
                        let target = self.fs_mutation_select_on_done.take();
                        if target.is_some() {
                            self.refresh_file_tree_with_target(target);
                        } else {
                            self.refresh_file_tree();
                        }
                        events.push(AppRuntimeEvent::FsMutationDone {
                            kind,
                            result: Ok(()),
                        });
                    }
                }
                Err(error) => {
                    if self
                        .fs_mutation_load
                        .complete_err(generation, error.clone())
                    {
                        events.push(AppRuntimeEvent::DismissConfirm);
                        self.fs_mutation_select_on_done = None;
                        self.fs_mutation_load.stale = false;
                        self.fs_mutation_load.error = None;
                        events.push(AppRuntimeEvent::FsMutationDone {
                            kind,
                            result: Err(error),
                        });
                    }
                }
            },
            WorkerResult::ReplaceProgress {
                generation,
                files_done,
                files_total,
            } => {
                if generation == self.replace_load.generation {
                    self.global_search.replace_progress = Some((files_done, files_total));
                }
            }
            WorkerResult::ReplaceDone { generation, result } => {
                if !self.replace_load.complete_ok(generation) {
                    return events;
                }
                self.global_search.replace_progress = None;
                if result.is_ok() {
                    self.global_search.excluded.clear();
                    self.reload_global_search(now);
                    self.refresh_status();
                }
                events.push(AppRuntimeEvent::ReplaceDone { result });
            }
            WorkerResult::NavWorkspaceBuilt { generation, result } => {
                if !self.nav_workspace_load.complete_ok(generation) {
                    return events;
                }
                self.nav_workspace = result.ok().map(Arc::new);
            }
            WorkerResult::LspStateChange { lang, state } => {
                self.apply_lsp_state_change(lang, state);
            }
            WorkerResult::Preview { .. } | WorkerResult::LspRefineDone { .. } => {}
        }
        events
    }
}
