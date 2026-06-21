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

use crate::app::{App, Panel, SelectedFile, Tab};
use crate::input_edit;
use crate::keymap::{Command, InputScope, Keymap};
use crossterm::event::KeyEvent;
use reef_core::search::{SearchMatch, build_row_index, ranges_on_row};
use std::collections::HashMap;
use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTarget {
    FileTree,
    GitStatus,
    CommitGraph,
    FilePreview,
    Diff,
    CommitDetail,
    /// Graph tab 三列布局下右侧 diff 栏的 `/` 搜索目标。行索引对齐
    /// `commit_detail.file_diff.display.unified_row_texts`——和 Git tab 的
    /// `SearchTarget::Diff` 同款(共享同一份 worker-built `DiffDisplay` 缓存)
    /// —— 这样渲染层的 `ranges_on_row` 直接拿到匹配区间,跟
    /// `diff_panel::render_diff` 的行号系统无缝对接。
    GraphDiff,
}

pub type MatchLoc = SearchMatch;

/// Pre-search scroll / selection state, restored on Esc.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub preview_scroll: usize,
    pub preview_h_scroll: usize,
    pub diff_scroll: usize,
    pub diff_h_scroll: usize,
    pub commit_detail_scroll: usize,
    /// Pre-search scroll for the Graph-tab 3-col diff column (restored on Esc).
    pub graph_diff_scroll: usize,
    pub file_tree_selected: usize,
    pub tree_scroll: usize,
    pub git_status_scroll: usize,
    pub git_status_selected_file: Option<SelectedFile>,
    pub git_graph_selected_idx: usize,
    pub git_graph_scroll: usize,
}

/// Non-fatal status shown in the search prompt line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapMsg {
    Top,
    Bottom,
    NoMatch,
}

#[derive(Debug, Default)]
pub struct SearchState {
    /// True while the prompt is visible and eats keystrokes.
    pub active: bool,
    pub backwards: bool,
    pub query: String,
    /// Byte offset into `query`. Always on a char boundary.
    pub cursor: usize,
    pub target: Option<SearchTarget>,
    /// Match locations. **Invariant**: must agree with `row_index` —
    /// mutate ONLY through `set_matches` / `clear_matches`. Direct
    /// `matches.push(...)` or `matches = vec![...]` leaves `row_index`
    /// stale and `ranges_on_row` will silently return empty results
    /// for the desynced rows.
    pub matches: Vec<MatchLoc>,
    pub current: Option<usize>,
    pub snapshot: Option<Snapshot>,
    pub wrap_msg: Option<WrapMsg>,
    /// `row → indices into `matches`` index. Rebuilt whenever `matches`
    /// changes via [`set_matches`] / [`clear_matches`]. Lets per-row
    /// renderers look up overlays in O(1+k) (k = matches on that row)
    /// instead of scanning all matches every frame — at 10k+ hits the
    /// linear scan was the dominant render cost for global-search
    /// previews.
    ///
    /// **Invariant:** kept in sync via `set_matches` / `clear_matches`.
    /// External code (tests etc.) using the struct-literal form should
    /// pass `Default::default()` here; mutating after construction is
    /// not supported.
    pub row_index: HashMap<usize, Vec<usize>>,
}

impl SearchState {
    /// Whether `n` / `N` are currently meaningful. False if the user is
    /// actively typing, has no matches, or has no committed search.
    pub fn can_step(&self) -> bool {
        !self.active && !self.matches.is_empty() && self.target.is_some()
    }

    /// Replace `matches` and rebuild the per-row lookup. Also resets
    /// `current` to `None` — callers picking a fresh "current" must
    /// assign it right after this call. (Pre-fix, leaving the stale
    /// `current` index hanging past the new `matches.len()` made
    /// `step` / `jump_to_current` silently no-op; the asymmetry with
    /// `clear_matches` (which always wiped `current`) was a footgun.)
    ///
    /// Panic-safe: `self.matches` is assigned BEFORE we build the
    /// `row_index` over it. If a HashMap rehash inside the loop panics
    /// (OOM), we leave `row_index` partial — but the indices it holds
    /// still point into `self.matches`, so `ranges_on_row` returns a
    /// truncated but consistent view rather than reading past
    /// stale-vec bounds.
    pub fn set_matches(&mut self, matches: Vec<MatchLoc>) {
        self.matches = matches;
        self.current = None;
        self.row_index = build_row_index(&self.matches);
    }

    /// Empty `matches` + clear the index. Cheaper than building a fresh
    /// `SearchState` when only matches need to drop.
    pub fn clear_matches(&mut self) {
        self.matches.clear();
        self.row_index.clear();
        self.current = None;
    }

    /// Ranges of matches falling on a given row, plus the current-match
    /// range if it lives on this row. Consumed by the overlay renderer.
    /// O(1+k) where k = match count on `row` — backed by `row_index`.
    pub fn ranges_on_row(
        &self,
        target: SearchTarget,
        row: usize,
    ) -> (Vec<Range<usize>>, Option<Range<usize>>) {
        if self.target != Some(target) || self.matches.is_empty() {
            return (Vec::new(), None);
        }
        ranges_on_row(&self.matches, &self.row_index, self.current, row)
    }

    /// Reset search entirely (used on tab / panel switch).
    pub fn clear(&mut self) {
        *self = SearchState::default();
    }
}

// ─── Entry points ─────────────────────────────────────────────────────────────

/// Start a search session. Picks the target from the focused panel, snapshots
/// pre-search scroll / selection so Esc can restore, and enters input mode.
pub fn begin(app: &mut App, backwards: bool) {
    let Some(target) = resolve_target(app) else {
        return;
    };
    let snap = take_snapshot(app);
    app.search = SearchState {
        active: true,
        backwards,
        query: String::new(),
        cursor: 0,
        target: Some(target),
        matches: Vec::new(),
        current: None,
        snapshot: Some(snap),
        wrap_msg: None,
        row_index: HashMap::new(),
    };
}

/// Accept current match position, exit input mode. Matches stay populated so
/// `n` / `N` continue to work.
pub fn exit_confirm(app: &mut App) {
    app.search.active = false;
    app.search.wrap_msg = None;
    // If the user Enter'd with no matches, just drop the session entirely so
    // the status bar goes back to normal.
    if app.search.query.is_empty() || app.search.matches.is_empty() {
        app.search.clear();
    }
}

/// Cancel: restore the pre-search scroll / selection and clear the session.
pub fn exit_cancel(app: &mut App) {
    if let Some(snap) = app.search.snapshot.clone() {
        restore_snapshot(app, &snap);
    }
    app.search.clear();
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
    let outcome = input_edit::dispatch_key(&key, &mut app.search.query, &mut app.search.cursor);
    if outcome == input_edit::Outcome::Edited {
        app.search.wrap_msg = None;
        recompute_and_jump(app, /*from_step=*/ false);
    }
}

/// Bracketed-paste arrival while the search prompt is active. Same shape
/// as `quick_open::handle_paste`: fold the payload in as typed chars,
/// dropping newlines so a multi-line paste doesn't break the single-line
/// prompt model. Called from `input::handle_paste` after the drop-path
/// parser has declined the payload.
pub fn handle_paste(s: &str, app: &mut App) {
    if input_edit::paste_single_line(s, &mut app.search.query, &mut app.search.cursor) {
        app.search.wrap_msg = None;
        recompute_and_jump(app, /*from_step=*/ false);
    }
}

/// Move to next (`reverse=false`) or previous (`reverse=true`) match. Wraps
/// around with a Top/Bottom status flash.
pub fn step(app: &mut App, reverse: bool) {
    if app.search.matches.is_empty() {
        return;
    }
    let n = app.search.matches.len();
    let go_back = app.search.backwards ^ reverse; // `n` obeys direction, `N` flips
    let cur = app.search.current.unwrap_or(0);
    let (next, wrapped) = if go_back {
        if cur == 0 {
            (n - 1, true)
        } else {
            (cur - 1, false)
        }
    } else if cur + 1 >= n {
        (0, true)
    } else {
        (cur + 1, false)
    };
    app.search.current = Some(next);
    app.search.wrap_msg = if wrapped {
        Some(if go_back {
            WrapMsg::Top
        } else {
            WrapMsg::Bottom
        })
    } else {
        None
    };
    jump_to_current(app);
}

// ─── Target resolution ────────────────────────────────────────────────────────

pub(crate) fn resolve_target(app: &App) -> Option<SearchTarget> {
    match (app.active_tab, app.active_panel) {
        (Tab::Files, Panel::Files) => Some(SearchTarget::FileTree),
        (Tab::Files, Panel::Diff) => Some(SearchTarget::FilePreview),
        (Tab::Git, Panel::Files) => Some(SearchTarget::GitStatus),
        (Tab::Git, Panel::Diff) => Some(SearchTarget::Diff),
        (Tab::Graph, Panel::Files) => Some(SearchTarget::CommitGraph),
        // Graph middle column (3-col only) = commit metadata + file tree.
        // Same target as the 2-col fallback; `collect_rows` decides whether
        // inline diff rows are included based on `app.graph_uses_three_col`.
        (Tab::Graph, Panel::Commit) => Some(SearchTarget::CommitDetail),
        (Tab::Graph, Panel::Diff) => {
            // In 3-col mode, the diff owns its own column with its own
            // row indexing (matches `unified_display_rows(&file_diff.diff)`).
            // In 2-col fallback the diff is inline so we fall through to
            // CommitDetail, which already covers it.
            if app.graph_uses_three_col() {
                Some(SearchTarget::GraphDiff)
            } else {
                Some(SearchTarget::CommitDetail)
            }
        }
        // `Panel::Commit` outside Graph tab should never happen (only Graph
        // sets it, and `normalize_active_panel` demotes it otherwise). Treat
        // it defensively as "no search" rather than panicking.
        (_, Panel::Commit) => None,
        // Left panel of the Search tab is already a search input — `/` there
        // would be ambiguous, so it's a no-op for now. Right panel mirrors
        // Files-tab preview.
        (Tab::Search, Panel::Files) => None,
        (Tab::Search, Panel::Diff) => Some(SearchTarget::FilePreview),
    }
}

// ─── Row collection (per target) ──────────────────────────────────────────────

/// Build the searchable row list for a target. Row indices returned in match
/// locations are into this vec.
/// Returns a row vec where each entry is borrowed from `app` when possible
/// (`Cow::Borrowed(&str)`) and only allocates for the synthesized empty
/// separator rows in diffs. Saves the per-keystroke `to_string()` storm
/// over Arc<str>-backed diff lines.
fn collect_rows(app: &App, target: SearchTarget) -> Vec<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    match target {
        SearchTarget::FileTree => app
            .file_tree
            .entries
            .iter()
            .map(|e| Cow::Borrowed(e.name.as_str()))
            .collect(),
        SearchTarget::GitStatus => {
            // Unified staged-then-unstaged path list — matches the order of
            // selectable rows in the panel. Search in git_status is file-only
            // (banners and section headers are skipped).
            let mut rows: Vec<Cow<'_, str>> =
                Vec::with_capacity(app.staged_files.len() + app.unstaged_files.len());
            for f in &app.staged_files {
                rows.push(Cow::Borrowed(f.path.as_str()));
            }
            for f in &app.unstaged_files {
                rows.push(Cow::Borrowed(f.path.as_str()));
            }
            rows
        }
        SearchTarget::CommitGraph => app
            .git_graph
            .rows
            .iter()
            .map(|r| Cow::Borrowed(r.commit.subject.as_str()))
            .collect(),
        SearchTarget::FilePreview => match &app.preview_content {
            Some(p) => p.body.display_text_rows(),
            _ => Vec::new(),
        },
        SearchTarget::Diff => match &app.diff_content {
            // Read directly from the pre-built `display.unified_row_texts`
            // (Arc<Vec<DiffRowText>> built once at diff load) instead of
            // re-flattening the diff on every keystroke. Each row borrows
            // from the Arc<str> inside the DiffRowText — zero per-row alloc.
            Some(d) => row_texts_to_cows(&d.display.unified_row_texts),
            None => Vec::new(),
        },
        SearchTarget::GraphDiff => match &app.commit_detail.file_diff {
            // Graph tab 3-col diff column — same row layout as the Git tab's
            // diff panel, just sourced from `commit_detail.file_diff` instead
            // of `app.diff_content`.
            Some(d) => row_texts_to_cows(&d.display.unified_row_texts),
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
                .into_iter()
                .map(Cow::Owned)
                .collect()
        }
    }
}

/// Project a slice of `DiffRowText` into the `Cow<&str>` shape `collect_rows`
/// returns. Each row borrows directly from the underlying `Arc<str>` —
/// zero per-row allocation, mirroring the find_widget SBS path.
fn row_texts_to_cows(rows: &[reef_core::diff::DiffRowText]) -> Vec<std::borrow::Cow<'_, str>> {
    use reef_core::diff::DiffRowText;
    use std::borrow::Cow;
    rows.iter()
        .map(|r| match r {
            DiffRowText::Separator => Cow::Borrowed(""),
            DiffRowText::Header(s) => Cow::Borrowed(s.as_ref()),
            DiffRowText::Unified(s) => Cow::Borrowed(s.as_ref()),
            // Sbs variants don't appear in `unified_row_texts`; defensive.
            DiffRowText::Sbs { .. } => Cow::Borrowed(""),
        })
        .collect()
}

// ─── Matching ─────────────────────────────────────────────────────────────────

/// Recompute `matches` for the current query, update `current` to the nearest
/// match relative to the cursor baseline (snapshot), and jump. Called on each
/// keystroke.
fn recompute_and_jump(app: &mut App, from_step: bool) {
    let Some(target) = app.search.target else {
        return;
    };
    let query = app.search.query.clone();

    if query.is_empty() {
        app.search.clear_matches();
        // Re-apply the snapshot so the view shows the starting position while
        // the user is still editing.
        if let Some(snap) = app.search.snapshot.clone() {
            restore_snapshot(app, &snap);
        }
        return;
    }

    let rows = collect_rows(app, target);
    let case_insensitive = reef_core::search::smart_case_insensitive(&query);
    let mut matches = Vec::new();
    for (idx, text) in rows.iter().enumerate() {
        for r in reef_core::search::find_literal_all(text, &query, case_insensitive) {
            matches.push(MatchLoc {
                row: idx,
                byte_range: r,
            });
        }
    }

    app.search.set_matches(matches);
    app.search.current = pick_current(app, target, from_step);
    jump_to_current(app);
}

/// Choose initial match index based on the pre-search cursor and direction.
fn pick_current(app: &App, target: SearchTarget, _from_step: bool) -> Option<usize> {
    if app.search.matches.is_empty() {
        return None;
    }
    let snap = app.search.snapshot.clone().unwrap_or_default();
    let baseline_row = baseline_row(target, &snap);
    if app.search.backwards {
        // Largest row <= baseline; if none, wrap to last.
        let mut chosen: Option<usize> = None;
        for (i, m) in app.search.matches.iter().enumerate() {
            if m.row <= baseline_row {
                chosen = Some(i);
            } else {
                break;
            }
        }
        chosen.or_else(|| Some(app.search.matches.len() - 1))
    } else {
        // Smallest row >= baseline; if none, wrap to first.
        app.search
            .matches
            .iter()
            .position(|m| m.row >= baseline_row)
            .or(Some(0))
    }
}

fn baseline_row(target: SearchTarget, snap: &Snapshot) -> usize {
    match target {
        SearchTarget::FileTree => snap.file_tree_selected,
        SearchTarget::GitStatus => snap.git_status_scroll,
        SearchTarget::CommitGraph => snap.git_graph_selected_idx,
        SearchTarget::FilePreview => snap.preview_scroll,
        SearchTarget::Diff => snap.diff_scroll,
        SearchTarget::GraphDiff => snap.graph_diff_scroll,
        SearchTarget::CommitDetail => snap.commit_detail_scroll,
    }
}

// ─── Snapshot / restore ───────────────────────────────────────────────────────

pub(crate) fn take_snapshot(app: &App) -> Snapshot {
    Snapshot {
        preview_scroll: app.preview_scroll,
        preview_h_scroll: app.preview_h_scroll,
        diff_scroll: app.diff_scroll,
        diff_h_scroll: app.diff_h_scroll,
        commit_detail_scroll: app.commit_detail.scroll,
        graph_diff_scroll: app.commit_detail.file_diff_scroll,
        file_tree_selected: app.file_tree.selected,
        tree_scroll: app.tree_scroll,
        git_status_scroll: app.git_status.scroll,
        git_status_selected_file: app.selected_file.clone(),
        git_graph_selected_idx: app.git_graph.selected_idx,
        git_graph_scroll: app.git_graph.scroll,
    }
}

pub(crate) fn restore_snapshot(app: &mut App, snap: &Snapshot) {
    app.preview_scroll = snap.preview_scroll;
    app.preview_h_scroll = snap.preview_h_scroll;
    app.diff_scroll = snap.diff_scroll;
    app.diff_h_scroll = snap.diff_h_scroll;
    app.commit_detail.scroll = snap.commit_detail_scroll;
    app.commit_detail.file_diff_scroll = snap.graph_diff_scroll;
    app.file_tree.selected = snap.file_tree_selected;
    app.tree_scroll = snap.tree_scroll;
    app.git_status.scroll = snap.git_status_scroll;
    if app.selected_file != snap.git_status_selected_file {
        app.selected_file = snap.git_status_selected_file.clone();
        app.diff_scroll = snap.diff_scroll;
        app.diff_h_scroll = snap.diff_h_scroll;
        app.load_diff();
    }
    if app.git_graph.selected_idx != snap.git_graph_selected_idx {
        app.git_graph.selected_idx = snap.git_graph_selected_idx;
        app.git_graph.selected_commit = app
            .git_graph
            .rows
            .get(snap.git_graph_selected_idx)
            .map(|r| r.commit.oid.clone());
        // Snapshot restore doesn't carry the anchor, so any in-flight range
        // would be stale relative to the restored cursor. Drop it cleanly.
        app.git_graph.selection_anchor = None;
        app.commit_detail.range_detail = None;
        app.commit_detail.scroll = snap.commit_detail_scroll;
        app.load_commit_detail();
    }
    app.git_graph.scroll = snap.git_graph_scroll;
}

// ─── Jumping ──────────────────────────────────────────────────────────────────

fn jump_to_current(app: &mut App) {
    let Some(cur) = app.search.current else {
        return;
    };
    let Some(m) = app.search.matches.get(cur).cloned() else {
        return;
    };
    let Some(target) = app.search.target else {
        return;
    };
    match target {
        SearchTarget::FileTree => {
            if m.row < app.file_tree.entries.len() {
                app.file_tree.selected = m.row;
                app.load_preview();
            }
        }
        SearchTarget::GitStatus => {
            let staged_len = app.staged_files.len();
            if m.row < staged_len {
                let f = app.staged_files[m.row].clone();
                app.select_file(&f.path, true);
            } else {
                let idx = m.row - staged_len;
                if let Some(f) = app.unstaged_files.get(idx).cloned() {
                    app.select_file(&f.path, false);
                }
            }
        }
        SearchTarget::CommitGraph => {
            if m.row < app.git_graph.rows.len() {
                app.git_graph.selected_idx = m.row;
                app.git_graph.selected_commit =
                    app.git_graph.rows.get(m.row).map(|r| r.commit.oid.clone());
                // Search jumps always collapse any active range back to
                // single-select: the anchor points at the commit the user was
                // looking at before they searched, which is rarely relevant
                // once they've jumped to a match.
                app.git_graph.selection_anchor = None;
                app.commit_detail.range_detail = None;
                app.commit_detail.scroll = 0;
                app.load_commit_detail();
            }
        }
        SearchTarget::FilePreview => {
            let view_h = app.last_preview_view_h as usize;
            app.preview_scroll = center_scroll(m.row, view_h);
        }
        SearchTarget::Diff => {
            let view_h = app.last_diff_view_h as usize;
            app.diff_scroll = center_scroll(m.row, view_h);
        }
        SearchTarget::GraphDiff => {
            // The 3-col diff column writes its own view height to the shared
            // `last_diff_view_h` slot at render time (same field the Git tab
            // uses — only one of the two panels is visible at once).
            let view_h = app.last_diff_view_h as usize;
            app.commit_detail.file_diff_scroll = center_scroll(m.row, view_h);
        }
        SearchTarget::CommitDetail => {
            let view_h = app.last_commit_detail_view_h as usize;
            app.commit_detail.scroll = center_scroll(m.row, view_h);
        }
    }
}

pub(crate) fn center_scroll(row: usize, view_h: usize) -> usize {
    if view_h <= 1 {
        return row;
    }
    row.saturating_sub(view_h / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn center_scroll_centers_when_possible() {
        assert_eq!(center_scroll(10, 4), 8);
        assert_eq!(center_scroll(10, 10), 5);
    }

    #[test]
    fn center_scroll_clamps_at_zero() {
        assert_eq!(center_scroll(2, 20), 0);
        assert_eq!(center_scroll(0, 10), 0);
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
