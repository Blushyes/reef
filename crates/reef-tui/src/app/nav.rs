//! Code-navigation request side (`gd` / `gr` / Ctrl+click / Ctrl-o /
//! Ctrl-i) plus the LSP-refine + post-jump-highlight glue. Split out of
//! `app/mod.rs` as a child `impl App` block so this ~900-line subsystem
//! lives together instead of bloating the core App module — `app::nav`
//! is a child of `app`, so it still has access to `App`'s private state.
//!
//! Entry points are called from `crate::input` (keyboard/mouse) and the
//! `App::tick` worker-result drain; the four `pub(super)` methods are the
//! ones the core tick handler calls back into.

use super::*;

/// How a navigation request was triggered. Determines where the
/// candidates popup anchors if there's more than one result, and how
/// the engine picks the originating cursor.
#[derive(Debug, Clone, Copy)]
pub enum NavAnchor {
    Keyboard,
    Mouse { col: u16, row: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPosition {
    pub line: usize,
    pub byte_col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollPosition {
    pub vertical: usize,
    pub horizontal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationSurface {
    FilePreview,
    GitDiff {
        file_path: String,
        is_staged: bool,
    },
    GraphDiff {
        commit_oid: String,
        file_path: String,
    },
    SearchPreview,
}

/// Snapshot of the app location we are leaving before an explicit jump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationSnapshot {
    pub surface: LocationSurface,
    pub path: std::path::PathBuf,
    pub cursor: CursorPosition,
    pub scroll: ScrollPosition,
}

/// Async-jump intent registered when an LSP-only language fires a
/// definition request and waits for the worker response.
#[derive(Debug, Clone)]
pub struct NavPendingJump {
    pub lang: reef_core::nav::NavLang,
    pub cache_key: String,
    pub origin: LocationSnapshot,
    pub generation: u64,
}

/// Floating candidates popup for multi-target gd/gr results.
#[derive(Debug, Clone)]
pub struct NavCandidatesPopup {
    pub anchor_col: u16,
    pub anchor_row: u16,
    pub candidates: Vec<reef_core::nav::Location>,
    pub selected: usize,
    pub scroll: usize,
    pub current_path: std::path::PathBuf,
    pub origin: LocationSnapshot,
    pub opened_by_ctrl_click: bool,
    pub max_row_width: u16,
}

impl NavCandidatesPopup {
    pub const MAX_VISIBLE_ROWS: usize = 8;

    pub fn visible_rows(&self) -> usize {
        self.candidates.len().min(Self::MAX_VISIBLE_ROWS)
    }

    pub fn clamp_scroll(&mut self) {
        let visible = self.visible_rows();
        if visible == 0 {
            self.scroll = 0;
            return;
        }
        let max_scroll = self.candidates.len().saturating_sub(visible);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        self.scroll = self.scroll.min(max_scroll);
    }
}

// ─── Code navigation (gd / Ctrl+click / Ctrl-o-Ctrl-i) ────────────────
// Tree-sitter parse is attached to `PreviewBody::Text.parsed` by the
// preview worker; these methods are the request side.
impl App {
    /// Soft cap on the back/forward stacks. Prevents unbounded growth
    /// in long sessions; oldest entry is dropped on overflow.
    pub const NAV_HISTORY_CAP: usize = 64;

    /// How long the post-jump highlight lingers before tick clears it.
    /// VSCode's reveal highlight is ~1.5s; matching that keeps the
    /// muscle memory the same. Long enough to register the landing,
    /// short enough that the highlighted band doesn't compete with
    /// the actual code while you read.
    pub const PREVIEW_HIGHLIGHT_TTL: std::time::Duration = std::time::Duration::from_millis(1500);

    /// Hard cap on how long a fading highlight may sit `Pending` (target
    /// file still loading) before it's cleared anyway. Bounds the case
    /// where a cross-file jump's preview load never lands (deleted /
    /// unreadable file) — without it the band would wait forever for a
    /// file that never appears.
    pub const PREVIEW_HIGHLIGHT_LOAD_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

    /// Set a post-jump highlight that auto-fades after the TTL — the
    /// VSCode "Reveal" band used by `gd` / Ctrl-o navigation. The
    /// countdown is deferred until the target file is on screen (see
    /// `advance_preview_highlight_fade`).
    pub fn set_preview_highlight(
        &mut self,
        path: std::path::PathBuf,
        row: usize,
        byte_range: std::ops::Range<usize>,
    ) {
        self.set_highlight(
            path,
            row,
            byte_range,
            HighlightFade::Pending {
                armed_at: std::time::Instant::now(),
            },
        );
    }

    /// Set a PERSISTENT highlight that never auto-fades — the global-
    /// search result locator band. The user reads against it; clearing
    /// it on a timer (regression) would yank the locator out from under
    /// them while they're still parked on the result.
    pub fn set_preview_highlight_persistent(
        &mut self,
        path: std::path::PathBuf,
        row: usize,
        byte_range: std::ops::Range<usize>,
    ) {
        self.set_highlight(path, row, byte_range, HighlightFade::Persistent);
    }

    /// Shared constructor for the two public setters — single place the
    /// `PreviewHighlight` literal lives.
    fn set_highlight(
        &mut self,
        path: std::path::PathBuf,
        row: usize,
        byte_range: std::ops::Range<usize>,
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

    /// Jump to an LSP definition `loc`, resolving the symbol highlight at
    /// the right depth. Intra-file targets resolve immediately
    /// (`lsp_byte_range` has the source on hand). Cross-file targets load
    /// asynchronously, so the UTF-16 column span is stashed on the
    /// highlight for `resolve_pending_highlight` to convert once the
    /// destination preview lands — that's what lights up the actual
    /// identifier on the other file instead of just its row.
    pub(super) fn nav_jump_to_lsp(&mut self, loc: &reef_core::nav::LspLocation) {
        // `lsp_byte_range` resolves synchronously ONLY for the on-screen
        // (intra-file) target; for a cross-file target it returns empty
        // because the source isn't loaded yet. Capture which case we're
        // in BEFORE the jump (the jump's async load doesn't change the
        // on-screen file synchronously, but read it up front to be
        // explicit) so we only DEFER for a genuine cross-file jump.
        let cross_file = !self.preview_is_for(&loc.path);
        let byte_range = self.lsp_byte_range(loc);
        self.nav_jump_to(
            loc.path.clone(),
            reef_core::nav::Location {
                path: Some(loc.path.clone()),
                line: loc.line as usize,
                byte_range,
                snippet: String::new(),
            },
        );
        // Defer the column→byte resolution until the destination preview
        // lands — but ONLY for a cross-file jump with a real span. An
        // intra-file jump is already resolved (or genuinely empty, in
        // which case `resolve_pending_highlight` would never run for the
        // already-loaded file, leaving a dangling pending), so it must
        // not set one.
        if cross_file
            && loc.character_end > loc.character
            && let Some(hl) = self.preview_highlight.as_mut()
        {
            hl.pending_utf16 = Some(loc.character..loc.character_end);
        }
    }

    /// Convert a cross-file LSP jump's deferred UTF-16 columns
    /// (`PreviewHighlight::pending_utf16`) into a real byte range now that
    /// the destination preview is loaded and its parsed source is on
    /// hand. No-op unless the on-screen preview is the highlight's file,
    /// is a parsed text body, and has a pending span. Called from the
    /// `Preview` worker-result branch.
    pub(super) fn resolve_pending_highlight(&mut self) {
        let Some(hl) = self.preview_highlight.as_ref() else {
            return;
        };
        let Some(span) = hl.pending_utf16.clone() else {
            return;
        };
        let (path, row) = (hl.path.clone(), hl.row);
        let byte_range = self.utf16_range_on_preview(&path, row, span.start, span.end);
        // Only commit a real (non-empty) resolution. An empty result
        // means the preview isn't this file yet, isn't parsed, or the
        // row/columns fell out of range — leave the pending span set so a
        // later, correct preview-load can still resolve it.
        if byte_range.is_empty() {
            return;
        }
        if let Some(hl) = self.preview_highlight.as_mut() {
            hl.byte_range = byte_range;
            hl.pending_utf16 = None;
        }
    }

    /// Drive the post-jump highlight fade. Idempotent and cheap — runs
    /// every tick. Persistent highlights are left alone. A `Pending`
    /// fading highlight starts its TTL only once the target file is on
    /// screen, but is force-cleared after `PREVIEW_HIGHLIGHT_LOAD_GRACE`
    /// if the file never loads.
    pub fn advance_preview_highlight_fade(&mut self) {
        enum FadeStep {
            Keep,
            StartCounting,
            Clear,
        }
        let now = std::time::Instant::now();
        let Some(hl) = self.preview_highlight.as_ref() else {
            return;
        };
        // Decide while holding only the immutable borrow, then apply a
        // single mutation after it ends — no double Option lookup.
        let step = match hl.fade {
            HighlightFade::Persistent => FadeStep::Keep,
            HighlightFade::Pending { armed_at } => {
                // `shown` is only needed here — compute lazily so the
                // common Persistent/Counting cases don't pay the
                // `to_string_lossy` allocation every tick.
                let shown = self.preview_is_for(&hl.path);
                if shown {
                    FadeStep::StartCounting
                } else if now.duration_since(armed_at) > Self::PREVIEW_HIGHLIGHT_LOAD_GRACE {
                    FadeStep::Clear
                } else {
                    FadeStep::Keep
                }
            }
            HighlightFade::Counting { since } => {
                if now.duration_since(since) > Self::PREVIEW_HIGHLIGHT_TTL {
                    FadeStep::Clear
                } else {
                    FadeStep::Keep
                }
            }
        };
        match step {
            FadeStep::Keep => {}
            FadeStep::StartCounting => {
                if let Some(h) = self.preview_highlight.as_mut() {
                    h.fade = HighlightFade::Counting { since: now };
                }
            }
            FadeStep::Clear => self.preview_highlight = None,
        }
    }

    /// Apply an LSP supervisor state change. Extracted from the worker-
    /// result drain so the "Off/Crashed clears a waiting pending jump"
    /// invariant is directly testable. A spawn-failure / crash means
    /// the matching `LspRefineDone` won't arrive, so a Vue-style
    /// pending jump would otherwise sit forever — drop it and tell the
    /// user the server is unavailable.
    pub fn handle_lsp_state_change(
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

    /// Re-probe which LSP binaries are on PATH and cache the result.
    /// Called off the render path (construction + post-install) so
    /// render reads `lsp_installed` instead of walking PATH per frame.
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

    /// Convert an absolute path returned by an LSP server into a
    /// workdir-relative path the nav pipeline (`file_tree.reveal`,
    /// `load_preview_for_path`, the `intra_file` check) expects.
    /// Returns `None` when the path is outside the workspace (e.g. a
    /// definition in a dependency under `~/.cargo`) — callers surface
    /// a toast rather than jumping nowhere. Canonicalizes both sides
    /// because rust-analyzer returns canonical paths while
    /// `workdir_path()` may not be.
    pub fn workdir_relative(&self, abs: &std::path::Path) -> Option<std::path::PathBuf> {
        let root = self.backend.workdir_path();
        let canon_root = std::fs::canonicalize(&root).unwrap_or(root.clone());
        // Canonicalize `abs` so a symlinked workdir (macOS /var →
        // /private/var, tempdirs) doesn't defeat strip_prefix. When
        // `abs` itself doesn't exist on disk (LSP virtual/generated
        // files — Volar virtual TS, proc-macro output), full
        // canonicalize fails, so fall back to canonicalizing its
        // PARENT (which usually does exist) and re-appending the leaf.
        // That keeps both sides in the same namespace.
        let canon_abs =
            std::fs::canonicalize(abs).unwrap_or_else(|_| match (abs.parent(), abs.file_name()) {
                (Some(parent), Some(name)) => std::fs::canonicalize(parent)
                    .map(|cp| cp.join(name))
                    .unwrap_or_else(|_| abs.to_path_buf()),
                _ => abs.to_path_buf(),
            });
        // Try the canonical root first, then the raw root, so an
        // in-workspace file is found whichever namespace `abs` landed
        // in.
        canon_abs
            .strip_prefix(&canon_root)
            .or_else(|_| canon_abs.strip_prefix(&root))
            .ok()
            .map(|p| p.to_path_buf())
    }

    /// `gd` keyboard / Ctrl+click entry point. Resolves the click site
    /// to a `(line, byte_col)` cursor, looks for matching definitions
    /// in the current file's tree-sitter parse, and either jumps
    /// directly (single candidate) or opens the candidates popup
    /// (multiple). No-op when there's no parsed file, no cursor, or
    /// no candidates — Phase 3 may surface a toast for the last case.
    pub fn goto_definition_at_cursor(&mut self, anchor: crate::app::nav::NavAnchor) {
        // The popup or an in-flight LSP-only jump owns navigation —
        // a bare `gd` shouldn't re-resolve under either.
        if self.nav_candidates.is_some() || self.nav_pending_lsp_jump.is_some() {
            return;
        }
        let Some(cursor) = self.resolve_nav_cursor(anchor) else {
            return;
        };
        let Some(preview) = self.preview_content.as_ref() else {
            return;
        };
        let current_path = std::path::PathBuf::from(&preview.path);
        let parsed = match &preview.body {
            reef_core::preview::PreviewBody::Text(text) => match text.parsed.as_ref() {
                Some(p) => std::sync::Arc::clone(p),
                None => return,
            },
            _ => return,
        };

        // LSP-only languages (Vue) — tree-sitter has no semantic
        // queries here (`<script>` is a raw_text blob), so identifier
        // extraction returns None and the rest of the flow gives up.
        // Mirror VSCode's Vue extension: the client sends a vanilla
        // `textDocument/definition { uri, position }`, all SFC virtual
        // -file mapping happens server-side (Volar's
        // `@vue/typescript-plugin`). We register a pending jump and
        // let the tick-time response drain execute it. Skipped in SSH
        // mode (LSP disabled by design).
        if !parsed.language.has_semantic_queries()
            && parsed.language.profile().lsp.is_some()
            && !self.backend.is_remote()
        {
            self.goto_definition_lsp_only(anchor, cursor, parsed.language, &current_path, &parsed);
            return;
        }

        // Snapshot the identifier text — needed for the workspace
        // fallback + the find-references fallthrough.
        let needle = reef_core::nav::identifier_at(&parsed, cursor).map(str::to_owned);

        // Phase 3 — consult the LSP refine cache FIRST, keyed by
        // POSITION (`lang, path:line:col`) not by bare name, so two
        // distinct same-named symbols don't share one cached target.
        // A hit is the authoritative answer; jump straight to it.
        let cache_key = reef_core::nav::refine_key(&current_path, cursor);
        // Cached paths are already workdir-relative (converted at
        // write time in the LspRefineDone handler), so a hit is the
        // authoritative answer — jump straight to it.
        let refined: Option<reef_core::nav::Location> = self
            .nav_refine_cache
            .get(&(parsed.language, cache_key.clone()))
            .map(|loc| reef_core::nav::Location {
                path: Some(loc.path.clone()),
                line: loc.line as usize,
                byte_range: self.lsp_byte_range(loc),
                snippet: String::new(),
            });
        let from_cache = refined.is_some();

        let mut candidates = if let Some(loc) = refined {
            vec![loc]
        } else {
            reef_core::nav::intrafile::resolve_definition_intrafile(&parsed, cursor)
        };

        // Phase 2 cross-file fallback: append workspace-wide definitions
        // for the same identifier, filtered by language. Skipped when
        // the LSP cache already gave us an authoritative answer (a
        // single precise location must not be diluted into a picker).
        if !from_cache
            && candidates.len() <= 1
            && let Some(name) = needle.as_deref()
            && let Some(ws) = self.nav_workspace.as_ref()
            && let Some(defs) = ws.defs_by_name.get(name)
        {
            let current_rel = current_path.to_string_lossy().to_string();
            for d in defs {
                if d.lang != parsed.language {
                    continue;
                }
                let path_str = d.path.to_string_lossy().to_string();
                if path_str == current_rel {
                    continue;
                }
                candidates.push(reef_core::nav::Location {
                    path: Some(d.path.clone()),
                    line: d.line,
                    byte_range: d.byte_range.clone(),
                    snippet: d.snippet.clone(),
                });
            }
        }

        // Phase 3 — fire a fire-and-forget LSP refine in the
        // background, keyed by the same position cache_key. Result
        // lands in `nav_refine_cache`; the current jump is untouched.
        // Skip if the cache already answered. `character` is converted
        // from a UTF-8 byte column to the UTF-16 column LSP expects.
        if !from_cache
            && needle.is_some()
            && parsed.language.profile().lsp.is_some()
            && !self.backend.is_remote()
        {
            let workspace_root = self.backend.workdir_path();
            let abs_file = workspace_root.join(&current_path);
            let utf16_col = reef_core::nav::byte_col_to_utf16(
                reef_core::nav::line_bytes_at(&parsed.source, cursor.0),
                cursor.1,
            );
            // generation 0: the semantic path is fire-and-forget and
            // registers no pending jump, so the response's generation
            // is never matched against anything — only the Vue / LSP-
            // only path (`goto_definition_lsp_only`) bumps and consults
            // `nav_refine_gen`.
            self.tasks.lsp_refine_definition(
                0,
                self.nav_refine_epoch,
                parsed.language,
                cache_key,
                workspace_root,
                abs_file,
                std::sync::Arc::clone(&parsed.source),
                cursor.0 as u32,
                utf16_col,
            );
        }

        let origin = self.snapshot_location();

        match candidates.len() {
            0 => {
                // No definition found — most commonly because the
                // user is clicking on the definition itself (the
                // skip-self filter in `resolve_definition_intrafile`
                // hides the only intra-file hit) or because no
                // declaration exists in this workspace. Either way,
                // fall through to references: VSCode does the same
                // when Ctrl+clicking on a decl. Skip the fallthrough
                // for "LSP-only" languages (e.g. Vue) where the empty
                // popup would be misleading — the refine we already
                // fired off feeds the cache, and the next click
                // hits the LSP answer.
                if needle.is_some() && parsed.language.has_semantic_queries() {
                    self.find_references_at_cursor(anchor);
                }
            }
            1 => {
                if let Some(origin) = origin {
                    self.nav_push_back(origin);
                }
                let loc = candidates.into_iter().next().expect("len==1 above");
                let target_path = loc.path.clone().unwrap_or(current_path);
                self.nav_jump_to(target_path, loc);
            }
            _ => {
                let Some(origin) = origin else {
                    return;
                };
                let (anchor_col, anchor_row) = self.compute_nav_popup_anchor(anchor);
                let max_row_width =
                    crate::ui::nav_candidates_popup::candidates_max_width(&candidates);
                self.nav_candidates = Some(crate::app::nav::NavCandidatesPopup {
                    anchor_col,
                    anchor_row,
                    candidates,
                    selected: 0,
                    scroll: 0,
                    current_path,
                    origin,
                    opened_by_ctrl_click: matches!(
                        anchor,
                        crate::app::nav::NavAnchor::Mouse { .. }
                    ),
                    max_row_width,
                });
            }
        }
    }

    /// Translate a `NavAnchor` into the file `(line, byte_col)` the
    /// engine cares about. Keyboard uses the last preview-selection
    /// anchor — reef has no persistent text cursor, but a single-click
    /// (no drag) leaves an empty selection at the click point which
    /// serves as the focus. Mouse routes through the shared
    /// `mouse_to_file_coord` helper.
    fn resolve_nav_cursor(&self, anchor: crate::app::nav::NavAnchor) -> Option<(usize, usize)> {
        match anchor {
            crate::app::nav::NavAnchor::Keyboard => self.preview_selection.map(|s| s.active),
            crate::app::nav::NavAnchor::Mouse { col, row } => {
                let origin = self.last_preview_content_origin?;
                crate::input::mouse_to_file_coord(self, col, row, origin)
            }
        }
    }

    /// Where to position the candidates popup. Mouse anchors one row
    /// below the click (so the popup doesn't cover the very token the
    /// user clicked); keyboard anchors below the focus row, defaulting
    /// to the viewport top when no selection exists.
    fn compute_nav_popup_anchor(&self, anchor: crate::app::nav::NavAnchor) -> (u16, u16) {
        match anchor {
            crate::app::nav::NavAnchor::Mouse { col, row } => (col, row.saturating_add(1)),
            crate::app::nav::NavAnchor::Keyboard => {
                let Some(origin) = self.last_preview_content_origin else {
                    return (0, 0);
                };
                let row_in_file = self
                    .preview_selection
                    .map(|s| s.active.0)
                    .unwrap_or(self.preview_scroll);
                let visible = row_in_file.saturating_sub(self.preview_scroll) as u16;
                (origin.0, origin.1.saturating_add(visible).saturating_add(1))
            }
        }
    }

    /// Push to the back-stack with cap enforcement (oldest dropped).
    pub(super) fn nav_push_back(&mut self, entry: crate::app::nav::LocationSnapshot) {
        self.location_history.push(entry);
    }

    pub fn snapshot_location(&self) -> Option<crate::app::nav::LocationSnapshot> {
        match self.active_tab {
            Tab::Files => self.snapshot_file_preview_location(),
            Tab::Search => self.snapshot_search_preview_location(),
            Tab::Git => self.snapshot_git_diff_location(),
            Tab::Graph => self.snapshot_graph_diff_location(),
        }
    }

    pub fn push_location_before_jump(&mut self) {
        if let Some(snapshot) = self.snapshot_location() {
            self.location_history.push(snapshot);
        }
    }

    fn snapshot_file_preview_location(&self) -> Option<crate::app::nav::LocationSnapshot> {
        let preview = self.preview_content.as_ref()?;
        let (line, byte_col) = self
            .preview_selection
            .map(|s| s.active)
            .unwrap_or((self.preview_scroll, 0));
        Some(crate::app::nav::LocationSnapshot {
            surface: crate::app::nav::LocationSurface::FilePreview,
            path: std::path::PathBuf::from(&preview.path),
            cursor: crate::app::nav::CursorPosition { line, byte_col },
            scroll: crate::app::nav::ScrollPosition {
                vertical: self.preview_scroll,
                horizontal: self.preview_h_scroll,
            },
        })
    }

    fn snapshot_search_preview_location(&self) -> Option<crate::app::nav::LocationSnapshot> {
        let preview = self.preview_content.as_ref()?;
        let (line, byte_col) = self
            .preview_selection
            .map(|s| s.active)
            .unwrap_or((self.preview_scroll, 0));
        Some(crate::app::nav::LocationSnapshot {
            surface: crate::app::nav::LocationSurface::SearchPreview,
            path: std::path::PathBuf::from(&preview.path),
            cursor: crate::app::nav::CursorPosition { line, byte_col },
            scroll: crate::app::nav::ScrollPosition {
                vertical: self.preview_scroll,
                horizontal: self.preview_h_scroll,
            },
        })
    }

    fn snapshot_git_diff_location(&self) -> Option<crate::app::nav::LocationSnapshot> {
        let selected = self.selected_file.as_ref()?;
        Some(crate::app::nav::LocationSnapshot {
            surface: crate::app::nav::LocationSurface::GitDiff {
                file_path: selected.path.clone(),
                is_staged: selected.is_staged,
            },
            path: std::path::PathBuf::from(&selected.path),
            cursor: crate::app::nav::CursorPosition {
                line: self.diff_scroll,
                byte_col: 0,
            },
            scroll: crate::app::nav::ScrollPosition {
                vertical: self.diff_scroll,
                horizontal: self.diff_h_scroll,
            },
        })
    }

    fn snapshot_graph_diff_location(&self) -> Option<crate::app::nav::LocationSnapshot> {
        let file_diff = self.commit_detail.file_diff.as_ref()?;
        let commit_oid = self.git_graph.selected_commit.clone().or_else(|| {
            self.git_graph
                .rows
                .get(self.git_graph.selected_idx)
                .map(|row| row.commit.oid.clone())
        })?;
        Some(crate::app::nav::LocationSnapshot {
            surface: crate::app::nav::LocationSurface::GraphDiff {
                commit_oid,
                file_path: file_diff.path.clone(),
            },
            path: std::path::PathBuf::from(&file_diff.path),
            cursor: crate::app::nav::CursorPosition {
                line: self.commit_detail.file_diff_scroll,
                byte_col: 0,
            },
            scroll: crate::app::nav::ScrollPosition {
                vertical: self.commit_detail.file_diff_scroll,
                horizontal: self.commit_detail.file_diff_h_scroll,
            },
        })
    }

    fn restore_preview_cursor(&mut self, target: &crate::app::nav::LocationSnapshot) {
        self.preview_scroll = target.scroll.vertical;
        self.preview_h_scroll = target.scroll.horizontal;
        self.preview_highlight = None;
        let cursor = (target.cursor.line, target.cursor.byte_col);
        let mut sel = crate::ui::selection::PreviewSelection::new(cursor);
        sel.active = cursor;
        sel.dragging = false;
        self.preview_selection = Some(sel);
    }

    pub fn jump_to_location(&mut self, target: crate::app::nav::LocationSnapshot) {
        self.g_pending_at = None;
        match target.surface.clone() {
            crate::app::nav::LocationSurface::FilePreview => {
                self.set_active_tab(Tab::Files);
                self.active_panel = Panel::Diff;
                self.file_tree.reveal(&target.path);
                self.refresh_file_tree_with_target(Some(target.path.clone()));
                self.restore_preview_cursor(&target);
                self.load_preview_for_path(target.path);
            }
            crate::app::nav::LocationSurface::SearchPreview => {
                self.set_active_tab(Tab::Search);
                self.active_panel = Panel::Diff;
                self.restore_preview_cursor(&target);
                self.load_preview_for_path(target.path);
            }
            crate::app::nav::LocationSurface::GitDiff {
                file_path,
                is_staged,
            } => {
                self.set_active_tab(Tab::Git);
                self.active_panel = Panel::Diff;
                self.select_file(&file_path, is_staged);
                self.diff_scroll = target.scroll.vertical;
                self.diff_h_scroll = target.scroll.horizontal;
            }
            crate::app::nav::LocationSurface::GraphDiff {
                commit_oid,
                file_path,
            } => {
                self.set_active_tab(Tab::Graph);
                self.active_panel = Panel::Diff;
                if let Some(idx) = self.git_graph.find_row_by_oid(&commit_oid) {
                    self.git_graph.selected_idx = idx;
                    self.git_graph.selected_commit = Some(commit_oid);
                    self.git_graph.selection_anchor = None;
                    self.commit_detail.range_detail = None;
                }
                self.load_commit_file_diff(&file_path);
                self.commit_detail.file_diff_scroll = target.scroll.vertical;
                self.commit_detail.file_diff_h_scroll = target.scroll.horizontal;
            }
        }
    }

    pub fn location_back(&mut self) {
        let target = self.location_history.back(self.snapshot_location());
        if let Some(target) = target {
            self.jump_to_location(target);
        }
    }

    pub fn location_forward(&mut self) {
        let target = self.location_history.forward(self.snapshot_location());
        if let Some(target) = target {
            self.jump_to_location(target);
        }
    }

    /// LSP-only `gd` path used for languages whose tree-sitter
    /// grammar can't surface identifiers (Vue's `<script>` raw_text
    /// blob). Skips identifier extraction entirely — vue-language-
    /// server (Volar) maps `(uri, position)` to virtual-TS code on
    /// its side. We use a position-encoded cache key so a repeated
    /// click at the same spot hits the cache instead of re-asking.
    fn goto_definition_lsp_only(
        &mut self,
        _anchor: crate::app::nav::NavAnchor,
        cursor: (usize, usize),
        lang: reef_core::nav::NavLang,
        current_path: &std::path::Path,
        parsed: &reef_core::nav::FileParse,
    ) {
        let cache_key = reef_core::nav::refine_key(current_path, cursor);

        // Cache hit — execute the jump synchronously. Cached paths are
        // already workdir-relative (converted at write time).
        if let Some(loc) = self.nav_refine_cache.get(&(lang, cache_key.clone())) {
            let loc = loc.clone();
            if let Some(origin) = self.snapshot_location() {
                self.nav_push_back(origin);
            }
            self.nav_jump_to_lsp(&loc);
            return;
        }

        // Cache miss — fire LSP, register pending jump, toast.
        self.nav_refine_gen += 1;
        let gen_id = self.nav_refine_gen;
        let workspace_root = self.backend.workdir_path();
        let abs_file = workspace_root.join(current_path);
        let Some(origin) = self.snapshot_location() else {
            return;
        };
        self.nav_pending_lsp_jump = Some(crate::app::nav::NavPendingJump {
            lang,
            cache_key: cache_key.clone(),
            origin,
            generation: gen_id,
        });
        let utf16_col = reef_core::nav::byte_col_to_utf16(
            reef_core::nav::line_bytes_at(&parsed.source, cursor.0),
            cursor.1,
        );
        self.tasks.lsp_refine_definition(
            gen_id,
            self.nav_refine_epoch,
            lang,
            cache_key,
            workspace_root,
            abs_file,
            std::sync::Arc::clone(&parsed.source),
            cursor.0 as u32,
            utf16_col,
        );
        let bin = lang.profile().lsp.as_ref().map(|p| p.bin).unwrap_or("LSP");
        self.toasts.push(Toast::info(format!("Querying {bin}…")));
    }

    /// True when the file currently in the preview is `path`. Single
    /// source of truth for the "is this the on-screen file?" check used
    /// by the intra-file jump fast path and `lsp_byte_range` — both
    /// compared `preview.path` against a relative path the same way,
    /// so they share this helper rather than open-coding the stringify.
    pub(super) fn preview_is_for(&self, path: &std::path::Path) -> bool {
        self.preview_content
            .as_ref()
            .map(|p| p.path == path.to_string_lossy())
            .unwrap_or(false)
    }

    /// Convert a UTF-16 column span (`start..end`) on line `row` of
    /// `path` to a per-line byte range — but only when `path` is the file
    /// currently in the preview AND it's a parsed text body, i.e. when
    /// its source is on hand. Returns `0..0` (empty) otherwise. Shared by
    /// `lsp_byte_range` (synchronous, intra-file) and
    /// `resolve_pending_highlight` (deferred, after a cross-file load)
    /// so the two never diverge. The LSP `character` is UTF-16 (the
    /// protocol default); reef highlight ranges are bytes, so a non-ASCII
    /// prefix on the line needs `utf16_range_to_byte` to land right.
    fn utf16_range_on_preview(
        &self,
        path: &std::path::Path,
        row: usize,
        start: u32,
        end: u32,
    ) -> std::ops::Range<usize> {
        // Single `preview_content` lookup: this is the one site that needs
        // BOTH the path check and the parsed body together, so it matches
        // them in one pass rather than calling `preview_is_for` (which
        // would re-fetch + re-stringify). `preview_is_for` still owns the
        // pure boolean checks elsewhere.
        let Some(preview) = self.preview_content.as_ref() else {
            return 0..0;
        };
        if preview.path != path.to_string_lossy() {
            return 0..0;
        }
        let reef_core::preview::PreviewBody::Text(text) = &preview.body else {
            return 0..0;
        };
        let Some(parse) = text.parsed.as_ref() else {
            return 0..0;
        };
        let line_bytes = reef_core::nav::line_bytes_at(&parse.source, row);
        reef_core::nav::utf16_range_to_byte(line_bytes, start, end)
    }

    /// Synchronous LSP-definition range, used when the target is (or may
    /// be) the on-screen file. Cross-file targets resolve later via
    /// `resolve_pending_highlight`.
    fn lsp_byte_range(&self, loc: &reef_core::nav::LspLocation) -> std::ops::Range<usize> {
        self.utf16_range_on_preview(
            &loc.path,
            loc.line as usize,
            loc.character,
            loc.character_end,
        )
    }

    /// Jump-to-location pathway shared by single-candidate `gd` and
    /// candidate-pick. Intra-file (same path as current preview) takes
    /// a fast path: no tab switch, no async reload, just scroll +
    /// highlight. Cross-file falls through to the full
    /// `enter_focused_preview_with_file`-shaped chain.
    fn nav_jump_to(&mut self, path: std::path::PathBuf, target: reef_core::nav::Location) {
        // Any committed jump cancels a half-typed `gg`/`gd`/`gr` chord.
        // The keyboard chord paths already clear it, but mouse Ctrl+click,
        // popup-pick, and cache-hit jumps reach here via the intra-file
        // fast path without going through `dispatch_preview_load`, so a
        // stray `g` armed just before the jump would otherwise survive
        // and make the next bare `g` resolve as `gg` (scroll to top).
        self.g_pending_at = None;
        let line = target.line;
        let byte_range = target.byte_range.clone();

        let intra_file = self.preview_is_for(&path);

        if intra_file {
            self.set_preview_highlight(path, line, byte_range);
            let view_h = self.last_preview_view_h as usize;
            self.preview_scroll = crate::search::center_scroll(line, view_h);
            return;
        }

        // Cross-file: same shape as `global_search::accept` /
        // `enter_focused_preview_with_file`. Tick's Preview branch
        // re-centers scroll using `preview_highlight.row` when the
        // worker result lands.
        self.set_active_tab(Tab::Files);
        self.file_tree.reveal(&path);
        self.refresh_file_tree_with_target(Some(path.clone()));
        self.set_preview_highlight(path.clone(), line, byte_range);
        self.load_preview_for_path(path);
    }

    /// `Ctrl-o` — pop the back-stack, push current state to forward,
    /// restore the popped entry. No-op when the back-stack is empty.
    pub fn nav_back(&mut self) {
        self.location_back();
    }

    /// `Ctrl-i` — symmetric to `nav_back`.
    pub fn nav_forward(&mut self) {
        self.location_forward();
    }

    /// User picked a candidate from the popup. Commits the navigation:
    /// pushes the originating cursor to the back-stack, jumps to the
    /// chosen location, closes the popup. Cross-file candidates carry
    /// their own `path`; intra-file ones default to the popup's
    /// `current_path`.
    pub fn nav_pick_candidate(&mut self) {
        let Some(popup) = self.nav_candidates.take() else {
            return;
        };
        let Some(target) = popup.candidates.into_iter().nth(popup.selected) else {
            return;
        };
        let path = target.path.clone().unwrap_or(popup.current_path);
        self.nav_push_back(popup.origin);
        self.nav_jump_to(path, target);
    }

    /// User dismissed the popup without picking. Drop the candidates;
    /// no back-stack mutation.
    pub fn nav_close_candidates(&mut self) {
        self.nav_candidates = None;
    }

    /// Resolve the per-language Settings row state.
    pub fn lsp_view_for(&self, lang: reef_core::nav::NavLang) -> crate::settings::LspRowState {
        use crate::settings::LspRowState;
        use reef_core::nav::LspBadge;
        let badge = self.lsp_states.get(&lang).cloned().unwrap_or(LspBadge::Off);
        if matches!(badge, LspBadge::Crashed) {
            return LspRowState::Crashed;
        }
        if matches!(badge, LspBadge::Booting) {
            return LspRowState::Booting;
        }
        if matches!(badge, LspBadge::Ready) {
            return LspRowState::Ready;
        }
        // Steady state: read the cached PATH-probe result (refreshed
        // off the render path by `refresh_lsp_installed`) instead of
        // walking PATH here — this runs per row per frame.
        if self.lsp_installed.get(&lang).copied().unwrap_or(false) {
            LspRowState::Available
        } else {
            LspRowState::Missing
        }
    }

    /// Settings-row click / Enter handler for an Lsp(NavLang) row.
    pub fn activate_lsp_row(&mut self, lang: reef_core::nav::NavLang) {
        use crate::settings::LspRowState;
        if self.lsp_view_for(lang) == LspRowState::Missing {
            let Some(profile) = lang.profile().lsp.as_ref() else {
                return;
            };
            let hint = profile
                .install_command
                .map(|cmd| format!("Install `{}`: {cmd}", profile.bin))
                .unwrap_or_else(|| format!("Install `{}` and put it on PATH", profile.bin));
            self.toasts.push(Toast::info(hint));
        }
    }

    /// Cursor key handlers for the popup. Up/Down wrap.
    pub fn nav_candidates_move(&mut self, delta: i32) {
        let Some(popup) = self.nav_candidates.as_mut() else {
            return;
        };
        let n = popup.candidates.len();
        if n == 0 {
            return;
        }
        let cur = popup.selected as i32;
        let next = (cur + delta).rem_euclid(n as i32) as usize;
        popup.selected = next;
        popup.clamp_scroll();
    }

    /// Mouse-wheel scroll over the candidates popup. Moves the visible
    /// window without changing `selected` (matches how list popups
    /// scroll independently of the highlighted row). Clamped to the
    /// last full page.
    pub fn nav_candidates_scroll(&mut self, delta: i32) {
        let Some(popup) = self.nav_candidates.as_mut() else {
            return;
        };
        let visible = popup.visible_rows();
        let max_scroll = popup.candidates.len().saturating_sub(visible);
        let next = (popup.scroll as i32 + delta).clamp(0, max_scroll as i32);
        popup.scroll = next as usize;
    }

    /// Kick a workspace symbol index build. Skipped when:
    /// - we're already loading (one build at a time),
    /// - the backend is remote (SSH mode = intra-file only).
    pub fn dispatch_nav_workspace_build(&mut self) {
        if self.backend.is_remote() {
            return;
        }
        if self.nav_workspace_load.loading {
            return;
        }
        let root = self.backend.workdir_path();
        let generation = self.nav_workspace_load.begin();
        self.tasks.build_nav_workspace(generation, root);
    }

    /// Phase 2 `gr` entry point. Looks up every reference site for the
    /// identifier under the cursor across the workspace, then opens
    /// the candidates popup populated with the hits. Falls back to
    /// intra-file matches when the workspace index isn't ready yet.
    pub fn find_references_at_cursor(&mut self, anchor: crate::app::nav::NavAnchor) {
        if self.nav_candidates.is_some() || self.nav_pending_lsp_jump.is_some() {
            return;
        }
        let Some(cursor) = self.resolve_nav_cursor(anchor) else {
            return;
        };
        let Some(preview) = self.preview_content.as_ref() else {
            return;
        };
        let current_path = std::path::PathBuf::from(&preview.path);
        let parsed = match &preview.body {
            reef_core::preview::PreviewBody::Text(text) => match text.parsed.as_ref() {
                Some(p) => std::sync::Arc::clone(p),
                None => return,
            },
            _ => return,
        };
        let Some(needle) = reef_core::nav::identifier_at(&parsed, cursor) else {
            return;
        };
        let needle = needle.to_owned();

        // Pull references from the workspace index. Filter by the
        // file's NavLang so a `foo` in Python doesn't surface for a
        // `foo` in Rust.
        let mut candidates: Vec<reef_core::nav::Location> = Vec::new();
        if let Some(ws) = self.nav_workspace.as_ref()
            && let Some(refs) = ws.refs_by_name.get(&needle)
        {
            for r in refs {
                if r.lang != parsed.language {
                    continue;
                }
                candidates.push(reef_core::nav::Location {
                    path: Some(r.path.clone()),
                    line: r.line,
                    byte_range: r.byte_range.clone(),
                    snippet: r.snippet.clone(),
                });
            }
        }

        // Zero hits → a toast, NOT an empty invisible popup. An empty
        // popup renders nothing but still captures the keyboard until
        // the user presses a key, which reads as a frozen UI.
        if candidates.is_empty() {
            self.toasts
                .push(Toast::info(format!("No references to `{needle}`")));
            return;
        }

        let (anchor_col, anchor_row) = self.compute_nav_popup_anchor(anchor);
        let Some(origin) = self.snapshot_location() else {
            return;
        };
        let max_row_width = crate::ui::nav_candidates_popup::candidates_max_width(&candidates);
        self.nav_candidates = Some(crate::app::nav::NavCandidatesPopup {
            anchor_col,
            anchor_row,
            candidates,
            selected: 0,
            scroll: 0,
            current_path,
            origin,
            opened_by_ctrl_click: matches!(anchor, crate::app::nav::NavAnchor::Mouse { .. }),
            max_row_width,
        });
    }

    // ─── Code navigation FROM a diff view ─────────────────────────────────
    //
    // The Git-tab and Graph-tab diff panels don't carry a tree-sitter
    // `FileParse` (the rendered content is hunks, not a clean file), so
    // these can't run the intra-file / LSP tiers the preview path uses.
    // Instead they resolve the clicked identifier *by name* against the
    // workspace index — which already maps every identifier to its
    // definition / reference sites across the repo (including the current
    // file), so same-file and cross-file both work. The jump itself reuses
    // `nav_jump_to` and the candidates popup, landing in the Files-tab
    // preview. Ctrl-o restores preview positions only.

    /// Identifier + location resolved from a click / cursor inside the
    /// active diff panel. `line` is the 0-based file line (new side
    /// preferred); `anchor_col/row` position a candidates popup below the
    /// hit.
    fn resolve_diff_nav(&self, anchor: crate::app::nav::NavAnchor) -> Option<DiffNavCursor> {
        // Pick the diff that's currently rendered (Git working-tree/staged
        // vs Graph commit). Only one renders at a time, and it owns
        // `last_diff_hit`.
        let (display, path_str) = match self.active_tab {
            Tab::Git => {
                let d = self.diff_content.as_ref()?;
                (&d.display, d.diff.path.as_str())
            }
            Tab::Graph => {
                let f = self.commit_detail.file_diff.as_ref()?;
                (&f.display, f.path.as_str())
            }
            _ => return None,
        };
        let hit = self.last_diff_hit.as_ref()?;
        let lang = std::path::Path::new(path_str)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(reef_core::nav::NavLang::from_extension)?;

        // Resolve (display-row index, identifier byte range, SBS side) +
        // a screen anchor for the popup. Both paths reuse `DiffHit`'s
        // identifier extraction so the resolver and the Ctrl+hover
        // underline agree on what's clickable. Mouse routes screen coords
        // through `identifier_at`; keyboard reads the diff selection caret
        // and goes through `identifier_in_row`.
        let (row_idx, range, side, anchor_col, anchor_row) = match anchor {
            crate::app::nav::NavAnchor::Mouse { col, row } => {
                let (r, range, side) = hit.identifier_at(col, row)?;
                (r, range, side, col, row.saturating_add(1))
            }
            crate::app::nav::NavAnchor::Keyboard => {
                let sel = self.diff_selection?;
                let (r, b) = sel.sel.active;
                let range = hit.identifier_in_row(r, b, sel.side)?;
                let visible = (r.saturating_sub(hit.scroll)) as u16;
                let acol = match sel.side {
                    reef_core::diff::DiffSide::SbsLeft => hit.content_x_left,
                    reef_core::diff::DiffSide::SbsRight => hit.content_x_right,
                    reef_core::diff::DiffSide::Unified => hit.content_x_unified,
                };
                let arow = hit.content_y.saturating_add(visible).saturating_add(1);
                (r, range, sel.side, acol, arow)
            }
        };

        let identifier = hit.rows.get(row_idx)?.text_for(side).get(range)?.to_owned();
        // git line numbers are 1-based; the engine + workspace index are
        // 0-based rows.
        let line = display
            .nav_line_at(hit.layout, row_idx, side)?
            .saturating_sub(1) as usize;

        Some(DiffNavCursor {
            identifier,
            lang,
            path: std::path::PathBuf::from(path_str),
            line,
            anchor_col,
            anchor_row,
        })
    }

    /// `gd` / Ctrl+click inside a diff. Resolves the clicked identifier to
    /// its workspace definition(s); single → jump, multiple → peek popup,
    /// none → fall through to references (mirroring the preview path).
    pub fn goto_definition_in_diff(&mut self, anchor: crate::app::nav::NavAnchor) {
        if self.nav_candidates.is_some() || self.nav_pending_lsp_jump.is_some() {
            return;
        }
        let Some(c) = self.resolve_diff_nav(anchor) else {
            return;
        };
        let mut candidates: Vec<reef_core::nav::Location> = Vec::new();
        if let Some(ws) = self.nav_workspace.as_ref()
            && let Some(defs) = ws.defs_by_name.get(&c.identifier)
        {
            let cur_rel = c.path.to_string_lossy();
            for d in defs {
                if d.lang != c.lang {
                    continue;
                }
                // Skip the definition that IS the click site — pressing
                // `gd` on a decl shouldn't echo back to itself.
                if d.line == c.line && d.path.to_string_lossy() == cur_rel {
                    continue;
                }
                candidates.push(reef_core::nav::Location {
                    path: Some(d.path.clone()),
                    line: d.line,
                    byte_range: d.byte_range.clone(),
                    snippet: d.snippet.clone(),
                });
            }
        }
        let origin = self.snapshot_location();
        match candidates.len() {
            0 => {
                // No definition — fall through to references, same as the
                // preview path does when clicking on a decl.
                self.find_references_in_diff(anchor);
            }
            1 => {
                if let Some(origin) = origin {
                    self.nav_push_back(origin);
                }
                let loc = candidates.into_iter().next().expect("len==1 above");
                let target_path = loc.path.clone().unwrap_or_else(|| c.path.clone());
                self.nav_jump_to(target_path, loc);
            }
            _ => {
                let Some(origin) = origin else {
                    return;
                };
                let max_row_width =
                    crate::ui::nav_candidates_popup::candidates_max_width(&candidates);
                self.nav_candidates = Some(crate::app::nav::NavCandidatesPopup {
                    anchor_col: c.anchor_col,
                    anchor_row: c.anchor_row,
                    candidates,
                    selected: 0,
                    scroll: 0,
                    current_path: c.path,
                    origin,
                    opened_by_ctrl_click: matches!(
                        anchor,
                        crate::app::nav::NavAnchor::Mouse { .. }
                    ),
                    max_row_width,
                });
            }
        }
    }

    /// `gr` inside a diff. Lists every workspace reference to the clicked
    /// identifier in the candidates popup.
    pub fn find_references_in_diff(&mut self, anchor: crate::app::nav::NavAnchor) {
        if self.nav_candidates.is_some() || self.nav_pending_lsp_jump.is_some() {
            return;
        }
        let Some(c) = self.resolve_diff_nav(anchor) else {
            return;
        };
        let mut candidates: Vec<reef_core::nav::Location> = Vec::new();
        if let Some(ws) = self.nav_workspace.as_ref()
            && let Some(refs) = ws.refs_by_name.get(&c.identifier)
        {
            for r in refs {
                if r.lang != c.lang {
                    continue;
                }
                candidates.push(reef_core::nav::Location {
                    path: Some(r.path.clone()),
                    line: r.line,
                    byte_range: r.byte_range.clone(),
                    snippet: r.snippet.clone(),
                });
            }
        }
        if candidates.is_empty() {
            self.toasts
                .push(Toast::info(format!("No references to `{}`", c.identifier)));
            return;
        }
        let Some(origin) = self.snapshot_location() else {
            return;
        };
        let max_row_width = crate::ui::nav_candidates_popup::candidates_max_width(&candidates);
        self.nav_candidates = Some(crate::app::nav::NavCandidatesPopup {
            anchor_col: c.anchor_col,
            anchor_row: c.anchor_row,
            candidates,
            selected: 0,
            scroll: 0,
            current_path: c.path,
            origin,
            opened_by_ctrl_click: matches!(anchor, crate::app::nav::NavAnchor::Mouse { .. }),
            max_row_width,
        });
    }
}

/// Resolved identifier + file location from a diff click/cursor — see
/// `App::resolve_diff_nav`.
struct DiffNavCursor {
    identifier: String,
    lang: reef_core::nav::NavLang,
    path: std::path::PathBuf,
    line: usize,
    anchor_col: u16,
    anchor_row: u16,
}
