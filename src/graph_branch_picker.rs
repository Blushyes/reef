//! `b` branch picker for the Graph tab.
//!
//! Overlay that lets the user restrict the commit graph to a single
//! local or remote-tracking branch. Modeled on `hosts_picker` — a
//! filter input on top of a scrolling list, plus a `[ All refs ]`
//! sentinel row that resets the scope to the default and a "recent"
//! section seeded from `GitGraphState::recent_branches`.

use crate::git::{GraphScope, RefLabel};
use std::collections::HashMap;

/// Soft cap for how many branches we render below the recents list.
/// Big monorepos can have thousands of remote branches and the picker
/// only loses signal at that scale — the filter input narrows things
/// fast enough that a cap on the rendered list keeps us out of the
/// "10,000-row Vec" trap on every redraw.
pub const MAX_BRANCH_ROWS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchKind {
    Local,
    Remote,
}

/// One selectable branch row.
#[derive(Debug, Clone)]
pub struct BranchEntry {
    /// Fully-qualified ref name (`refs/heads/main`).
    pub full_ref: String,
    /// User-facing label (`main`, `origin/feature/x`).
    pub display: String,
    pub kind: BranchKind,
    /// Whether HEAD currently points at this branch.
    pub is_head: bool,
}

/// Picker-list row in render order. `AllRefs` is the sentinel; recents
/// come from MRU prefs; locals / remotes are alphabetised.
#[derive(Debug, Clone)]
pub enum BranchPickerRow {
    AllRefs,
    Recent(BranchEntry),
    Branch(BranchEntry),
}

impl BranchPickerRow {
    /// What scope this row selects when the user presses Enter.
    pub fn to_scope(&self) -> GraphScope {
        match self {
            BranchPickerRow::AllRefs => GraphScope::AllRefs,
            BranchPickerRow::Recent(b) | BranchPickerRow::Branch(b) => {
                GraphScope::Branch(b.full_ref.clone())
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct GraphBranchPickerState {
    /// Shared overlay scaffolding (`active`, `filter`, `cursor`,
    /// `selected_idx`, `last_popup_area`). Edits route through
    /// [`crate::picker_core::PickerCore::dispatch_key`]; see
    /// `input::handle_key_graph_branch_picker` for the call site.
    pub core: crate::picker_core::PickerCore,
    /// All branches derived once from `git_graph.ref_map` when the
    /// picker opens. Stored sorted (locals first, then remotes,
    /// alphabetised within each group).
    pub all_branches: Vec<BranchEntry>,
    /// Recents as fully-qualified refs (newest first). Filtered against
    /// `all_branches` so a deleted recent silently disappears from the
    /// rendered list.
    pub recent: Vec<String>,
    /// First visible row in the list — read by the panel renderer to
    /// scroll the viewport when `selected_idx` leaves the window.
    /// Without this, big monorepos (hundreds of branches > visible
    /// list_h) leave the user's selection rendered off-screen on
    /// Down-key autorepeat. Reset to 0 by `open()` / `close()`.
    pub scroll: usize,
    /// Memoised result of [`visible_rows`].
    ///
    /// Cache key is `(lowercased_filter, all_branches.len(),
    /// recent.len())`. The two length fields guard against the case
    /// where a future caller mutates `all_branches` or `recent` in
    /// place without going through `open()` / `close()` — any
    /// push/pop changes the length and forces a recompute. Identical
    /// length but different content (e.g. swap one entry for another
    /// of the same kind) would still hit a stale cache, but that
    /// scenario has no current writer in the codebase.
    ///
    /// `RefCell` so `visible_rows(&self)` can populate it lazily
    /// without forcing every caller to a `&mut` receiver. The
    /// alternative — recomputing N keystrokes/second on a 2k-branch
    /// monorepo — would allocate ~60k Strings per second of held
    /// autorepeat.
    cache: std::cell::RefCell<Option<(CacheKey, Vec<BranchPickerRow>)>>,
}

/// Composite key for [`GraphBranchPickerState::cache`].
type CacheKey = (String, usize, usize);

impl GraphBranchPickerState {
    /// Prep state from the cached `ref_map` and recents and activate
    /// the overlay. Idempotent — re-opening just resets filter +
    /// selection so the user always lands at the top with no leftover
    /// query.
    pub fn open(
        &mut self,
        ref_map: &HashMap<String, Vec<RefLabel>>,
        recent: Vec<String>,
        current_scope: &GraphScope,
    ) {
        self.all_branches = collect_branches(ref_map);
        self.recent = recent;
        self.cache.get_mut().take(); // ref_map / recent changed — drop cache
        self.scroll = 0;
        self.core.open();
        // Land on whatever row matches the active scope so the picker
        // is "where you are" by default — pressing Esc cancels with no
        // surprise, and Enter is a no-op rebuild rather than an
        // accidental change.
        self.core.selected_idx = self
            .visible_rows()
            .iter()
            .position(|row| match (row, current_scope) {
                (BranchPickerRow::AllRefs, GraphScope::AllRefs) => true,
                (BranchPickerRow::Recent(b), GraphScope::Branch(s))
                | (BranchPickerRow::Branch(b), GraphScope::Branch(s)) => b.full_ref == *s,
                _ => false,
            })
            .unwrap_or(0);
    }

    pub fn close(&mut self) {
        self.cache.get_mut().take();
        self.scroll = 0;
        self.core.close();
    }

    /// Drop the memoised [`visible_rows`] result. Call this if any
    /// future writer mutates `all_branches` or `recent` IN PLACE
    /// (e.g. swapping one entry for another of the same kind, or
    /// editing a `BranchEntry` field). Push/pop changes are already
    /// covered by the `.len()` components of the cache key, but
    /// content-only mutations would otherwise return stale rows.
    pub fn mark_dirty(&mut self) {
        self.cache.get_mut().take();
    }

    /// Apply the current filter against AllRefs + recents + branches.
    /// `AllRefs` always renders when the filter is empty.
    ///
    /// Memoised via [`Self::cache`] keyed on the lowercased filter
    /// string — autorepeat on j/k holding 30Hz used to reallocate a
    /// HashSet + N String::to_ascii_lowercase + up to MAX_BRANCH_ROWS
    /// `BranchEntry` clones per tick. The cache is invalidated on
    /// `open` / `close` (data inputs change) and inside this method
    /// whenever the filter shifts.
    pub fn visible_rows(&self) -> Vec<BranchPickerRow> {
        let f = self.core.filter.to_ascii_lowercase();
        let key: CacheKey = (f, self.all_branches.len(), self.recent.len());
        // Cache hit?
        if let Some((cached_key, cached_rows)) = self.cache.borrow().as_ref() {
            if cached_key == &key {
                return cached_rows.clone();
            }
        }
        let f: &str = key.0.as_str();
        let mut out: Vec<BranchPickerRow> = Vec::new();

        // Sentinel is shown ONLY on the empty filter. Substring /
        // prefix matching against literal labels turned out to be a
        // footgun: common letters (`r`, `e`, `f`, `s`, space) keep
        // AllRefs visible at row 0 while every keystroke also resets
        // `selected_idx` to 0 — pressing Enter mid-typing would
        // commit AllRefs instead of the branch the user was
        // filtering toward. Once the user has typed anything, they
        // can clearly only be looking for a branch; cancel out with
        // Backspace or Esc to get back to AllRefs.
        if f.is_empty() {
            out.push(BranchPickerRow::AllRefs);
        }

        let recent_set: std::collections::HashSet<&str> =
            self.recent.iter().map(String::as_str).collect();

        for full in &self.recent {
            // Drop recents that point at a branch no longer in the
            // ref_map (deleted upstream). They'll silently fall off
            // the list rather than waste a row.
            let Some(entry) = self
                .all_branches
                .iter()
                .find(|b| b.full_ref == *full)
                .cloned()
            else {
                continue;
            };
            if f.is_empty() || entry.display.to_ascii_lowercase().contains(f) {
                out.push(BranchPickerRow::Recent(entry));
            }
        }

        let mut shown = 0usize;
        for entry in &self.all_branches {
            if recent_set.contains(entry.full_ref.as_str()) {
                continue;
            }
            if !(f.is_empty() || entry.display.to_ascii_lowercase().contains(f)) {
                continue;
            }
            out.push(BranchPickerRow::Branch(entry.clone()));
            shown += 1;
            if shown >= MAX_BRANCH_ROWS {
                break;
            }
        }

        // Populate cache. Borrow is short-lived; `Vec::clone` is a
        // single heap-block memcpy of small enums (~24 bytes each).
        *self.cache.borrow_mut() = Some((key, out.clone()));
        out
    }

    pub fn confirm(&self) -> Option<GraphScope> {
        self.visible_rows()
            .get(self.core.selected_idx)
            .map(BranchPickerRow::to_scope)
    }

    /// Move the row cursor by `delta`, clamped against the current
    /// `visible_rows()` count. Thin wrapper around the shared
    /// [`crate::picker_core::PickerCore::move_selection`] that handles
    /// the row-count lookup so callers (mouse wheel, etc.) don't have
    /// to recompute `visible_rows()` themselves.
    pub fn move_selection(&mut self, delta: i32) {
        let visible_count = self.visible_rows().len();
        self.core.move_selection(visible_count, delta);
    }
}

/// Flatten a ref_map (OID → labels) into the picker's branch list.
/// Local branches first (alphabetised), then remote-tracking branches
/// (alphabetised). Tags + HEAD don't get rows — HEAD is annotated on
/// whichever local row matches.
pub fn collect_branches(ref_map: &HashMap<String, Vec<RefLabel>>) -> Vec<BranchEntry> {
    let mut locals: Vec<BranchEntry> = Vec::new();
    let mut remotes: Vec<BranchEntry> = Vec::new();

    let mut head_local: Option<String> = None;
    for labels in ref_map.values() {
        if labels.iter().any(|l| matches!(l, RefLabel::Head)) {
            for label in labels {
                if let RefLabel::Branch(name) = label {
                    head_local = Some(name.clone());
                    break;
                }
            }
        }
    }

    for labels in ref_map.values() {
        for label in labels {
            match label {
                RefLabel::Branch(name) => locals.push(BranchEntry {
                    full_ref: format!("refs/heads/{name}"),
                    display: name.clone(),
                    kind: BranchKind::Local,
                    is_head: head_local.as_deref() == Some(name.as_str()),
                }),
                RefLabel::RemoteBranch(name) => remotes.push(BranchEntry {
                    full_ref: format!("refs/remotes/{name}"),
                    display: name.clone(),
                    kind: BranchKind::Remote,
                    is_head: false,
                }),
                _ => {}
            }
        }
    }

    locals.sort_by(|a, b| a.display.cmp(&b.display));
    remotes.sort_by(|a, b| a.display.cmp(&b.display));
    locals.append(&mut remotes);
    locals
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_map(pairs: &[(&str, &[RefLabel])]) -> HashMap<String, Vec<RefLabel>> {
        pairs
            .iter()
            .map(|(oid, labels)| (oid.to_string(), labels.to_vec()))
            .collect()
    }

    #[test]
    fn collect_branches_sorts_and_flags_head() {
        let map = ref_map(&[
            (
                "aaaa",
                &[
                    RefLabel::Head,
                    RefLabel::Branch("main".into()),
                    RefLabel::Branch("dev".into()),
                ],
            ),
            ("bbbb", &[RefLabel::Branch("feature".into())]),
            (
                "cccc",
                &[
                    RefLabel::RemoteBranch("origin/main".into()),
                    RefLabel::Tag("v1".into()),
                ],
            ),
        ]);
        let branches = collect_branches(&map);
        // Locals alphabetised, then remotes. HEAD points at "main".
        assert_eq!(
            branches.iter().map(|b| b.display.as_str()).collect::<Vec<_>>(),
            vec!["dev", "feature", "main", "origin/main"]
        );
        assert!(branches.iter().find(|b| b.display == "main").unwrap().is_head);
        assert!(!branches.iter().find(|b| b.display == "dev").unwrap().is_head);
    }

    #[test]
    fn visible_rows_starts_with_all_refs_and_filters_others() {
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[
                ("aaaa", &[RefLabel::Branch("main".into())]),
                ("bbbb", &[RefLabel::Branch("dev".into())]),
            ]),
            vec![],
            &GraphScope::AllRefs,
        );
        let rows = s.visible_rows();
        assert!(matches!(rows[0], BranchPickerRow::AllRefs));
        s.core.filter = "mai".into();
        let rows = s.visible_rows();
        assert!(matches!(
            rows.last().unwrap(),
            BranchPickerRow::Branch(b) if b.display == "main"
        ));
    }

    #[test]
    fn open_resets_cursor_to_zero() {
        // Make sure re-opening the picker after a previous filter
        // leaves the cursor at the start, matching the cleared filter.
        let mut s = GraphBranchPickerState::default();
        s.core.cursor = 7;
        s.core.filter = "stale-from-last-time".into();
        s.open(
            &ref_map(&[("a", &[RefLabel::Branch("main".into())])]),
            vec![],
            &GraphScope::AllRefs,
        );
        assert_eq!(s.core.cursor, 0);
        assert!(s.core.filter.is_empty());
    }

    #[test]
    fn dispatch_key_drives_filter_via_input_edit() {
        // Sanity that the shared input_edit primitives operate on the
        // picker's filter + cursor pair. We don't exhaustively test
        // editor keys here (input_edit::tests already covers those) —
        // just confirm the wiring round-trips for an insert + Ctrl+U
        // clear, and that the cursor advances past UTF-8 chars cleanly.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[("a", &[RefLabel::Branch("main".into())])]),
            vec![],
            &GraphScope::AllRefs,
        );

        // Insert "ab"
        for c in ['a', 'b'] {
            let outcome = crate::input_edit::dispatch_key(
                &KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut s.core.filter,
                &mut s.core.cursor,
            );
            assert_eq!(outcome, crate::input_edit::Outcome::Edited);
        }
        assert_eq!(s.core.filter, "ab");
        assert_eq!(s.core.cursor, 2);

        // Ctrl+U wipes the line via input_edit's editor vocabulary.
        let outcome = crate::input_edit::dispatch_key(
            &KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut s.core.filter,
            &mut s.core.cursor,
        );
        assert_eq!(outcome, crate::input_edit::Outcome::Edited);
        assert!(s.core.filter.is_empty());
        assert_eq!(s.core.cursor, 0);

        // Insert a CJK codepoint, then confirm the cursor lands on a
        // valid char boundary (3-byte char advances cursor by 3).
        let outcome = crate::input_edit::dispatch_key(
            &KeyEvent::new(KeyCode::Char('你'), KeyModifiers::NONE),
            &mut s.core.filter,
            &mut s.core.cursor,
        );
        assert_eq!(outcome, crate::input_edit::Outcome::Edited);
        assert_eq!(s.core.filter, "你");
        assert_eq!(s.core.cursor, 3);
    }

    #[test]
    fn sentinel_hidden_once_user_starts_typing() {
        // Regression: previously the sentinel used substring matching
        // against literal labels ("all refs"), so common letters like
        // 'r', 'e', 'f', 's', space kept it at row 0 even mid-typing.
        // Combined with `selected_idx` resetting to 0 on each
        // keystroke, that meant Enter mid-typing committed AllRefs
        // instead of the branch the user was filtering toward.
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[
                ("aaaa", &[RefLabel::Branch("release/v1".into())]),
                ("bbbb", &[RefLabel::Branch("main".into())]),
            ]),
            vec![],
            &GraphScope::AllRefs,
        );
        // Empty filter: sentinel visible at top.
        assert!(matches!(s.visible_rows()[0], BranchPickerRow::AllRefs));

        // Any non-empty filter, even a single letter that "happens to
        // be in 'all refs'", must drop the sentinel.
        for f in ["a", "r", "e", "f", "s", " ", "all"] {
            s.core.filter = f.to_string();
            let rows = s.visible_rows();
            assert!(
                !rows.iter().any(|r| matches!(r, BranchPickerRow::AllRefs)),
                "filter {f:?} unexpectedly kept the AllRefs sentinel visible"
            );
        }
    }

    #[test]
    fn recents_render_above_branches_and_dedupe() {
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[
                ("a", &[RefLabel::Branch("main".into())]),
                ("b", &[RefLabel::Branch("feature".into())]),
            ]),
            vec!["refs/heads/main".to_string()],
            &GraphScope::AllRefs,
        );
        let rows = s.visible_rows();
        // [AllRefs, Recent(main), Branch(feature)]
        assert!(matches!(rows[0], BranchPickerRow::AllRefs));
        assert!(matches!(&rows[1], BranchPickerRow::Recent(e) if e.display == "main"));
        assert!(matches!(&rows[2], BranchPickerRow::Branch(e) if e.display == "feature"));
    }

    #[test]
    fn confirm_returns_scope_for_selected_row() {
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[("a", &[RefLabel::Branch("main".into())])]),
            vec![],
            &GraphScope::AllRefs,
        );
        // Row 0 is AllRefs, row 1 is "main".
        s.core.selected_idx = 1;
        assert_eq!(
            s.confirm(),
            Some(GraphScope::Branch("refs/heads/main".into()))
        );
        s.core.selected_idx = 0;
        assert_eq!(s.confirm(), Some(GraphScope::AllRefs));
    }

    #[test]
    fn stale_recent_silently_disappears() {
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[("a", &[RefLabel::Branch("main".into())])]),
            vec!["refs/heads/ghost".to_string()],
            &GraphScope::AllRefs,
        );
        let rows = s.visible_rows();
        // Only AllRefs + main; ghost dropped.
        assert_eq!(rows.len(), 2);
    }
}
