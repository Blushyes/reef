//! VSCode-style "Find" widget for the file-preview and diff panels.
//!
//! Independent of the vim-style `/` (`crate::search`). Opened via `Space+F`,
//! anchored to the upper-right of the active content panel, supports
//! side-by-side diff (targets one half based on selection side), and
//! exposes Match Case / Whole Word / Regex toggles.
//!
//! Mutual exclusion: opening the widget force-clears `app.search` so there's
//! exactly one source of match highlights at any time. Opening `/` is
//! expected to call `find_widget::close` to do the reverse.

use crate::app::App;
use crate::input;
use crate::input_edit;
use crate::search::{
    self, MatchLoc, Snapshot, center_scroll, find_all, restore_snapshot, take_snapshot,
};
use crate::ui::selection::{DiffRowText, DiffSide};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::ops::Range;
use std::time::Instant;

/// VSCode-style find widget state. Mutually exclusive with `app.search`:
/// opening either side clears the other so a frame never paints two
/// match colors over the same rows.
#[derive(Debug, Default)]
pub struct FindWidgetState {
    pub active: bool,

    pub query: String,
    /// Byte offset into `query`; always on a char boundary.
    pub cursor: usize,

    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,

    pub target: Option<FindTarget>,
    pub matches: Vec<MatchLoc>,
    pub current: Option<usize>,
    pub snapshot: Option<Snapshot>,

    /// Cached widget rect for mouse hit-testing.
    pub last_widget_rect: Option<ratatui::layout::Rect>,
    /// Mirror leader slot so a second `Space+F` inside the widget toggles
    /// it off (when the query is empty), same idiom as `quick_open` /
    /// `global_search`.
    pub space_leader_at: Option<Instant>,
    /// `Some(msg)` when the regex toggle is on and the current query
    /// fails to compile. UI surfaces this in place of the counter.
    pub regex_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindTarget {
    FilePreview,
    DiffUnified,
    DiffSbsLeft,
    DiffSbsRight,
    GraphDiffUnified,
    GraphDiffSbsLeft,
    GraphDiffSbsRight,
}

impl FindWidgetState {
    /// Ranges of matches on `row` for `target` plus the current-match
    /// range if it lives on this row. Render-time helper, mirrors
    /// `SearchState::ranges_on_row`.
    pub fn ranges_on_row(
        &self,
        target: FindTarget,
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

    pub fn clear(&mut self) {
        *self = FindWidgetState::default();
    }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/// Open the widget on the active content panel, seeding the query from
/// the current text selection (file-preview drag-select or diff
/// drag-select). VSCode `Cmd+F` semantics: prompt stays active, cursor
/// at query end, matches highlighted, scroll jumped to first hit.
///
/// No-op when the active panel has no in-panel find target (e.g. focus
/// is on the Search tab's left list).
pub fn begin_with_selection(app: &mut App) {
    let Some(target) = resolve_target(app) else {
        return;
    };
    // Force-close legacy `/` find first so we don't overlap highlights.
    app.search.clear();
    let snapshot = take_snapshot(app);
    // Preserve the three toggles across re-opens so the user doesn't
    // lose Aa/ab/.* selections each time they pop the widget.
    let preserve_match_case = app.find_widget.match_case;
    let preserve_whole_word = app.find_widget.whole_word;
    let preserve_regex = app.find_widget.regex;
    let query = crate::global_search::current_text_selection(app).unwrap_or_default();
    let cursor = query.len();
    app.find_widget = FindWidgetState {
        active: true,
        query,
        cursor,
        match_case: preserve_match_case,
        whole_word: preserve_whole_word,
        regex: preserve_regex,
        target: Some(target),
        snapshot: Some(snapshot),
        ..FindWidgetState::default()
    };
    recompute(app);
}

/// Close the widget, restoring pre-find scroll. Idempotent — safe to call
/// when widget is already inactive.
pub fn close(app: &mut App) {
    if let Some(snap) = app.find_widget.snapshot.clone() {
        restore_snapshot(app, &snap);
    }
    app.find_widget.clear();
}

/// Step to next (`reverse=false`) or previous (`reverse=true`) match.
/// `Space+G` / `Space+Shift+G` route here. Works regardless of whether
/// the widget is currently `active` — as long as `matches` is non-empty
/// and the target is set, navigation is meaningful.
pub fn step(app: &mut App, reverse: bool) {
    let state = &mut app.find_widget;
    if state.matches.is_empty() {
        return;
    }
    let n = state.matches.len();
    let cur = state.current.unwrap_or(0);
    let next = if reverse {
        if cur == 0 { n - 1 } else { cur - 1 }
    } else if cur + 1 >= n {
        0
    } else {
        cur + 1
    };
    state.current = Some(next);
    jump_to_current(app);
}

/// Owned-input dispatch while `app.find_widget.active`. Mirrors the
/// pattern in `search::handle_key_in_search_mode` — every key landing
/// here either edits one of the inputs, navigates matches, toggles an
/// option, or closes the widget.
pub fn handle_key(key: KeyEvent, app: &mut App) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // In-widget leader chord: a second `Space+F` toggles the widget off
    // when the query is empty (palette-style isolation).
    let allow_arm = app.find_widget.query.is_empty();
    match input::leader_decision(
        &key,
        allow_arm,
        app.find_widget.space_leader_at,
        Instant::now(),
        input::LEADER_TIMEOUT,
    ) {
        input::LeaderVerdict::Arm => {
            app.find_widget.space_leader_at = Some(Instant::now());
            return;
        }
        input::LeaderVerdict::Fire => {
            app.find_widget.space_leader_at = None;
            // Plain Space+F closes the widget; other chord targets are
            // swallowed so the palette behaves like other overlays.
            if key.code == KeyCode::Char('f') && !ctrl && !alt {
                close(app);
            }
            return;
        }
        input::LeaderVerdict::Consume => {
            app.find_widget.space_leader_at = None;
        }
        input::LeaderVerdict::None => {}
    }

    match key.code {
        KeyCode::Esc => {
            close(app);
            return;
        }
        KeyCode::Char('c') if ctrl => {
            close(app);
            return;
        }
        KeyCode::Enter => {
            // Enter = next match (Shift+Enter = previous), VSCode-style.
            if !app.find_widget.matches.is_empty() {
                step(app, /*reverse=*/ shift);
            }
            return;
        }
        KeyCode::Down => {
            step(app, /*reverse=*/ false);
            return;
        }
        KeyCode::Up => {
            step(app, /*reverse=*/ true);
            return;
        }
        // Alt+C / Alt+W / Alt+R are the three toggles. Must match
        // before `input_edit::dispatch_key`, which binds Alt+B / Alt+F /
        // Alt+D and would otherwise eat them as word-motion / delete.
        KeyCode::Char(c) if alt && !ctrl && matches!(c, 'c' | 'C') => {
            app.find_widget.match_case = !app.find_widget.match_case;
            recompute(app);
            return;
        }
        KeyCode::Char(c) if alt && !ctrl && matches!(c, 'w' | 'W') => {
            app.find_widget.whole_word = !app.find_widget.whole_word;
            recompute(app);
            return;
        }
        KeyCode::Char(c) if alt && !ctrl && matches!(c, 'r' | 'R') => {
            app.find_widget.regex = !app.find_widget.regex;
            recompute(app);
            return;
        }
        _ => {}
    }

    let outcome = input_edit::dispatch_key(
        &key,
        &mut app.find_widget.query,
        &mut app.find_widget.cursor,
    );
    if outcome == input_edit::Outcome::Edited {
        recompute(app);
    }
}

// ─── Internals ───────────────────────────────────────────────────────────────

fn resolve_target(app: &App) -> Option<FindTarget> {
    use crate::app::{Panel, Tab};

    match (app.active_tab, app.active_panel) {
        (Tab::Files, Panel::Diff) | (Tab::Search, Panel::Diff) => Some(FindTarget::FilePreview),
        (Tab::Git, Panel::Diff) => Some(diff_target_from_layout(
            app.diff_layout,
            app.diff_selection.map(|s| s.side),
            /*graph=*/ false,
        )),
        (Tab::Graph, Panel::Diff) => {
            if app.graph_uses_three_col() {
                Some(diff_target_from_layout(
                    app.diff_layout,
                    app.diff_selection.map(|s| s.side),
                    /*graph=*/ true,
                ))
            } else {
                // 2-col Graph view inlines the diff into the Commit
                // detail panel; no in-panel widget target there yet.
                None
            }
        }
        _ => None,
    }
}

fn diff_target_from_layout(
    layout: crate::app::DiffLayout,
    selection_side: Option<DiffSide>,
    graph: bool,
) -> FindTarget {
    use crate::app::DiffLayout;
    match layout {
        DiffLayout::Unified => {
            if graph {
                FindTarget::GraphDiffUnified
            } else {
                FindTarget::DiffUnified
            }
        }
        DiffLayout::SideBySide => match selection_side {
            Some(DiffSide::SbsLeft) => {
                if graph {
                    FindTarget::GraphDiffSbsLeft
                } else {
                    FindTarget::DiffSbsLeft
                }
            }
            Some(DiffSide::SbsRight) | Some(DiffSide::Unified) | None => {
                // No selection / unified-side hint in SBS layout → default
                // to the right (new code) side. Matches VSCode's "Find in
                // selection" default of the modified side.
                if graph {
                    FindTarget::GraphDiffSbsRight
                } else {
                    FindTarget::DiffSbsRight
                }
            }
        },
    }
}

fn collect_rows(app: &App, target: FindTarget) -> Vec<String> {
    match target {
        FindTarget::FilePreview => match app.preview_content.as_ref().map(|p| &p.body) {
            Some(crate::file_tree::PreviewBody::Text { lines, .. }) => lines.clone(),
            _ => Vec::new(),
        },
        FindTarget::DiffUnified => match &app.diff_content {
            Some(d) => search::unified_display_rows(&d.diff),
            None => Vec::new(),
        },
        FindTarget::GraphDiffUnified => match &app.commit_detail.file_diff {
            Some(d) => search::unified_display_rows(&d.diff),
            None => Vec::new(),
        },
        FindTarget::DiffSbsLeft => match &app.diff_content {
            Some(d) => sbs_side_rows(&d.diff, DiffSide::SbsLeft),
            None => Vec::new(),
        },
        FindTarget::DiffSbsRight => match &app.diff_content {
            Some(d) => sbs_side_rows(&d.diff, DiffSide::SbsRight),
            None => Vec::new(),
        },
        FindTarget::GraphDiffSbsLeft => match &app.commit_detail.file_diff {
            Some(d) => sbs_side_rows(&d.diff, DiffSide::SbsLeft),
            None => Vec::new(),
        },
        FindTarget::GraphDiffSbsRight => match &app.commit_detail.file_diff {
            Some(d) => sbs_side_rows(&d.diff, DiffSide::SbsRight),
            None => Vec::new(),
        },
    }
}

fn sbs_side_rows(diff: &crate::git::DiffContent, side: DiffSide) -> Vec<String> {
    crate::ui::diff_panel::build_sbs_row_texts(diff)
        .into_iter()
        .map(|r| match r {
            DiffRowText::Separator => String::new(),
            DiffRowText::Header(h) => h,
            // `build_sbs_row_texts` only ever emits Separator / Header /
            // Sbs (paired rows), never `Unified` — exhaustive match keeps
            // the compiler honest if the row enum grows new variants.
            DiffRowText::Unified(s) => s,
            DiffRowText::Sbs { left, right } => match side {
                DiffSide::SbsLeft => left,
                DiffSide::SbsRight => right,
                DiffSide::Unified => String::new(),
            },
        })
        .collect()
}

fn baseline_row(app: &App, target: FindTarget) -> usize {
    let snap = app.find_widget.snapshot.clone().unwrap_or_default();
    match target {
        FindTarget::FilePreview => snap.preview_scroll,
        FindTarget::DiffUnified | FindTarget::DiffSbsLeft | FindTarget::DiffSbsRight => {
            snap.diff_scroll
        }
        FindTarget::GraphDiffUnified
        | FindTarget::GraphDiffSbsLeft
        | FindTarget::GraphDiffSbsRight => snap.graph_diff_scroll,
    }
}

fn jump_to_current(app: &mut App) {
    let Some(cur) = app.find_widget.current else {
        return;
    };
    let Some(m) = app.find_widget.matches.get(cur).cloned() else {
        return;
    };
    let Some(target) = app.find_widget.target else {
        return;
    };
    match target {
        FindTarget::FilePreview => {
            let view_h = app.last_preview_view_h as usize;
            app.preview_scroll = center_scroll(m.row, view_h);
        }
        FindTarget::DiffUnified | FindTarget::DiffSbsLeft | FindTarget::DiffSbsRight => {
            let view_h = app.last_diff_view_h as usize;
            app.diff_scroll = center_scroll(m.row, view_h);
        }
        FindTarget::GraphDiffUnified
        | FindTarget::GraphDiffSbsLeft
        | FindTarget::GraphDiffSbsRight => {
            let view_h = app.last_diff_view_h as usize;
            app.commit_detail.file_diff_scroll = center_scroll(m.row, view_h);
        }
    }
}

/// Recompute matches for the current query + toggles. Called from:
///   - `handle_key` after every keystroke that may have edited the query
///   - `App::handle_action` when a mouse-driven toggle flips
///   - Integration tests that poke `query` directly and need the match
///     set rebuilt to reflect the change
pub fn recompute(app: &mut App) {
    let Some(target) = app.find_widget.target else {
        return;
    };
    let query = app.find_widget.query.clone();

    if query.is_empty() {
        app.find_widget.matches.clear();
        app.find_widget.current = None;
        app.find_widget.regex_error = None;
        // Re-apply snapshot scroll so the view rests at the starting
        // position while the user is mid-edit. Matches the legacy
        // `/`-prompt feel where deleting back to empty undoes the jump.
        if let Some(snap) = app.find_widget.snapshot.clone() {
            restore_snapshot(app, &snap);
        }
        return;
    }

    let opts = MatchOptions {
        case_sensitive: app.find_widget.match_case,
        whole_word: app.find_widget.whole_word,
        regex: app.find_widget.regex,
    };

    let rows = collect_rows(app, target);
    let mut matches: Vec<MatchLoc> = Vec::new();
    let mut regex_error: Option<String> = None;
    for (idx, text) in rows.iter().enumerate() {
        match find_all_with(text, &query, &opts) {
            Ok(rs) => {
                for r in rs {
                    matches.push(MatchLoc {
                        row: idx,
                        byte_range: r,
                    });
                }
            }
            Err(e) => {
                // Regex compile error: surface once and bail — no
                // partial matches across rows would be useful.
                regex_error = Some(e.to_string());
                matches.clear();
                break;
            }
        }
    }

    app.find_widget.matches = matches;
    app.find_widget.regex_error = regex_error;
    app.find_widget.current = pick_current(app, target);
    jump_to_current(app);
}

fn pick_current(app: &App, target: FindTarget) -> Option<usize> {
    if app.find_widget.matches.is_empty() {
        return None;
    }
    let base = baseline_row(app, target);
    app.find_widget
        .matches
        .iter()
        .position(|m| m.row >= base)
        .or(Some(0))
}

// ─── Matching engine ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub(crate) struct MatchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// All byte-range matches of `needle` in `haystack` honouring the three
/// VSCode-style toggles. The `regex` toggle uses the standard `regex` crate;
/// the literal-substring path reuses `search::find_all` and post-filters
/// whole-word boundaries so CJK haystacks behave predictably.
pub(crate) fn find_all_with(
    haystack: &str,
    needle: &str,
    opts: &MatchOptions,
) -> Result<Vec<Range<usize>>, regex::Error> {
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    if opts.regex {
        let pattern = if opts.whole_word {
            format!(r"\b(?:{}){}", needle, r"\b")
        } else {
            needle.to_string()
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!opts.case_sensitive)
            .multi_line(false)
            .build()?;
        let mut out = Vec::new();
        for m in re.find_iter(haystack) {
            // Zero-width matches (e.g. `(?:)`) can't anchor highlight ranges,
            // and infinite-looping on them is pointless — skip silently.
            if m.start() < m.end() {
                out.push(m.start()..m.end());
            }
        }
        return Ok(out);
    }

    let case_insensitive = !opts.case_sensitive;
    let raw = find_all(haystack, needle, case_insensitive);
    if !opts.whole_word {
        return Ok(raw);
    }
    Ok(raw
        .into_iter()
        .filter(|r| is_word_boundary(haystack, r))
        .collect())
}

fn is_word_boundary(text: &str, range: &Range<usize>) -> bool {
    let before_ok = range.start == 0
        || !text[..range.start]
            .chars()
            .next_back()
            .is_some_and(input_edit::is_word_char);
    let after_ok = range.end >= text.len()
        || !text[range.end..]
            .chars()
            .next()
            .is_some_and(input_edit::is_word_char);
    before_ok && after_ok
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(case_sensitive: bool, whole_word: bool, regex: bool) -> MatchOptions {
        MatchOptions {
            case_sensitive,
            whole_word,
            regex,
        }
    }

    #[test]
    fn literal_case_insensitive_by_default() {
        let r = find_all_with("Foo FOO foo", "foo", &opts(false, false, false)).unwrap();
        assert_eq!(r, vec![0..3, 4..7, 8..11]);
    }

    #[test]
    fn literal_case_sensitive_when_toggled() {
        let r = find_all_with("Foo FOO foo", "foo", &opts(true, false, false)).unwrap();
        assert_eq!(r, vec![8..11]);
    }

    #[test]
    fn whole_word_rejects_substring_match() {
        // "food" should not match "foo" with whole_word.
        let r = find_all_with("food foo foo!", "foo", &opts(false, true, false)).unwrap();
        assert_eq!(r, vec![5..8, 9..12]);
    }

    #[test]
    fn whole_word_at_string_boundaries_passes() {
        let r = find_all_with("foo", "foo", &opts(false, true, false)).unwrap();
        assert_eq!(r, vec![0..3]);
    }

    #[test]
    fn regex_basic() {
        let r = find_all_with("a1 b22 c333", r"\d+", &opts(false, false, true)).unwrap();
        assert_eq!(r, vec![1..2, 4..6, 8..11]);
    }

    #[test]
    fn regex_invalid_returns_err() {
        let r = find_all_with("anything", "(unclosed", &opts(false, false, true));
        assert!(r.is_err());
    }

    #[test]
    fn regex_with_whole_word_anchors_boundaries() {
        let r = find_all_with("foo food foo!", r"foo", &opts(false, true, true)).unwrap();
        assert_eq!(r, vec![0..3, 9..12]);
    }

    #[test]
    fn regex_case_sensitivity_obeys_toggle() {
        let r_i = find_all_with("Foo foo FOO", r"foo", &opts(false, false, true)).unwrap();
        let r_s = find_all_with("Foo foo FOO", r"foo", &opts(true, false, true)).unwrap();
        assert_eq!(r_i, vec![0..3, 4..7, 8..11]);
        assert_eq!(r_s, vec![4..7]);
    }

    #[test]
    fn empty_needle_returns_no_matches() {
        let r = find_all_with("anything", "", &opts(false, false, false)).unwrap();
        assert!(r.is_empty());
        let r = find_all_with("anything", "", &opts(false, false, true)).unwrap();
        assert!(r.is_empty());
    }
}
