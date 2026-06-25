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

    pub fn open_nav_candidates(&mut self, popup: NavCandidatesPopup) {
        self.nav_candidates = Some(popup);
    }

    pub fn take_nav_candidates(&mut self) -> Option<NavCandidatesPopup> {
        self.nav_candidates.take()
    }

    pub fn close_nav_candidates(&mut self) {
        self.nav_candidates = None;
    }

    pub fn move_nav_candidates_selection(&mut self, delta: i32) {
        let Some(popup) = self.nav_candidates.as_mut() else {
            return;
        };
        let n = popup.candidates.len();
        if n == 0 {
            return;
        }
        let cur = popup.selected as i32;
        popup.selected = (cur + delta).rem_euclid(n as i32) as usize;
        popup.clamp_scroll();
    }

    pub fn scroll_nav_candidates(&mut self, delta: i32) {
        let Some(popup) = self.nav_candidates.as_mut() else {
            return;
        };
        let visible = popup.visible_rows();
        let max_scroll = popup.candidates.len().saturating_sub(visible);
        popup.scroll = (popup.scroll as i32 + delta).clamp(0, max_scroll as i32) as usize;
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
            self.staged_collapsed,
            self.unstaged_collapsed,
            self.git_status.tree_mode,
            &self.git_status.collapsed_dirs,
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
