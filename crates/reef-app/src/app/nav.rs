use super::*;

impl AppState {
    pub fn navigate_file_tree_and_schedule_preview(&mut self, delta: i32) {
        self.file_tree.navigate(delta);
        self.load_preview();
    }

    pub fn navigate_file_tree(&mut self, delta: i32) {
        self.file_tree.navigate(delta);
    }

    pub fn extend_file_selection_after_tree_nav(&mut self, delta: i32) {
        if self.file_selection.is_empty()
            && let Some(path) = self.file_tree.selected_path()
        {
            self.file_selection.replace_with_single(path);
        }
        self.navigate_file_tree_and_schedule_preview(delta);
        if let Some(target) = self.file_tree.selected_path() {
            let entries = self.file_tree.entries.clone();
            self.file_selection.extend_to(target, &entries);
        }
    }

    pub fn navigate_tree_context_menu(&mut self, delta: i32) {
        self.tree_context_menu.navigate(delta);
    }

    pub fn clear_nav_pending_lsp_jump(&mut self) {
        self.nav_pending_lsp_jump = None;
    }

    pub fn apply_lsp_state_change(
        &mut self,
        lang: reef_core::nav::NavLang,
        state: reef_core::nav::LspBadge,
    ) {
        if matches!(
            state,
            reef_core::nav::LspBadge::Off | reef_core::nav::LspBadge::Crashed
        ) && let Some(pending) = self.nav_pending_lsp_jump.as_ref()
            && pending.lang == lang
        {
            self.nav_pending_lsp_jump = None;
            let bin = lang.profile().lsp.as_ref().map(|p| p.bin).unwrap_or("LSP");
            self.toasts.push(Toast::warn(format!("{bin} unavailable")));
        }
        self.lsp_states.insert(lang, state);
    }

    pub fn refresh_lsp_installed(&mut self) {
        for &lang in reef_core::nav::NavLang::ALL {
            let installed = lang
                .profile()
                .lsp
                .as_ref()
                .and_then(|p| reef_core::nav::lsp::locate_binary(p.bin))
                .is_some();
            self.lsp_installed.insert(lang, installed);
        }
    }

    pub fn lsp_badge(&self, lang: reef_core::nav::NavLang) -> reef_core::nav::LspBadge {
        self.lsp_states
            .get(&lang)
            .cloned()
            .unwrap_or(reef_core::nav::LspBadge::Off)
    }

    pub fn is_lsp_installed(&self, lang: reef_core::nav::NavLang) -> bool {
        self.lsp_installed.get(&lang).copied().unwrap_or(false)
    }

    pub fn set_nav_pending_lsp_jump(&mut self, pending: NavPendingJump) {
        self.nav_pending_lsp_jump = Some(pending);
    }

    pub fn next_nav_refine_generation(&mut self) -> u64 {
        self.nav_refine_gen += 1;
        self.nav_refine_gen
    }

    pub fn open_nav_candidates(
        &mut self,
        popup: NavCandidatesPopup,
        dark: bool,
        viewport_rows: usize,
    ) {
        self.nav_candidates = Some(popup);
        self.clamp_nav_candidates_scroll(viewport_rows);
        self.nav_preview_content = None;
        self.nav_preview_target_path = None;
        self.nav_preview_dark = dark;
        self.request_selected_nav_preview_if_expanded();
    }

    pub fn take_nav_candidates(&mut self) -> Option<NavCandidatesPopup> {
        self.nav_candidates.take()
    }

    pub fn close_nav_candidates(&mut self) {
        self.nav_candidates = None;
        self.nav_preview_content = None;
        self.nav_preview_target_path = None;
        self.nav_preview_load.invalidate();
    }

    pub fn move_nav_candidates_selection(&mut self, delta: i32, viewport_rows: usize) {
        let Some(popup) = self.nav_candidates.as_mut() else {
            return;
        };
        let n = popup.candidates.len();
        if n == 0 {
            return;
        }
        let cur = popup.selected as i32;
        let selected = (cur + delta).rem_euclid(n as i32) as usize;
        popup.select(selected);
        self.clamp_nav_candidates_scroll(viewport_rows);
        self.request_selected_nav_preview_if_expanded();
    }

    pub fn select_nav_candidate(&mut self, index: usize, viewport_rows: usize) {
        let selected = self
            .nav_candidates
            .as_mut()
            .is_some_and(|popup| popup.select(index));
        if selected {
            self.clamp_nav_candidates_scroll(viewport_rows);
            self.request_selected_nav_preview_if_expanded();
        }
    }

    pub fn toggle_nav_candidate_group(&mut self, group_index: usize, viewport_rows: usize) {
        if let Some(popup) = self.nav_candidates.as_mut() {
            popup.toggle_group(group_index);
        }
        self.clamp_nav_candidates_scroll(viewport_rows);
    }

    pub fn scroll_nav_candidates(&mut self, delta: i32, viewport_rows: usize) {
        let Some(popup) = self.nav_candidates.as_mut() else {
            return;
        };
        let (visible, total) = match self.settings.nav_peek_mode {
            crate::NavPeekMode::Expanded => {
                (popup.visible_rows(viewport_rows), popup.tree_row_count())
            }
            crate::NavPeekMode::Compact => (
                popup.compact_visible_rows(viewport_rows),
                popup.candidates.len(),
            ),
        };
        if visible == 0 {
            popup.scroll = 0;
            return;
        }
        let max_scroll = total.saturating_sub(visible);
        popup.scroll = (popup.scroll as i32 + delta).clamp(0, max_scroll as i32) as usize;
    }

    pub fn toggle_nav_peek_mode(&mut self, viewport_rows: usize) {
        self.set_nav_peek_mode(self.settings.nav_peek_mode.next(), viewport_rows);
    }

    pub fn set_nav_peek_mode(&mut self, mode: crate::NavPeekMode, viewport_rows: usize) {
        if self.settings.nav_peek_mode == mode {
            return;
        }
        self.settings.nav_peek_mode = mode;
        self.clamp_nav_candidates_scroll(viewport_rows);
        match self.settings.nav_peek_mode {
            crate::NavPeekMode::Expanded => self.request_selected_nav_preview(),
            crate::NavPeekMode::Compact => {
                self.nav_preview_content = None;
                self.nav_preview_target_path = None;
                self.nav_preview_load.invalidate();
            }
        }
    }

    pub fn navigate_preview_definition_at(
        &mut self,
        cursor: crate::CursorPosition,
        dark: bool,
        view_height: usize,
        peek_viewport_rows: usize,
    ) {
        if self.nav_candidates.is_some() || self.nav_pending_lsp_jump.is_some() {
            return;
        }
        let Some((current_path, parsed)) = self.preview_parse_for_navigation() else {
            if self.preview_enrichment_pending.is_some() {
                let Some(preview) = self.preview_content.as_ref() else {
                    return;
                };
                self.defer_preview_definition(
                    cursor,
                    PathBuf::from(&preview.path),
                    dark,
                    view_height,
                    peek_viewport_rows,
                );
            }
            return;
        };
        self.pending_preview_definition = None;
        let Some(symbol) = reef_core::nav::identifier_at(&parsed, (cursor.line, cursor.byte_col))
            .map(str::to_owned)
        else {
            return;
        };

        let mut candidates = reef_core::nav::intrafile::resolve_definition_intrafile(
            &parsed,
            (cursor.line, cursor.byte_col),
        );
        if candidates.len() <= 1
            && let Some(workspace) = self.nav_workspace.as_ref()
        {
            candidates.extend(workspace.definitions_for(
                &symbol,
                parsed.language,
                Some((&current_path, None)),
            ));
        }
        if candidates.is_empty()
            && self.nav_workspace.is_none()
            && !self.backend.is_remote()
            && (self.nav_workspace_load.loading || self.nav_workspace_load.should_request())
        {
            self.defer_preview_definition(
                cursor,
                current_path,
                dark,
                view_height,
                peek_viewport_rows,
            );
            return;
        }
        if candidates.is_empty()
            && let Some(workspace) = self.nav_workspace.as_ref()
        {
            candidates = workspace.references_for(&symbol, parsed.language);
            self.open_or_commit_navigation_candidates(
                candidates,
                ResolvedPreviewNavigation {
                    current_path,
                    cursor,
                    symbol,
                    kind: NavCandidateKind::References,
                    dark,
                    view_height,
                    peek_viewport_rows,
                },
            );
            return;
        }
        self.open_or_commit_navigation_candidates(
            candidates,
            ResolvedPreviewNavigation {
                current_path,
                cursor,
                symbol,
                kind: NavCandidateKind::Definitions,
                dark,
                view_height,
                peek_viewport_rows,
            },
        );
    }

    fn defer_preview_definition(
        &mut self,
        cursor: crate::CursorPosition,
        path: PathBuf,
        dark: bool,
        view_height: usize,
        peek_viewport_rows: usize,
    ) {
        self.pending_preview_definition = Some(PendingPreviewDefinition {
            cursor,
            path,
            preview_generation: self.preview_load.generation,
            dark,
            view_height,
            peek_viewport_rows,
        });
    }

    pub fn retry_pending_preview_definition(&mut self) {
        let Some(pending) = self.pending_preview_definition.take() else {
            return;
        };
        let current_matches = self.preview_content.as_ref().is_some_and(|preview| {
            Path::new(&preview.path) == pending.path
                && self.preview_load.generation == pending.preview_generation
        });
        if current_matches {
            self.navigate_preview_definition_at(
                pending.cursor,
                pending.dark,
                pending.view_height,
                pending.peek_viewport_rows,
            );
        }
    }

    pub fn confirm_nav_candidate(&mut self, view_height: usize) {
        let Some(popup) = self.nav_candidates.take() else {
            return;
        };
        self.nav_preview_content = None;
        self.nav_preview_target_path = None;
        self.nav_preview_load.invalidate();
        let Some(target) = popup.candidates.get(popup.selected).cloned() else {
            return;
        };
        self.commit_navigation_target(popup.origin, popup.current_path, target, view_height);
    }

    fn preview_parse_for_navigation(&self) -> Option<(PathBuf, Arc<reef_core::nav::FileParse>)> {
        let preview = self.preview_content.as_ref()?;
        let reef_core::preview::PreviewBody::Text(text) = &preview.body else {
            return None;
        };
        Some((
            PathBuf::from(&preview.path),
            Arc::clone(text.parsed.as_ref()?),
        ))
    }

    fn open_or_commit_navigation_candidates(
        &mut self,
        candidates: Vec<reef_core::nav::Location>,
        navigation: ResolvedPreviewNavigation,
    ) {
        let Some(origin) =
            self.preview_navigation_origin(navigation.current_path.clone(), navigation.cursor)
        else {
            return;
        };
        match candidates.len() {
            0 => {}
            1 => {
                let target = candidates.into_iter().next().expect("single candidate");
                self.commit_navigation_target(
                    origin,
                    navigation.current_path,
                    target,
                    navigation.view_height,
                );
            }
            _ => self.open_nav_candidates(
                NavCandidatesPopup::new(
                    candidates,
                    navigation.current_path,
                    origin,
                    navigation.symbol,
                    navigation.kind,
                ),
                navigation.dark,
                navigation.peek_viewport_rows,
            ),
        }
    }

    fn preview_navigation_origin(
        &self,
        path: PathBuf,
        cursor: crate::CursorPosition,
    ) -> Option<crate::LocationSnapshot> {
        let surface = match self.active_tab {
            crate::AppTab::Files => crate::LocationSurface::FilePreview,
            crate::AppTab::Search => crate::LocationSurface::SearchPreview,
            crate::AppTab::Git | crate::AppTab::Graph => return None,
        };
        Some(crate::LocationSnapshot {
            surface,
            path,
            cursor,
            scroll: crate::ScrollPosition {
                vertical: self.preview_scroll,
                horizontal: self.preview_h_scroll,
            },
        })
    }

    fn commit_navigation_target(
        &mut self,
        origin: crate::LocationSnapshot,
        current_path: PathBuf,
        target: reef_core::nav::Location,
        view_height: usize,
    ) {
        self.push_location_history(origin);
        let path = target.path.clone().unwrap_or(current_path);
        let row = target.line;
        let byte_range = target.byte_range;
        self.set_preview_highlight_with_fade(
            path.clone(),
            row,
            byte_range,
            crate::HighlightFade::Pending {
                armed_at: Instant::now(),
            },
        );
        if self
            .preview_content
            .as_ref()
            .is_some_and(|preview| Path::new(&preview.path) == path)
        {
            self.center_preview_on_line(row, view_height);
            return;
        }
        self.set_active_tab(crate::AppTab::Files);
        self.active_panel = crate::AppPanel::Diff;
        self.file_tree.reveal(&path);
        self.refresh_file_tree_with_target(Some(path.clone()));
        self.load_preview_for_path(path);
    }

    fn clamp_nav_candidates_scroll(&mut self, viewport_rows: usize) {
        let Some(popup) = self.nav_candidates.as_mut() else {
            return;
        };
        match self.settings.nav_peek_mode {
            crate::NavPeekMode::Expanded => popup.clamp_scroll(viewport_rows),
            crate::NavPeekMode::Compact => popup.clamp_compact_scroll(viewport_rows),
        }
    }

    pub(crate) fn request_selected_nav_preview_if_expanded(&mut self) {
        if self.settings.nav_peek_mode == crate::NavPeekMode::Expanded {
            self.request_selected_nav_preview();
        }
    }

    pub(crate) fn request_selected_nav_preview(&mut self) {
        let Some(path) = self
            .nav_candidates
            .as_ref()
            .map(|popup| popup.selected_path().to_path_buf())
        else {
            return;
        };
        if !self.nav_preview_load.stale
            && self
                .nav_preview_target_path
                .as_deref()
                .is_some_and(|target| target == path)
        {
            return;
        }
        let generation = self.nav_preview_load.begin();
        self.nav_preview_target_path = Some(path.clone());
        self.tasks.load_nav_preview(
            generation,
            Arc::clone(&self.backend),
            path,
            self.nav_preview_dark,
        );
    }

    pub fn push_location_history(&mut self, entry: LocationSnapshot) {
        self.location_history.push(entry);
    }

    pub fn dispatch_nav_workspace_build(&mut self) {
        if self.backend.is_remote() || self.nav_workspace_load.loading {
            return;
        }
        let generation = self.nav_workspace_load.begin();
        self.tasks
            .build_nav_workspace(generation, Arc::clone(&self.backend));
    }

    pub fn navigate_files(&mut self, delta: i32) {
        let items = navigable_git_files(
            &self.staged_files,
            &self.unstaged_files,
            &self.git_status.staged_tree_rows,
            &self.git_status.unstaged_tree_rows,
            self.staged_collapsed,
            self.unstaged_collapsed,
            self.git_status.tree_mode,
        );

        if items.is_empty() {
            return;
        }

        let current_idx = self
            .selected_file
            .as_ref()
            .and_then(|selected| {
                items.iter().position(|(path, staged)| {
                    path == &selected.path && *staged == selected.is_staged
                })
            })
            .unwrap_or(0);

        let new_idx = if delta > 0 {
            (current_idx + delta as usize).min(items.len() - 1)
        } else {
            current_idx.saturating_sub((-delta) as usize)
        };

        let (path, is_staged) = items[new_idx].clone();
        self.selected_file = Some(SelectedFile { path, is_staged });
        self.diff_scroll = 0;
        self.diff_h_scroll = 0;
        self.sbs_left_h_scroll = 0;
        self.sbs_right_h_scroll = 0;
    }
}
