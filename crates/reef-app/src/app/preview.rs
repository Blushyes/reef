use super::*;

impl AppState {
    pub fn set_preview_highlight_persistent(
        &mut self,
        path: PathBuf,
        row: usize,
        byte_range: Range<usize>,
    ) {
        self.set_preview_highlight_with_fade(path, row, byte_range, HighlightFade::Persistent);
    }

    pub fn set_preview_highlight_with_fade(
        &mut self,
        path: PathBuf,
        row: usize,
        byte_range: Range<usize>,
        fade: HighlightFade,
    ) {
        self.preview_highlight = Some(PreviewHighlight {
            path,
            row,
            byte_range,
            fade,
            pending_utf16: None,
        });
    }

    pub fn set_preview_highlight_pending_utf16(&mut self, pending_utf16: Option<Range<u32>>) {
        if let Some(highlight) = self.preview_highlight.as_mut() {
            highlight.pending_utf16 = pending_utf16;
        }
    }

    pub fn start_preview_highlight_counting(&mut self, since: Instant) {
        if let Some(highlight) = self.preview_highlight.as_mut() {
            highlight.fade = HighlightFade::Counting { since };
        }
    }

    pub fn clear_preview_highlight(&mut self) {
        self.preview_highlight = None;
    }

    pub fn restore_preview_scroll_and_clear_highlight(&mut self, target: &LocationSnapshot) {
        self.preview_scroll = target.scroll.vertical;
        self.preview_h_scroll = target.scroll.horizontal;
        self.clear_preview_highlight();
    }

    pub fn center_preview_on_line(&mut self, line: usize, view_h: usize) {
        self.preview_scroll = center_scroll(line, view_h);
    }

    pub fn load_preview(&mut self) {
        if let Some(entry) = self.file_tree.selected_entry()
            && !entry.is_dir
        {
            self.load_preview_for_path(entry.path.clone());
        }
    }

    pub fn load_preview_for_path(&mut self, rel_path: PathBuf) {
        if let Some(hl) = self.preview_highlight.as_ref()
            && hl.path != rel_path
        {
            self.preview_highlight = None;
        }
        self.preview_schedule = Some((rel_path, Instant::now() + PREVIEW_DEBOUNCE));
        self.prefetch_schedule = None;
    }

    pub fn dispatch_preview_load(
        &mut self,
        rel_path: PathBuf,
        dark: bool,
        wants_decoded_image: bool,
    ) {
        let generation = self.preview_load.begin();
        self.preview_in_flight_path = Some(rel_path.clone());
        self.tasks.load_preview(
            generation,
            Arc::clone(&self.backend),
            rel_path,
            dark,
            wants_decoded_image,
        );
    }

    pub fn reload_preview_now(&mut self, dark: bool, wants_decoded_image: bool) {
        let Some(entry) = self.file_tree.selected_entry() else {
            return;
        };
        if entry.is_dir {
            return;
        }
        let path = entry.path.clone();
        self.preview_schedule = None;
        self.prefetch_schedule = None;
        self.dispatch_preview_load(path, dark, wants_decoded_image);
    }

    pub fn drain_preview_schedule(&mut self, now: Instant, options: TickOptions) {
        let Some((_, deadline)) = self.preview_schedule.as_ref() else {
            return;
        };
        if now < *deadline {
            return;
        }
        let (path, _) = self.preview_schedule.take().expect("checked above");
        self.dispatch_preview_load(path, options.dark, options.wants_decoded_image);
    }

    pub fn drain_prefetch_schedule(&mut self, now: Instant, options: TickOptions) {
        let Some(deadline) = self.prefetch_schedule else {
            return;
        };
        if now < deadline {
            return;
        }
        self.prefetch_schedule = None;
        if self.preview_schedule.is_some() {
            return;
        }
        self.prefetch_preview_neighbors(options);
    }

    pub fn apply_preview_result(
        &mut self,
        generation: u64,
        result: Result<Option<PreviewContent>, String>,
        preview_view_h: usize,
    ) -> PreviewMergeOutcome {
        match result {
            Ok(content) => self.apply_preview_content(generation, content, preview_view_h),
            Err(error) => {
                if self.preview_load.complete_err(generation, error) {
                    self.preview_load.stale = false;
                    self.preview_load.error = None;
                    self.preview_in_flight_path = None;
                }
                PreviewMergeOutcome::default()
            }
        }
    }

    pub fn apply_preview_content(
        &mut self,
        generation: u64,
        content: Option<PreviewContent>,
        preview_view_h: usize,
    ) -> PreviewMergeOutcome {
        if !self.preview_load.complete_ok(generation) {
            return PreviewMergeOutcome::default();
        }
        self.preview_in_flight_path = None;
        let same_file = matches!(
            (self.preview_content.as_deref(), content.as_ref()),
            (Some(old), Some(new)) if old.path == new.path
        );
        self.preview_content = content.map(Arc::new);
        if !same_file {
            self.preview_scroll = 0;
            self.preview_h_scroll = 0;
        }
        self.db_goto_input = None;
        self.db_goto_cursor = 0;
        self.sync_db_preview_state();
        if let Some(highlight) = self.preview_highlight.as_ref()
            && self.preview_is_for(&highlight.path)
        {
            self.preview_scroll = center_scroll(highlight.row, preview_view_h);
        }
        if self.preview_schedule.is_none() {
            self.prefetch_schedule = Some(Instant::now() + PREFETCH_DELAY);
        }
        PreviewMergeOutcome {
            accepted: true,
            same_file,
            clear_preview_selection: !same_file,
            resolve_pending_highlight: true,
        }
    }

    pub fn preview_is_for(&self, path: &Path) -> bool {
        self.preview_content
            .as_ref()
            .map(|preview| preview.path == path.to_string_lossy())
            .unwrap_or(false)
    }

    fn prefetch_preview_neighbors(&self, options: TickOptions) {
        if self.active_tab != AppTab::Files {
            return;
        }
        if self.place_mode.active || self.tree_edit.active || self.tree_context_menu.active {
            return;
        }
        let sel = self.file_tree.selected;
        let entries = &self.file_tree.entries;
        if entries.is_empty() || sel >= entries.len() {
            return;
        }
        let candidates = [
            sel.checked_sub(1),
            (sel + 1 < entries.len()).then_some(sel + 1),
        ];
        for idx in candidates.into_iter().flatten() {
            let entry = &entries[idx];
            if entry.is_dir {
                continue;
            }
            self.tasks.prefetch_preview(
                Arc::clone(&self.backend),
                entry.path.clone(),
                options.dark,
                options.wants_decoded_image,
            );
        }
    }
}
