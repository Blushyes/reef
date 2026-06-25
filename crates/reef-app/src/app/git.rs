use super::*;

impl AppState {
    pub fn reset_graph_visual_anchor(&mut self) {
        self.git_graph.selection_anchor = Some(self.git_graph.selected_idx);
    }

    pub fn refresh_graph_uncached_and_mark(&mut self) {
        self.git_graph.cache_key = None;
        self.refresh_graph();
    }

    pub fn request_edit_selected_git_file(&mut self) {
        let Some(sel) = self.selected_file.as_ref() else {
            return;
        };
        let workdir = self.backend.workdir_path();
        self.pending_edit = Some(workdir.join(&sel.path));
    }

    pub fn paste_commit_message(&mut self, s: &str) {
        let _ = crate::text_input::paste_multi_line_strip_cr(
            s,
            &mut self.git_status.commit_message,
            &mut self.git_status.commit_cursor,
        );
    }

    pub fn edit_commit_message(&mut self, op: crate::TextEditOp) -> crate::TextEditOutcome {
        crate::text_input::apply_multi_line_op(
            op,
            &mut self.git_status.commit_message,
            &mut self.git_status.commit_cursor,
        )
    }

    pub fn visible_file_count(&self) -> usize {
        let mut count = 0;
        if !self.staged_files.is_empty() {
            count += 1;
            if !self.staged_collapsed {
                count += self.staged_files.len();
            }
        }
        count += 1;
        if !self.unstaged_collapsed {
            count += self.unstaged_files.len();
        }
        count
    }

    pub fn refresh_status(&mut self) {
        if !self.backend.has_repo() {
            return;
        }
        let generation = self.git_status_load.begin();
        self.tasks
            .refresh_status(generation, Arc::clone(&self.backend));
    }

    pub fn select_file(&mut self, path: String, is_staged: bool, dark: bool) {
        self.selected_file = Some(SelectedFile { path, is_staged });
        self.git_status.confirm_discard = None;
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
        self.load_diff(dark);
    }

    pub fn select_git_file_for_discard(&mut self, path: String, is_staged: bool, dark: bool) {
        self.selected_file = Some(SelectedFile {
            path: path.clone(),
            is_staged,
        });
        self.git_status.confirm_discard = Some(DiscardTarget::File { is_staged, path });
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
        self.load_diff(dark);
    }

    pub fn toggle_staged_section(&mut self) {
        self.staged_collapsed = !self.staged_collapsed;
    }

    pub fn toggle_unstaged_section(&mut self) {
        self.unstaged_collapsed = !self.unstaged_collapsed;
    }

    pub fn toggle_git_status_dir(&mut self, is_staged: bool, path: &str) {
        if path.is_empty() {
            return;
        }
        let key = reef_core::git::tree::collapsed_key(is_staged, path);
        if !self.git_status.collapsed_dirs.remove(&key) {
            self.git_status.collapsed_dirs.insert(key);
        }
    }

    pub fn prompt_discard_file(&mut self, is_staged: bool, path: String) {
        if !path.is_empty() {
            self.git_status.confirm_discard = Some(DiscardTarget::File { is_staged, path });
        }
    }

    pub fn prompt_discard_folder(&mut self, is_staged: bool, path: String) {
        if !path.is_empty() {
            self.git_status.confirm_discard = Some(DiscardTarget::Folder { is_staged, path });
        }
    }

    pub fn prompt_discard_section(&mut self, is_staged: bool) {
        let has_any = if is_staged {
            !self.staged_files.is_empty()
        } else {
            !self.unstaged_files.is_empty()
        };
        if has_any {
            self.git_status.confirm_discard = Some(DiscardTarget::Section { is_staged });
        }
    }

    pub fn cancel_git_confirmations(&mut self) {
        self.git_status.confirm_discard = None;
        self.git_status.confirm_push = false;
        self.git_status.confirm_force_push = false;
    }

    pub fn dismiss_push_error(&mut self) {
        self.git_status.push_error = None;
    }

    pub fn prompt_push(&mut self, force: bool) {
        if self.push_load.loading {
            return;
        }
        self.git_status.confirm_push = !force;
        self.git_status.confirm_force_push = force;
        self.git_status.push_error = None;
    }

    pub fn confirm_push(&mut self, force: bool) {
        if force {
            self.git_status.confirm_force_push = false;
        } else {
            self.git_status.confirm_push = false;
        }
        self.run_push(force);
    }

    pub fn set_commit_editing(&mut self, editing: bool) {
        self.git_status.commit_editing = editing;
    }

    pub fn dismiss_commit_error(&mut self) {
        self.git_status.commit_error = None;
    }

    pub fn load_diff(&mut self, dark: bool) {
        let Some(sel) = self.selected_file.clone() else {
            self.diff_content = None;
            return;
        };
        if !self.backend.has_repo() {
            self.diff_content = None;
            return;
        }
        let context = match self.diff_mode {
            DiffMode::FullFile => 9999,
            DiffMode::Compact => 3,
        };
        let generation = self.diff_load.begin();
        self.tasks.load_diff(
            generation,
            Arc::clone(&self.backend),
            sel.path,
            sel.is_staged,
            context,
            dark,
        );
    }

    pub fn toggle_diff_layout(&mut self) {
        self.diff_layout = match self.diff_layout {
            DiffLayout::Unified => DiffLayout::SideBySide,
            DiffLayout::SideBySide => DiffLayout::Unified,
        };
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
    }

    pub fn toggle_diff_mode(&mut self, dark: bool) {
        self.diff_mode = match self.diff_mode {
            DiffMode::Compact => DiffMode::FullFile,
            DiffMode::FullFile => DiffMode::Compact,
        };
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
        self.load_diff(dark);
    }

    pub fn toggle_status_tree_mode(&mut self) {
        self.git_status.tree_mode = !self.git_status.tree_mode;
    }

    pub fn toggle_commit_diff_layout(&mut self) {
        self.commit_detail.diff_layout = match self.commit_detail.diff_layout {
            DiffLayout::Unified => DiffLayout::SideBySide,
            DiffLayout::SideBySide => DiffLayout::Unified,
        };
    }

    pub fn toggle_commit_diff_mode(&mut self) {
        self.commit_detail.diff_mode = match self.commit_detail.diff_mode {
            DiffMode::Compact => DiffMode::FullFile,
            DiffMode::FullFile => DiffMode::Compact,
        };
    }

    pub fn toggle_commit_files_tree_mode(&mut self) {
        self.commit_detail.files_tree_mode = !self.commit_detail.files_tree_mode;
    }

    pub fn toggle_commit_files_dir_collapsed(&mut self, path: &str) {
        if !self.commit_detail.files_collapsed.remove(path) {
            self.commit_detail.files_collapsed.insert(path.to_string());
        }
    }

    pub fn stage_file(&mut self, path: &str, _dark: bool) {
        self.dispatch_git_mutation(GitMutation::Stage(vec![path.to_string()]));
    }

    pub fn unstage_file(&mut self, path: &str, _dark: bool) {
        self.dispatch_git_mutation(GitMutation::Unstage(vec![path.to_string()]));
    }

    pub fn stage_all(&mut self, _dark: bool) {
        let paths: Vec<String> = self.unstaged_files.iter().map(|f| f.path.clone()).collect();
        self.dispatch_git_mutation(GitMutation::Stage(paths));
    }

    pub fn unstage_all(&mut self, _dark: bool) {
        let paths: Vec<String> = self.staged_files.iter().map(|f| f.path.clone()).collect();
        self.dispatch_git_mutation(GitMutation::Unstage(paths));
    }

    pub fn stage_folder(&mut self, folder_path: &str, _dark: bool) {
        let paths: Vec<String> = self
            .unstaged_files
            .iter()
            .filter(|f| folder_contains(folder_path, &f.path))
            .map(|f| f.path.clone())
            .collect();
        self.dispatch_git_mutation(GitMutation::Stage(paths));
    }

    pub fn unstage_folder(&mut self, folder_path: &str, _dark: bool) {
        let paths: Vec<String> = self
            .staged_files
            .iter()
            .filter(|f| folder_contains(folder_path, &f.path))
            .map(|f| f.path.clone())
            .collect();
        self.dispatch_git_mutation(GitMutation::Unstage(paths));
    }

    pub fn confirm_discard(&mut self, _dark: bool) {
        let Some(target) = self.git_status.confirm_discard.take() else {
            return;
        };
        let paths = self.discard_paths_for_target(&target);
        self.dispatch_git_mutation(GitMutation::Revert(paths));
    }

    pub(super) fn discard_paths_for_target(&self, target: &DiscardTarget) -> Vec<GitRevertPath> {
        match target {
            DiscardTarget::File { is_staged, path } => vec![GitRevertPath {
                path: path.clone(),
                is_staged: *is_staged,
            }],
            DiscardTarget::Folder { is_staged, path } => {
                let source: Vec<String> = if *is_staged {
                    self.staged_files.iter().map(|f| f.path.clone()).collect()
                } else {
                    self.unstaged_files.iter().map(|f| f.path.clone()).collect()
                };
                source
                    .into_iter()
                    .filter(|candidate| folder_contains(path, candidate))
                    .map(|candidate| GitRevertPath {
                        path: candidate,
                        is_staged: *is_staged,
                    })
                    .collect()
            }
            DiscardTarget::Section { is_staged } => {
                let source: Vec<String> = if *is_staged {
                    self.staged_files.iter().map(|f| f.path.clone()).collect()
                } else {
                    self.unstaged_files.iter().map(|f| f.path.clone()).collect()
                };
                source
                    .into_iter()
                    .map(|candidate| GitRevertPath {
                        path: candidate,
                        is_staged: *is_staged,
                    })
                    .collect()
            }
        }
    }

    fn dispatch_git_mutation(&mut self, mutation: GitMutation) {
        if !self.backend.has_repo() {
            return;
        }
        let is_empty = match &mutation {
            GitMutation::Stage(paths) | GitMutation::Unstage(paths) => paths.is_empty(),
            GitMutation::Revert(paths) => paths.is_empty(),
        };
        if is_empty {
            return;
        }
        let generation = self.git_mutation_load.begin();
        self.tasks
            .mutate_git(generation, Arc::clone(&self.backend), mutation);
    }

    pub(super) fn apply_git_mutation_payload(
        &mut self,
        payload: GitMutationPayload,
        events: &mut Vec<AppRuntimeEvent>,
    ) {
        if !payload.errors.is_empty() {
            self.push_toast(Toast::warn(format!(
                "git update partially failed: {}",
                payload.errors.join("\n")
            )));
        }

        let touched: HashSet<String> = payload.touched.into_iter().collect();
        match payload.mutation {
            GitMutation::Stage(_) => {
                if let Some(ref mut sel) = self.selected_file
                    && touched.contains(&sel.path)
                {
                    sel.is_staged = true;
                }
            }
            GitMutation::Unstage(_) => {
                if let Some(ref mut sel) = self.selected_file
                    && touched.contains(&sel.path)
                {
                    sel.is_staged = false;
                }
            }
            GitMutation::Revert(_) => {
                if let Some(sel) = self.selected_file.as_ref()
                    && touched.contains(&sel.path)
                {
                    self.selected_file = None;
                    self.diff_content = None;
                }
            }
        }

        self.refresh_status();
        events.push(AppRuntimeEvent::LoadDiffRequested);
    }

    pub fn run_commit(&mut self) {
        if self.commit_load.loading || !self.backend.has_repo() {
            return;
        }
        let message = self.git_status.commit_message.trim().to_string();
        if message.is_empty() {
            return;
        }
        if self.staged_files.is_empty() {
            self.git_status.commit_error = Some(CommitError::NothingStaged);
            return;
        }
        let generation = self.commit_load.begin();
        self.tasks
            .commit(generation, Arc::clone(&self.backend), message);
    }

    pub(super) fn apply_commit_result(
        &mut self,
        generation: u64,
        result: Result<(), String>,
        events: &mut Vec<AppRuntimeEvent>,
    ) {
        if !self.commit_load.complete_ok(generation) {
            return;
        }
        match &result {
            Ok(()) => {
                self.git_status.commit_error = None;
                self.git_status.commit_message.clear();
                self.git_status.commit_cursor = 0;
                self.git_status.commit_editing = false;
                events.push(AppRuntimeEvent::LoadDiffRequested);
            }
            Err(error) => {
                self.git_status.commit_error = Some(CommitError::Failed(error.clone()));
            }
        }
        self.git_graph.cache_key = None;
        self.git_status_load.mark_stale();
        self.graph_load.mark_stale();
        events.push(AppRuntimeEvent::CommitDone { result });
    }

    pub fn run_push(&mut self, force: bool) {
        if self.push_load.loading || !self.backend.has_repo() {
            return;
        }
        let generation = self.push_load.begin();
        self.tasks
            .push(generation, Arc::clone(&self.backend), force);
    }

    pub(super) fn apply_push_result(
        &mut self,
        generation: u64,
        force: bool,
        result: Result<(), String>,
        events: &mut Vec<AppRuntimeEvent>,
    ) {
        if !self.push_load.complete_ok(generation) {
            return;
        }
        match &result {
            Ok(()) => {
                self.git_status.push_error = None;
            }
            Err(error) => {
                self.git_status.push_error = Some(PushError::Failed(error.clone()));
            }
        }
        self.git_graph.cache_key = None;
        self.git_status_load.mark_stale();
        self.graph_load.mark_stale();
        events.push(AppRuntimeEvent::PushDone { force, result });
    }

    pub fn refresh_graph(&mut self) {
        const GRAPH_COMMIT_LIMIT: usize = 500;
        if !self.backend.has_repo() {
            self.git_graph.rows.clear();
            self.git_graph.ref_map.clear();
            self.git_graph.cache_key = None;
            return;
        }
        let generation = self.graph_load.begin();
        self.tasks.refresh_graph(
            generation,
            Arc::clone(&self.backend),
            GRAPH_COMMIT_LIMIT,
            self.git_graph.scope.clone(),
        );
    }

    pub fn set_graph_scope(&mut self, scope: GraphScope) -> GraphScopeChangeOutcome {
        if self.git_graph.scope == scope {
            return GraphScopeChangeOutcome::default();
        }
        if let GraphScope::Branch(full_ref) = &scope {
            self.git_graph
                .recent_branches
                .retain(|existing| existing != full_ref);
            self.git_graph.recent_branches.insert(0, full_ref.clone());
            if self.git_graph.recent_branches.len() > GRAPH_RECENT_BRANCHES_MAX {
                self.git_graph
                    .recent_branches
                    .truncate(GRAPH_RECENT_BRANCHES_MAX);
            }
        }
        self.apply_graph_scope_no_refresh(scope);
        self.refresh_graph();
        GraphScopeChangeOutcome {
            changed: true,
            clear_commit_graph_search: true,
        }
    }

    pub fn apply_graph_scope_no_refresh(&mut self, scope: GraphScope) -> GraphScopeChangeOutcome {
        self.git_graph.scope = scope;
        self.git_graph.cache_key = None;
        self.git_graph.rows.clear();
        self.git_graph.selected_idx = 0;
        self.git_graph.scroll = 0;
        self.git_graph.selection_anchor = None;
        self.git_graph.selected_commit = None;
        self.commit_detail.detail = None;
        self.commit_detail.range_detail = None;
        self.commit_detail.file_diff = None;
        self.commit_detail_load.invalidate();
        self.commit_file_diff_load.invalidate();
        GraphScopeChangeOutcome {
            changed: true,
            clear_commit_graph_search: true,
        }
    }

    pub fn load_commit_detail(&mut self) {
        self.commit_detail.file_diff = None;
        self.commit_detail.diff_h_scroll = 0;
        self.commit_detail.sbs_left_h_scroll = 0;
        self.commit_detail.sbs_right_h_scroll = 0;
        let Some(oid) = self.git_graph.selected_commit.clone() else {
            self.commit_detail.detail = None;
            return;
        };
        if !self.backend.has_repo() {
            self.commit_detail.detail = None;
            return;
        }
        let generation = self.commit_detail_load.begin();
        self.tasks
            .load_commit_detail(generation, Arc::clone(&self.backend), oid);
    }

    pub fn load_commit_range_detail(&mut self) {
        self.commit_detail.file_diff = None;
        self.commit_detail.diff_h_scroll = 0;
        self.commit_detail.sbs_left_h_scroll = 0;
        self.commit_detail.sbs_right_h_scroll = 0;
        if !self.git_graph.is_range() {
            self.commit_detail.range_detail = None;
            return;
        }
        let (lo, hi) = self.git_graph.selected_range();
        let Some(oldest_row) = self.git_graph.rows.get(hi) else {
            self.commit_detail.range_detail = None;
            return;
        };
        let Some(newest_row) = self.git_graph.rows.get(lo) else {
            self.commit_detail.range_detail = None;
            return;
        };
        let oldest_oid = oldest_row.commit.oid.clone();
        let newest_oid = newest_row.commit.oid.clone();
        let commits: Vec<CommitInfo> = self.git_graph.rows[lo..=hi]
            .iter()
            .map(|r| r.commit.clone())
            .collect();
        let commit_count = commits.len();
        self.commit_detail.range_detail = Some(RangeDetail {
            oldest_oid: oldest_oid.clone(),
            newest_oid: newest_oid.clone(),
            commit_count,
            commits,
            files: Vec::new(),
        });
        if !self.backend.has_repo() {
            return;
        }
        let generation = self.commit_detail_load.begin();
        self.tasks.load_commit_range_detail(
            generation,
            Arc::clone(&self.backend),
            oldest_oid,
            newest_oid,
        );
    }

    pub fn reload_graph_selection(&mut self) {
        if self.git_graph.is_range() {
            self.commit_detail.detail = None;
            self.load_commit_range_detail();
        } else {
            self.commit_detail.range_detail = None;
            self.load_commit_detail();
        }
    }

    pub fn load_commit_file_diff(
        &mut self,
        path: &str,
        dark: bool,
        uses_three_col: bool,
    ) -> CommitFileDiffLoadOutcome {
        if self.active_tab == AppTab::Graph && uses_three_col {
            self.active_panel = AppPanel::Diff;
        }
        let context = match self.commit_detail.diff_mode {
            DiffMode::Compact => 3,
            DiffMode::FullFile => 9999,
        };
        if !self.backend.has_repo() {
            self.commit_detail.file_diff = None;
            return CommitFileDiffLoadOutcome::default();
        }
        let is_new_file = self
            .commit_detail
            .file_diff
            .as_ref()
            .map(|d| d.path.as_str() != path)
            .unwrap_or(true);
        let mut outcome = CommitFileDiffLoadOutcome::default();
        if is_new_file {
            outcome.clear_commit_detail_selection = true;
            outcome.clear_diff_selection = true;
            self.commit_detail.diff_h_scroll = 0;
            self.commit_detail.sbs_left_h_scroll = 0;
            self.commit_detail.sbs_right_h_scroll = 0;
            self.commit_detail.file_diff_scroll = 0;
            self.commit_detail.file_diff_h_scroll = 0;
            self.commit_detail.file_diff_sbs_left_h_scroll = 0;
            self.commit_detail.file_diff_sbs_right_h_scroll = 0;
        }
        if self.git_graph.is_range() {
            let Some(range) = self.commit_detail.range_detail.as_ref() else {
                self.commit_detail.file_diff = None;
                return outcome;
            };
            let (oldest, newest) = (range.oldest_oid.clone(), range.newest_oid.clone());
            let generation = self.commit_file_diff_load.begin();
            self.tasks.load_range_file_diff(
                generation,
                Arc::clone(&self.backend),
                oldest,
                newest,
                path.to_string(),
                context,
                dark,
            );
            return outcome;
        }
        let Some(oid) = self.git_graph.selected_commit.clone() else {
            self.commit_detail.file_diff = None;
            return outcome;
        };
        let generation = self.commit_file_diff_load.begin();
        self.tasks.load_commit_file_diff(
            generation,
            Arc::clone(&self.backend),
            oid,
            path.to_string(),
            context,
            dark,
        );
        outcome
    }

    pub fn reload_commit_file_diff(
        &mut self,
        dark: bool,
        uses_three_col: bool,
    ) -> CommitFileDiffLoadOutcome {
        let path = self
            .commit_detail
            .file_diff
            .as_ref()
            .map(|d| d.path.clone());
        if let Some(path) = path {
            self.load_commit_file_diff(&path, dark, uses_three_col)
        } else {
            CommitFileDiffLoadOutcome::default()
        }
    }

    pub fn move_graph_selection(&mut self, delta: i32) {
        if self.git_graph.rows.is_empty() {
            return;
        }
        self.git_graph.selection_anchor = None;
        let last = self.git_graph.rows.len() - 1;
        let current = self.git_graph.selected_idx as i32;
        let next = (current + delta).clamp(0, last as i32) as usize;
        if next == self.git_graph.selected_idx {
            if self.commit_detail.range_detail.is_some() {
                self.commit_detail.range_detail = None;
                self.load_commit_detail();
            }
            return;
        }
        self.git_graph.selected_idx = next;
        self.git_graph.selected_commit =
            self.git_graph.rows.get(next).map(|r| r.commit.oid.clone());
        self.commit_detail.scroll = 0;
        self.commit_detail.range_detail = None;
        self.load_commit_detail();
    }

    pub fn select_graph_commit(&mut self, oid: &str) {
        let Some(idx) = self.git_graph.find_row_by_oid(oid) else {
            return;
        };
        if self.git_graph.in_visual_mode() {
            let delta = idx as i32 - self.git_graph.selected_idx as i32;
            self.extend_graph_selection(delta);
            return;
        }
        self.focus_graph_commit(oid);
    }

    pub fn focus_graph_commit(&mut self, oid: &str) {
        let Some(idx) = self.git_graph.find_row_by_oid(oid) else {
            return;
        };
        self.git_graph.selected_idx = idx;
        self.git_graph.selected_commit = Some(oid.to_string());
        self.git_graph.selection_anchor = None;
        self.commit_detail.range_detail = None;
        self.commit_detail.scroll = 0;
        self.load_commit_detail();
    }

    pub fn refresh_graph_uncached(&mut self) {
        self.git_graph.cache_key = None;
        self.refresh_graph();
    }

    pub fn extend_graph_selection(&mut self, delta: i32) {
        if self.git_graph.rows.is_empty() {
            return;
        }
        if self.git_graph.selection_anchor.is_none() {
            self.git_graph.selection_anchor = Some(self.git_graph.selected_idx);
        }
        let last = self.git_graph.rows.len() - 1;
        let current = self.git_graph.selected_idx as i32;
        let next = (current + delta).clamp(0, last as i32) as usize;
        if next == self.git_graph.selected_idx {
            return;
        }
        self.git_graph.selected_idx = next;
        self.git_graph.selected_commit =
            self.git_graph.rows.get(next).map(|r| r.commit.oid.clone());
        self.commit_detail.scroll = 0;
        self.reload_graph_selection();
    }

    pub fn clear_graph_range(&mut self) {
        if self.git_graph.selection_anchor.take().is_some() {
            self.commit_detail.scroll = 0;
            self.commit_detail.range_detail = None;
            self.reload_graph_selection();
        }
    }

    pub(super) fn apply_graph_result(
        &mut self,
        generation: u64,
        result: Result<GraphPayload, String>,
        events: &mut Vec<AppRuntimeEvent>,
    ) {
        match result {
            Ok(payload) => {
                if self.graph_load.complete_ok(generation) {
                    if self.git_graph.scope != GraphScope::AllRefs
                        && payload.rows.is_empty()
                        && matches!(payload.scope, GraphScope::Branch(_))
                        && self.git_graph.scope == payload.scope
                        && payload_scope_ref_missing(&payload)
                    {
                        if let GraphScope::Branch(missing) = &payload.scope {
                            self.git_graph
                                .recent_branches
                                .retain(|existing| existing != missing);
                            let short = shorthand_for_full_ref(missing).to_string();
                            events.push(AppRuntimeEvent::GraphScopeFallback { short_ref: short });
                        }
                        self.apply_graph_scope_no_refresh(GraphScope::AllRefs);
                        self.refresh_graph();
                        events.push(AppRuntimeEvent::PersistGraphScope);
                        return;
                    }

                    let previous_commit = self.git_graph.selected_commit.clone();
                    let previous_anchor_oid = self
                        .git_graph
                        .selection_anchor
                        .and_then(|idx| self.git_graph.rows.get(idx))
                        .map(|r| r.commit.oid.clone());
                    let cache_key_changed =
                        self.git_graph.cache_key.as_ref() != Some(&payload.cache_key);

                    self.git_graph.rows = payload.rows;
                    self.git_graph.ref_map = payload.ref_map;
                    self.git_graph.cache_key = Some(payload.cache_key);

                    if let Some(ref oid) = previous_commit
                        && let Some(idx) = self.git_graph.find_row_by_oid(oid)
                    {
                        self.git_graph.selected_idx = idx;
                    }
                    if self.git_graph.selected_idx >= self.git_graph.rows.len() {
                        self.git_graph.selected_idx = self.git_graph.rows.len().saturating_sub(1);
                    }
                    self.git_graph.selected_commit = self
                        .git_graph
                        .rows
                        .get(self.git_graph.selected_idx)
                        .map(|r| r.commit.oid.clone());

                    let anchor_survived = previous_anchor_oid
                        .as_ref()
                        .and_then(|oid| self.git_graph.find_row_by_oid(oid));
                    self.git_graph.selection_anchor = anchor_survived;
                    if anchor_survived.is_none() {
                        self.commit_detail.range_detail = None;
                    }

                    if self.git_graph.selection_anchor.is_some() {
                        if cache_key_changed {
                            events.push(AppRuntimeEvent::ClearCommitDetailSelection);
                            self.reload_graph_selection();
                        }
                    } else if self.git_graph.selected_commit != previous_commit
                        || self.commit_detail.detail.is_none()
                    {
                        events.push(AppRuntimeEvent::ClearCommitDetailSelection);
                        self.load_commit_detail();
                    }
                }
            }
            Err(error) => {
                self.graph_load.complete_err(generation, error);
            }
        }
    }
}
