use crate::git::CommitInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneCell {
    Empty,
    Pass,
    Node,
    Merge { from: usize },
    Fork { to: usize },
}

#[derive(Debug, Clone)]
pub struct GraphRow {
    pub cells: Vec<LaneCell>,
    pub node_col: usize,
    pub commit: CommitInfo,
}

/// Incremental lane tracker. Input commits must be in topological order
/// (child before parent). Output has the same length as input, one row per
/// commit, with cells describing this commit's lane layout relative to the
/// active lanes at that moment.
///
/// Cells semantics per row:
///   - Node(col)       : this row's commit sits on `col`
///   - Pass(col)       : lane `col` passes through unchanged
///   - Merge{from}(col): lane `col` merges into `from` (another lane, usually
///                       the node's lane) — drawn as a connector glyph
///   - Fork{to}(col)   : lane `col` was just forked off from lane `to` (the
///                       node) because the commit has multiple parents
pub fn build_graph(commits: &[CommitInfo]) -> Vec<GraphRow> {
    let mut rows = Vec::with_capacity(commits.len());
    // lanes[i] = Some(oid) means lane i is waiting for commit `oid`
    let mut lanes: Vec<Option<String>> = Vec::new();

    for commit in commits {
        // Find own lane: first lane whose waiting-oid matches this commit.
        // If none, pick first empty lane or append a new one.
        let own_lane = match lanes
            .iter()
            .position(|l| l.as_deref() == Some(commit.oid.as_str()))
        {
            Some(i) => i,
            None => match lanes.iter().position(|l| l.is_none()) {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };

        // Other lanes also waiting for this oid → merge into own_lane
        let merging: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter_map(|(i, l)| {
                if i != own_lane && l.as_deref() == Some(commit.oid.as_str()) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();

        // Build cells for this row (pre-substitution view of lanes)
        let width = lanes.len();
        let mut cells: Vec<LaneCell> = Vec::with_capacity(width);
        for col in 0..width {
            let cell = if col == own_lane {
                LaneCell::Node
            } else if merging.contains(&col) {
                LaneCell::Merge { from: own_lane }
            } else if lanes[col].is_some() {
                LaneCell::Pass
            } else {
                LaneCell::Empty
            };
            cells.push(cell);
        }

        // Clear merging lanes — their target commit has been reached
        for &col in &merging {
            lanes[col] = None;
        }

        // Substitute own_lane with first parent, or clear if root
        if !commit.parents.is_empty() {
            lanes[own_lane] = Some(commit.parents[0].clone());
        } else {
            lanes[own_lane] = None;
        }

        // Extra parents (merge commit) → fork into new lanes
        for parent in commit.parents.iter().skip(1) {
            let new_col = match lanes.iter().position(|l| l.is_none()) {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            };
            lanes[new_col] = Some(parent.clone());
            while cells.len() <= new_col {
                cells.push(LaneCell::Empty);
            }
            cells[new_col] = LaneCell::Fork { to: own_lane };
        }

        rows.push(GraphRow {
            cells,
            node_col: own_lane,
            commit: commit.clone(),
        });

        // Trim trailing empty lanes so subsequent rows stay compact
        while matches!(lanes.last(), Some(None)) {
            lanes.pop();
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(oid: &str, parents: &[&str]) -> CommitInfo {
        CommitInfo {
            oid: oid.into(),
            short_oid: oid.chars().take(7).collect(),
            parents: parents.iter().map(|s| (*s).into()).collect(),
            author_name: String::new(),
            author_email: String::new(),
            time: 0,
            subject: String::new(),
        }
    }

    #[test]
    fn linear_history() {
        let commits = vec![c("a", &["b"]), c("b", &["d"]), c("d", &[])];
        let rows = build_graph(&commits);
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(row.node_col, 0);
            assert_eq!(row.cells, vec![LaneCell::Node]);
        }
    }

    #[test]
    fn fork_and_merge() {
        // M merges L and R; both descend from C.
        //   M
        //  / \
        // L   R
        //  \ /
        //   C
        let commits = vec![
            c("m", &["l", "r"]),
            c("l", &["cc"]),
            c("r", &["cc"]),
            c("cc", &[]),
        ];
        let rows = build_graph(&commits);
        assert_eq!(rows.len(), 4);

        // M: node col 0, fork to col 1
        assert_eq!(rows[0].node_col, 0);
        assert_eq!(rows[0].cells, vec![LaneCell::Node, LaneCell::Fork { to: 0 }]);

        // L: node col 0, R still passing on col 1
        assert_eq!(rows[1].node_col, 0);
        assert_eq!(rows[1].cells, vec![LaneCell::Node, LaneCell::Pass]);

        // R: passes on col 0 (waiting for C), node on col 1
        assert_eq!(rows[2].node_col, 1);
        assert_eq!(rows[2].cells, vec![LaneCell::Pass, LaneCell::Node]);

        // C: node col 0, merge from col 1 into 0
        assert_eq!(rows[3].node_col, 0);
        assert_eq!(
            rows[3].cells,
            vec![LaneCell::Node, LaneCell::Merge { from: 0 }]
        );
    }

    #[test]
    fn root_commit_without_parents() {
        let commits = vec![c("only", &[])];
        let rows = build_graph(&commits);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node_col, 0);
        assert_eq!(rows[0].cells, vec![LaneCell::Node]);
    }

    #[test]
    fn parent_outside_slice_keeps_lane() {
        // Only A is in the slice; its parent B is not walked.
        let commits = vec![c("a", &["b"])];
        let rows = build_graph(&commits);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cells, vec![LaneCell::Node]);
    }
}
