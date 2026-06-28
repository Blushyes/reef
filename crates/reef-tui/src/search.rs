//! Vim-style in-panel search (`/`, `?`, `n`, `N`, `Enter`, `Esc`).
//!
//! Target is implicit from the focused tab + panel at `begin()`. Each target
//! exposes a `rows(): Vec<String>` view used both for finding matches and for
//! driving per-row highlight overlays at render time. Jumping sets either a
//! `scroll` field (content panels) or a selection cursor (list panels).
//!
//! Match finding is smart-case substring (all-lowercase query = insensitive,
//! any uppercase = sensitive). Byte ranges in matches are absolute offsets
//! within the row's displayable text; `ui::text::overlay_match_highlight`
//! consumes them.

use crate::TuiApp as App;
use crate::keymap::{Command, InputScope, Keymap};
use crossterm::event::KeyEvent;

use reef_app::{AppCommand, SearchTarget, SearchViewport};

// ─── Entry points ─────────────────────────────────────────────────────────────

/// Start a search session. Picks the target from the focused panel, snapshots
/// pre-search scroll / selection so Esc can restore, and enters input mode.
pub fn begin(app: &mut App, backwards: bool) {
    let Some(target) = resolve_target(app) else {
        return;
    };
    app.engine
        .dispatch(AppCommand::BeginVimSearch { target, backwards });
}

/// Accept current match position, exit input mode. Matches stay populated so
/// `n` / `N` continue to work.
pub fn exit_confirm(app: &mut App) {
    app.engine.dispatch(AppCommand::ConfirmVimSearch);
}

/// Cancel: restore the pre-search scroll / selection and clear the session.
pub fn exit_cancel(app: &mut App) {
    app.engine.dispatch(AppCommand::CancelVimSearch {
        dark: app.theme.is_dark,
    });
}

/// Dispatch one key while in search input mode. Returns true if the key was
/// handled (always true when active — we fully own input while the prompt
/// is up).
pub fn handle_key_in_search_mode(key: KeyEvent, app: &mut App) {
    // Phase 1: prompt-specific shortcuts. Must precede `dispatch_key`,
    // which would otherwise treat Enter as Unhandled (no-op) and
    // Ctrl+C would fall through too.
    match Keymap::resolve(InputScope::VimSearch, &key) {
        Some(Command::Close) | Some(Command::Quit) => {
            exit_cancel(app);
            return;
        }
        Some(Command::Confirm) => {
            exit_confirm(app);
            return;
        }
        _ => {}
    }

    // Phase 2: shared text-input dispatch. Any edit re-runs
    // `recompute_and_jump` so the highlight tracks the query.
    if let Some(op) = crate::input_edit::op_for_key(&key) {
        app.engine.dispatch(AppCommand::EditVimSearchInput(op));
        app.drain_engine_runtime_events();
    }
}

/// Bracketed-paste arrival while the search prompt is active. Same shape
/// as `quick_open::handle_paste`: fold the payload in as typed chars,
/// dropping newlines so a multi-line paste doesn't break the single-line
/// prompt model. Called from `input::handle_paste` after the drop-path
/// parser has declined the payload.
pub fn handle_paste(s: &str, app: &mut App) {
    app.engine
        .dispatch(AppCommand::PasteVimSearchInput(s.to_string()));
    app.drain_engine_runtime_events();
}

/// Move to next (`reverse=false`) or previous (`reverse=true`) match. Wraps
/// around with a Top/Bottom status flash.
pub fn step(app: &mut App, reverse: bool) {
    app.engine.dispatch(AppCommand::StepVimSearch {
        reverse,
        dark: app.theme.is_dark,
        viewport: viewport(app),
    });
}

// ─── Target resolution ────────────────────────────────────────────────────────

pub(crate) fn resolve_target(app: &App) -> Option<SearchTarget> {
    app.engine.resolve_search_target(app.graph_uses_three_col())
}

// ─── Row collection (per target) ──────────────────────────────────────────────

/// Build the searchable row list for a target. Row indices returned in match
/// locations are into this vec.
fn collect_rows(app: &App, target: SearchTarget) -> Vec<String> {
    match target {
        SearchTarget::FileTree => app
            .engine
            .file_tree_entries()
            .iter()
            .map(|e| e.name.clone())
            .collect(),
        SearchTarget::GitStatus => {
            // Unified staged-then-unstaged path list — matches the order of
            // selectable rows in the panel. Search in git_status is file-only
            // (banners and section headers are skipped).
            let mut rows = Vec::with_capacity(
                app.engine.staged_files().len() + app.engine.unstaged_files().len(),
            );
            for f in app.engine.staged_files() {
                rows.push(f.path.clone());
            }
            for f in app.engine.unstaged_files() {
                rows.push(f.path.clone());
            }
            rows
        }
        SearchTarget::CommitGraph => app
            .engine
            .git_graph()
            .rows
            .iter()
            .map(|r| r.commit.subject.clone())
            .collect(),
        SearchTarget::FilePreview => match app.engine.preview_content_ref() {
            Some(p) => p
                .body
                .display_text_rows()
                .into_iter()
                .map(|row| row.into_owned())
                .collect(),
            _ => Vec::new(),
        },
        SearchTarget::Diff => match app.engine.diff_content() {
            // Read directly from the pre-built `display.unified_row_texts`
            // (Arc<Vec<DiffRowText>> built once at diff load) instead of
            // re-flattening the diff on every keystroke. Each row borrows
            // from the Arc<str> inside the DiffRowText — zero per-row alloc.
            Some(d) => row_texts_to_strings(&d.display.unified_row_texts),
            None => Vec::new(),
        },
        SearchTarget::GraphDiff => match app.engine.commit_detail().file_diff.as_ref() {
            // Graph tab 3-col diff column — same row layout as the Git tab's
            // diff panel, just sourced from `commit_detail.file_diff` instead
            // of `app.engine.diff_content()`.
            Some(d) => row_texts_to_strings(&d.display.unified_row_texts),
            None => Vec::new(),
        },
        SearchTarget::CommitDetail => {
            // Delegate to the panel so row indices line up 1:1 with what the
            // panel renders (header rows, message lines, file rows, and — in
            // 2-col fallback only — inline diff rows). The panel's
            // `searchable_rows` consults `graph_uses_three_col` and skips
            // inline diff rows in 3-col mode so match coordinates stay
            // aligned with what's actually rendered under Panel::Commit.
            crate::ui::commit_detail_panel::searchable_rows(app)
        }
    }
}

fn row_texts_to_strings(rows: &[reef_core::diff::DiffRowText]) -> Vec<String> {
    use reef_core::diff::DiffRowText;
    rows.iter()
        .map(|r| match r {
            DiffRowText::Separator => String::new(),
            DiffRowText::Header(s) => s.to_string(),
            DiffRowText::Unified(s) => s.to_string(),
            // Sbs variants don't appear in `unified_row_texts`; defensive.
            DiffRowText::Sbs { .. } => String::new(),
        })
        .collect()
}

// ─── Matching ─────────────────────────────────────────────────────────────────

/// Recompute `matches` for the current query, update `current` to the nearest
/// match relative to the cursor baseline (snapshot), and jump. Called on each
/// keystroke.
pub(crate) fn recompute_and_jump(app: &mut App) {
    let Some(target) = app.engine.search().target else {
        return;
    };
    let rows = collect_rows(app, target);
    app.engine.dispatch(AppCommand::RecomputeVimSearch {
        rows,
        dark: app.theme.is_dark,
        viewport: viewport(app),
    });
}

fn viewport(app: &App) -> SearchViewport {
    SearchViewport {
        preview_view_h: app.layout.last_preview_view_h as usize,
        diff_view_h: app.layout.last_diff_view_h as usize,
        commit_detail_view_h: app.layout.last_commit_detail_view_h as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_app::{MatchLoc, SearchState};

    #[test]
    fn center_scroll_centers_when_possible() {
        assert_eq!(reef_app::center_scroll(10, 4), 8);
        assert_eq!(reef_app::center_scroll(10, 10), 5);
    }

    #[test]
    fn center_scroll_clamps_at_zero() {
        assert_eq!(reef_app::center_scroll(2, 20), 0);
        assert_eq!(reef_app::center_scroll(0, 10), 0);
    }

    #[test]
    fn can_step_requires_committed_matches() {
        let s0 = SearchState::default();
        assert!(!s0.can_step());
        let mut s = SearchState {
            target: Some(SearchTarget::FilePreview),
            matches: vec![MatchLoc {
                row: 0,
                byte_range: 0..3,
            }],
            ..SearchState::default()
        };
        assert!(s.can_step());
        s.active = true;
        assert!(!s.can_step());
    }

    #[test]
    fn ranges_on_row_filters_correctly() {
        let mut s = SearchState {
            target: Some(SearchTarget::FilePreview),
            ..SearchState::default()
        };
        s.set_matches(vec![
            MatchLoc {
                row: 1,
                byte_range: 0..3,
            },
            MatchLoc {
                row: 1,
                byte_range: 5..8,
            },
            MatchLoc {
                row: 2,
                byte_range: 0..3,
            },
        ]);
        // `set_matches` resets `current` — assign it after so the test still
        // exercises the `Some(idx)` branch of `ranges_on_row`.
        s.current = Some(1);
        let (all, cur) = s.ranges_on_row(SearchTarget::FilePreview, 1);
        assert_eq!(all, vec![0..3, 5..8]);
        assert_eq!(cur, Some(5..8));
        let (all, _) = s.ranges_on_row(SearchTarget::FilePreview, 3);
        assert!(all.is_empty());
    }

    #[test]
    fn ranges_on_row_target_mismatch_returns_empty() {
        let mut s = SearchState {
            target: Some(SearchTarget::FilePreview),
            ..SearchState::default()
        };
        s.set_matches(vec![MatchLoc {
            row: 0,
            byte_range: 0..1,
        }]);
        let (all, _) = s.ranges_on_row(SearchTarget::Diff, 0);
        assert!(all.is_empty());
    }
}
