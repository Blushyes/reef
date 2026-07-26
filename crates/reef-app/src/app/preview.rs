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
        self.bind_global_search_hit_accept_to_preview_generation(&rel_path, generation);
        self.preview_enrichment_dark = dark;
        self.preview_enrichment_pending = None;
        self.preview_in_flight_path = Some(rel_path.clone());
        self.tasks.load_preview(
            generation,
            Arc::clone(&self.backend),
            rel_path,
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
                    self.preview_enrichment_pending = None;
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
        self.preview_enrichment_pending = None;
        self.preview_in_flight_path = None;
        let same_file = matches!(
            (self.preview_content.as_deref(), content.as_ref()),
            (Some(old), Some(new)) if old.path == new.path
        );
        self.preview_content = content.map(Arc::new);
        self.preview_source_revision = generation;
        self.bump_preview_content_revision();
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
        }
    }

    pub fn complete_preview_enrichment(
        &mut self,
        generation: u64,
        path: &str,
        enrichment: Option<reef_core::preview::PreviewEnrichment>,
    ) -> bool {
        let Some(pending) = self.preview_enrichment_pending.as_ref() else {
            return false;
        };
        if pending.generation != generation || pending.path != path {
            return false;
        }
        self.preview_enrichment_pending = None;

        let Some(enrichment) = enrichment else {
            return true;
        };
        let Some(content) = self.preview_content.as_mut() else {
            return true;
        };
        if content.path != path {
            return true;
        }
        let content = Arc::make_mut(content);
        match (&mut content.body, enrichment) {
            (
                reef_core::preview::PreviewBody::Text(text),
                reef_core::preview::PreviewEnrichment::Text(enrichment),
            ) => {
                text.highlighted = enrichment.highlighted;
                text.parsed = enrichment.parsed;
            }
            (
                reef_core::preview::PreviewBody::Markdown(markdown),
                reef_core::preview::PreviewEnrichment::Markdown(enriched),
            ) if markdown.source == enriched.source => {
                *markdown = enriched;
            }
            _ => return true,
        }
        self.bump_preview_content_revision();
        true
    }

    pub fn request_current_preview_enrichment(&mut self, generation: u64) -> bool {
        if generation != self.preview_load.generation {
            return false;
        }
        let Some(content) = self.preview_content.as_deref() else {
            return false;
        };
        let path = content.path.clone();
        let queued = self
            .tasks
            .enrich_preview(generation, content, self.preview_enrichment_dark);
        self.preview_enrichment_pending =
            queued.then_some(PendingPreviewEnrichment { generation, path });
        queued
    }

    pub fn preview_enrichment_pending(&self) -> bool {
        self.preview_enrichment_pending.is_some()
    }

    fn bump_preview_content_revision(&mut self) {
        self.preview_content_revision = self.preview_content_revision.wrapping_add(1).max(1);
        self.preview_snapshot = self.preview_content.as_deref().map(|preview| {
            Arc::new(crate::PreviewDocumentSnapshot::from_document(
                preview,
                self.preview_content_revision,
                self.preview_source_revision,
            ))
        });
    }

    pub fn preview_is_for(&self, path: &Path) -> bool {
        self.preview_content
            .as_ref()
            .map(|preview| preview.path == path.to_string_lossy())
            .unwrap_or(false)
    }

    /// Returns whether the accepted preview or every outstanding preview
    /// request is for `path`. A matching pending request is sufficient while
    /// the first result is still loading; a request for another path is not.
    pub fn preview_target_matches(&self, path: &Path) -> bool {
        let schedule_matches = self
            .preview_schedule
            .as_ref()
            .is_none_or(|(scheduled, _)| scheduled == path);
        let in_flight_matches = self
            .preview_in_flight_path
            .as_ref()
            .is_none_or(|in_flight| in_flight == path);
        schedule_matches
            && in_flight_matches
            && (self.preview_is_for(path)
                || self
                    .preview_schedule
                    .as_ref()
                    .is_some_and(|(scheduled, _)| scheduled == path)
                || self
                    .preview_in_flight_path
                    .as_ref()
                    .is_some_and(|in_flight| in_flight == path))
    }

    /// Discards preview work that would otherwise overwrite a newly selected
    /// path. Worker requests cannot be interrupted, so invalidating their
    /// generation is the cancellation boundary for late results.
    pub fn cancel_preview_work_for_other_path(&mut self, path: &Path) {
        if self
            .preview_schedule
            .as_ref()
            .is_some_and(|(scheduled, _)| scheduled != path)
        {
            self.preview_schedule = None;
        }
        if self
            .preview_in_flight_path
            .as_ref()
            .is_some_and(|in_flight| in_flight != path)
        {
            self.preview_load.invalidate();
            self.preview_in_flight_path = None;
            self.preview_enrichment_pending = None;
        }
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
                options.wants_decoded_image,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Instant};

    use reef_core::preview::{
        PreviewBody, PreviewDocument, PreviewEnrichment, TextPreview, TextPreviewEnrichment,
    };
    use reef_core::text::{StyledToken, TextStyle};
    use reef_io::LocalBackend;

    use crate::{AppPrefs, AppState, AppStateConfig};

    #[test]
    fn preview_error_keeps_previous_content_and_exposes_error() {
        let backend = Arc::new(LocalBackend::open_at(PathBuf::from(".")));
        let mut state = AppState::new(AppStateConfig {
            backend,
            prefs: AppPrefs::default(),
            now: Instant::now(),
            subscribe_fs_events: false,
        });

        let accepted_generation = state.preview_load.begin();
        let outcome =
            state.apply_preview_content(accepted_generation, Some(text_preview("src/main.rs")), 20);
        assert!(outcome.accepted);

        let failing_generation = state.preview_load.begin();
        state.preview_in_flight_path = Some(PathBuf::from("src/broken.rs"));

        let outcome =
            state.apply_preview_result(failing_generation, Err("decoder failed".to_string()), 20);

        assert!(!outcome.accepted);
        assert_eq!(
            state
                .preview_content
                .as_ref()
                .map(|preview| preview.path.as_str()),
            Some("src/main.rs")
        );
        assert_eq!(state.preview_load.error.as_deref(), Some("decoder failed"));
        assert!(!state.preview_load.loading);
        assert!(!state.preview_load.stale);
        assert!(state.preview_in_flight_path.is_none());
    }

    #[test]
    fn preview_enrichment_updates_only_the_current_generation() {
        let backend = Arc::new(LocalBackend::open_at(PathBuf::from(".")));
        let mut state = AppState::new(AppStateConfig {
            backend,
            prefs: AppPrefs::default(),
            now: Instant::now(),
            subscribe_fs_events: false,
        });
        let generation = state.preview_load.begin();
        state.apply_preview_content(generation, Some(text_preview("src/main.rs")), 20);
        assert!(state.request_current_preview_enrichment(generation));
        let base_revision = state.preview_content_revision;
        let source_revision = state.preview_source_revision;

        let accepted = state.complete_preview_enrichment(
            generation,
            "src/main.rs",
            Some(PreviewEnrichment::Text(TextPreviewEnrichment {
                highlighted: Some(vec![vec![StyledToken::new(TextStyle::default(), "hello")]]),
                parsed: None,
            })),
        );

        assert!(accepted);
        assert!(state.preview_content_revision > base_revision);
        assert_eq!(state.preview_source_revision, source_revision);
        assert_eq!(
            state
                .preview_snapshot
                .as_deref()
                .map(|preview| preview.source_revision),
            Some(source_revision)
        );
        let Some(PreviewBody::Text(text)) = state
            .preview_content
            .as_deref()
            .map(|preview| &preview.body)
        else {
            panic!("expected text preview");
        };
        assert!(text.highlighted.is_some());

        let stale = state.complete_preview_enrichment(
            generation.wrapping_add(1),
            "src/main.rs",
            Some(PreviewEnrichment::Text(TextPreviewEnrichment {
                highlighted: None,
                parsed: None,
            })),
        );
        assert!(!stale);
    }

    #[test]
    fn preview_enrichment_completion_without_payload_clears_pending_request() {
        let backend = Arc::new(LocalBackend::open_at(PathBuf::from(".")));
        let mut state = AppState::new(AppStateConfig {
            backend,
            prefs: AppPrefs::default(),
            now: Instant::now(),
            subscribe_fs_events: false,
        });
        let generation = state.preview_load.begin();
        state.apply_preview_content(generation, Some(text_preview("src/main.rs")), 20);
        assert!(state.request_current_preview_enrichment(generation));
        let base_revision = state.preview_content_revision;

        assert!(state.complete_preview_enrichment(generation, "src/main.rs", None));
        assert!(!state.preview_enrichment_pending());
        assert_eq!(state.preview_content_revision, base_revision);
    }

    #[test]
    fn markdown_enrichment_replaces_the_matching_base_model() {
        let backend = Arc::new(LocalBackend::open_at(PathBuf::from(".")));
        let mut state = AppState::new(AppStateConfig {
            backend,
            prefs: AppPrefs::default(),
            now: Instant::now(),
            subscribe_fs_events: false,
        });
        let source = "```rs\nfn main() {}\n```\n";
        let base = reef_core::markdown::build_markdown_preview("README.md", source)
            .expect("base markdown preview");
        assert!(
            base.rows()
                .expect("base markdown render model")
                .iter()
                .flatten()
                .all(|span| span.syntax.is_none())
        );

        let generation = state.preview_load.begin();
        state.apply_preview_content(
            generation,
            Some(PreviewDocument {
                path: "README.md".to_string(),
                local_path: None,
                bytes_on_disk: source.len() as u64,
                mime: Some("text/markdown".to_string()),
                body: PreviewBody::Markdown(base),
            }),
            20,
        );
        assert!(state.request_current_preview_enrichment(generation));
        let base_revision = state.preview_content_revision;
        let source_revision = state.preview_source_revision;
        let enriched =
            reef_core::markdown::build_markdown_preview_with_syntax("README.md", source, false)
                .expect("enriched markdown preview");

        assert!(state.complete_preview_enrichment(
            generation,
            "README.md",
            Some(PreviewEnrichment::Markdown(enriched)),
        ));
        assert!(state.preview_content_revision > base_revision);
        assert_eq!(state.preview_source_revision, source_revision);
        assert_eq!(
            state
                .preview_snapshot
                .as_deref()
                .map(|preview| preview.source_revision),
            Some(source_revision)
        );
        let Some(PreviewBody::Markdown(markdown)) = state
            .preview_content
            .as_deref()
            .map(|preview| &preview.body)
        else {
            panic!("expected markdown preview");
        };
        assert_eq!(markdown.source, source);
        assert!(
            markdown
                .rows()
                .expect("enriched markdown render model")
                .iter()
                .flatten()
                .any(|span| span.syntax.is_some())
        );
    }

    fn text_preview(path: &str) -> PreviewDocument {
        PreviewDocument {
            path: path.to_string(),
            local_path: None,
            bytes_on_disk: 0,
            mime: Some("text/rust".to_string()),
            body: PreviewBody::Text(TextPreview {
                lines: vec!["fn main() {}".to_string()],
                highlighted: None,
                parsed: None,
            }),
        }
    }
}
