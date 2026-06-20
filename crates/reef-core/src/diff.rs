use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DiffContent {
    pub path: String,
    pub hunks: Vec<DiffHunk>,
}

/// One hunk of a diff. `header` is the `@@ -.. +.. @@` line plus optional
/// section context. Stored as `Arc<str>` so display caches can share it.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub header: Arc<str>,
    pub lines: Vec<DiffLine>,
}

/// One line within a hunk. `content` excludes the leading `+`/`-`/` ` marker.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag: LineTag,
    pub content: Arc<str>,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTag {
    Context,
    Added,
    Removed,
}

pub type DiffHighlighted<T> = Vec<Vec<T>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLayout {
    Unified,
    SideBySide,
}

impl DiffLayout {
    pub fn pref_str(self) -> &'static str {
        match self {
            DiffLayout::Unified => "unified",
            DiffLayout::SideBySide => "side_by_side",
        }
    }

    pub fn from_pref_str(s: &str) -> Self {
        match s {
            "side_by_side" => DiffLayout::SideBySide,
            _ => DiffLayout::Unified,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    Unified,
    SbsLeft,
    SbsRight,
}

#[derive(Debug, Clone)]
pub enum DiffRowText {
    Separator,
    Header(Arc<str>),
    Unified(Arc<str>),
    Sbs { left: Arc<str>, right: Arc<str> },
}

impl DiffRowText {
    pub fn text_for(&self, side: DiffSide) -> &str {
        match self {
            DiffRowText::Separator => "",
            DiffRowText::Header(h) => h.as_ref(),
            DiffRowText::Unified(s) => s.as_ref(),
            DiffRowText::Sbs { left, right } => match side {
                DiffSide::SbsLeft => left.as_ref(),
                DiffSide::SbsRight | DiffSide::Unified => right.as_ref(),
            },
        }
    }
}

#[derive(Debug)]
pub enum UnifiedLine<T> {
    Separator,
    HunkHeader(Arc<str>),
    Content {
        tag: LineTag,
        old_lineno: Option<u32>,
        new_lineno: Option<u32>,
        text: Arc<str>,
        tokens: Option<T>,
    },
}

#[derive(Debug, Clone)]
pub struct SbsRow<T> {
    pub left_tag: LineTag,
    pub left_no: Option<u32>,
    pub left_text: Arc<str>,
    pub left_tokens: Option<T>,
    pub right_tag: LineTag,
    pub right_no: Option<u32>,
    pub right_text: Arc<str>,
    pub right_tokens: Option<T>,
}

#[derive(Debug)]
pub enum SbsDisplayLine<T> {
    Separator,
    HunkHeader(Arc<str>),
    Row(SbsRow<T>),
}

/// Pre-built display rows and per-row text snapshots for both diff layouts.
/// Build this once when a diff loads, then render/search from the cached rows.
#[derive(Debug)]
pub struct DiffDisplay<T> {
    pub unified_lines: Vec<UnifiedLine<T>>,
    pub sbs_lines: Vec<SbsDisplayLine<T>>,
    pub unified_row_texts: Arc<Vec<DiffRowText>>,
    pub sbs_row_texts: Arc<Vec<DiffRowText>>,
}

impl<T: Clone> DiffDisplay<T> {
    pub fn build(diff: &DiffContent, highlighted: Option<&DiffHighlighted<T>>) -> Self {
        let mut unified_lines: Vec<UnifiedLine<T>> = Vec::new();
        let mut sbs_lines: Vec<SbsDisplayLine<T>> = Vec::new();
        for (hi, hunk) in diff.hunks.iter().enumerate() {
            if hi > 0 {
                unified_lines.push(UnifiedLine::Separator);
                sbs_lines.push(SbsDisplayLine::Separator);
            }
            unified_lines.push(UnifiedLine::HunkHeader(Arc::clone(&hunk.header)));
            let hunk_tokens = highlighted.and_then(|h| h.get(hi).map(Vec::as_slice));
            for (li, line) in hunk.lines.iter().enumerate() {
                unified_lines.push(UnifiedLine::Content {
                    tag: line.tag,
                    old_lineno: line.old_lineno,
                    new_lineno: line.new_lineno,
                    text: Arc::clone(&line.content),
                    tokens: hunk_tokens.and_then(|t| t.get(li)).cloned(),
                });
            }
            sbs_lines.extend(build_sbs_lines(hunk, hunk_tokens));
        }
        let unified_row_texts = Arc::new(unified_row_texts_from(&unified_lines));
        let sbs_row_texts = Arc::new(sbs_row_texts_from(&sbs_lines));
        DiffDisplay {
            unified_lines,
            sbs_lines,
            unified_row_texts,
            sbs_row_texts,
        }
    }

    /// File line number for a display row, used by diff code navigation.
    /// Unified prefers the new-side line and falls back to the old side for
    /// removed rows. SBS uses `side` to pick the half before falling back.
    pub fn nav_line_at(&self, layout: DiffLayout, row_idx: usize, side: DiffSide) -> Option<u32> {
        match layout {
            DiffLayout::Unified => match self.unified_lines.get(row_idx)? {
                UnifiedLine::Content {
                    new_lineno,
                    old_lineno,
                    ..
                } => new_lineno.or(*old_lineno),
                _ => None,
            },
            DiffLayout::SideBySide => match self.sbs_lines.get(row_idx)? {
                SbsDisplayLine::Row(r) => match side {
                    DiffSide::SbsLeft => r.left_no.or(r.right_no),
                    DiffSide::SbsRight | DiffSide::Unified => r.right_no.or(r.left_no),
                },
                _ => None,
            },
        }
    }
}

fn unified_row_texts_from<T>(lines: &[UnifiedLine<T>]) -> Vec<DiffRowText> {
    lines
        .iter()
        .map(|dl| match dl {
            UnifiedLine::Separator => DiffRowText::Separator,
            UnifiedLine::HunkHeader(h) => DiffRowText::Header(Arc::clone(h)),
            UnifiedLine::Content { text, .. } => DiffRowText::Unified(Arc::clone(text)),
        })
        .collect()
}

fn sbs_row_texts_from<T>(lines: &[SbsDisplayLine<T>]) -> Vec<DiffRowText> {
    lines
        .iter()
        .map(|dl| match dl {
            SbsDisplayLine::Separator => DiffRowText::Separator,
            SbsDisplayLine::HunkHeader(h) => DiffRowText::Header(Arc::clone(h)),
            SbsDisplayLine::Row(r) => DiffRowText::Sbs {
                left: Arc::clone(&r.left_text),
                right: Arc::clone(&r.right_text),
            },
        })
        .collect()
}

pub fn build_sbs_lines<T: Clone>(
    hunk: &DiffHunk,
    hunk_tokens: Option<&[T]>,
) -> Vec<SbsDisplayLine<T>> {
    let mut rows: Vec<SbsDisplayLine<T>> = Vec::new();
    rows.push(SbsDisplayLine::HunkHeader(Arc::clone(&hunk.header)));
    rows.extend(
        pair_hunk_lines(hunk, hunk_tokens)
            .into_iter()
            .map(SbsDisplayLine::Row),
    );
    rows
}

pub fn pair_hunk_lines<T: Clone>(hunk: &DiffHunk, hunk_tokens: Option<&[T]>) -> Vec<SbsRow<T>> {
    let mut rows: Vec<SbsRow<T>> = Vec::new();
    let mut pending_removed: Vec<(Option<u32>, Arc<str>, Option<T>)> = Vec::new();
    let tokens_for = |li: usize| -> Option<T> { hunk_tokens.and_then(|t| t.get(li)).cloned() };
    let empty: Arc<str> = Arc::from("");

    for (li, line) in hunk.lines.iter().enumerate() {
        match line.tag {
            LineTag::Removed => {
                pending_removed.push((line.old_lineno, Arc::clone(&line.content), tokens_for(li)));
            }
            LineTag::Added => {
                let added_tokens = tokens_for(li);
                if !pending_removed.is_empty() {
                    let (old_no, old_text, old_tokens) = pending_removed.remove(0);
                    rows.push(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        left_tokens: old_tokens,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: Arc::clone(&line.content),
                        right_tokens: added_tokens,
                    });
                } else {
                    rows.push(SbsRow {
                        left_tag: LineTag::Context,
                        left_no: None,
                        left_text: Arc::clone(&empty),
                        left_tokens: None,
                        right_tag: LineTag::Added,
                        right_no: line.new_lineno,
                        right_text: Arc::clone(&line.content),
                        right_tokens: added_tokens,
                    });
                }
            }
            LineTag::Context => {
                for (old_no, old_text, old_tokens) in pending_removed.drain(..) {
                    rows.push(SbsRow {
                        left_tag: LineTag::Removed,
                        left_no: old_no,
                        left_text: old_text,
                        left_tokens: old_tokens,
                        right_tag: LineTag::Context,
                        right_no: None,
                        right_text: Arc::clone(&empty),
                        right_tokens: None,
                    });
                }
                let ctx_tokens = tokens_for(li);
                rows.push(SbsRow {
                    left_tag: LineTag::Context,
                    left_no: line.old_lineno,
                    left_text: Arc::clone(&line.content),
                    left_tokens: ctx_tokens.clone(),
                    right_tag: LineTag::Context,
                    right_no: line.new_lineno,
                    right_text: Arc::clone(&line.content),
                    right_tokens: ctx_tokens,
                });
            }
        }
    }

    for (old_no, old_text, old_tokens) in pending_removed.drain(..) {
        rows.push(SbsRow {
            left_tag: LineTag::Removed,
            left_no: old_no,
            left_text: old_text,
            left_tokens: old_tokens,
            right_tag: LineTag::Context,
            right_no: None,
            right_text: Arc::clone(&empty),
            right_tokens: None,
        });
    }

    rows
}

/// Flatten a diff into searchable rows that line up with Unified display rows.
pub fn unified_display_rows(diff: &DiffContent) -> Vec<Arc<str>> {
    let empty: Arc<str> = Arc::from("");
    let mut rows: Vec<Arc<str>> = Vec::new();
    for (i, hunk) in diff.hunks.iter().enumerate() {
        if i > 0 {
            rows.push(Arc::clone(&empty));
        }
        rows.push(Arc::clone(&hunk.header));
        for line in &hunk.lines {
            rows.push(Arc::clone(&line.content));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_line(
        tag: LineTag,
        content: &str,
        old_no: Option<u32>,
        new_no: Option<u32>,
    ) -> DiffLine {
        DiffLine {
            tag,
            content: Arc::from(content),
            old_lineno: old_no,
            new_lineno: new_no,
        }
    }

    fn make_hunk(header: &str, lines: Vec<DiffLine>) -> DiffHunk {
        DiffHunk {
            header: Arc::from(header),
            lines,
        }
    }

    fn count_rows<T>(v: &[SbsDisplayLine<T>]) -> usize {
        v.iter()
            .filter(|l| matches!(l, SbsDisplayLine::Row(_)))
            .count()
    }

    fn get_rows<T>(v: &[SbsDisplayLine<T>]) -> Vec<&SbsRow<T>> {
        v.iter()
            .filter_map(|l| {
                if let SbsDisplayLine::Row(r) = l {
                    Some(r)
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn diff_row_text_sbs_picks_side() {
        let row = DiffRowText::Sbs {
            left: "old".into(),
            right: "new".into(),
        };
        assert_eq!(row.text_for(DiffSide::SbsLeft), "old");
        assert_eq!(row.text_for(DiffSide::SbsRight), "new");
        assert_eq!(row.text_for(DiffSide::Unified), "new");
    }

    #[test]
    fn nav_line_at_maps_rows_to_file_lines() {
        let diff = DiffContent {
            path: "src/a.rs".to_string(),
            hunks: vec![make_hunk(
                "@@ -1,2 +1,2 @@",
                vec![
                    make_line(LineTag::Context, "ctx", Some(1), Some(1)),
                    make_line(LineTag::Added, "added", None, Some(2)),
                    make_line(LineTag::Removed, "gone", Some(2), None),
                ],
            )],
        };
        let d = DiffDisplay::<Arc<str>>::build(&diff, None);
        assert_eq!(
            d.nav_line_at(DiffLayout::Unified, 0, DiffSide::Unified),
            None
        );
        assert_eq!(
            d.nav_line_at(DiffLayout::Unified, 1, DiffSide::Unified),
            Some(1)
        );
        assert_eq!(
            d.nav_line_at(DiffLayout::Unified, 2, DiffSide::Unified),
            Some(2)
        );
        assert_eq!(
            d.nav_line_at(DiffLayout::Unified, 3, DiffSide::Unified),
            Some(2)
        );
        assert_eq!(
            d.nav_line_at(DiffLayout::Unified, 99, DiffSide::Unified),
            None
        );
    }

    #[test]
    fn build_sbs_lines_starts_with_hunk_header() {
        let hunk = make_hunk("@@ -1,2 +1,2 @@", vec![]);
        let lines = build_sbs_lines::<Arc<str>>(&hunk, None);
        assert!(matches!(lines.first(), Some(SbsDisplayLine::HunkHeader(_))));
    }

    #[test]
    fn build_sbs_lines_context_appears_on_both_sides() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Context, "same", Some(1), Some(1))],
        );
        let lines = build_sbs_lines::<Arc<str>>(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_tag, LineTag::Context);
        assert_eq!(rows[0].right_tag, LineTag::Context);
        assert_eq!(rows[0].left_text.as_ref(), "same");
        assert_eq!(rows[0].right_text.as_ref(), "same");
    }

    #[test]
    fn build_sbs_lines_add_only_has_empty_left() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Added, "new line", None, Some(1))],
        );
        let lines = build_sbs_lines::<Arc<str>>(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_tag, LineTag::Context);
        assert!(rows[0].left_text.is_empty());
        assert_eq!(rows[0].right_tag, LineTag::Added);
        assert_eq!(rows[0].right_text.as_ref(), "new line");
    }

    #[test]
    fn build_sbs_lines_remove_only_has_empty_right() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Removed, "old line", Some(1), None)],
        );
        let lines = build_sbs_lines::<Arc<str>>(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_tag, LineTag::Removed);
        assert_eq!(rows[0].left_text.as_ref(), "old line");
        assert_eq!(rows[0].right_tag, LineTag::Context);
        assert!(rows[0].right_text.is_empty());
    }

    #[test]
    fn build_sbs_lines_remove_then_add_are_paired() {
        let hunk = make_hunk(
            "@@ @@",
            vec![
                make_line(LineTag::Removed, "old", Some(1), None),
                make_line(LineTag::Added, "new", None, Some(1)),
            ],
        );
        let lines = build_sbs_lines::<Arc<str>>(&hunk, None);
        let rows = get_rows(&lines);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left_text.as_ref(), "old");
        assert_eq!(rows[0].right_text.as_ref(), "new");
    }

    #[test]
    fn build_sbs_lines_multiple_context_rows() {
        let hunk = make_hunk(
            "@@ @@",
            vec![
                make_line(LineTag::Context, "line1", Some(1), Some(1)),
                make_line(LineTag::Context, "line2", Some(2), Some(2)),
            ],
        );
        assert_eq!(count_rows(&build_sbs_lines::<Arc<str>>(&hunk, None)), 2);
    }

    #[test]
    fn pair_hunk_lines_paired_threads_tokens() {
        let hunk = make_hunk(
            "@@ @@",
            vec![
                make_line(LineTag::Removed, "old", Some(1), None),
                make_line(LineTag::Added, "new", None, Some(1)),
            ],
        );
        let tok_removed: Arc<str> = Arc::from("removed-token");
        let tok_added: Arc<str> = Arc::from("added-token");
        let hunk_tokens = vec![Arc::clone(&tok_removed), Arc::clone(&tok_added)];
        let rows = pair_hunk_lines(&hunk, Some(&hunk_tokens));
        assert_eq!(rows.len(), 1);
        assert!(Arc::ptr_eq(
            rows[0].left_tokens.as_ref().unwrap(),
            &tok_removed
        ));
        assert!(Arc::ptr_eq(
            rows[0].right_tokens.as_ref().unwrap(),
            &tok_added
        ));
    }

    #[test]
    fn pair_hunk_lines_end_of_hunk_flush_preserves_tokens() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Removed, "gone", Some(1), None)],
        );
        let tok: Arc<str> = Arc::from("gone-token");
        let hunk_tokens = vec![Arc::clone(&tok)];
        let rows = pair_hunk_lines(&hunk, Some(&hunk_tokens));
        assert_eq!(rows.len(), 1);
        assert!(Arc::ptr_eq(rows[0].left_tokens.as_ref().unwrap(), &tok));
        assert!(rows[0].right_tokens.is_none());
    }

    #[test]
    fn pair_hunk_lines_context_shares_tokens_across_halves() {
        let hunk = make_hunk(
            "@@ @@",
            vec![make_line(LineTag::Context, "same", Some(1), Some(1))],
        );
        let tok: Arc<str> = Arc::from("same-token");
        let hunk_tokens = vec![Arc::clone(&tok)];
        let rows = pair_hunk_lines(&hunk, Some(&hunk_tokens));
        assert_eq!(rows.len(), 1);
        assert!(Arc::ptr_eq(rows[0].left_tokens.as_ref().unwrap(), &tok));
        assert!(Arc::ptr_eq(rows[0].right_tokens.as_ref().unwrap(), &tok));
    }

    #[test]
    fn unified_display_rows_matches_unified_layout() {
        let diff = DiffContent {
            path: "src/a.rs".to_string(),
            hunks: vec![
                make_hunk(
                    "@@ -1 +1 @@",
                    vec![make_line(LineTag::Context, "one", Some(1), Some(1))],
                ),
                make_hunk(
                    "@@ -9 +9 @@",
                    vec![make_line(LineTag::Added, "two", None, Some(9))],
                ),
            ],
        };
        let rows = unified_display_rows(&diff);
        let text: Vec<&str> = rows.iter().map(|r| r.as_ref()).collect();
        assert_eq!(text, vec!["@@ -1 +1 @@", "one", "", "@@ -9 +9 @@", "two"]);
    }
}
