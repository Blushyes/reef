//! Property-based invariants for `build_graph`. Generates random topologically-
//! ordered commit sequences and verifies structural guarantees on every row.

use proptest::prelude::*;
use reef::git::CommitInfo;
use reef::git::graph::{LaneCell, build_graph};

/// Generate a topologically-ordered commit sequence: each commit's parents
/// reference only commits later in the vec (child-before-parent order). Up to
/// 2 parents per commit so merge topologies are exercised.
fn topo_commits(max_len: usize) -> impl Strategy<Value = Vec<CommitInfo>> {
    prop::collection::vec((0usize..=2, 1usize..=100, 1usize..=100), 1..=max_len).prop_map(|seeds| {
        let n = seeds.len();
        let mut commits = Vec::with_capacity(n);
        for (i, (num_parents, off0, off1)) in seeds.iter().enumerate() {
            let remaining = n.saturating_sub(i + 1);
            let mut parents: Vec<String> = Vec::new();
            if remaining > 0 && *num_parents >= 1 {
                parents.push(format!("c{}", i + 1 + (off0 % remaining)));
            }
            if remaining > 0 && *num_parents >= 2 {
                parents.push(format!("c{}", i + 1 + (off1 % remaining)));
            }
            parents.sort();
            parents.dedup();
            commits.push(CommitInfo {
                oid: format!("c{}", i),
                short_oid: format!("c{}", i),
                parents,
                author_name: String::new(),
                author_email: String::new(),
                time: 0,
                subject: String::new(),
            });
        }
        commits
    })
}

proptest! {
    #[test]
    fn rows_len_matches_commits(commits in topo_commits(30)) {
        let rows = build_graph(&commits);
        prop_assert_eq!(rows.len(), commits.len());
    }

    #[test]
    fn every_row_has_node_cell_at_node_col(commits in topo_commits(30)) {
        for row in build_graph(&commits) {
            prop_assert!(row.node_col < row.cells.len(),
                "node_col {} out of bounds (cells.len = {})",
                row.node_col, row.cells.len());
            prop_assert_eq!(row.cells[row.node_col], LaneCell::Node);
        }
    }

    #[test]
    fn fork_and_merge_targets_are_valid(commits in topo_commits(30)) {
        for row in build_graph(&commits) {
            let w = row.cells.len();
            for cell in &row.cells {
                match cell {
                    LaneCell::Fork { to } => {
                        prop_assert!(*to < w, "Fork{{to: {}}} >= width {}", to, w);
                    }
                    LaneCell::Merge { from } => {
                        prop_assert!(*from < w, "Merge{{from: {}}} >= width {}", from, w);
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn exactly_one_node_cell_per_row(commits in topo_commits(30)) {
        for row in build_graph(&commits) {
            let node_count = row.cells.iter()
                .filter(|c| matches!(c, LaneCell::Node))
                .count();
            prop_assert_eq!(node_count, 1);
        }
    }

    #[test]
    fn empty_input_produces_empty_output(_ in Just(())) {
        prop_assert!(build_graph(&[]).is_empty());
    }
}
