use super::*;

impl AppState {
    pub fn reconcile_file_tree_scroll(
        &mut self,
        visible_rows: usize,
        selection_changed: bool,
    ) -> usize {
        let max_scroll = self.file_tree.entries.len().saturating_sub(visible_rows);
        self.tree_scroll = self.tree_scroll.min(max_scroll);
        if selection_changed && !self.file_tree.selected_cleared() {
            if self.file_tree.selected < self.tree_scroll {
                self.tree_scroll = self.file_tree.selected;
            } else if visible_rows > 0 && self.file_tree.selected >= self.tree_scroll + visible_rows
            {
                self.tree_scroll = self.file_tree.selected.saturating_sub(visible_rows - 1);
            }
        }
        self.tree_scroll
    }

    pub fn toggle_file_tree_expand_and_refresh(&mut self, idx: usize) {
        self.file_tree.toggle_expand(idx);
        self.refresh_file_tree_with_target(self.file_tree.selected_path());
    }

    pub fn activate_selected_file_tree_entry(&mut self) {
        let idx = self.file_tree.selected;
        let Some(entry) = self.file_tree.entries.get(idx).cloned() else {
            return;
        };
        if entry.is_dir {
            self.file_tree.toggle_expand(idx);
            self.refresh_file_tree_with_target(self.file_tree.selected_path());
        } else {
            self.pending_edit = Some(self.file_tree.root.join(entry.path));
        }
    }

    pub fn activate_file_tree_entry_at_index(&mut self, idx: usize) {
        self.file_tree.selected = idx;
        let Some(entry) = self.file_tree.entries.get(idx).cloned() else {
            return;
        };
        if entry.is_dir {
            self.file_tree.toggle_expand(idx);
            self.refresh_file_tree_with_target(self.file_tree.selected_path());
        } else {
            self.load_preview();
        }
    }

    pub fn reveal_file_tree_path(&mut self, path: &Path) {
        self.file_tree.reveal(path);
    }

    pub fn clear_file_tree_selection_and_edit(&mut self) {
        self.file_tree.clear_selection();
        if self.tree_edit.active {
            self.tree_edit.clear();
        }
    }

    pub fn toggle_current_file_selection(&mut self) {
        if let Some(path) = self.file_tree.selected_path() {
            self.file_selection.toggle(path);
        }
    }

    pub fn clear_file_selection(&mut self) -> bool {
        if self.file_selection.is_empty() {
            return false;
        }
        self.file_selection.clear();
        true
    }

    pub fn extend_file_selection_to_index(&mut self, idx: usize) {
        let Some(entry_path) = self
            .file_tree
            .entries
            .get(idx)
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        if self.file_selection.is_empty()
            && let Some(current) = self.file_tree.selected_path()
        {
            self.file_selection.replace_with_single(current);
        }
        let entries = self.file_tree.entries.clone();
        self.file_selection.extend_to(entry_path, &entries);
        self.file_tree.selected = idx;
    }

    pub fn toggle_file_selection_at_index(&mut self, idx: usize) {
        let Some(entry_path) = self
            .file_tree
            .entries
            .get(idx)
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        self.file_selection.toggle(entry_path);
        self.file_tree.selected = idx;
    }

    pub fn arm_file_tree_drag_press(
        &mut self,
        idx: usize,
        col: u16,
        row: u16,
        mods: crate::InputModifiers,
    ) {
        let Some(entry_path) = self
            .file_tree
            .entries
            .get(idx)
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        if !self.file_selection.is_empty() && !self.file_selection.contains(&entry_path) {
            self.file_selection.clear();
        }
        self.tree_drag.arm(col, row, idx, mods);
    }

    pub fn begin_tree_drag(&mut self, sources: Vec<PathBuf>, mods: crate::InputModifiers) {
        self.tree_drag.start(sources, mods);
    }

    pub fn cancel_tree_drag(&mut self) {
        self.tree_drag.cancel();
    }

    pub fn update_tree_drag_hover(&mut self, idx: Option<usize>) {
        self.tree_drag.update_hover(idx);
    }

    pub fn update_tree_drag_modifiers(&mut self, mods: crate::InputModifiers) {
        self.tree_drag.update_modifiers(mods);
    }

    pub fn auto_expand_tree_drag_hover(&mut self, now: Instant) {
        let Some(idx) = self.tree_drag.auto_expand_due(now) else {
            return;
        };
        if let Some(entry) = self.file_tree.entries.get(idx).cloned()
            && entry.is_dir
            && !entry.is_expanded
        {
            self.file_tree.toggle_expand(idx);
            self.refresh_file_tree_with_target(self.file_tree.selected_path());
        }
        self.tree_drag.clear_hover_timer();
    }

    pub fn auto_expand_place_mode_hover(&mut self, now: Instant) {
        if !self.place_mode.active || self.file_tree_load.loading {
            return;
        }
        let Some(idx) = self.place_mode.auto_expand_due(now) else {
            return;
        };
        let should_expand = self
            .file_tree
            .entries
            .get(idx)
            .map(|entry| entry.is_dir && !entry.is_expanded)
            .unwrap_or(false);
        self.place_mode.hover_since = None;
        if should_expand {
            self.toggle_file_tree_expand_and_refresh(idx);
        }
    }

    pub fn resolve_tree_drag_drop(
        &mut self,
        release_mods: crate::InputModifiers,
    ) -> Option<(bool, PathBuf, Vec<PathBuf>)> {
        if !self.tree_drag.active {
            return None;
        }
        self.tree_drag.update_modifiers(release_mods);
        let is_copy = self.tree_drag.is_copy_op();
        let dest_rel = match self.tree_drag.hover_idx {
            Some(idx) => match crate::features::place_mode::resolve_hover_target(
                &self.file_tree.entries,
                idx,
            ) {
                crate::features::place_mode::HoverTarget::Folder { folder_idx, .. } => self
                    .file_tree
                    .entries
                    .get(folder_idx)
                    .map(|e| e.path.clone())
                    .unwrap_or_default(),
                crate::features::place_mode::HoverTarget::Root => PathBuf::new(),
            },
            None => PathBuf::new(),
        };
        let sources = std::mem::take(&mut self.tree_drag.sources);
        self.tree_drag.cancel();
        Some((is_copy, dest_rel, sources))
    }

    pub fn open_tree_context_menu(&mut self, target_entry_idx: Option<usize>, anchor: (u16, u16)) {
        self.tree_context_menu.open(anchor, target_entry_idx);
    }

    pub fn close_tree_context_menu(&mut self) {
        self.tree_context_menu.close();
    }

    pub fn selected_tree_context_menu_target(&self) -> Option<usize> {
        self.tree_context_menu.target_entry_idx
    }

    pub fn request_edit_selected_file_tree_entry(&mut self) {
        let idx = self.file_tree.selected;
        let Some(entry) = self.file_tree.entries.get(idx) else {
            return;
        };
        if !entry.is_dir {
            self.pending_edit = Some(self.file_tree.root.join(&entry.path));
        }
    }

    pub fn clear_tree_edit_error(&mut self) {
        self.tree_edit.error = None;
    }

    pub fn clear_place_mode_hover_timer(&mut self) {
        self.place_mode.hover_since = None;
    }

    pub fn begin_fs_mutation(&mut self) -> u64 {
        self.fs_mutation_load.begin()
    }

    pub fn paste_tree_edit(&mut self, s: &str) {
        if crate::text_input::paste_single_line_filtered(
            s,
            &mut self.tree_edit.buffer,
            &mut self.tree_edit.cursor,
            |c| c != '/' && c != '\\' && c != '\0' && !c.is_control(),
        ) {
            self.tree_edit.error = None;
        }
    }

    pub fn edit_tree_edit_input(&mut self, op: crate::TextEditOp) -> crate::TextEditOutcome {
        let outcome = crate::text_input::apply_single_line_op_filtered(
            op,
            &mut self.tree_edit.buffer,
            &mut self.tree_edit.cursor,
            |c| c != '/' && c != '\\' && c != '\0' && !c.is_control(),
        );
        if !matches!(outcome, crate::TextEditOutcome::Unhandled) {
            self.tree_edit.error = None;
        }
        outcome
    }

    pub fn refresh_file_tree(&mut self) {
        self.refresh_file_tree_with_target(self.file_tree.selected_path());
    }

    pub fn refresh_file_tree_with_target(&mut self, selected_path: Option<PathBuf>) {
        let generation = self.file_tree_load.begin();
        self.tasks.rebuild_tree(
            generation,
            Arc::clone(&self.backend),
            self.file_tree.expanded_paths(),
            self.file_tree.git_statuses(),
            selected_path,
            self.file_tree.selected,
        );
    }

    pub fn enter_place_mode(&mut self, sources: Vec<PathBuf>) -> Vec<AppRuntimeEvent> {
        if sources.is_empty() {
            return Vec::new();
        }
        if self.file_copy_load.loading {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::PlaceCopyInFlight,
            )];
        }
        self.quick_open.core.active = false;
        self.show_help = false;
        self.tree_edit.clear();
        self.tree_context_menu.close();
        self.active_tab = AppTab::Files;
        self.place_mode.active = true;
        self.place_mode.sources = sources;
        vec![AppRuntimeEvent::DismissConfirm]
    }

    pub fn exit_place_mode(&mut self) {
        self.place_mode.active = false;
        self.place_mode.sources.clear();
    }

    pub fn request_file_copy(
        &mut self,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    ) -> Vec<AppRuntimeEvent> {
        if self.file_copy_load.loading {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::PlaceCopyInFlight,
            )];
        }
        let generation = self.file_copy_load.begin();
        self.tasks
            .copy_files(generation, Arc::clone(&self.backend), sources, dest_dir);
        Vec::new()
    }

    pub fn effective_action_paths(&self) -> Vec<PathBuf> {
        let cursor = self.file_tree.selected_path();
        if let Some(cursor_path) = cursor.as_ref()
            && !self.file_selection.is_empty()
            && self.file_selection.contains(cursor_path)
        {
            return self.file_selection.to_vec();
        }
        cursor.into_iter().collect()
    }

    pub fn paste_target_dir(&self) -> PathBuf {
        match self.file_tree.selected_entry() {
            Some(entry) if entry.is_dir => entry.path.clone(),
            Some(entry) => entry.path.parent().map(PathBuf::from).unwrap_or_default(),
            None => PathBuf::new(),
        }
    }

    pub fn mark_cut(&mut self, paths: Vec<PathBuf>) {
        self.file_clipboard
            .set(reef_core::file_ops::ClipMode::Cut, paths);
    }

    pub fn mark_copy(&mut self, paths: Vec<PathBuf>) {
        self.file_clipboard
            .set(reef_core::file_ops::ClipMode::Copy, paths);
    }

    pub fn clear_clipboard(&mut self) {
        self.file_clipboard.clear();
    }

    pub fn paste_into(&mut self, dest_rel: PathBuf) -> Vec<AppRuntimeEvent> {
        if self.file_clipboard.is_empty() {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::PasteClipboardEmpty,
            )];
        }
        if self.fs_mutation_load.loading {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::TreeOpInFlight,
            )];
        }
        let Some(op) = self.file_clipboard.mode else {
            return Vec::new();
        };
        let sources = self.file_clipboard.paths.clone();
        self.dispatch_paste_op(op, dest_rel, sources)
    }

    pub fn duplicate_selection(&mut self) -> Vec<AppRuntimeEvent> {
        self.duplicate_paths(self.effective_action_paths())
    }

    pub fn duplicate_paths(&mut self, sources: Vec<PathBuf>) -> Vec<AppRuntimeEvent> {
        if sources.is_empty() {
            return Vec::new();
        }
        if self.fs_mutation_load.loading {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::TreeOpInFlight,
            )];
        }
        let mut events = Vec::new();
        let mut by_parent: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for source in sources {
            let parent = source.parent().map(PathBuf::from).unwrap_or_default();
            by_parent.entry(parent).or_default().push(source);
        }
        for (parent, group) in by_parent {
            events.extend(self.dispatch_paste_op(
                reef_core::file_ops::ClipMode::Copy,
                parent,
                group,
            ));
            if self.fs_mutation_load.loading {
                break;
            }
        }
        events
    }

    pub fn resolve_paste_conflict(
        &mut self,
        resolution: reef_core::file_ops::Resolution,
        apply_to_all: bool,
    ) -> Vec<AppRuntimeEvent> {
        let Some(prompt) = self.paste_conflict.as_mut() else {
            return Vec::new();
        };
        if apply_to_all {
            prompt.resolve_all_with(resolution);
        } else {
            prompt.resolve_one(resolution);
        }
        if !prompt.is_done() {
            return Vec::new();
        }

        let prompt = self.paste_conflict.take().expect("checked above");
        let cancelled = prompt.was_cancelled();
        let op = prompt.op();
        let dest = prompt.dest_dir().to_path_buf();
        let decisions = prompt.into_decisions();
        if cancelled {
            vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::PasteCancelled,
            )]
        } else {
            self.dispatch_paste_resolved(op, dest, decisions)
        }
    }

    pub fn cancel_paste_conflict(&mut self) -> Vec<AppRuntimeEvent> {
        if self.paste_conflict.take().is_some() {
            vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::PasteCancelled,
            )]
        } else {
            Vec::new()
        }
    }

    pub fn keep_both_name_for_current_conflict(&self) -> Option<String> {
        self.paste_conflict
            .as_ref()
            .and_then(|prompt| prompt.keep_both_name_for_current())
    }

    pub fn begin_tree_edit(
        &mut self,
        mode: crate::features::tree_edit::TreeEditMode,
        parent_dir: PathBuf,
        rename_target: Option<PathBuf>,
        anchor_idx: Option<usize>,
    ) -> Vec<AppRuntimeEvent> {
        if self.fs_mutation_load.loading {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::TreeOpInFlight,
            )];
        }
        self.tree_context_menu.close();
        self.exit_place_mode();
        self.active_tab = AppTab::Files;
        let buffer = match &rename_target {
            Some(path) => path
                .file_name()
                .and_then(|name| name.to_str())
                .map(String::from)
                .unwrap_or_default(),
            None => String::new(),
        };
        let cursor = buffer.len();
        self.tree_edit = crate::features::tree_edit::TreeEditState {
            active: true,
            mode: Some(mode),
            parent_dir: Some(parent_dir),
            rename_target,
            buffer,
            cursor,
            anchor_idx,
            error: None,
        };
        vec![AppRuntimeEvent::DismissConfirm]
    }

    pub fn commit_tree_edit(&mut self) {
        if self.fs_mutation_load.loading {
            return;
        }
        let Some(mode) = self.tree_edit.mode else {
            self.tree_edit.clear();
            return;
        };
        let Some(parent_dir) = self.tree_edit.parent_dir.clone() else {
            self.tree_edit.clear();
            return;
        };
        let name = match reef_core::file_ops::validate_basename(&self.tree_edit.buffer) {
            Ok(name) => name,
            Err(error) => {
                self.tree_edit.error = Some(error);
                return;
            }
        };
        let target_path = parent_dir.join(&name);

        if let Some(old) = &self.tree_edit.rename_target
            && old == &target_path
        {
            self.tree_edit.clear();
            return;
        }
        let generation = self.fs_mutation_load.begin();
        self.tasks.plan_tree_edit(
            generation,
            Arc::clone(&self.backend),
            mode,
            parent_dir,
            self.tree_edit.rename_target.clone(),
            name,
        );
    }

    pub fn cancel_tree_edit(&mut self) {
        self.tree_edit.clear();
    }

    pub fn drop_tree_drag(
        &mut self,
        is_copy: bool,
        dest_rel: PathBuf,
        sources: Vec<PathBuf>,
    ) -> Vec<AppRuntimeEvent> {
        if self.fs_mutation_load.loading {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::TreeOpInFlight,
            )];
        }
        let op = if is_copy {
            reef_core::file_ops::ClipMode::Copy
        } else {
            reef_core::file_ops::ClipMode::Cut
        };
        self.dispatch_paste_op(op, dest_rel, sources)
    }

    pub(super) fn revalidate_tree_edit_anchor(&mut self) {
        if !self.tree_edit.active {
            return;
        }
        let len = self.file_tree.entries.len();
        let Some(idx) = self.tree_edit.anchor_idx else {
            return;
        };
        let stale = match self.tree_edit.mode {
            Some(crate::features::tree_edit::TreeEditMode::Rename) => {
                let current = self.file_tree.entries.get(idx).map(|e| e.path.clone());
                current.as_ref() != self.tree_edit.rename_target.as_ref()
            }
            _ => idx >= len,
        };
        if !stale {
            return;
        }
        match self.tree_edit.mode {
            Some(crate::features::tree_edit::TreeEditMode::Rename) => {
                self.tree_edit.clear();
            }
            _ => {
                self.tree_edit.anchor_idx = None;
                self.tree_edit.parent_dir = Some(PathBuf::new());
            }
        }
    }

    pub(super) fn apply_tree_edit_plan_result(
        &mut self,
        generation: u64,
        result: Result<TreeEditPlan, TreeEditPlanError>,
        events: &mut Vec<AppRuntimeEvent>,
    ) {
        match result {
            Ok(plan) => {
                if !self.fs_mutation_load.complete_ok(generation) {
                    return;
                }
                self.fs_mutation_select_on_done = plan.select_on_done;
                let generation = self.fs_mutation_load.begin();
                match plan.mutation {
                    TreeEditMutation::CreateFile { rel, display_name } => {
                        self.tasks.create_file(
                            generation,
                            Arc::clone(&self.backend),
                            rel,
                            display_name,
                        );
                    }
                    TreeEditMutation::CreateFolder { rel, display_name } => {
                        self.tasks.create_folder(
                            generation,
                            Arc::clone(&self.backend),
                            rel,
                            display_name,
                        );
                    }
                    TreeEditMutation::Rename {
                        old_rel,
                        new_rel,
                        old_name,
                        new_name,
                    } => {
                        self.tasks.rename_path(
                            generation,
                            Arc::clone(&self.backend),
                            old_rel,
                            new_rel,
                            old_name,
                            new_name,
                        );
                    }
                }
            }
            Err(TreeEditPlanError::Validation { error }) => {
                if self.fs_mutation_load.complete_ok(generation) {
                    self.tree_edit.error = Some(error);
                }
            }
            Err(TreeEditPlanError::Backend { mutation, error }) => {
                if self
                    .fs_mutation_load
                    .complete_err(generation, error.clone())
                {
                    self.fs_mutation_load.stale = false;
                    self.fs_mutation_load.error = None;
                    events.push(AppRuntimeEvent::FsMutationDone {
                        kind: fs_mutation_kind_for_tree_edit_plan_error(mutation),
                        result: Err(error),
                    });
                }
            }
        }
    }

    pub(super) fn apply_paste_plan_result(
        &mut self,
        generation: u64,
        result: Result<PastePlanPayload, PastePlanError>,
        events: &mut Vec<AppRuntimeEvent>,
    ) {
        match result {
            Ok(payload) => {
                if !self.fs_mutation_load.complete_ok(generation) {
                    return;
                }
                if payload.self_descent_blocked > 0 {
                    events.push(AppRuntimeEvent::FileActionNotice(
                        FileActionNotice::PasteSelfIntoDescendant,
                    ));
                }
                if payload.pending.is_empty() {
                    events.extend(self.dispatch_paste_resolved(
                        payload.op,
                        payload.dest_rel,
                        payload.auto_decisions,
                    ));
                } else {
                    self.paste_conflict = Some(reef_core::file_ops::PasteConflictPrompt::new(
                        payload.op,
                        payload.dest_rel,
                        payload.auto_decisions,
                        payload.pending,
                        payload.used_names,
                    ));
                }
            }
            Err(error) => {
                if self
                    .fs_mutation_load
                    .complete_err(generation, error.error.clone())
                {
                    self.fs_mutation_load.stale = false;
                    self.fs_mutation_load.error = None;
                    events.push(AppRuntimeEvent::FsMutationDone {
                        kind: match error.op {
                            reef_core::file_ops::ClipMode::Cut => {
                                FsMutationKind::MovedMulti { count: 0 }
                            }
                            reef_core::file_ops::ClipMode::Copy => {
                                FsMutationKind::CopiedMulti { count: 0 }
                            }
                        },
                        result: Err(error.error),
                    });
                }
            }
        }
    }

    fn entry_is_dir(&self, rel: &Path) -> Option<bool> {
        self.file_tree
            .entries
            .iter()
            .find(|entry| entry.path == rel)
            .map(|entry| entry.is_dir)
    }

    fn dispatch_paste_op(
        &mut self,
        op: reef_core::file_ops::ClipMode,
        dest_rel: PathBuf,
        sources: Vec<PathBuf>,
    ) -> Vec<AppRuntimeEvent> {
        let generation = self.fs_mutation_load.begin();
        self.tasks
            .plan_paste(generation, Arc::clone(&self.backend), op, dest_rel, sources);
        Vec::new()
    }

    fn dispatch_paste_resolved(
        &mut self,
        op: reef_core::file_ops::ClipMode,
        dest_rel: PathBuf,
        decisions: Vec<(PathBuf, reef_core::file_ops::Resolution)>,
    ) -> Vec<AppRuntimeEvent> {
        use reef_core::file_ops::Resolution;

        let actionable: Vec<_> = decisions
            .into_iter()
            .filter(|(_, resolution)| !matches!(resolution, Resolution::Skip | Resolution::Cancel))
            .collect();
        if actionable.is_empty() {
            return vec![AppRuntimeEvent::FileActionNotice(
                FileActionNotice::PasteNothingToDo,
            )];
        }

        self.fs_mutation_select_on_done = actionable.first().map(|(source, resolution)| {
            let basename = match resolution {
                Resolution::KeepBoth(name) => name.clone(),
                _ => source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(String::from)
                    .unwrap_or_default(),
            };
            dest_rel.join(basename)
        });

        let items: Vec<PasteItem> = actionable
            .into_iter()
            .map(|(source, resolution)| {
                let is_dir = self.entry_is_dir(&source).unwrap_or(false);
                PasteItem {
                    source,
                    is_dir,
                    resolution,
                }
            })
            .collect();

        let generation = self.fs_mutation_load.begin();
        match op {
            reef_core::file_ops::ClipMode::Cut => {
                self.tasks
                    .move_paths(generation, Arc::clone(&self.backend), items, dest_rel);
                self.clear_clipboard();
            }
            reef_core::file_ops::ClipMode::Copy => {
                self.tasks
                    .copy_paths(generation, Arc::clone(&self.backend), items, dest_rel);
            }
        }
        self.file_selection.clear();
        Vec::new()
    }
}

fn fs_mutation_kind_for_tree_edit_plan_error(mutation: TreeEditMutation) -> FsMutationKind {
    match mutation {
        TreeEditMutation::CreateFile { display_name, .. } => {
            FsMutationKind::CreatedFile { name: display_name }
        }
        TreeEditMutation::CreateFolder { display_name, .. } => {
            FsMutationKind::CreatedFolder { name: display_name }
        }
        TreeEditMutation::Rename {
            old_name, new_name, ..
        } => FsMutationKind::Renamed { old_name, new_name },
    }
}
