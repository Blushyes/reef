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
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    /// `unified_display_rows(&file_diff.diff)`——和 Git tab 的
    /// `SearchTarget::Diff` 同款 —— 这样渲染层的 `ranges_on_row` 直接拿
    /// 到匹配区间,跟 `diff_panel::render_diff` 的行号系统无缝对接。
    GraphDiff,
}

#[derive(Debug, Clone)]
pub struct MatchLoc {
    pub row: usize,
    pub byte_range: Range<usize>,
}

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
    pub matches: Vec<MatchLoc>,
    pub current: Option<usize>,
    pub snapshot: Option<Snapshot>,
    pub wrap_msg: Option<WrapMsg>,
}

impl SearchState {
    /// Whether `n` / `N` are currently meaningful. False if the user is
    /// actively typing, has no matches, or has no committed search.
    pub fn can_step(&self) -> bool {
        !self.active && !self.matches.is_empty() && self.target.is_some()
    }

    /// Ranges of matches falling on a given row, plus the current-match
    /// range if it lives on this row. Consumed by the overlay renderer.
    pub fn ranges_on_row(
        &self,
        target: SearchTarget,
        row: usize,
    ) -> (Vec<Range<usize>>, Option<Range<usize>>) {
        if self.target != Some(target) || self.matches.is_empty() {
            return (Vec::new(), None);
        }
        let mut all = Vec::new();
        let mut cur = None;
        for (i, m) in self.matches.iter().enumerate() {
            if m.row != row {
                continue;
            }
            if Some(i) == self.current {
                cur = Some(m.byte_range.clone());
            }
            all.push(m.byte_range.clone());
        }
        (all, cur)
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
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    // Helper: called after any op that mutates `query`. Pure cursor moves
    // don't need this — they leave matches / wrap_msg untouched.
    fn after_edit(app: &mut App) {
        app.search.wrap_msg = None;
        recompute_and_jump(app, /*from_step=*/ false);
    }

    match key.code {
        // ── Exit ────────────────────────────────────────────────
        KeyCode::Esc => exit_cancel(app),
        KeyCode::Char('c') if ctrl => exit_cancel(app),
        KeyCode::Enter => exit_confirm(app),

        // ── Deletion ─────────────────────────────────────────────
        // Word-delete variants come before plain Backspace so modifier
        // combos win the match.
        KeyCode::Backspace if alt || ctrl => {
            input_edit::delete_word_backward(&mut app.search.query, &mut app.search.cursor);
            after_edit(app);
        }
        KeyCode::Char('w') if ctrl => {
            input_edit::delete_word_backward(&mut app.search.query, &mut app.search.cursor);
            after_edit(app);
        }
        KeyCode::Char('u') if ctrl => {
            input_edit::clear(&mut app.search.query, &mut app.search.cursor);
            after_edit(app);
        }
        KeyCode::Backspace => {
            input_edit::backspace(&mut app.search.query, &mut app.search.cursor);
            after_edit(app);
        }

        // ── Cursor movement ─────────────────────────────────────
        // Alt/Ctrl + arrow = jump by word; must precede bare-arrow arms.
        KeyCode::Left if alt || ctrl => {
            input_edit::move_cursor_word_backward(&app.search.query, &mut app.search.cursor);
        }
        KeyCode::Right if alt || ctrl => {
            input_edit::move_cursor_word_forward(&app.search.query, &mut app.search.cursor);
        }
        KeyCode::Left => {
            input_edit::move_cursor(&app.search.query, &mut app.search.cursor, -1);
        }
        KeyCode::Right => {
            input_edit::move_cursor(&app.search.query, &mut app.search.cursor, 1);
        }
        KeyCode::Home => {
            app.search.cursor = 0;
        }
        KeyCode::End => {
            app.search.cursor = app.search.query.len();
        }

        // ── Forward-delete ──────────────────────────────────────
        KeyCode::Delete if alt || ctrl => {
            input_edit::delete_word_forward(&mut app.search.query, &mut app.search.cursor);
            after_edit(app);
        }
        KeyCode::Delete => {
            input_edit::delete_char_forward(&mut app.search.query, &mut app.search.cursor);
            after_edit(app);
        }

        // ── Readline aliases ────────────────────────────────────
        // Fallback for terminals that don't forward Alt/Ctrl+Arrow cleanly.
        KeyCode::Char('b') if alt => {
            input_edit::move_cursor_word_backward(&app.search.query, &mut app.search.cursor);
        }
        KeyCode::Char('f') if alt => {
            input_edit::move_cursor_word_forward(&app.search.query, &mut app.search.cursor);
        }
        KeyCode::Char('d') if alt => {
            input_edit::delete_word_forward(&mut app.search.query, &mut app.search.cursor);
            after_edit(app);
        }
        KeyCode::Char('a') if ctrl => {
            app.search.cursor = 0;
        }
        KeyCode::Char('e') if ctrl => {
            app.search.cursor = app.search.query.len();
        }

        // ── Insertion ───────────────────────────────────────────
        KeyCode::Char(c) if !ctrl => {
            input_edit::insert_char(&mut app.search.query, &mut app.search.cursor, c);
            after_edit(app);
        }
        _ => {}
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
fn collect_rows(app: &App, target: SearchTarget) -> Vec<String> {
    match target {
        SearchTarget::FileTree => app
            .file_tree
            .entries
            .iter()
            .map(|e| e.name.clone())
            .collect(),
        SearchTarget::GitStatus => {
            // Unified staged-then-unstaged path list — matches the order of
            // selectable rows in the panel. Search in git_status is file-only
            // (banners and section headers are skipped).
            let mut rows = Vec::with_capacity(app.staged_files.len() + app.unstaged_files.len());
            for f in &app.staged_files {
                rows.push(f.path.clone());
            }
            for f in &app.unstaged_files {
                rows.push(f.path.clone());
            }
            rows
        }
        SearchTarget::CommitGraph => app
            .git_graph
            .rows
            .iter()
            .map(|r| r.commit.subject.clone())
            .collect(),
        SearchTarget::FilePreview => match &app.preview_content {
            Some(p) => match &p.body {
                crate::file_tree::PreviewBody::Text { lines, .. } => lines.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        },
        SearchTarget::Diff => match &app.diff_content {
            // Row layout matches the Unified diff panel's `all_lines`:
            // separator rows between hunks, then hunk header + per-line rows.
            // `diff_scroll` is an offset into that vec, so our row index is
            // directly usable as a scroll target.
            Some(d) => unified_display_rows(&d.diff),
            None => Vec::new(),
        },
        SearchTarget::GraphDiff => match &app.commit_detail.file_diff {
            // Graph tab 3-col diff column — same row layout as the Git tab's
            // diff panel, just sourced from `commit_detail.file_diff` instead
            // of `app.diff_content`.
            Some(d) => unified_display_rows(&d.diff),
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

/// Flatten a diff into searchable rows that line up with the Unified diff
/// panel's `all_lines` layout (separator between hunks → header → body).
pub(crate) fn unified_display_rows(diff: &crate::git::DiffContent) -> Vec<String> {
    let mut rows = Vec::new();
    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            rows.push(String::new()); // Separator row — never matches.
        }
        rows.push(hunk.header.clone());
        for line in &hunk.lines {
            rows.push(line.content.clone());
        }
    }
    rows
}

// ─── Matching ─────────────────────────────────────────────────────────────────

pub(crate) fn smart_case(query: &str) -> bool {
    query.chars().all(|c| !c.is_uppercase())
}

/// All byte-range matches of `needle` in `haystack`, non-overlapping, with
/// smart-case folding. Needle must be non-empty.
pub(crate) fn find_all(haystack: &str, needle: &str, case_insensitive: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    if case_insensitive {
        let h = haystack.to_ascii_lowercase();
        let n = needle.to_ascii_lowercase();
        let mut start = 0usize;
        while let Some(pos) = h[start..].find(&n) {
            let abs = start + pos;
            results.push(abs..abs + needle.len());
            start = abs + needle.len().max(1);
        }
    } else {
        let mut start = 0usize;
        while let Some(pos) = haystack[start..].find(needle) {
            let abs = start + pos;
            results.push(abs..abs + needle.len());
            start = abs + needle.len().max(1);
        }
    }
    results
}

/// Recompute `matches` for the current query, update `current` to the nearest
/// match relative to the cursor baseline (snapshot), and jump. Called on each
/// keystroke.
fn recompute_and_jump(app: &mut App, from_step: bool) {
    let Some(target) = app.search.target else {
        return;
    };
    let query = app.search.query.clone();

    if query.is_empty() {
        app.search.matches.clear();
        app.search.current = None;
        // Re-apply the snapshot so the view shows the starting position while
        // the user is still editing.
        if let Some(snap) = app.search.snapshot.clone() {
            restore_snapshot(app, &snap);
        }
        return;
    }

    let rows = collect_rows(app, target);
    let case_insensitive = smart_case(&query);
    let mut matches = Vec::new();
    for (idx, text) in rows.iter().enumerate() {
        for r in find_all(text, &query, case_insensitive) {
            matches.push(MatchLoc {
                row: idx,
                byte_range: r,
            });
        }
    }

    app.search.matches = matches;
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
    fn find_all_basic() {
        let r = find_all("foobar foo baz", "foo", true);
        assert_eq!(r, vec![0..3, 7..10]);
    }

    #[test]
    fn find_all_smart_case_insensitive() {
        let r = find_all("Foo FOO foo", "foo", true);
        assert_eq!(r, vec![0..3, 4..7, 8..11]);
    }

    #[test]
    fn find_all_smart_case_sensitive_when_upper() {
        // Capital letter in query → case-sensitive.
        let r = find_all("Foo FOO foo", "Foo", false);
        assert_eq!(r, vec![0..3]);
    }

    #[test]
    fn find_all_empty_needle() {
        assert!(find_all("abc", "", true).is_empty());
    }

    #[test]
    fn find_all_no_match() {
        assert!(find_all("abc", "xyz", true).is_empty());
    }

    #[test]
    fn find_all_overlapping_advances_by_needle_len() {
        // "aaaa" searching "aa" → [0..2, 2..4], non-overlapping.
        let r = find_all("aaaa", "aa", true);
        assert_eq!(r, vec![0..2, 2..4]);
    }

    #[test]
    fn smart_case_all_lowercase_is_insensitive() {
        assert!(smart_case("foo"));
        assert!(!smart_case("Foo"));
        assert!(!smart_case("FOO"));
    }

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
        let s = SearchState {
            target: Some(SearchTarget::FilePreview),
            matches: vec![
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
            ],
            current: Some(1),
            ..SearchState::default()
        };
        let (all, cur) = s.ranges_on_row(SearchTarget::FilePreview, 1);
        assert_eq!(all, vec![0..3, 5..8]);
        assert_eq!(cur, Some(5..8));
        let (all, _) = s.ranges_on_row(SearchTarget::FilePreview, 3);
        assert!(all.is_empty());
    }

    #[test]
    fn ranges_on_row_target_mismatch_returns_empty() {
        let s = SearchState {
            target: Some(SearchTarget::FilePreview),
            matches: vec![MatchLoc {
                row: 0,
                byte_range: 0..1,
            }],
            ..SearchState::default()
        };
        let (all, _) = s.ranges_on_row(SearchTarget::Diff, 0);
        assert!(all.is_empty());
    }
}
