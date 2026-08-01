use super::*;

enum DbPreviewSyncAction {
    None,
    LoadPage {
        key: reef_sqlite_preview::DbObjectKey,
        page: u64,
    },
    LoadDetail(reef_sqlite_preview::DbObjectKey),
}

impl AppState {
    fn cancel_db_cell_request(&mut self) {
        if let Some(cancellation) = self.db_cell_cancellation.take() {
            cancellation.cancel();
        }
    }

    fn invalidate_db_cell_load(&mut self) {
        self.cancel_db_cell_request();
        self.db_cell_load.invalidate();
    }

    pub fn db_preview(&self) -> Option<&DbPreviewState> {
        self.db_preview.as_ref()
    }

    pub fn db_preview_mut(&mut self) -> Option<&mut DbPreviewState> {
        self.db_preview.as_mut()
    }

    pub(super) fn preview_database_info(&self) -> Option<&reef_sqlite_preview::DatabaseInfoV2> {
        match self.preview_content.as_deref()?.body {
            reef_core::preview::PreviewBody::Database(ref info) => Some(info),
            _ => None,
        }
    }

    pub(super) fn sync_db_preview_state(&mut self) {
        let Some(preview) = self.preview_content.clone() else {
            self.clear_db_preview_state();
            return;
        };
        let reef_core::preview::PreviewBody::Database(info) = &preview.body else {
            self.clear_db_preview_state();
            return;
        };

        let path = preview.path.as_str();
        let source_revision = self.preview_source_revision;
        let mut replace_from_initial = false;
        let action = match self.db_preview.as_mut() {
            None => {
                replace_from_initial = true;
                DbPreviewSyncAction::None
            }
            Some(state) if state.path != path => {
                replace_from_initial = true;
                DbPreviewSyncAction::None
            }
            Some(state) if state.source_revision == source_revision => DbPreviewSyncAction::None,
            Some(state) => {
                let Some(object) = info.lookup(&state.selection) else {
                    return self.replace_db_preview_from_initial(path, source_revision, info);
                };
                state.source_revision = source_revision;
                state
                    .expanded
                    .retain(|schema| info.schemas.iter().any(|item| item.name == *schema));
                state.expanded.insert(state.selection.schema.clone());
                state.detail = None;
                state.cell = None;
                if object.kind.has_rows() {
                    state.last_page = max_page_for_object(object, state.rows_per_page);
                    if let Some(last_page) = state.last_page {
                        state.page = state.page.min(last_page);
                    }
                    DbPreviewSyncAction::LoadPage {
                        key: state.selection.clone(),
                        page: state.page,
                    }
                } else {
                    state.last_page = None;
                    DbPreviewSyncAction::LoadDetail(state.selection.clone())
                }
            }
        };

        if replace_from_initial {
            self.replace_db_preview_from_initial(path, source_revision, info);
            return;
        }
        match action {
            DbPreviewSyncAction::None => {}
            DbPreviewSyncAction::LoadPage { key, page } => {
                self.dispatch_db_page_load(key, page, false);
            }
            DbPreviewSyncAction::LoadDetail(key) => self.dispatch_db_detail_load(key),
        }
    }

    fn replace_db_preview_from_initial(
        &mut self,
        path: &str,
        source_revision: u64,
        info: &reef_sqlite_preview::DatabaseInfoV2,
    ) {
        self.db_preview = Some(DbPreviewState::from_initial(
            path,
            source_revision,
            info,
            reef_core::preview::INITIAL_DB_PAGE_ROWS,
        ));
        self.db_page_load.invalidate();
        self.db_detail_load.invalidate();
        self.invalidate_db_cell_load();
    }

    fn clear_db_preview_state(&mut self) {
        if self.db_preview.is_some() {
            self.db_page_load.invalidate();
            self.db_detail_load.invalidate();
            self.invalidate_db_cell_load();
        }
        self.db_preview = None;
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
        let Some((cur_key, cur_page, last_page, current_row_count, rows_per_page)) =
            self.db_preview.as_ref().map(|s| {
                (
                    s.selection.clone(),
                    s.page,
                    s.last_page,
                    s.current_rows.len(),
                    s.rows_per_page,
                )
            })
        else {
            return;
        };
        let Some(info) = self.preview_database_info() else {
            return;
        };
        let visible: Vec<reef_sqlite_preview::DbObjectKey> = info
            .iter_row_bearing()
            .map(reef_sqlite_preview::DbObject::key)
            .collect();
        if visible.is_empty() {
            return;
        }
        let cur_idx = visible
            .iter()
            .position(|key| key == &cur_key)
            .unwrap_or(0)
            .min(visible.len() - 1);
        let max_idx = visible.len() - 1;
        let current_max_page = info
            .lookup(&cur_key)
            .and_then(|object| max_page_for_object(object, rows_per_page))
            .or(last_page);
        let can_probe_next = current_row_count == rows_per_page as usize;
        let (new_idx, new_page) = match action {
            DbNav::PrevPage => (cur_idx, cur_page.saturating_sub(1)),
            DbNav::NextPage => (
                cur_idx,
                match current_max_page {
                    Some(max_page) => (cur_page + 1).min(max_page),
                    None if can_probe_next => cur_page + 1,
                    None => cur_page,
                },
            ),
            DbNav::PrevTable => (cur_idx.saturating_sub(1), 0),
            DbNav::NextTable => ((cur_idx + 1).min(max_idx), 0),
            DbNav::FirstPage => (cur_idx, 0),
            DbNav::LastPage => (cur_idx, current_max_page.unwrap_or(cur_page)),
        };
        if new_idx == cur_idx && new_page == cur_page {
            return;
        }
        self.dispatch_db_page_load(visible[new_idx].clone(), new_page, new_idx != cur_idx);
    }

    pub fn db_navigate_to_page(&mut self, page_one_based: u64) {
        let Some((selection, cur_page, last_page, rows_per_page)) = self
            .db_preview
            .as_ref()
            .map(|s| (s.selection.clone(), s.page, s.last_page, s.rows_per_page))
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
        let requested_page = page_one_based.saturating_sub(1);
        let max_page = max_page_for_object(object, rows_per_page).or(last_page);
        let target_page = match max_page {
            Some(max_page) => requested_page.min(max_page),
            None => requested_page,
        };
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
        self.invalidate_db_cell_load();
        if let Some(state) = self.db_preview.as_mut() {
            state.cell = None;
        }
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
        self.invalidate_db_cell_load();
        if let Some(state) = self.db_preview.as_mut() {
            state.cell = None;
        }
        self.tasks
            .load_db_detail(generation, Arc::clone(&self.backend), path, key);
    }

    pub fn dispatch_db_cell_load(&mut self, row: usize, column: usize) {
        let Some((path, object_key, row_offset, row_locator)) =
            self.db_preview.as_ref().and_then(|state| {
                state.current_rows.get(row)?.get(column)?;
                let row_locator = state.current_row_locators.get(row)?.clone();
                Some((
                    PathBuf::from(&state.path),
                    state.selection.clone(),
                    state
                        .page
                        .saturating_mul(state.rows_per_page as u64)
                        .saturating_add(row as u64),
                    row_locator,
                ))
            })
        else {
            return;
        };
        self.cancel_db_cell_request();
        let generation = self.db_cell_load.begin();
        let cancellation = reef_io::CancellationToken::default();
        self.db_cell_cancellation = Some(cancellation.clone());
        if let Some(state) = self.db_preview.as_mut() {
            state.cell = Some(crate::DbCellPreviewState {
                object_key: object_key.clone(),
                row_offset,
                row_locator: row_locator.clone(),
                column,
                value: None,
            });
        }
        self.tasks.load_db_cell(
            generation,
            Arc::clone(&self.backend),
            DbCellRequest {
                path,
                key: object_key,
                row_offset,
                row_locator,
                column,
                cancellation,
            },
        );
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
