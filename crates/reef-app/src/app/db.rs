use super::*;

impl AppState {
    pub fn db_preview(&self) -> Option<&DbPreviewState> {
        self.db_preview.as_ref()
    }

    pub fn db_preview_mut(&mut self) -> Option<&mut DbPreviewState> {
        self.db_preview.as_mut()
    }

    fn preview_database_info(&self) -> Option<&reef_sqlite_preview::DatabaseInfoV2> {
        match self.preview_content.as_deref()?.body {
            reef_core::preview::PreviewBody::Database(ref info) => Some(info),
            _ => None,
        }
    }

    pub(super) fn sync_db_preview_state(&mut self) {
        match self
            .preview_content
            .as_deref()
            .map(|preview| (&preview.body, preview.path.as_str()))
        {
            Some((reef_core::preview::PreviewBody::Database(info), path)) => {
                let state_matches = self
                    .db_preview
                    .as_ref()
                    .map(|state| state.path == path)
                    .unwrap_or(false);
                if !state_matches {
                    self.db_preview = Some(DbPreviewState::from_initial(
                        path,
                        info,
                        reef_core::preview::INITIAL_DB_PAGE_ROWS,
                    ));
                    self.db_page_load.invalidate();
                    self.db_detail_load.invalidate();
                }
            }
            _ => {
                if self.db_preview.is_some() {
                    self.db_page_load.invalidate();
                    self.db_detail_load.invalidate();
                }
                self.db_preview = None;
            }
        }
    }

    pub fn db_toggle_schema(&mut self, name: &str) {
        let Some(state) = self.db_preview.as_mut() else {
            return;
        };
        if !state.expanded.remove(name) {
            state.expanded.insert(name.to_string());
        }
    }

    pub fn db_select_object(&mut self, key: reef_sqlite_preview::DbObjectKey) {
        let current = self.db_preview.as_ref().map(|s| s.selection.clone());
        if current.as_ref() == Some(&key) {
            return;
        }
        if key.kind.has_rows() {
            self.dispatch_db_page_load(key, 0, true);
        } else {
            self.dispatch_db_detail_load(key);
        }
    }

    pub fn db_navigate(&mut self, action: DbNav) {
        let Some((cur_key, cur_page, rows_per_page)) = self
            .db_preview
            .as_ref()
            .map(|s| (s.selection.clone(), s.page, s.rows_per_page))
        else {
            return;
        };
        let Some(info) = self.preview_database_info() else {
            return;
        };
        let visible: Vec<(reef_sqlite_preview::DbObjectKey, u64)> = info
            .iter_row_bearing()
            .map(|object| (object.key(), max_page_for_object(object, rows_per_page)))
            .collect();
        if visible.is_empty() {
            return;
        }
        let cur_idx = visible
            .iter()
            .position(|(key, _)| key == &cur_key)
            .unwrap_or(0)
            .min(visible.len() - 1);
        let max_idx = visible.len() - 1;
        let (new_idx, new_page) = match action {
            DbNav::PrevPage => (cur_idx, cur_page.saturating_sub(1)),
            DbNav::NextPage => (cur_idx, (cur_page + 1).min(visible[cur_idx].1)),
            DbNav::PrevTable => (cur_idx.saturating_sub(1), 0),
            DbNav::NextTable => ((cur_idx + 1).min(max_idx), 0),
            DbNav::FirstPage => (cur_idx, 0),
            DbNav::LastPage => (cur_idx, visible[cur_idx].1),
        };
        if new_idx == cur_idx && new_page == cur_page {
            return;
        }
        self.dispatch_db_page_load(visible[new_idx].0.clone(), new_page, new_idx != cur_idx);
    }

    pub fn db_navigate_to_page(&mut self, page_one_based: u64) {
        let Some((selection, cur_page, rows_per_page)) = self
            .db_preview
            .as_ref()
            .map(|s| (s.selection.clone(), s.page, s.rows_per_page))
        else {
            return;
        };
        if !selection.kind.has_rows() {
            return;
        }
        let Some(info) = self.preview_database_info() else {
            return;
        };
        let Some(object) = info.lookup(&selection) else {
            return;
        };
        let max_page = max_page_for_object(object, rows_per_page);
        let target_page = page_one_based.saturating_sub(1).min(max_page);
        if target_page == cur_page {
            return;
        }
        self.dispatch_db_page_load(selection, target_page, false);
    }

    fn dispatch_db_page_load(
        &mut self,
        key: reef_sqlite_preview::DbObjectKey,
        page: u64,
        reset_h_scroll: bool,
    ) {
        let Some((path, rows_per_page)) = self
            .db_preview
            .as_ref()
            .map(|s| (PathBuf::from(&s.path), s.rows_per_page))
        else {
            return;
        };
        let generation = self.db_page_load.begin();
        self.db_detail_load.invalidate();
        self.tasks.load_db_page(
            generation,
            Arc::clone(&self.backend),
            DbPageRequest {
                path,
                key,
                page,
                rows_per_page,
                reset_h_scroll,
            },
        );
    }

    fn dispatch_db_detail_load(&mut self, key: reef_sqlite_preview::DbObjectKey) {
        let Some(path) = self.db_preview.as_ref().map(|s| PathBuf::from(&s.path)) else {
            return;
        };
        let generation = self.db_detail_load.begin();
        self.db_page_load.invalidate();
        self.tasks
            .load_db_detail(generation, Arc::clone(&self.backend), path, key);
    }

    pub fn open_db_goto(&mut self) {
        self.db_goto_input = Some(String::new());
        self.db_goto_cursor = 0;
    }

    pub fn close_db_goto(&mut self) {
        self.db_goto_input = None;
        self.db_goto_cursor = 0;
    }

    pub fn confirm_db_goto(&mut self) -> Option<u64> {
        let parsed = self
            .db_goto_input
            .as_deref()
            .filter(|buf| !buf.is_empty())
            .and_then(|buf| buf.parse::<u64>().ok())
            .filter(|page| *page > 0);
        self.close_db_goto();
        parsed
    }

    pub fn edit_db_goto_input(&mut self, op: crate::TextEditOp) -> crate::TextEditOutcome {
        let Some(buf) = self.db_goto_input.as_mut() else {
            return crate::TextEditOutcome::Unhandled;
        };
        let current_len = buf.len();
        crate::text_input::apply_single_line_op_filtered(op, buf, &mut self.db_goto_cursor, |c| {
            c.is_ascii_digit() && current_len < 18
        })
    }

    pub fn paste_db_goto_input(&mut self, s: &str) {
        let Some(buf) = self.db_goto_input.as_mut() else {
            return;
        };
        let _ = crate::text_input::paste_ascii_digits_capped(s, buf, &mut self.db_goto_cursor, 18);
    }
}
