use super::*;

impl AppState {
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        push_min_deadline(&mut next, self.preview_schedule.as_ref().map(|(_, t)| *t));
        push_min_deadline(&mut next, self.prefetch_schedule);
        push_min_deadline(
            &mut next,
            self.global_search
                .last_keystroke_at
                .map(|t| t + GLOBAL_SEARCH_DEBOUNCE),
        );
        push_min_deadline(&mut next, self.global_search.preview_sync_at);

        if self.active_tab == AppTab::Graph && self.backend.has_repo() && !self.graph_load.loading {
            push_min_deadline(&mut next, Some(self.next_graph_revalidate_at));
        }
        if self.tree_drag.active {
            push_min_deadline(&mut next, self.tree_drag.auto_expand_deadline());
        }
        if self.place_mode.active && !self.file_tree_load.loading {
            push_min_deadline(&mut next, self.place_mode.auto_expand_deadline());
        }
        next
    }

    pub fn has_step_work_due(&self, now: Instant) -> bool {
        if self.next_deadline().is_some_and(|deadline| deadline <= now) {
            return true;
        }
        if self.file_tree_load.should_request() || self.nav_workspace_load.should_request() {
            return true;
        }
        match self.active_tab {
            AppTab::Files => self.preview_load.should_request() && self.preview_schedule.is_none(),
            AppTab::Git => {
                self.git_status_load.should_request()
                    || self.should_request_git_status_stats()
                    || self.diff_load.should_request()
            }
            AppTab::Graph => {
                self.commit_detail_load.should_request()
                    || self.commit_file_diff_load.should_request()
                    || (!self.graph_load.loading
                        && ((self.graph_load.stale && self.graph_load.error.is_none())
                            || (self.backend.has_repo() && now >= self.next_graph_revalidate_at)))
            }
            AppTab::Search => {
                self.preview_load.should_request()
                    && self.preview_schedule.is_none()
                    && self
                        .global_search
                        .results
                        .get(self.global_search.core.selected_idx)
                        .is_some()
            }
        }
    }

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
                if self.git_status_load.should_request() {
                    self.refresh_status();
                } else if self.should_request_git_status_stats() {
                    self.refresh_status_stats();
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

    pub fn drain_fs_watcher_events(&mut self) -> bool {
        let mut changes = reef_io::FsChangeCoalescer::default();
        if let Some(rx) = self.fs_watcher_rx.as_ref() {
            while let Ok(next_change) = rx.try_recv() {
                changes.push(next_change);
            }
        }
        let change = changes.take();
        if !change.workspace_changed
            && !change.git_metadata_changed
            && !change.repo_presence_changed
        {
            return false;
        }

        self.apply_fs_change(change)
    }

    pub fn apply_fs_change(&mut self, change: reef_io::FsChange) -> bool {
        let workspace_refresh_needed = change.workspace_changed || change.repo_presence_changed;
        if workspace_refresh_needed {
            self.file_tree_load.mark_stale();
            if change.repo_presence_changed
                || change.workspace_paths.is_empty()
                || self.preview_path_changed(&change.workspace_paths)
            {
                self.preview_load.mark_stale();
            }
        }
        let has_repo = self.backend.has_repo();
        if change.repo_presence_changed {
            self.cancel_git_confirmations();
            self.diff_load.invalidate();
            self.git_status_load.invalidate();
            self.git_status_stats_load.invalidate();
            self.git_mutation_load.invalidate();
            self.commit_load.invalidate();
            self.push_load.invalidate();
            self.graph_load.invalidate();
            self.commit_detail_load.invalidate();
            self.commit_file_diff_load.invalidate();
            self.staged_files.clear();
            self.unstaged_files.clear();
            self.rebuild_git_status_tree_rows();
            self.selected_file = None;
            self.diff_content = None;
            self.git_status.ahead_behind = None;
            self.branch_name.clear();
            self.git_graph.rows.clear();
            self.git_graph.ref_map.clear();
            self.git_graph.cache_key = None;
            self.git_graph.selected_idx = 0;
            self.git_graph.selected_commit = None;
            self.git_graph.selection_anchor = None;
            self.commit_detail.detail = None;
            self.commit_detail.range_detail = None;
            self.commit_detail.file_diff = None;
            if has_repo {
                self.git_status_load.mark_stale();
                self.git_status_stats_load.mark_stale();
                self.graph_load.mark_stale();
            }
        } else if has_repo && (change.workspace_changed || change.git_metadata_changed) {
            mark_stale_after_cancel(&mut self.git_status_load);
            mark_stale_after_cancel(&mut self.git_status_stats_load);
            if self.selected_file.is_some() {
                mark_stale_after_cancel(&mut self.diff_load);
            } else {
                self.diff_load.invalidate();
            }
            if change.git_metadata_changed {
                mark_stale_after_cancel(&mut self.graph_load);
            }
        }
        if workspace_refresh_needed {
            crate::features::quick_open::mark_stale(&mut self.quick_open);
            self.nav_workspace_load.mark_stale();
            self.nav_refine_cache.clear();
            self.nav_refine_epoch = self.nav_refine_epoch.wrapping_add(1);
        }
        true
    }

    fn preview_path_changed(&self, changed_paths: &[PathBuf]) -> bool {
        if let Some((path, _)) = self.preview_schedule.as_ref() {
            return preview_dependencies_changed(std::slice::from_ref(path), changed_paths);
        }
        let mut dependency_paths = Vec::new();
        if let Some(path) = self.preview_in_flight_path.as_ref() {
            dependency_paths.push(path.clone());
        }
        if let Some(preview) = self.preview_content.as_deref() {
            dependency_paths.extend(preview.dependency_paths());
        }
        preview_dependencies_changed(&dependency_paths, changed_paths)
    }

    pub fn apply_worker_result_core(
        &mut self,
        result: WorkerResult,
        now: Instant,
    ) -> Vec<AppRuntimeEvent> {
        let mut events = Vec::new();
        match result {
            WorkerResult::FileTree {
                generation,
                tree_revision,
                result,
            } => match result {
                Ok(payload) => {
                    if generation == self.file_tree_load.generation
                        && tree_revision != self.file_tree_revision
                    {
                        self.file_tree_load.complete_ok(generation);
                        self.file_tree_load.mark_stale();
                    } else if self.file_tree_load.complete_ok(generation) {
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
                    if generation == self.file_tree_load.generation
                        && tree_revision != self.file_tree_revision
                    {
                        self.file_tree_load.complete_ok(generation);
                        self.file_tree_load.mark_stale();
                    } else {
                        self.file_tree_load.complete_err(generation, error);
                    }
                }
            },
            WorkerResult::FileTreeSubtree {
                request_id,
                parent_path,
                result,
            } => match result {
                Ok(payload) => {
                    if self.file_tree_subtree_requests.get(&parent_path) == Some(&request_id) {
                        self.file_tree_subtree_requests.remove(&parent_path);
                        let parent_is_expanded = self
                            .file_tree
                            .entries
                            .iter()
                            .find(|entry| entry.path == parent_path)
                            .is_some_and(|entry| entry.is_dir && entry.is_expanded);
                        if !parent_is_expanded {
                            return events;
                        }
                        let before = self.file_tree.selected_path();
                        let mut entries = payload.entries;
                        self.file_tree
                            .decorate_entries_with_git_statuses(&mut entries);
                        self.file_tree
                            .replace_visible_descendants(&payload.parent_path, entries);
                        self.revalidate_tree_edit_anchor();
                        if before != self.file_tree.selected_path() {
                            events.push(AppRuntimeEvent::LoadPreviewSelected);
                        }
                    }
                }
                Err(error) => {
                    if self.file_tree_subtree_requests.get(&parent_path) == Some(&request_id) {
                        self.file_tree_subtree_requests.remove(&parent_path);
                        self.file_tree_load.error = Some(error);
                    }
                }
            },
            WorkerResult::GitStatus { generation, result } => match result {
                Ok(payload) => {
                    if self.git_status_load.complete_ok(generation) {
                        let before = self.selected_file.clone();
                        let mut staged = payload.staged;
                        let mut unstaged = payload.unstaged;
                        Self::retain_cached_git_status_stats(&mut staged, &self.staged_files);
                        Self::retain_cached_git_status_stats(&mut unstaged, &self.unstaged_files);
                        let tree_needs_rebuild =
                            self.git_status_tree_needs_rebuild(&staged, &unstaged);
                        self.staged_files = staged;
                        self.unstaged_files = unstaged;
                        self.git_status.ahead_behind = payload.ahead_behind;
                        self.branch_name = payload.branch_name;
                        if tree_needs_rebuild {
                            self.rebuild_git_status_tree_rows();
                        }

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
                        if self.active_tab == AppTab::Git {
                            self.refresh_status_stats();
                        } else {
                            self.git_status_stats_load.mark_stale();
                        }
                    }
                }
                Err(error) => {
                    self.git_status_load.complete_err(generation, error);
                }
            },
            WorkerResult::GitStatusStats { generation, result } => match result {
                Ok(stats) => {
                    if self.git_status_stats_load.complete_ok(generation) {
                        self.apply_git_status_stats(stats);
                    }
                }
                Err(error) => {
                    self.git_status_stats_load.complete_err(generation, error);
                }
            },
            WorkerResult::GitMutation { generation, result } => match result {
                Ok(payload) => {
                    if !self.git_mutation_load.complete_ok(generation) {
                        return events;
                    }
                    self.apply_git_mutation_payload(payload, &mut events);
                }
                Err(error) => {
                    if !self
                        .git_mutation_load
                        .complete_err(generation, error.clone())
                    {
                        return events;
                    }
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
                    let known_last_page = self
                        .preview_database_info()
                        .and_then(|info| info.lookup(&payload.key))
                        .and_then(|object| {
                            max_page_for_object(
                                object,
                                self.db_preview
                                    .as_ref()
                                    .map(|state| state.rows_per_page)
                                    .unwrap_or(0),
                            )
                        });
                    let Some(state) = self.db_preview.as_mut() else {
                        return events;
                    };
                    if Path::new(&state.path) != payload.path.as_path() {
                        return events;
                    }
                    if payload.key == state.selection
                        && payload.page > state.page
                        && payload.rows.is_empty()
                        && known_last_page.is_none()
                    {
                        if payload.page == state.page.saturating_add(1) {
                            state.last_page = Some(state.page);
                        }
                        return events;
                    }
                    let short_page = payload.rows.len() < state.rows_per_page as usize;
                    state.selection = payload.key;
                    state.page = payload.page;
                    state.last_page =
                        known_last_page.or_else(|| short_page.then_some(payload.page));
                    state.current_rows = payload.rows;
                    state.current_row_locators = payload.row_locators;
                    state.detail = None;
                    self.reset_preview_scroll(payload.reset_h_scroll);
                }
                Err(error) => {
                    if self.db_page_load.complete_err(generation, error.clone()) {
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
                    state.current_row_locators.clear();
                    self.reset_preview_scroll(true);
                }
                Err(error) => {
                    if self.db_detail_load.complete_err(generation, error.clone()) {
                        self.push_toast(Toast::warn(format!("sqlite detail load failed: {error}")));
                    }
                }
            },
            WorkerResult::DbCell { generation, result } => match result {
                Ok(payload) => {
                    if !self.db_cell_load.complete_ok(generation) {
                        return events;
                    }
                    self.db_cell_cancellation = None;
                    let Some(state) = self.db_preview.as_mut() else {
                        return events;
                    };
                    if Path::new(&state.path) != payload.path.as_path() {
                        return events;
                    }
                    let Some(cell) = state.cell.as_mut() else {
                        return events;
                    };
                    if cell.object_key != payload.key
                        || cell.row_offset != payload.row_offset
                        || cell.row_locator != payload.row_locator
                        || cell.column != payload.column
                    {
                        return events;
                    }
                    cell.value = Some(payload.value);
                }
                Err(error) => {
                    if self.db_cell_load.complete_err(generation, error) {
                        self.db_cell_cancellation = None;
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
            WorkerResult::PreviewEnrichmentFinished {
                generation,
                path,
                enrichment,
            } => {
                if self.complete_preview_enrichment(generation, &path, enrichment) {
                    events.push(AppRuntimeEvent::RetryDeferredPreviewActions);
                }
            }
            WorkerResult::Preview { .. } | WorkerResult::LspRefineDone { .. } => {}
        }
        events
    }

    fn should_request_git_status_stats(&self) -> bool {
        self.git_status_stats_load.error.is_none() && self.git_status_stats_load.should_request()
    }
}

fn preview_dependencies_changed(dependency_paths: &[PathBuf], changed_paths: &[PathBuf]) -> bool {
    changed_paths.iter().any(|changed_path| {
        dependency_paths.iter().any(|dependency_path| {
            changed_path == dependency_path || dependency_path.starts_with(changed_path)
        })
    })
}

fn push_min_deadline(target: &mut Option<Instant>, candidate: Option<Instant>) {
    let Some(candidate) = candidate else {
        return;
    };
    match target {
        Some(current) if *current <= candidate => {}
        _ => *target = Some(candidate),
    }
}

fn mark_stale_after_cancel(state: &mut AsyncState) {
    if state.loading {
        state.invalidate();
    }
    state.mark_stale();
}
