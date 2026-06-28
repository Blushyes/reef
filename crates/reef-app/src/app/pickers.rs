use super::*;

impl AppState {
    pub fn return_hosts_picker_to_search_mode(&mut self) {
        self.hosts_picker.return_to_search_mode();
    }

    pub fn paste_hosts_picker(&mut self, s: &str) {
        self.hosts_picker.handle_paste(s);
    }

    pub fn apply_hosts_picker_search_input(
        &mut self,
        input: crate::PickerInput,
    ) -> crate::PickerInputOutcome {
        let visible = self.hosts_picker.visible_rows().len();
        crate::features::picker::apply_picker_input(&mut self.hosts_picker.core, input, visible)
    }

    pub fn edit_hosts_picker_path_input(
        &mut self,
        op: crate::TextEditOp,
    ) -> crate::TextEditOutcome {
        crate::text_input::apply_single_line_op(
            op,
            &mut self.hosts_picker.path_buffer,
            &mut self.hosts_picker.path_cursor,
        )
    }

    pub fn request_session_swap(&mut self, target: crate::features::hosts_picker::SshTarget) {
        self.pending_ssh_target = Some(target);
    }

    pub fn open_hosts_picker(
        &mut self,
        parsed: Vec<reef_core::hosts::HostEntry>,
        recent: Vec<crate::features::hosts_picker::SshTarget>,
    ) {
        self.hosts_picker.open(parsed, recent);
    }

    pub fn close_hosts_picker(&mut self) {
        self.hosts_picker.close();
    }

    pub fn confirm_hosts_picker(&mut self) -> Option<crate::features::hosts_picker::SshTarget> {
        let target = self.hosts_picker.confirm();
        self.hosts_picker.close();
        target
    }

    pub fn select_hosts_picker_row(&mut self, idx: usize) {
        self.hosts_picker.core.selected_idx = idx;
    }

    pub fn select_graph_branch_picker_row(&mut self, idx: usize) {
        self.graph_branch_picker.core.selected_idx = idx;
    }

    pub fn ensure_graph_branch_picker_selection_visible(&mut self, visible_rows: usize) -> usize {
        let sel = self.graph_branch_picker.core.selected_idx;
        if sel < self.graph_branch_picker.scroll {
            self.graph_branch_picker.scroll = sel;
        } else if visible_rows > 0 && sel >= self.graph_branch_picker.scroll + visible_rows {
            self.graph_branch_picker.scroll = sel + 1 - visible_rows;
        }
        self.graph_branch_picker.scroll
    }

    pub fn open_graph_branch_picker(&mut self) -> GraphBranchPickerOpenOutcome {
        if self.git_graph.ref_map.is_empty() {
            return GraphBranchPickerOpenOutcome::default();
        }
        let mut outcome = GraphBranchPickerOpenOutcome {
            opened: true,
            ..GraphBranchPickerOpenOutcome::default()
        };
        if let GraphScope::Branch(target) = self.git_graph.scope.clone() {
            let still_present = self.git_graph.ref_map.values().any(|labels| {
                labels.iter().any(|label| match label {
                    RefLabel::Branch(name) => format!("refs/heads/{name}") == target,
                    RefLabel::RemoteBranch(name) => format!("refs/remotes/{name}") == target,
                    _ => false,
                })
            });
            if !still_present {
                self.git_graph
                    .recent_branches
                    .retain(|existing| existing != &target);
                outcome.stale_branch_short_ref = Some(shorthand_for_full_ref(&target).to_string());
                outcome.scope_change = self.apply_graph_scope_no_refresh(GraphScope::AllRefs);
                self.refresh_graph();
            }
        }
        let recent = self.git_graph.recent_branches.clone();
        let scope = self.git_graph.scope.clone();
        let ref_map = self.git_graph.ref_map.clone();
        self.graph_branch_picker.open(&ref_map, recent, &scope);
        outcome
    }

    pub fn close_graph_branch_picker(&mut self) {
        self.graph_branch_picker.close();
    }

    pub fn confirm_graph_branch_picker(&mut self) -> Option<GraphScope> {
        let scope = self.graph_branch_picker.confirm();
        self.graph_branch_picker.close();
        scope
    }

    pub fn paste_graph_branch_picker(&mut self, s: &str) {
        self.graph_branch_picker.handle_paste(s);
    }

    pub fn apply_graph_branch_picker_input(
        &mut self,
        input: crate::PickerInput,
    ) -> crate::PickerInputOutcome {
        let visible = self.graph_branch_picker.visible_rows().len();
        crate::features::picker::apply_picker_input(
            &mut self.graph_branch_picker.core,
            input,
            visible,
        )
    }
}
