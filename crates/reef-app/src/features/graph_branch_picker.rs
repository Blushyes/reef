use crate::PickerState;
use reef_core::git::{GraphScope, RefLabel};
use std::collections::HashMap;

pub const MAX_BRANCH_ROWS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchKind {
    Local,
    Remote,
}

#[derive(Debug, Clone)]
pub struct BranchEntry {
    pub full_ref: String,
    pub display: String,
    pub kind: BranchKind,
    pub is_head: bool,
}

#[derive(Debug, Clone)]
pub enum BranchPickerRow {
    AllRefs,
    Recent(BranchEntry),
    Branch(BranchEntry),
}

impl BranchPickerRow {
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
    pub core: PickerState,
    pub all_branches: Vec<BranchEntry>,
    pub recent: Vec<String>,
    pub scroll: usize,
    cache: std::cell::RefCell<Option<(CacheKey, Vec<BranchPickerRow>)>>,
}

type CacheKey = (String, usize, usize);

impl GraphBranchPickerState {
    pub fn open(
        &mut self,
        ref_map: &HashMap<String, Vec<RefLabel>>,
        recent: Vec<String>,
        current_scope: &GraphScope,
    ) {
        self.all_branches = collect_branches(ref_map);
        self.recent = recent;
        self.cache.get_mut().take();
        self.scroll = 0;
        self.core.open();
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

    pub fn handle_paste(&mut self, s: &str) {
        if crate::text_input::paste_single_line(s, &mut self.core.filter, &mut self.core.cursor) {
            self.core.selected_idx = 0;
        }
    }

    pub fn mark_dirty(&mut self) {
        self.cache.get_mut().take();
    }

    pub fn visible_rows(&self) -> Vec<BranchPickerRow> {
        let f = self.core.filter.to_ascii_lowercase();
        let key: CacheKey = (f, self.all_branches.len(), self.recent.len());
        if let Some((cached_key, cached_rows)) = self.cache.borrow().as_ref() {
            if cached_key == &key {
                return cached_rows.clone();
            }
        }
        let f: &str = key.0.as_str();
        let mut out: Vec<BranchPickerRow> = Vec::new();

        if f.is_empty() {
            out.push(BranchPickerRow::AllRefs);
        }

        let recent_set: std::collections::HashSet<&str> =
            self.recent.iter().map(String::as_str).collect();

        for full in &self.recent {
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

        *self.cache.borrow_mut() = Some((key, out.clone()));
        out
    }

    pub fn confirm(&self) -> Option<GraphScope> {
        self.visible_rows()
            .get(self.core.selected_idx)
            .map(BranchPickerRow::to_scope)
    }

    pub fn move_selection(&mut self, delta: i32) {
        let visible_count = self.visible_rows().len();
        self.core.move_selection(visible_count, delta);
    }
}

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
        assert_eq!(
            branches
                .iter()
                .map(|b| b.display.as_str())
                .collect::<Vec<_>>(),
            vec!["dev", "feature", "main", "origin/main"]
        );
        assert!(
            branches
                .iter()
                .find(|b| b.display == "main")
                .unwrap()
                .is_head
        );
        assert!(
            !branches
                .iter()
                .find(|b| b.display == "dev")
                .unwrap()
                .is_head
        );
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
    fn handle_paste_inserts_into_filter_and_resets_selection() {
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[
                ("aaaa", &[RefLabel::Branch("main".into())]),
                ("bbbb", &[RefLabel::Branch("feature/x".into())]),
                ("cccc", &[RefLabel::Branch("release/v1".into())]),
            ]),
            vec![],
            &GraphScope::AllRefs,
        );
        s.core.selected_idx = 2;
        s.scroll = 5;
        s.handle_paste("feat");
        assert_eq!(s.core.filter, "feat");
        assert_eq!(s.core.cursor, 4);
        assert_eq!(s.core.selected_idx, 0);
        assert_eq!(s.scroll, 5);
        let rows = s.visible_rows();
        assert!(
            rows.iter()
                .any(|r| matches!(r, BranchPickerRow::Branch(b) if b.display == "feature/x"))
        );
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r, BranchPickerRow::Branch(b) if b.display == "main"))
        );
    }

    #[test]
    fn sentinel_hidden_once_user_starts_typing() {
        let mut s = GraphBranchPickerState::default();
        s.open(
            &ref_map(&[
                ("aaaa", &[RefLabel::Branch("release/v1".into())]),
                ("bbbb", &[RefLabel::Branch("main".into())]),
            ]),
            vec![],
            &GraphScope::AllRefs,
        );
        assert!(matches!(s.visible_rows()[0], BranchPickerRow::AllRefs));

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
        assert_eq!(rows.len(), 2);
    }
}
