use std::time::Instant;

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

use reef_core::diff::DiffLayout;
use reef_core::git::{FileEntry, GraphScope};
use reef_core::preview::PreviewDocument;
use reef_io::{Backend, BackendError, EditorLaunchSpec};

use crate::app::TabChangeOutcome;
use crate::tasks::WorkerResult;
use crate::{
    AppCommand, AppEffect, AppPanel, AppRuntimeEvent, AppSnapshot, AppState, AppTab, AsyncState,
    CommitDetailState, CommitFileDiffLoadOutcome, ConfirmRequest, ContextMenuItem, DbPreviewState,
    FileClipboard, FindWidgetState, GitGraphState, GitStatusState, GlobalSearchRowSnapshot,
    GraphBranchPickerRowSnapshot, GraphScopeChangeOutcome, HighlightedDiff, HostsPickerRowSnapshot,
    HoverTarget, LocationSnapshot, LspRefineOutcome, MatchHit, NormalizeActivePanelOutcome,
    PickerInputOutcome, PlaceModeState, PreviewHighlight, QuickOpenRowSnapshot, SearchState,
    SelectedFile, SelectionSet, SettingsState, TextEditOutcome, TickOptions, TreeDragState,
    TreeEditState, TreeEntry, ViewMode,
};

pub struct ReefApp {
    #[cfg(any(test, feature = "test-helpers"))]
    pub state: AppState,
    #[cfg(not(any(test, feature = "test-helpers")))]
    state: AppState,
    effects: Vec<AppEffect>,
    runtime_events: Vec<AppRuntimeEvent>,
}

#[derive(Debug, Clone, Default)]
pub struct AppCommandOutcome {
    pub text_edit: Option<TextEditOutcome>,
    pub nav_refine_generation: Option<u64>,
    pub normalize_active_panel: Option<NormalizeActivePanelOutcome>,
    pub committed_editor_command: Option<String>,
    pub global_search_preview_sync_due: bool,
}

impl ReefApp {
    pub fn new(config: AppConfig) -> Self {
        Self {
            state: config.state,
            effects: Vec::new(),
            runtime_events: Vec::new(),
        }
    }

    fn push_tab_change_outcome(&mut self, outcome: TabChangeOutcome) {
        if outcome.changed {
            self.runtime_events
                .push(AppRuntimeEvent::TabChanged(outcome));
        }
    }

    fn push_tab_changed_for_state_transition(&mut self, before: AppTab, after: AppTab) {
        if before == after {
            return;
        }
        self.push_tab_change_outcome(TabChangeOutcome {
            changed: true,
            clear_preview_selection: true,
            clear_commit_detail_selection: true,
            clear_diff_selection: true,
            close_find_widget: true,
            dismiss_confirm: before == AppTab::Files,
            sync_search_preview: after == AppTab::Search,
        });
    }

    fn push_graph_scope_change_outcome(&mut self, outcome: GraphScopeChangeOutcome) {
        if !outcome.changed {
            return;
        }
        if outcome.clear_commit_graph_search {
            self.runtime_events
                .push(AppRuntimeEvent::ClearCommitGraphSearch);
        }
        self.runtime_events
            .push(AppRuntimeEvent::ClearCommitDetailSelection);
        self.runtime_events
            .push(AppRuntimeEvent::ClearDiffSelection);
        self.runtime_events.push(AppRuntimeEvent::PersistGraphScope);
    }

    fn confirm_hosts_picker_selection(&mut self) {
        let Some(target) = self.state.confirm_hosts_picker() else {
            return;
        };
        let recent = crate::features::hosts_picker::bump_recent(
            self.state.hosts_picker.recent.clone(),
            target.clone(),
        );
        self.state.hosts_picker.recent = recent.clone();
        self.runtime_events
            .push(AppRuntimeEvent::PersistHostsRecent(recent));
        self.state.request_session_swap(target);
    }

    fn open_graph_branch_picker_command(&mut self) {
        let outcome = self.state.open_graph_branch_picker();
        self.push_graph_scope_change_outcome(outcome.scope_change);
        if !outcome.opened {
            self.runtime_events
                .push(AppRuntimeEvent::GraphBranchPickerNotReady);
            return;
        }
        if let Some(short_ref) = outcome.stale_branch_short_ref {
            self.runtime_events
                .push(AppRuntimeEvent::GraphBranchPickerStaleBranch { short_ref });
        }
    }

    fn confirm_graph_branch_picker_selection(&mut self) {
        let Some(scope) = self.state.confirm_graph_branch_picker() else {
            return;
        };
        let outcome = self.state.set_graph_scope(scope);
        self.push_graph_scope_change_outcome(outcome);
    }

    fn apply_focused_preview_file_pick(
        &mut self,
        row: crate::FocusedPreviewFileRow,
        dark: bool,
        uses_three_col: bool,
    ) {
        match row.source {
            crate::FocusedPreviewFileSource::GitStaged => {
                self.state.select_file(row.path, true, dark);
                self.runtime_events
                    .push(AppRuntimeEvent::ClearDiffSelection);
            }
            crate::FocusedPreviewFileSource::GitUnstaged => {
                self.state.select_file(row.path, false, dark);
                self.runtime_events
                    .push(AppRuntimeEvent::ClearDiffSelection);
            }
            crate::FocusedPreviewFileSource::GraphCommit => {
                let outcome = self
                    .state
                    .load_commit_file_diff(&row.path, dark, uses_three_col);
                if outcome.clear_commit_detail_selection {
                    self.runtime_events
                        .push(AppRuntimeEvent::ClearCommitDetailSelection);
                }
                if outcome.clear_diff_selection {
                    self.runtime_events
                        .push(AppRuntimeEvent::ClearDiffSelection);
                }
            }
        }
    }

    fn push_commit_file_diff_outcome(&mut self, outcome: CommitFileDiffLoadOutcome) {
        if outcome.clear_commit_detail_selection {
            self.runtime_events
                .push(AppRuntimeEvent::ClearCommitDetailSelection);
        }
        if outcome.clear_diff_selection {
            self.runtime_events
                .push(AppRuntimeEvent::ClearDiffSelection);
        }
    }

    fn push_preview_merge_outcome(&mut self, outcome: crate::PreviewMergeOutcome) {
        if !outcome.accepted {
            return;
        }
        if outcome.clear_preview_selection {
            self.runtime_events
                .push(AppRuntimeEvent::ClearPreviewSelection);
        }
        if outcome.resolve_pending_highlight {
            self.runtime_events
                .push(AppRuntimeEvent::ResolvePendingHighlight);
        }
    }

    fn push_location_jump_outcome(&mut self, outcome: crate::app::JumpToLocationOutcome) {
        if outcome.restore_preview_cursor.is_some()
            || outcome.clear_commit_detail_selection
            || outcome.clear_diff_selection
        {
            self.runtime_events
                .push(AppRuntimeEvent::LocationJumped(outcome));
        }
    }

    fn jump_to_location_command(
        &mut self,
        target: LocationSnapshot,
        dark: bool,
        uses_three_col: bool,
    ) {
        let before = self.state.active_tab;
        let outcome = self.state.jump_to_location(target, dark, uses_three_col);
        let after = self.state.active_tab;
        self.push_tab_changed_for_state_transition(before, after);
        self.push_location_jump_outcome(outcome);
    }

    fn apply_preview_result_command(
        &mut self,
        generation: u64,
        result: Result<Option<PreviewDocument>, String>,
        view_height: usize,
    ) {
        let outcome = self
            .state
            .apply_preview_result(generation, result, view_height);
        self.push_preview_merge_outcome(outcome);
    }

    fn apply_lsp_refine_done_command(
        &mut self,
        generation: u64,
        epoch: u64,
        lang: reef_core::nav::NavLang,
        cache_key: String,
        rel_location: Option<reef_core::nav::LspLocation>,
        server_returned_location: bool,
    ) -> Option<LspRefineOutcome> {
        let epoch_fresh = epoch == self.state.nav_refine_epoch;
        if let Some(loc) = &rel_location
            && epoch_fresh
        {
            self.state
                .nav_refine_cache
                .insert((lang, cache_key.clone()), loc.clone());
        }

        let pending = self.state.nav_pending_lsp_jump.as_ref()?;
        if pending.lang != lang
            || pending.cache_key != cache_key
            || pending.generation != generation
        {
            return None;
        }
        let pending = self.state.nav_pending_lsp_jump.take()?;
        match rel_location {
            Some(location) => {
                self.state.close_nav_candidates();
                Some(LspRefineOutcome {
                    pending_jump: pending,
                    location,
                })
            }
            None => {
                let message = if server_returned_location {
                    "Definition is outside the workspace"
                } else {
                    "No definition found"
                };
                self.state.push_toast(crate::Toast::info(message));
                None
            }
        }
    }

    pub fn dispatch(&mut self, command: AppCommand) -> AppCommandOutcome {
        let mut dispatch_outcome = AppCommandOutcome::default();
        match command {
            AppCommand::Quit => {
                self.effects.push(AppEffect::Quit);
            }
            AppCommand::OpenHelp => self.state.open_help(),
            AppCommand::CloseHelp => self.state.close_help(),
            AppCommand::OpenSettings => {
                self.state.open_settings();
            }
            AppCommand::CloseSettings => self.state.close_settings(),
            AppCommand::SetSettingsPrefCache {
                theme_pref,
                editor_command,
            } => self
                .state
                .settings
                .set_pref_cache(theme_pref, editor_command),
            AppCommand::MoveSettingsSelection(delta) => self.state.settings.move_selection(delta),
            AppCommand::SelectSettingsRow(idx) => self.state.settings.select(idx),
            AppCommand::BeginSettingsEditorCommandEdit => {
                self.state.settings.begin_edit_editor_command();
            }
            AppCommand::CommitSettingsEditorCommandEdit => {
                dispatch_outcome.committed_editor_command =
                    self.state.settings.commit_editor_command();
            }
            AppCommand::CancelSettingsEditorCommandEdit => {
                self.state.settings.cancel_editor_command();
            }
            AppCommand::EditSettingsEditorCommand(op) => {
                if let Some(edit) = self.state.settings.editor_edit.as_mut() {
                    let _ = crate::text_input::apply_single_line_op(
                        op,
                        &mut edit.buffer,
                        &mut edit.cursor,
                    );
                }
            }
            AppCommand::PasteSettingsEditorCommand(text) => {
                if let Some(edit) = self.state.settings.editor_edit.as_mut() {
                    crate::text_input::paste_single_line(&text, &mut edit.buffer, &mut edit.cursor);
                }
            }
            AppCommand::CloseActivePalettes => self.state.close_active_palettes(),
            AppCommand::SetActiveTab(tab) => {
                let outcome = self.state.set_active_tab(tab);
                self.push_tab_change_outcome(outcome);
            }
            AppCommand::SetActivePanel(panel) => self.state.set_active_panel(panel),
            AppCommand::SetViewMode(mode) => self.state.view_mode = mode,
            AppCommand::ToggleSidebar => {
                let outcome = self.state.toggle_sidebar();
                self.runtime_events
                    .push(AppRuntimeEvent::SidebarToggled(outcome));
            }
            AppCommand::EnterFocusedPreview { uses_three_col } => {
                self.state.enter_focused_preview(uses_three_col);
            }
            AppCommand::ToggleFocusedPreview { uses_three_col } => {
                self.state.toggle_focused_preview(uses_three_col);
            }
            AppCommand::EnterFocusedPreviewWithFile {
                rel,
                dark,
                wants_decoded_image,
            } => {
                let outcome =
                    self.state
                        .enter_focused_preview_with_file(rel, dark, wants_decoded_image);
                self.push_tab_change_outcome(outcome);
            }
            AppCommand::ToggleDiffLayout => {
                self.state.toggle_diff_layout();
            }
            AppCommand::ToggleDiffMode { dark } => {
                self.state.toggle_diff_mode(dark);
            }
            AppCommand::CyclePanel {
                reverse,
                uses_three_col,
            } => {
                self.state.cycle_active_panel(reverse, uses_three_col);
            }
            AppCommand::NormalizeActivePanel { uses_three_col } => {
                dispatch_outcome.normalize_active_panel =
                    Some(self.state.normalize_active_panel(uses_three_col));
            }
            AppCommand::SetDiffLayout(layout) => self.state.diff_layout = layout,
            AppCommand::SetDiffMode(mode) => self.state.diff_mode = mode,
            AppCommand::LoadDiff { dark } => {
                self.state.load_diff(dark);
                self.runtime_events
                    .push(AppRuntimeEvent::ClearDiffSelection);
            }
            AppCommand::SetPreviewVerticalScroll(value) => {
                self.state.set_preview_vertical_scroll(value);
            }
            AppCommand::ClampPreviewVerticalScroll(max_scroll) => {
                self.state.clamp_preview_vertical_scroll(max_scroll);
            }
            AppCommand::CenterPreviewOnLine { line, view_height } => {
                self.state.center_preview_on_line(line, view_height);
            }
            AppCommand::SetPreviewHorizontalScroll(value) => {
                self.state.set_preview_horizontal_scroll(value);
            }
            AppCommand::ClampPreviewHorizontalScroll(max_scroll) => {
                self.state.clamp_preview_horizontal_scroll(max_scroll);
            }
            AppCommand::SetDiffVerticalScroll(value) => {
                self.state.set_diff_vertical_scroll(value);
            }
            AppCommand::SetDiffHorizontalScroll(value) => {
                self.state.set_diff_horizontal_scroll(value);
            }
            AppCommand::SetDiffScrollState {
                scroll,
                h_scroll,
                sbs_left_h_scroll,
                sbs_right_h_scroll,
            } => {
                self.state.set_diff_scroll_state(
                    scroll,
                    h_scroll,
                    sbs_left_h_scroll,
                    sbs_right_h_scroll,
                );
            }
            AppCommand::SetActiveDiffVerticalScroll(value) => {
                let _ = self.state.set_active_diff_vertical_scroll(value);
            }
            AppCommand::SetCommitFileDiffScrollState {
                scroll,
                h_scroll,
                sbs_left_h_scroll,
                sbs_right_h_scroll,
            } => {
                self.state.set_commit_file_diff_scroll_state(
                    scroll,
                    h_scroll,
                    sbs_left_h_scroll,
                    sbs_right_h_scroll,
                );
            }
            AppCommand::CommitReplaceInFiles => self.state.commit_replace_in_files(),
            AppCommand::RefreshStatus => self.state.refresh_status(),
            AppCommand::RefreshFileTree => self.state.refresh_file_tree(),
            AppCommand::RefreshFileTreeWithTarget(target) => {
                self.state.refresh_file_tree_with_target(target);
            }
            AppCommand::RevealFileTreePath(path) => self.state.reveal_file_tree_path(&path),
            AppCommand::NavigateFileTree(delta) => {
                self.state.navigate_file_tree_and_schedule_preview(delta)
            }
            AppCommand::NavigateGitFiles(delta) => self.state.navigate_files(delta),
            AppCommand::ScrollFileTree(delta) => self.state.scroll_tree_vertical(delta),
            AppCommand::ReconcileFileTreeScroll {
                visible_rows,
                selection_changed,
            } => {
                self.state
                    .reconcile_file_tree_scroll(visible_rows, selection_changed);
            }
            AppCommand::ToggleFileTreeExpand(idx) => {
                self.state.toggle_file_tree_expand_and_refresh(idx);
            }
            AppCommand::ActivateFileTreeEntryAtIndex(idx) => {
                self.state.activate_file_tree_entry_at_index(idx);
            }
            AppCommand::SelectFileTreeEntry(idx) => {
                self.state.file_tree.state.selected = idx;
                if let Some(entry) = self.state.file_tree.selected_entry()
                    && !entry.is_dir
                {
                    self.state.preview_schedule = Some((entry.path.clone(), Instant::now()));
                }
            }
            AppCommand::ActivateSelectedFileTreeEntry => {
                self.state.activate_selected_file_tree_entry();
            }
            AppCommand::RequestEditSelectedFileTreeEntry => {
                self.state.request_edit_selected_file_tree_entry();
            }
            AppCommand::RequestEditSelectedGitFile => {
                self.state.request_edit_selected_git_file();
            }
            AppCommand::ClearFileTreeSelectionAndEdit => {
                self.state.clear_file_tree_selection_and_edit();
            }
            AppCommand::ToggleCurrentFileSelection => {
                self.state.toggle_current_file_selection();
            }
            AppCommand::ClearFileSelection => {
                self.state.clear_file_selection();
            }
            AppCommand::ExtendFileSelectionAfterTreeNav(delta) => {
                self.state.extend_file_selection_after_tree_nav(delta);
            }
            AppCommand::ToggleFileSelectionAtIndex(idx) => {
                self.state.toggle_file_selection_at_index(idx);
            }
            AppCommand::LoadSelectedPreview => self.state.load_preview(),
            AppCommand::LoadPreview(path) => self.state.load_preview_for_path(path),
            AppCommand::ReloadPreviewNow {
                dark,
                wants_decoded_image,
            } => self.state.reload_preview_now(dark, wants_decoded_image),
            AppCommand::ApplyPreviewResult {
                generation,
                result,
                preview_view_h,
            } => {
                self.apply_preview_result_command(generation, result, preview_view_h);
            }
            AppCommand::PreviewScroll(delta) => {
                self.state.scroll_preview_vertical(delta);
            }
            AppCommand::PreviewHorizontalScroll(delta) => {
                self.state.scroll_preview_horizontal(delta);
            }
            AppCommand::DiffScroll(delta) => {
                self.state.scroll_diff_vertical(delta);
            }
            AppCommand::DiffHorizontalScroll(delta) => {
                self.state.scroll_diff_horizontal(delta);
            }
            AppCommand::ScrollCommitDetailVertical(delta) => {
                self.state.scroll_commit_detail_vertical(delta);
            }
            AppCommand::SetCommitDetailVerticalScroll(value) => {
                self.state.set_commit_detail_vertical_scroll(value);
            }
            AppCommand::ClampCommitDetailVerticalScroll(max_scroll) => {
                self.state.clamp_commit_detail_vertical_scroll(max_scroll);
            }
            AppCommand::ClampCommitDetailDiffHorizontalScroll(max_scroll) => {
                self.state
                    .clamp_commit_detail_diff_horizontal_scroll(max_scroll);
            }
            AppCommand::ClampCommitDetailSbsHorizontalScrolls { left, right } => {
                self.state
                    .clamp_commit_detail_sbs_horizontal_scrolls(left, right);
            }
            AppCommand::ScrollCommitDetailFileDiffVertical(delta) => {
                self.state.scroll_commit_detail_file_diff_vertical(delta);
            }
            AppCommand::SetCommitDetailFileDiffVerticalScroll(value) => {
                self.state
                    .set_commit_detail_file_diff_vertical_scroll(value);
            }
            AppCommand::OpenDbGoto => self.state.open_db_goto(),
            AppCommand::CloseDbGoto => self.state.close_db_goto(),
            AppCommand::DbNavigate(action) => self.state.db_navigate(action),
            AppCommand::DbToggleSchema(name) => self.state.db_toggle_schema(&name),
            AppCommand::DbSelectObject(key) => self.state.db_select_object(key),
            AppCommand::DbNavigateToPage(page) => self.state.db_navigate_to_page(page),
            AppCommand::EditDbGoto(op) => {
                let _ = self.state.edit_db_goto_input(op);
            }
            AppCommand::PasteDbGoto(text) => {
                self.state.paste_db_goto_input(&text);
            }
            AppCommand::ConfirmDbGoto => {
                if let Some(page) = self.state.confirm_db_goto() {
                    self.state.db_navigate_to_page(page);
                }
            }
            AppCommand::HorizontalScrollAtColumn {
                column,
                total_width,
                delta,
            } => self
                .state
                .scroll_horizontal_at_column(column, total_width, delta),
            AppCommand::OpenQuickOpen => self.state.open_quick_open(),
            AppCommand::CloseQuickOpen => self.state.close_quick_open(),
            AppCommand::ApplyQuickOpenPickerInput {
                input,
                visible_rows,
            } => match self.state.apply_quick_open_picker_input(input) {
                PickerInputOutcome::Cancel => self.state.close_quick_open(),
                PickerInputOutcome::Quit => {
                    self.state.close_quick_open();
                    self.effects.push(AppEffect::Quit);
                }
                PickerInputOutcome::Confirm => self
                    .runtime_events
                    .push(AppRuntimeEvent::AcceptQuickOpenSelection),
                PickerInputOutcome::SelectionMoved => {
                    self.state.ensure_quick_open_selection_visible(visible_rows);
                }
                PickerInputOutcome::Edited
                | PickerInputOutcome::Rejected
                | PickerInputOutcome::CursorMoved
                | PickerInputOutcome::Unhandled => {}
            },
            AppCommand::MoveQuickOpenSelection {
                delta,
                visible_rows,
            } => {
                self.state.move_quick_open_selection(delta);
                self.state.ensure_quick_open_selection_visible(visible_rows);
            }
            AppCommand::SelectQuickOpenMatch { idx, visible_rows } => {
                self.state.select_quick_open_match(idx);
                self.state.ensure_quick_open_selection_visible(visible_rows);
            }
            AppCommand::PasteQuickOpen(text) => self.state.paste_quick_open_filter(&text),
            AppCommand::AcceptQuickOpenSelection { mru_cap } => {
                let Some(rel) = self.state.quick_open_selected_path() else {
                    self.state.close_quick_open();
                    return dispatch_outcome;
                };
                self.state.bump_quick_open_mru(rel.clone(), mru_cap);
                let outcome = self.state.accept_quick_open_path(rel);
                self.push_tab_change_outcome(outcome);
                self.runtime_events
                    .push(AppRuntimeEvent::PersistQuickOpenMru(
                        reef_core::quick_open::encode_mru(&self.state.quick_open.mru),
                    ));
            }
            AppCommand::OpenGlobalSearch { seed } => self.state.open_global_search(seed),
            AppCommand::CloseGlobalSearch => self.state.close_global_search(),
            AppCommand::OpenGlobalReplaceTab => {
                let outcome = self.state.open_global_replace_tab();
                self.push_tab_change_outcome(outcome);
            }
            AppCommand::PinGlobalSearchToTab => {
                let outcome = self.state.pin_global_search_to_tab();
                self.push_tab_change_outcome(outcome);
            }
            AppCommand::AcceptGlobalSearchHit(hit) => {
                let outcome = self.state.accept_global_search_hit(hit);
                self.push_tab_change_outcome(outcome);
            }
            AppCommand::BeginVimSearch { target, backwards } => {
                self.state.begin_vim_search(target, backwards);
            }
            AppCommand::ConfirmVimSearch => self.state.confirm_vim_search(),
            AppCommand::CancelVimSearch { dark } => self.state.cancel_vim_search(dark),
            AppCommand::EditVimSearchInput(op) => {
                if self.state.edit_vim_search_input(op) == TextEditOutcome::Edited {
                    self.runtime_events
                        .push(AppRuntimeEvent::RecomputeVimSearch);
                }
            }
            AppCommand::PasteVimSearchInput(text) => {
                if self.state.paste_vim_search_input(&text) {
                    self.runtime_events
                        .push(AppRuntimeEvent::RecomputeVimSearch);
                }
            }
            AppCommand::RecomputeVimSearch {
                rows,
                dark,
                viewport,
            } => self.state.recompute_vim_search(rows, dark, viewport),
            AppCommand::StepVimSearch {
                reverse,
                dark,
                viewport,
            } => self.state.step_vim_search(reverse, dark, viewport),
            AppCommand::ClearVimSearch => self.state.clear_vim_search(),
            AppCommand::ClearVimSearchIfTarget(target) => {
                self.state.clear_vim_search_if_target(target);
            }
            AppCommand::BeginFindWidget { target, query } => {
                self.state.begin_find_widget(target, query);
                self.runtime_events
                    .push(AppRuntimeEvent::RecomputeFindWidget);
            }
            AppCommand::CloseFindWidget { dark } => self.state.close_find_widget(dark),
            AppCommand::EditFindWidgetInput(op) => {
                if self.state.edit_find_widget_input(op) == TextEditOutcome::Edited {
                    self.runtime_events
                        .push(AppRuntimeEvent::RecomputeFindWidget);
                }
            }
            AppCommand::PasteFindWidgetInput(text) => {
                if self.state.paste_find_widget_input(&text) {
                    self.runtime_events
                        .push(AppRuntimeEvent::RecomputeFindWidget);
                }
            }
            AppCommand::ToggleFindWidgetOption(option) => {
                self.state.toggle_find_widget_option(option);
                self.runtime_events
                    .push(AppRuntimeEvent::RecomputeFindWidget);
            }
            AppCommand::RecomputeFindWidget {
                rows,
                dark,
                viewport,
            } => self.state.recompute_find_widget(rows, dark, viewport),
            AppCommand::StepFindWidget { reverse, viewport } => {
                self.state.step_find_widget(reverse, viewport);
            }
            AppCommand::MoveGlobalSearchSelection {
                delta,
                visible_rows,
            } => {
                self.state.move_global_search_selection(delta);
                self.state
                    .ensure_global_search_selection_visible(visible_rows);
            }
            AppCommand::ApplyGlobalSearchPickerInput {
                input,
                now,
                visible_rows,
            } => match self.state.apply_global_search_picker_input(input, now) {
                PickerInputOutcome::Cancel => self.state.close_global_search(),
                PickerInputOutcome::Quit => {
                    self.state.close_global_search();
                    self.effects.push(AppEffect::Quit);
                }
                PickerInputOutcome::Confirm => self
                    .runtime_events
                    .push(AppRuntimeEvent::AcceptGlobalSearchSelection),
                PickerInputOutcome::SelectionMoved => {
                    self.state
                        .ensure_global_search_selection_visible(visible_rows);
                }
                PickerInputOutcome::Edited
                | PickerInputOutcome::Rejected
                | PickerInputOutcome::CursorMoved
                | PickerInputOutcome::Unhandled => {}
            },
            AppCommand::PasteGlobalSearchOverlay { text, now } => {
                self.state.paste_global_search_overlay(&text, now);
            }
            AppCommand::PasteGlobalSearchTab { text, now } => {
                self.state.paste_global_search_tab(&text, now);
            }
            AppCommand::ReloadGlobalSearch { now } => self.state.reload_global_search(now),
            AppCommand::ScheduleGlobalSearchPreviewSync { now } => {
                self.state.schedule_global_search_preview_sync(now);
            }
            AppCommand::ClearGlobalSearchPreviewSync => {
                self.state.clear_global_search_preview_sync();
            }
            AppCommand::DrainGlobalSearchPreviewSyncDebounce { now } => {
                dispatch_outcome.global_search_preview_sync_due =
                    self.state.consume_global_search_preview_sync_due(now);
            }
            AppCommand::SyncGlobalSearchPreviewToSelected => {
                self.state.clear_global_search_preview_sync();
                if let Some(hit) = self.state.selected_global_search_hit() {
                    self.state.set_preview_highlight_persistent(
                        hit.path.clone(),
                        hit.line,
                        hit.byte_range.clone(),
                    );
                    self.state.load_preview_for_path(hit.path);
                }
            }
            AppCommand::FocusGlobalSearchFindInput => self.state.focus_global_search_find_input(),
            AppCommand::FocusGlobalSearchReplaceInput => {
                self.state.focus_global_search_replace_input();
            }
            AppCommand::FocusGlobalSearchList => self.state.focus_global_search_list(),
            AppCommand::CycleGlobalSearchFocusForward => {
                self.state.cycle_global_search_focus_forward();
            }
            AppCommand::CycleGlobalSearchFocusBackward => {
                self.state.cycle_global_search_focus_backward();
            }
            AppCommand::ToggleGlobalSearchReplace => self.state.toggle_global_search_replace(),
            AppCommand::ToggleGlobalSearchReplaceForSearchTab => {
                self.state.toggle_global_search_replace_for_search_tab();
            }
            AppCommand::ToggleGlobalSearchMatchExcluded(idx) => {
                self.state.toggle_global_search_match_excluded(idx);
            }
            AppCommand::SelectGlobalSearchResult { idx, visible_rows } => {
                self.state.select_global_search_result(idx);
                self.state
                    .ensure_global_search_selection_visible(visible_rows);
            }
            AppCommand::ScrollGlobalSearchResultsHorizontal(delta) => {
                self.state.scroll_global_search_results_horizontal(delta);
            }
            AppCommand::SetGlobalSearchResultsHorizontalScroll(value) => {
                self.state
                    .set_global_search_results_horizontal_scroll(value);
            }
            AppCommand::EditGlobalSearchFindInput { op, now } => {
                let _ = self.state.edit_global_search_find_input(op, now);
            }
            AppCommand::EditGlobalSearchReplaceInput(op) => {
                let _ = self.state.edit_global_search_replace_input(op);
            }
            AppCommand::SetGraphScope(scope) => {
                let outcome = self.state.set_graph_scope(scope);
                self.push_graph_scope_change_outcome(outcome);
            }
            AppCommand::SetSplitPercent(percent) => self.state.set_split_percent(percent),
            AppCommand::SetGraphDiffSplitPercent(percent) => {
                self.state.set_graph_diff_split_percent(percent);
            }
            AppCommand::SetCtrlHoverTarget(target) => self.state.set_ctrl_hover_target(target),
            AppCommand::PushToast(toast) => self.state.push_toast(toast),
            AppCommand::RefreshGraph => self.state.refresh_graph(),
            AppCommand::RefreshGraphUncached => self.state.refresh_graph_uncached_and_mark(),
            AppCommand::LoadCommitDetail => {
                self.state.load_commit_detail();
                self.runtime_events
                    .push(AppRuntimeEvent::ClearCommitDetailSelection);
            }
            AppCommand::LoadCommitRangeDetail => {
                self.state.load_commit_range_detail();
                self.runtime_events
                    .push(AppRuntimeEvent::ClearCommitDetailSelection);
            }
            AppCommand::ReloadGraphSelection => {
                self.state.reload_graph_selection();
                self.runtime_events
                    .push(AppRuntimeEvent::ClearCommitDetailSelection);
            }
            AppCommand::LoadCommitFileDiff {
                path,
                dark,
                uses_three_col,
            } => {
                let outcome = self
                    .state
                    .load_commit_file_diff(&path, dark, uses_three_col);
                self.push_commit_file_diff_outcome(outcome);
            }
            AppCommand::ReloadCommitFileDiff {
                dark,
                uses_three_col,
            } => {
                let outcome = self.state.reload_commit_file_diff(dark, uses_three_col);
                self.push_commit_file_diff_outcome(outcome);
            }
            AppCommand::ScrollGraphVertical(delta) => self.state.scroll_graph_vertical(delta),
            AppCommand::ReconcileGraphScroll {
                visible_rows,
                selection_changed,
            } => {
                self.state
                    .reconcile_graph_scroll(visible_rows, selection_changed);
            }
            AppCommand::MoveGraphSelection(delta) => {
                let before_idx = self.state.git_graph.selected_idx;
                let before_range_detail = self.state.commit_detail.range_detail.is_some();
                self.state.move_graph_selection(delta);
                if !self.state.git_graph.rows.is_empty()
                    && (self.state.git_graph.selected_idx != before_idx
                        || (before_range_detail && !self.state.git_graph.is_range()))
                {
                    self.runtime_events
                        .push(AppRuntimeEvent::ClearCommitDetailSelection);
                }
            }
            AppCommand::SelectGraphCommit(oid) => {
                self.state.select_graph_commit(&oid);
                self.runtime_events
                    .push(AppRuntimeEvent::ClearCommitDetailSelection);
            }
            AppCommand::FocusGraphCommit(oid) => {
                self.state.focus_graph_commit(&oid);
                self.runtime_events
                    .push(AppRuntimeEvent::ClearCommitDetailSelection);
            }
            AppCommand::ExtendGraphSelection(delta) => {
                let before_idx = self.state.git_graph.selected_idx;
                self.state.extend_graph_selection(delta);
                if self.state.git_graph.selected_idx != before_idx {
                    self.runtime_events
                        .push(AppRuntimeEvent::ClearCommitDetailSelection);
                }
            }
            AppCommand::ClearGraphRange => {
                let had_anchor = self.state.git_graph.selection_anchor.is_some();
                self.state.clear_graph_range();
                if had_anchor {
                    self.runtime_events
                        .push(AppRuntimeEvent::ClearCommitDetailSelection);
                }
            }
            AppCommand::ResetGraphVisualAnchor => self.state.reset_graph_visual_anchor(),
            AppCommand::SetCommitEditing(editing) => self.state.set_commit_editing(editing),
            AppCommand::EditCommitMessage(op) => {
                dispatch_outcome.text_edit = Some(self.state.edit_commit_message(op));
            }
            AppCommand::ToggleStatusTreeMode => self.state.toggle_status_tree_mode(),
            AppCommand::ToggleCommitDiffLayout => self.state.toggle_commit_diff_layout(),
            AppCommand::ToggleCommitDiffMode => self.state.toggle_commit_diff_mode(),
            AppCommand::ToggleCommitFilesTreeMode => self.state.toggle_commit_files_tree_mode(),
            AppCommand::ToggleCommitFilesDirCollapsed(path) => {
                self.state.toggle_commit_files_dir_collapsed(&path);
            }
            AppCommand::ScrollGitStatusVertical(delta) => {
                self.state.scroll_git_status_vertical(delta);
            }
            AppCommand::ClampGitStatusScroll(max_scroll) => {
                self.state.clamp_git_status_scroll(max_scroll);
            }
            AppCommand::SelectGitFile {
                path,
                is_staged,
                dark,
            } => {
                self.state.select_file(path, is_staged, dark);
                self.runtime_events
                    .push(AppRuntimeEvent::ClearDiffSelection);
            }
            AppCommand::SelectGitFileForDiscard {
                path,
                is_staged,
                dark,
            } => {
                self.state
                    .select_git_file_for_discard(path, is_staged, dark);
                self.runtime_events
                    .push(AppRuntimeEvent::ClearDiffSelection);
            }
            AppCommand::StageFile { path, dark } => self.state.stage_file(&path, dark),
            AppCommand::UnstageFile { path, dark } => self.state.unstage_file(&path, dark),
            AppCommand::StageAll { dark } => self.state.stage_all(dark),
            AppCommand::UnstageAll { dark } => self.state.unstage_all(dark),
            AppCommand::StageFolder { path, dark } => self.state.stage_folder(&path, dark),
            AppCommand::UnstageFolder { path, dark } => self.state.unstage_folder(&path, dark),
            AppCommand::ConfirmDiscard { dark } => self.state.confirm_discard(dark),
            AppCommand::RunCommit => self.state.run_commit(),
            AppCommand::RunPush { force } => self.state.run_push(force),
            AppCommand::ToggleStagedSection => self.state.toggle_staged_section(),
            AppCommand::ToggleUnstagedSection => self.state.toggle_unstaged_section(),
            AppCommand::ToggleGitStatusDir { is_staged, path } => {
                self.state.toggle_git_status_dir(is_staged, &path);
            }
            AppCommand::PromptDiscardFile { is_staged, path } => {
                self.state.prompt_discard_file(is_staged, path);
            }
            AppCommand::PromptDiscardFolder { is_staged, path } => {
                self.state.prompt_discard_folder(is_staged, path);
            }
            AppCommand::PromptDiscardSection { is_staged } => {
                self.state.prompt_discard_section(is_staged);
            }
            AppCommand::CancelGitConfirmations => self.state.cancel_git_confirmations(),
            AppCommand::DismissPushError => self.state.dismiss_push_error(),
            AppCommand::PromptPush { force } => self.state.prompt_push(force),
            AppCommand::ConfirmPush { force } => self.state.confirm_push(force),
            AppCommand::DismissCommitError => self.state.dismiss_commit_error(),
            AppCommand::OpenHostsPicker { hosts, recent } => {
                self.state.open_hosts_picker(hosts, recent);
            }
            AppCommand::CloseHostsPicker => self.state.close_hosts_picker(),
            AppCommand::EnterHostsPickerPathMode => self.state.hosts_picker.enter_path_mode(),
            AppCommand::ReturnHostsPickerToSearchMode => {
                self.state.return_hosts_picker_to_search_mode();
            }
            AppCommand::ApplyHostsPickerSearchInput(input) => {
                match self.state.apply_hosts_picker_search_input(input) {
                    PickerInputOutcome::Cancel => self.state.close_hosts_picker(),
                    PickerInputOutcome::Quit => {
                        self.state.close_hosts_picker();
                        self.effects.push(AppEffect::Quit);
                    }
                    PickerInputOutcome::Confirm => self.confirm_hosts_picker_selection(),
                    PickerInputOutcome::Edited
                    | PickerInputOutcome::Rejected
                    | PickerInputOutcome::SelectionMoved
                    | PickerInputOutcome::CursorMoved
                    | PickerInputOutcome::Unhandled => {}
                }
            }
            AppCommand::EditHostsPickerPathInput(op) => {
                let _ = self.state.edit_hosts_picker_path_input(op);
            }
            AppCommand::ConfirmHostsPicker => self.confirm_hosts_picker_selection(),
            AppCommand::PasteHostsPicker(text) => self.state.paste_hosts_picker(&text),
            AppCommand::SelectHostsPickerRow(idx) => self.state.select_hosts_picker_row(idx),
            AppCommand::MoveHostsPickerSelection(delta) => {
                self.state.hosts_picker.move_selection(delta);
            }
            AppCommand::OpenGraphBranchPicker => self.open_graph_branch_picker_command(),
            AppCommand::CloseGraphBranchPicker => self.state.close_graph_branch_picker(),
            AppCommand::ApplyGraphBranchPickerInput {
                input,
                visible_rows,
            } => match self.state.apply_graph_branch_picker_input(input) {
                PickerInputOutcome::Cancel => self.state.close_graph_branch_picker(),
                PickerInputOutcome::Quit => {
                    self.state.close_graph_branch_picker();
                    self.effects.push(AppEffect::Quit);
                }
                PickerInputOutcome::Confirm => self.confirm_graph_branch_picker_selection(),
                PickerInputOutcome::SelectionMoved => {
                    self.state
                        .ensure_graph_branch_picker_selection_visible(visible_rows);
                }
                PickerInputOutcome::Edited
                | PickerInputOutcome::Rejected
                | PickerInputOutcome::CursorMoved
                | PickerInputOutcome::Unhandled => {}
            },
            AppCommand::ConfirmGraphBranchPicker => self.confirm_graph_branch_picker_selection(),
            AppCommand::PasteGraphBranchPicker(text) => {
                self.state.paste_graph_branch_picker(&text);
            }
            AppCommand::SelectGraphBranchPickerRow { idx, visible_rows } => {
                self.state.select_graph_branch_picker_row(idx);
                self.state
                    .ensure_graph_branch_picker_selection_visible(visible_rows);
            }
            AppCommand::MoveGraphBranchPickerSelection {
                delta,
                visible_rows,
            } => {
                self.state.graph_branch_picker.move_selection(delta);
                self.state
                    .ensure_graph_branch_picker_selection_visible(visible_rows);
            }
            AppCommand::ClearPreviewHighlight => self.state.clear_preview_highlight(),
            AppCommand::SetPreviewHighlight {
                path,
                row,
                byte_range,
                fade,
            } => self
                .state
                .set_preview_highlight_with_fade(path, row, byte_range, fade),
            AppCommand::SetPreviewHighlightPendingUtf16(pending_utf16) => {
                self.state
                    .set_preview_highlight_pending_utf16(pending_utf16);
            }
            AppCommand::ResolvePreviewHighlightByteRange(byte_range) => {
                if let Some(highlight) = self.state.preview_highlight.as_mut() {
                    highlight.byte_range = byte_range;
                    highlight.pending_utf16 = None;
                }
            }
            AppCommand::StartPreviewHighlightCounting(since) => {
                self.state.start_preview_highlight_counting(since);
            }
            AppCommand::RestorePreviewScrollAndClearHighlight(target) => {
                self.state
                    .restore_preview_scroll_and_clear_highlight(&target);
            }
            AppCommand::ApplyLspStateChange { lang, state } => {
                self.state.apply_lsp_state_change(lang, state);
            }
            AppCommand::RefreshLspInstalled => self.state.refresh_lsp_installed(),
            AppCommand::OpenNavCandidates(popup) => {
                self.state.open_nav_candidates(popup);
            }
            AppCommand::SetNavPendingLspJump(jump) => {
                self.state.set_nav_pending_lsp_jump(jump);
            }
            AppCommand::SelectNavCandidate(idx) => {
                if let Some(popup) = self.state.nav_candidates.as_mut()
                    && idx < popup.candidates.len()
                {
                    popup.selected = idx;
                }
            }
            AppCommand::CloseNavCandidates => self.state.close_nav_candidates(),
            AppCommand::MoveNavCandidatesSelection(delta) => {
                self.state.move_nav_candidates_selection(delta);
            }
            AppCommand::ScrollNavCandidates(delta) => self.state.scroll_nav_candidates(delta),
            AppCommand::PushLocationHistory(snapshot) => {
                self.state.push_location_history(snapshot);
            }
            AppCommand::JumpToLocation {
                target,
                dark,
                uses_three_col,
            } => self.jump_to_location_command(target, dark, uses_three_col),
            AppCommand::LocationBack {
                current,
                dark,
                uses_three_col,
            } => {
                if let Some(target) = self.state.location_history.back(current) {
                    self.jump_to_location_command(target, dark, uses_three_col);
                }
            }
            AppCommand::LocationForward {
                current,
                dark,
                uses_three_col,
            } => {
                if let Some(target) = self.state.location_history.forward(current) {
                    self.jump_to_location_command(target, dark, uses_three_col);
                }
            }
            AppCommand::NextNavRefineGeneration => {
                dispatch_outcome.nav_refine_generation =
                    Some(self.state.next_nav_refine_generation());
            }
            AppCommand::DispatchLspRefineDefinition {
                generation,
                lang,
                cache_key,
                workspace_root,
                abs_file,
                source,
                line,
                utf16_col,
            } => self.state.tasks.lsp_refine_definition(
                generation,
                self.state.nav_refine_epoch,
                lang,
                cache_key,
                workspace_root,
                abs_file,
                source,
                line,
                utf16_col,
            ),
            AppCommand::DispatchNavWorkspaceBuild => self.state.dispatch_nav_workspace_build(),
            AppCommand::OpenFocusedPreviewFiles => self.state.open_focused_preview_files(),
            AppCommand::CloseFocusedPreviewFiles => self.state.close_focused_preview_files(),
            AppCommand::ToggleFocusedPreviewFiles => self.state.toggle_focused_preview_files(),
            AppCommand::MoveFocusedPreviewFilesSelection(delta) => {
                self.state.move_focused_preview_files_selection(delta);
            }
            AppCommand::SetFocusedPreviewFilesSelection(idx) => {
                self.state.set_focused_preview_files_selection(idx);
            }
            AppCommand::ConfirmFocusedPreviewFilesSelection {
                dark,
                uses_three_col,
            } => {
                if let Some(row) = self.state.confirm_focused_preview_files_selection() {
                    self.apply_focused_preview_file_pick(row, dark, uses_three_col);
                }
            }
            AppCommand::PickFocusedPreviewFile {
                idx,
                dark,
                uses_three_col,
            } => {
                if let Some(row) = self.state.focused_preview_pick_row(idx) {
                    self.apply_focused_preview_file_pick(row, dark, uses_three_col);
                }
            }
            AppCommand::CloseFocusedPreview => self.state.close_focused_preview(),
            AppCommand::NavigateTreeContextMenu(delta) => {
                self.state.navigate_tree_context_menu(delta);
            }
            AppCommand::OpenTreeContextMenu {
                target_entry_idx,
                anchor,
            } => self.state.open_tree_context_menu(target_entry_idx, anchor),
            AppCommand::CloseTreeContextMenu => self.state.close_tree_context_menu(),
            AppCommand::RequestTreeDeleteConfirm { path, is_dir, hard } => {
                self.state.request_tree_delete_confirm(path, is_dir, hard);
            }
            AppCommand::ExecuteTreeDelete(pending) => {
                if self.state.fs_mutation_load.loading {
                    self.runtime_events.push(AppRuntimeEvent::FileActionNotice(
                        crate::FileActionNotice::TreeOpInFlight,
                    ));
                    self.state
                        .request_confirm(ConfirmRequest::TreeDelete(pending));
                } else {
                    let generation = self.state.begin_fs_mutation();
                    let rel = pending
                        .path
                        .strip_prefix(&self.state.file_tree.root)
                        .map(Path::to_path_buf)
                        .unwrap_or(pending.path);
                    if pending.hard {
                        self.state.tasks.hard_delete_paths(
                            generation,
                            Arc::clone(&self.state.backend),
                            vec![rel],
                            pending.display_name,
                        );
                    } else {
                        self.state.tasks.trash_paths(
                            generation,
                            Arc::clone(&self.state.backend),
                            vec![rel],
                            pending.display_name,
                        );
                    }
                }
            }
            AppCommand::RequestConfirm(request) => self.state.request_confirm(request),
            AppCommand::DismissConfirm => self.state.dismiss_confirm(),
            AppCommand::MarkCut(paths) => self.state.mark_cut(paths),
            AppCommand::MarkCopy(paths) => self.state.mark_copy(paths),
            AppCommand::ClearClipboard => self.state.clear_clipboard(),
            AppCommand::PasteInto(dest_rel) => {
                let events = self.state.paste_into(dest_rel);
                self.runtime_events.extend(events);
            }
            AppCommand::DuplicateSelection => {
                let events = self.state.duplicate_selection();
                self.runtime_events.extend(events);
            }
            AppCommand::DuplicatePaths(sources) => {
                let events = self.state.duplicate_paths(sources);
                self.runtime_events.extend(events);
            }
            AppCommand::ResolvePasteConflict {
                resolution,
                apply_to_all,
            } => {
                let events = self.state.resolve_paste_conflict(resolution, apply_to_all);
                self.runtime_events.extend(events);
            }
            AppCommand::CancelPasteConflict => {
                let events = self.state.cancel_paste_conflict();
                self.runtime_events.extend(events);
            }
            AppCommand::BeginTreeDrag { sources, mods } => {
                self.state.begin_tree_drag(sources, mods);
            }
            AppCommand::DropTreeDrag { release_mods } => {
                if let Some((is_copy, dest_rel, sources)) =
                    self.state.resolve_tree_drag_drop(release_mods)
                {
                    let events = self.state.drop_tree_drag(is_copy, dest_rel, sources);
                    self.runtime_events.extend(events);
                }
            }
            AppCommand::ArmFileTreeDragPress {
                idx,
                col,
                row,
                mods,
            } => self.state.arm_file_tree_drag_press(idx, col, row, mods),
            AppCommand::AutoExpandTreeDragHover { now } => {
                self.state.auto_expand_tree_drag_hover(now);
            }
            AppCommand::UpdateTreeDragHover(idx) => self.state.update_tree_drag_hover(idx),
            AppCommand::UpdateTreeDragModifiers(mods) => {
                self.state.update_tree_drag_modifiers(mods);
            }
            AppCommand::CancelTreeDrag => self.state.cancel_tree_drag(),
            AppCommand::EnterPlaceMode(sources) => {
                let events = self.state.enter_place_mode(sources);
                self.runtime_events.extend(events);
            }
            AppCommand::ExitPlaceMode => self.state.exit_place_mode(),
            AppCommand::RequestFileCopy { sources, dest_dir } => {
                let events = self.state.request_file_copy(sources, dest_dir);
                self.runtime_events.extend(events);
            }
            AppCommand::UpdatePlaceModeHover(idx) => self.state.place_mode.update_hover(idx),
            AppCommand::AutoExpandPlaceModeHover { now } => {
                self.state.auto_expand_place_mode_hover(now);
            }
            AppCommand::CommitTreeEdit => self.state.commit_tree_edit(),
            AppCommand::CancelTreeEdit => self.state.cancel_tree_edit(),
            AppCommand::EditTreeEditInput(op) => {
                let _ = self.state.edit_tree_edit_input(op);
            }
            AppCommand::PasteTreeEdit(text) => self.state.paste_tree_edit(&text),
            AppCommand::BeginTreeEdit {
                mode,
                parent_dir,
                rename_target,
                anchor_idx,
            } => {
                let events =
                    self.state
                        .begin_tree_edit(mode, parent_dir, rename_target, anchor_idx);
                self.runtime_events.extend(events);
            }
            AppCommand::CollapseAllTreeEntries => {
                self.state.file_tree.collapse_all();
            }
            AppCommand::ExtendFileSelectionToIndex(idx) => {
                self.state.extend_file_selection_to_index(idx);
            }
            AppCommand::PasteCommitMessage(text) => self.state.paste_commit_message(&text),
            AppCommand::CopyToClipboard {
                text,
                success,
                failure,
            } => {
                self.effects.push(AppEffect::CopyToClipboard {
                    text,
                    success,
                    failure,
                });
            }
            AppCommand::OpenUrl(url) => {
                self.effects.push(AppEffect::OpenUrl(url));
            }
        }
        dispatch_outcome
    }

    pub fn tick(&mut self, now: Instant, options: TickOptions) {
        loop {
            match self.state.tasks.try_recv() {
                Ok(result) => match result {
                    WorkerResult::Preview { generation, result } => {
                        self.runtime_events
                            .push(AppRuntimeEvent::PreviewResultForAdapter { generation, result });
                    }
                    WorkerResult::LspRefineDone {
                        generation,
                        epoch,
                        lang,
                        identifier,
                        rel_location,
                        server_returned_location,
                    } => {
                        if let Some(outcome) = self.apply_lsp_refine_done_command(
                            generation,
                            epoch,
                            lang,
                            identifier,
                            rel_location,
                            server_returned_location,
                        ) {
                            self.runtime_events
                                .push(AppRuntimeEvent::LspRefineJump(outcome));
                        }
                    }
                    result => {
                        let events = self.state.apply_worker_result_core(result, now);
                        self.runtime_events.extend(events);
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        self.state.maybe_kick_global_search(now);
        self.state.drain_fs_watcher_events();
        if self.state.nav_workspace_load.should_request() {
            self.state.dispatch_nav_workspace_build();
        }
        self.state.drain_preview_schedule(now, options);
        self.state.drain_prefetch_schedule(now, options);
        self.state.kick_active_tab_work(now, options);
    }

    pub fn snapshot(&self) -> AppSnapshot {
        AppSnapshot::from_state(&self.state)
    }

    pub fn settings(&self) -> &SettingsState {
        &self.state.settings
    }

    pub fn active_tab(&self) -> AppTab {
        self.state.active_tab
    }

    pub fn active_panel(&self) -> AppPanel {
        self.state.active_panel
    }

    pub fn view_mode(&self) -> ViewMode {
        self.state.view_mode
    }

    pub fn backend_has_repo(&self) -> bool {
        self.state.backend.has_repo()
    }

    pub fn backend_is_remote(&self) -> bool {
        self.state.backend.is_remote()
    }

    pub fn workdir_path(&self) -> PathBuf {
        self.state.backend.workdir_path()
    }

    pub fn markdown_file_link_target(&self, target: &str) -> Option<PathBuf> {
        let path_part = reef_core::markdown::link_path_without_fragment(target)?;
        let path = Path::new(path_part);
        if path.is_absolute() {
            if self.state.backend.is_remote() {
                return reef_core::markdown::remote_absolute_link_under_root(
                    &self.state.backend.workdir_path(),
                    path,
                );
            }
            return reef_io::workdir_relative_path(&self.state.backend.workdir_path(), path);
        }

        let preview_path = self
            .state
            .preview_content
            .as_ref()
            .map(|preview| Path::new(&preview.path))?;
        reef_core::markdown::resolve_relative_link(preview_path, target)
    }

    pub fn editor_launch_spec(&self, rel_path: &Path) -> Result<EditorLaunchSpec, BackendError> {
        self.state.backend.editor_launch_spec(rel_path)
    }

    pub fn backend_arc_for_worker(&self) -> Arc<dyn Backend> {
        Arc::clone(&self.state.backend)
    }

    pub fn confirm_request(&self) -> Option<&ConfirmRequest> {
        self.state.pending_confirm.as_ref()
    }

    pub fn quick_open_query_is_empty(&self) -> bool {
        self.state.quick_open.core.filter.is_empty()
    }

    pub fn quick_open_row(&self, match_idx: usize) -> Option<QuickOpenRowSnapshot> {
        let matched = self.state.quick_open.matches.get(match_idx)?;
        let candidate = self.state.quick_open.index.get(matched.idx)?;
        Some(QuickOpenRowSnapshot {
            display: candidate.display.clone(),
            indices: matched.indices.clone(),
        })
    }

    pub fn quick_open_has_selection(&self) -> bool {
        self.state.quick_open_selected_path().is_some()
    }

    pub fn quick_open_rows(
        &self,
        start: usize,
        len: usize,
    ) -> impl Iterator<Item = (usize, QuickOpenRowSnapshot)> + '_ {
        (start..start.saturating_add(len)).filter_map(|idx| {
            let row = self.quick_open_row(idx)?;
            Some((idx, row))
        })
    }

    pub fn global_search_query_is_empty(&self) -> bool {
        self.state.global_search.core.filter.is_empty()
    }

    pub fn selected_global_search_hit(&self) -> Option<MatchHit> {
        self.state.selected_global_search_hit()
    }

    pub fn selected_global_search_hit_if_preview_stale(&self) -> Option<MatchHit> {
        let hit = self.selected_global_search_hit()?;
        let stale = match &self.state.preview_highlight {
            Some(hl) => hl.path != hit.path || hl.row != hit.line,
            None => true,
        };
        stale.then_some(hit)
    }

    pub fn diff_layout(&self) -> DiffLayout {
        self.state.diff_layout
    }

    pub fn diff_mode(&self) -> crate::DiffMode {
        self.state.diff_mode
    }

    pub fn status_tree_mode(&self) -> bool {
        self.state.git_status.tree_mode
    }

    pub fn commit_diff_layout(&self) -> DiffLayout {
        self.state.commit_detail.diff_layout
    }

    pub fn commit_diff_mode(&self) -> crate::DiffMode {
        self.state.commit_detail.diff_mode
    }

    pub fn commit_files_tree_mode(&self) -> bool {
        self.state.commit_detail.files_tree_mode
    }

    pub fn db_preview(&self) -> Option<&DbPreviewState> {
        self.state.db_preview()
    }

    pub fn graph_sidebar_width(&self, total_width: u16) -> u16 {
        self.state.graph_sidebar_width(total_width)
    }

    pub fn graph_three_col_widths(&self, total_width: u16) -> (u16, u16, u16) {
        self.state.graph_three_col_widths(total_width)
    }

    pub fn graph_uses_three_col_for_width(&self, total_width: u16) -> bool {
        self.state.graph_uses_three_col_for_width(total_width)
    }

    pub fn effective_action_paths(&self) -> Vec<PathBuf> {
        self.state.effective_action_paths()
    }

    pub fn paste_target_dir(&self) -> PathBuf {
        self.state.paste_target_dir()
    }

    pub fn keep_both_name_for_current_conflict(&self) -> Option<String> {
        self.state.keep_both_name_for_current_conflict()
    }

    pub fn selected_tree_context_menu_target(&self) -> Option<usize> {
        self.state.selected_tree_context_menu_target()
    }

    pub fn context_menu_action_paths(&self, target_idx: Option<usize>) -> Vec<PathBuf> {
        if let Some(idx) = target_idx
            && let Some(entry) = self.state.file_tree.entries.get(idx)
        {
            let path = entry.path.clone();
            if !self.state.file_selection.is_empty() && self.state.file_selection.contains(&path) {
                return self.state.file_selection.to_vec();
            }
            return vec![path];
        }
        self.state.effective_action_paths()
    }

    pub fn context_menu_paste_target(&self, target_idx: Option<usize>) -> Option<PathBuf> {
        if self.state.file_clipboard.is_empty() {
            return None;
        }
        let dest = target_idx
            .and_then(|idx| self.state.file_tree.entries.get(idx))
            .map(|entry| {
                if entry.is_dir {
                    entry.path.clone()
                } else {
                    entry.path.parent().map(PathBuf::from).unwrap_or_default()
                }
            })
            .unwrap_or_default();
        Some(dest)
    }

    pub fn context_menu_entry(&self, target_idx: Option<usize>) -> Option<TreeEntry> {
        target_idx.and_then(|idx| self.state.file_tree.entries.get(idx).cloned())
    }

    pub fn focused_preview_chip_visible(&self, uses_three_col: bool) -> bool {
        self.state.focused_preview_chip_visible(uses_three_col)
    }

    pub fn focused_preview_file_entries(&self) -> Vec<crate::FocusedPreviewFileRow> {
        self.state.focused_preview_file_entries()
    }

    pub fn graph_scope(&self) -> &GraphScope {
        &self.state.git_graph.scope
    }

    pub fn graph_recent_branches(&self) -> &[String] {
        &self.state.git_graph.recent_branches
    }

    pub fn graph_has_range_anchor(&self) -> bool {
        self.state.git_graph.selection_anchor.is_some()
    }

    pub fn visible_file_count(&self) -> usize {
        self.state.visible_file_count()
    }

    pub fn activity_state(&self, tab: AppTab) -> Option<(&'static str, &AsyncState)> {
        match tab {
            AppTab::Files => [
                ("copy", &self.state.file_copy_load),
                ("files", &self.state.file_tree_load),
                ("preview", &self.state.preview_load),
            ]
            .into_iter()
            .find(|(_, state)| state.loading || state.error.is_some() || state.stale),
            AppTab::Git => [
                ("git", &self.state.git_status_load),
                ("diff", &self.state.diff_load),
            ]
            .into_iter()
            .find(|(_, state)| state.loading || state.error.is_some() || state.stale),
            AppTab::Graph => [
                ("graph", &self.state.graph_load),
                ("commit", &self.state.commit_detail_load),
                ("commit diff", &self.state.commit_file_diff_load),
            ]
            .into_iter()
            .find(|(_, state)| state.loading || state.error.is_some() || state.stale),
            AppTab::Search => {
                if self.state.global_search_load.loading {
                    None
                } else if self.state.preview_load.loading
                    || self.state.preview_load.error.is_some()
                    || self.state.preview_load.stale
                {
                    Some(("preview", &self.state.preview_load))
                } else {
                    None
                }
            }
        }
    }

    pub fn split_percent(&self) -> u16 {
        self.state.split_percent
    }

    pub fn is_commit_editing(&self) -> bool {
        self.state.git_status.commit_editing
    }

    pub fn commit_message_is_empty(&self) -> bool {
        self.state.git_status.commit_message.is_empty()
    }

    pub fn has_git_confirm_prompt(&self) -> bool {
        self.state.git_status.confirm_discard.is_some()
            || self.state.git_status.confirm_push
            || self.state.git_status.confirm_force_push
    }

    pub fn db_goto_active(&self) -> bool {
        self.state.db_goto_input.is_some()
    }

    pub fn focused_preview_files_open(&self) -> bool {
        self.state.focused_preview_files_open
    }

    pub fn focused_preview_files_selected(&self) -> usize {
        self.state.focused_preview_files_selected
    }

    pub fn graph_in_visual_mode(&self) -> bool {
        self.state.git_graph.in_visual_mode()
    }

    pub fn graph_has_rows(&self) -> bool {
        !self.state.git_graph.rows.is_empty()
    }

    pub fn graph_selected_idx(&self) -> usize {
        self.state.git_graph.selected_idx
    }

    pub fn graph_is_range(&self) -> bool {
        self.state.git_graph.is_range()
    }

    pub fn graph_has_range_detail(&self) -> bool {
        self.state.commit_detail.range_detail.is_some()
    }

    pub fn graph_find_row_by_oid(&self, oid: &str) -> Option<usize> {
        self.state.git_graph.find_row_by_oid(oid)
    }

    pub fn graph_head_oid(&self) -> Option<String> {
        self.state
            .git_graph
            .cache_key
            .as_ref()
            .map(|(head, _, _)| head.clone())
    }

    pub fn selected_file_tree_entry(&self) -> Option<TreeEntry> {
        self.state.file_tree.selected_entry().cloned()
    }

    pub fn selected_file_tree_idx(&self) -> usize {
        self.state.file_tree.selected
    }

    pub fn file_tree_entry(&self, idx: usize) -> Option<TreeEntry> {
        self.state.file_tree.entries.get(idx).cloned()
    }

    pub fn file_tree_entries(&self) -> &[TreeEntry] {
        &self.state.file_tree.entries
    }

    pub fn tree_scroll(&self) -> usize {
        self.state.tree_scroll
    }

    pub fn file_tree_entry_exists(&self, idx: usize) -> bool {
        self.state.file_tree.entries.get(idx).is_some()
    }

    pub fn file_tree_root(&self) -> PathBuf {
        self.state.file_tree.root.clone()
    }

    pub fn selected_file_tree_entry_abs_path(&self) -> Option<PathBuf> {
        self.selected_file_tree_entry()
            .map(|entry| self.state.file_tree.root.join(entry.path))
    }

    pub fn file_tree_entry_abs_path(&self, idx: usize) -> Option<PathBuf> {
        self.file_tree_entry(idx)
            .map(|entry| self.state.file_tree.root.join(entry.path))
    }

    pub fn file_tree_dir_abs_path(&self, idx: usize) -> Option<PathBuf> {
        let entry = self.state.file_tree.entries.get(idx)?;
        entry
            .is_dir
            .then(|| self.state.file_tree.root.join(&entry.path))
    }

    pub fn tree_context_menu_current(&self) -> Option<ContextMenuItem> {
        self.state.tree_context_menu.current()
    }

    pub fn tree_context_menu_active(&self) -> bool {
        self.state.tree_context_menu.active
    }

    pub fn tree_context_menu_items_len(&self) -> usize {
        self.state.tree_context_menu.items.len()
    }

    pub fn tree_context_menu_anchor(&self) -> (u16, u16) {
        self.state.tree_context_menu.anchor
    }

    pub fn tree_context_menu_items(&self) -> Vec<ContextMenuItem> {
        self.state.tree_context_menu.items.clone()
    }

    pub fn tree_context_menu_selected(&self) -> usize {
        self.state.tree_context_menu.selected
    }

    pub fn file_clipboard_empty(&self) -> bool {
        self.state.file_clipboard.is_empty()
    }

    pub fn file_clipboard(&self) -> &FileClipboard {
        &self.state.file_clipboard
    }

    pub fn file_selection(&self) -> &SelectionSet {
        &self.state.file_selection
    }

    pub fn tree_edit(&self) -> &TreeEditState {
        &self.state.tree_edit
    }

    pub fn tree_drag(&self) -> &TreeDragState {
        &self.state.tree_drag
    }

    pub fn place_mode(&self) -> &PlaceModeState {
        &self.state.place_mode
    }

    pub fn place_mode_sources(&self) -> Vec<PathBuf> {
        self.state.place_mode.sources.clone()
    }

    pub fn file_copy_loading(&self) -> bool {
        self.state.file_copy_load.loading
    }

    pub fn place_mode_active(&self) -> bool {
        self.state.place_mode.active
    }

    pub fn hover_target_for_file_tree_idx(&self, idx: usize) -> HoverTarget {
        crate::features::place_mode::resolve_hover_target(&self.state.file_tree.entries, idx)
    }

    pub fn paste_conflict_active(&self) -> bool {
        self.state.paste_conflict.is_some()
    }

    pub fn tree_drag_active(&self) -> bool {
        self.state.tree_drag.active
    }

    pub fn tree_drag_press_armed(&self) -> bool {
        self.state.tree_drag.press.is_some()
    }

    pub fn tree_drag_should_start_drag(&self, col: u16, row: u16) -> bool {
        self.state.tree_drag.should_start_drag(col, row)
    }

    pub fn tree_edit_active(&self) -> bool {
        self.state.tree_edit.active
    }

    pub fn nav_candidates_active(&self) -> bool {
        self.state.nav_candidates.is_some()
    }

    pub fn staged_collapsed(&self) -> bool {
        self.state.staged_collapsed
    }

    pub fn unstaged_collapsed(&self) -> bool {
        self.state.unstaged_collapsed
    }

    pub fn preview_content(&self) -> Option<Arc<PreviewDocument>> {
        self.state.preview_content.clone()
    }

    pub fn preview_content_ref(&self) -> Option<&PreviewDocument> {
        self.state.preview_content.as_deref()
    }

    pub fn preview_scheduled_path(&self) -> Option<PathBuf> {
        self.state
            .preview_schedule
            .as_ref()
            .map(|(path, _)| path.clone())
    }

    pub fn preview_is_database(&self) -> bool {
        self.state
            .preview_content
            .as_ref()
            .is_some_and(|preview| preview.is_database())
    }

    pub fn staged_files(&self) -> &[FileEntry] {
        &self.state.staged_files
    }

    pub fn unstaged_files(&self) -> &[FileEntry] {
        &self.state.unstaged_files
    }

    pub fn selected_file(&self) -> Option<&SelectedFile> {
        self.state.selected_file.as_ref()
    }

    pub fn selected_file_identity(&self) -> Option<(String, bool)> {
        self.state
            .selected_file
            .as_ref()
            .map(|selected| (selected.path.clone(), selected.is_staged))
    }

    pub fn selected_file_path_if_staged(&self, is_staged: bool) -> Option<String> {
        self.state
            .selected_file
            .as_ref()
            .filter(|selected| selected.is_staged == is_staged)
            .map(|selected| selected.path.clone())
    }

    pub fn git_ahead_count(&self) -> usize {
        self.state
            .git_status
            .ahead_behind
            .map(|(ahead, _)| ahead)
            .unwrap_or(0)
    }

    pub fn diff_content(&self) -> Option<&HighlightedDiff> {
        self.state.diff_content.as_ref()
    }

    pub fn git_status(&self) -> &GitStatusState {
        &self.state.git_status
    }

    pub fn git_graph(&self) -> &GitGraphState {
        &self.state.git_graph
    }

    pub fn commit_detail(&self) -> &CommitDetailState {
        &self.state.commit_detail
    }

    pub fn push_in_flight(&self) -> bool {
        self.state.push_load.loading
    }

    pub fn commit_in_flight(&self) -> bool {
        self.state.commit_load.loading
    }

    pub fn preview_scroll(&self) -> usize {
        self.state.preview_scroll
    }

    pub fn preview_h_scroll(&self) -> usize {
        self.state.preview_h_scroll
    }

    pub fn preview_in_flight_path(&self) -> Option<PathBuf> {
        self.state.preview_in_flight_path.clone()
    }

    pub fn preview_highlight(&self) -> Option<&crate::PreviewHighlight> {
        self.state.preview_highlight.as_ref()
    }

    pub fn ctrl_hover_target(&self) -> Option<&(usize, Range<usize>)> {
        self.state.ctrl_hover_target.as_ref()
    }

    pub fn db_goto_input(&self) -> Option<&str> {
        self.state.db_goto_input.as_deref()
    }

    pub fn db_goto_cursor(&self) -> usize {
        self.state.db_goto_cursor
    }

    pub fn active_diff_vertical_scroll(&self) -> Option<usize> {
        self.state.active_diff_vertical_scroll()
    }

    pub fn diff_scroll_state(&self) -> (usize, usize, usize, usize) {
        self.state.diff_scroll_state()
    }

    pub fn commit_file_diff_scroll_state(&self) -> (usize, usize, usize, usize) {
        self.state.commit_file_diff_scroll_state()
    }

    pub fn nav_candidates(&self) -> Option<crate::NavCandidatesPopup> {
        self.state.nav_candidates.clone()
    }

    pub fn paste_conflict_prompt(&self) -> Option<&reef_core::file_ops::PasteConflictPrompt> {
        self.state.paste_conflict.as_ref()
    }

    pub fn fs_mutation_loading(&self) -> bool {
        self.state.fs_mutation_load.loading
    }

    pub fn preview_generation(&self) -> u64 {
        self.state.preview_load.generation
    }

    pub fn preview_highlight_cloned(&self) -> Option<PreviewHighlight> {
        self.state.preview_highlight.clone()
    }

    pub fn nav_busy(&self) -> bool {
        self.state.nav_candidates.is_some() || self.state.nav_pending_lsp_jump.is_some()
    }

    pub fn nav_workspace(&self) -> Option<Arc<reef_core::nav::WorkspaceIndex>> {
        self.state.nav_workspace.clone()
    }

    pub fn nav_refine_cache_get(
        &self,
        lang: reef_core::nav::NavLang,
        cache_key: &str,
    ) -> Option<reef_core::nav::LspLocation> {
        self.state
            .nav_refine_cache
            .get(&(lang, cache_key.to_string()))
            .cloned()
    }

    pub fn nav_refine_epoch(&self) -> u64 {
        self.state.nav_refine_epoch
    }

    pub fn nav_candidates_opened_by_ctrl_click(&self) -> bool {
        self.state
            .nav_candidates
            .as_ref()
            .is_some_and(|popup| popup.opened_by_ctrl_click)
    }

    pub fn lsp_badge(&self, lang: reef_core::nav::NavLang) -> reef_core::nav::LspBadge {
        self.state.lsp_badge(lang)
    }

    pub fn is_lsp_installed(&self, lang: reef_core::nav::NavLang) -> bool {
        self.state.is_lsp_installed(lang)
    }

    pub fn global_search_row(&self, idx: usize) -> Option<GlobalSearchRowSnapshot> {
        let hit = self.state.global_search.results.get(idx)?.clone();
        Some(GlobalSearchRowSnapshot {
            included: self.state.global_search.is_match_included(idx),
            hit,
        })
    }

    pub fn global_search_rows(
        &self,
        start: usize,
        len: usize,
    ) -> impl Iterator<Item = (usize, GlobalSearchRowSnapshot)> + '_ {
        (start..start.saturating_add(len)).filter_map(|idx| {
            let row = self.global_search_row(idx)?;
            Some((idx, row))
        })
    }

    pub fn hosts_picker_input_mode(&self) -> crate::features::hosts_picker::InputMode {
        self.state.hosts_picker.input_mode
    }

    pub fn hosts_picker_row(&self, idx: usize) -> Option<HostsPickerRowSnapshot> {
        let rows = self.state.hosts_picker.visible_rows();
        let row = rows.get(idx)?;
        match row {
            crate::features::hosts_picker::PickerRow::Recent(target) => {
                Some(HostsPickerRowSnapshot {
                    left: target.to_arg(),
                    right: "recent".to_string(),
                    recent: true,
                })
            }
            crate::features::hosts_picker::PickerRow::Entry(host) => {
                let right = match (&host.hostname, &host.user) {
                    (Some(hostname), Some(user)) => format!("{user}@{hostname}"),
                    (Some(hostname), None) => hostname.clone(),
                    (None, Some(user)) => format!("{user}@"),
                    (None, None) => String::new(),
                };
                Some(HostsPickerRowSnapshot {
                    left: host.alias.clone(),
                    right,
                    recent: false,
                })
            }
        }
    }

    pub fn hosts_picker_rows(
        &self,
        start: usize,
        len: usize,
    ) -> impl Iterator<Item = (usize, HostsPickerRowSnapshot)> + '_ {
        (start..start.saturating_add(len)).filter_map(|idx| {
            let row = self.hosts_picker_row(idx)?;
            Some((idx, row))
        })
    }

    pub fn graph_branch_picker_row(&self, idx: usize) -> Option<GraphBranchPickerRowSnapshot> {
        use crate::features::graph_branch_picker::{BranchKind, BranchPickerRow};

        let rows = self.state.graph_branch_picker.visible_rows();
        let row = rows.get(idx)?;
        match row {
            BranchPickerRow::AllRefs => Some(GraphBranchPickerRowSnapshot {
                left: "[ All refs ]".to_string(),
                right: "default".to_string(),
                accent: true,
            }),
            BranchPickerRow::Recent(branch) => {
                let prefix = if branch.is_head { "* " } else { "  " };
                let kind = match branch.kind {
                    BranchKind::Local => "recent · local",
                    BranchKind::Remote => "recent · remote",
                };
                Some(GraphBranchPickerRowSnapshot {
                    left: format!("{prefix}{}", branch.display),
                    right: kind.to_string(),
                    accent: true,
                })
            }
            BranchPickerRow::Branch(branch) => {
                let prefix = if branch.is_head { "* " } else { "  " };
                let kind = match branch.kind {
                    BranchKind::Local => "local",
                    BranchKind::Remote => "remote",
                };
                Some(GraphBranchPickerRowSnapshot {
                    left: format!("{prefix}{}", branch.display),
                    right: kind.to_string(),
                    accent: false,
                })
            }
        }
    }

    pub fn graph_branch_picker_rows(
        &self,
        start: usize,
        len: usize,
    ) -> impl Iterator<Item = (usize, GraphBranchPickerRowSnapshot)> + '_ {
        (start..start.saturating_add(len)).filter_map(|idx| {
            let row = self.graph_branch_picker_row(idx)?;
            Some((idx, row))
        })
    }

    pub fn search_snapshot(&self) -> crate::SearchSnapshot {
        self.state.search_snapshot()
    }

    pub fn search(&self) -> &SearchState {
        &self.state.search
    }

    pub fn find_widget(&self) -> &FindWidgetState {
        &self.state.find_widget
    }

    pub fn resolve_search_target(&self, uses_three_col: bool) -> Option<crate::SearchTarget> {
        crate::features::search::resolve_search_target(&self.state, uses_three_col)
    }

    pub fn drain_effects(&mut self) -> Vec<AppEffect> {
        self.drain_state_effects();
        std::mem::take(&mut self.effects)
    }

    fn drain_state_effects(&mut self) {
        if let Some(path) = self.state.pending_edit.take() {
            self.effects.push(AppEffect::OpenInEditor(path));
        }
        if let Some(target) = self.state.pending_ssh_target.take() {
            self.effects.push(AppEffect::SwitchSession(target));
        }
    }

    pub fn drain_runtime_events(&mut self) -> Vec<AppRuntimeEvent> {
        std::mem::take(&mut self.runtime_events)
    }
}

pub struct AppConfig {
    pub state: AppState,
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_io::LocalBackend;
    use std::path::PathBuf;

    fn test_app() -> ReefApp {
        let backend = Arc::new(LocalBackend::open_at(PathBuf::from(".")));
        let state = AppState::new(crate::AppStateConfig {
            backend,
            prefs: crate::AppPrefs::default(),
            now: Instant::now(),
            subscribe_fs_events: false,
        });
        ReefApp::new(AppConfig { state })
    }

    #[test]
    fn dispatch_set_active_tab_emits_tab_changed_event() {
        let mut app = test_app();
        app.dispatch(AppCommand::SetActiveTab(AppTab::Git));

        let events = app.drain_runtime_events();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                AppRuntimeEvent::TabChanged(outcome)
                    if outcome.changed
                        && outcome.clear_preview_selection
                        && outcome.clear_diff_selection
            )
        }));
    }

    #[test]
    fn dispatch_set_active_tab_same_tab_emits_no_event() {
        let mut app = test_app();
        app.dispatch(AppCommand::SetActiveTab(AppTab::Files));

        assert!(app.drain_runtime_events().is_empty());
    }
}
