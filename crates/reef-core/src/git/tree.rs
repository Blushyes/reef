//! Pure data structures for turning a flat `Vec<FileEntry>` into a nested
//! directory tree, plus the `collapsed_key` helper used when persisting which
//! subtrees the user has collapsed in the Git status sidebar.
//!
//! Renderer-specific rows and click targets live with each frontend panel
//! so the tree stays decoupled from output format.

use super::FileEntry;
use std::collections::{BTreeMap, HashSet};

pub enum Node {
    Dir {
        path: String,
        children: BTreeMap<String, Node>,
    },
    File(FileEntry),
}

pub fn build(files: &[FileEntry]) -> BTreeMap<String, Node> {
    let mut root: BTreeMap<String, Node> = BTreeMap::new();
    for file in files {
        let parts: Vec<&str> = file.path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        insert(&mut root, file.clone(), &parts, 0, String::new());
    }
    root
}

pub fn sorted_entries(tree: &BTreeMap<String, Node>) -> Vec<(&str, &Node)> {
    let mut entries: Vec<(&str, &Node)> = tree
        .iter()
        .map(|(name, node)| (name.as_str(), node))
        .collect();
    entries.sort_by(|a, b| {
        let a_dir = matches!(a.1, Node::Dir { .. });
        let b_dir = matches!(b.1, Node::Dir { .. });
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        }
    });
    entries
}

pub fn visible_file_paths(
    files: &[FileEntry],
    is_staged: bool,
    collapsed: &HashSet<String>,
) -> Vec<String> {
    let tree = build(files);
    let mut paths = Vec::new();
    collect_visible_file_paths(&tree, is_staged, collapsed, &mut paths);
    paths
}

fn collect_visible_file_paths(
    tree: &BTreeMap<String, Node>,
    is_staged: bool,
    collapsed: &HashSet<String>,
    paths: &mut Vec<String>,
) {
    for (_, node) in sorted_entries(tree) {
        match node {
            Node::Dir { path, children } => {
                if !collapsed.contains(&collapsed_key(is_staged, path)) {
                    collect_visible_file_paths(children, is_staged, collapsed, paths);
                }
            }
            Node::File(entry) => paths.push(entry.path.clone()),
        }
    }
}

fn insert(
    map: &mut BTreeMap<String, Node>,
    file: FileEntry,
    parts: &[&str],
    idx: usize,
    prefix: String,
) {
    let part = parts[idx].to_string();
    let full_path = if prefix.is_empty() {
        part.clone()
    } else {
        format!("{}/{}", prefix, part)
    };
    let is_last = idx + 1 == parts.len();

    if is_last {
        map.insert(part, Node::File(file));
        return;
    }

    let node = map.entry(part).or_insert_with(|| Node::Dir {
        path: full_path.clone(),
        children: BTreeMap::new(),
    });
    if let Node::Dir { children, .. } = node {
        insert(children, file, parts, idx + 1, full_path);
    }
}

/// Composite key used by the Git status sidebar to remember which directory
/// subtrees the user has collapsed. The `is_staged` prefix lets the same
/// `src/` dir be independently collapsed in the staged and unstaged sections.
pub fn collapsed_key(is_staged: bool, path: &str) -> String {
    format!("{}:{}", if is_staged { "s" } else { "u" }, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::FileStatus;

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
        }
    }

    #[test]
    fn build_empty() {
        assert!(build(&[]).is_empty());
    }

    #[test]
    fn build_flat_files() {
        let tree = build(&[entry("foo.rs"), entry("bar.rs")]);
        assert_eq!(tree.len(), 2);
        assert!(matches!(tree.get("foo.rs"), Some(Node::File(_))));
        assert!(matches!(tree.get("bar.rs"), Some(Node::File(_))));
    }

    #[test]
    fn build_nested_dirs() {
        let tree = build(&[entry("a/b/c.rs")]);
        assert_eq!(tree.len(), 1);
        let Node::Dir {
            children: b_map, ..
        } = tree.get("a").unwrap()
        else {
            panic!("expected Dir")
        };
        let Node::Dir {
            children: c_map, ..
        } = b_map.get("b").unwrap()
        else {
            panic!("expected Dir")
        };
        assert!(matches!(c_map.get("c.rs"), Some(Node::File(_))));
    }

    #[test]
    fn build_multiple_files_same_dir() {
        let tree = build(&[entry("a/x.rs"), entry("a/y.rs")]);
        assert_eq!(tree.len(), 1);
        let Node::Dir { children, .. } = tree.get("a").unwrap() else {
            panic!()
        };
        assert_eq!(children.len(), 2);
        assert!(matches!(children.get("x.rs"), Some(Node::File(_))));
        assert!(matches!(children.get("y.rs"), Some(Node::File(_))));
    }

    #[test]
    fn build_btree_ordering() {
        let tree = build(&[entry("z.rs"), entry("a.rs"), entry("m.rs")]);
        let keys: Vec<&String> = tree.keys().collect();
        assert_eq!(keys, &["a.rs", "m.rs", "z.rs"]);
    }

    #[test]
    fn collapsed_key_staged() {
        assert_eq!(collapsed_key(true, "src"), "s:src");
    }

    #[test]
    fn collapsed_key_unstaged() {
        assert_eq!(collapsed_key(false, "src"), "u:src");
    }

    #[test]
    fn collapsed_key_empty_path() {
        assert_eq!(collapsed_key(true, ""), "s:");
    }

    #[test]
    fn visible_file_paths_follow_tree_render_order() {
        let files = vec![
            entry("z.txt"),
            entry("src/z.rs"),
            entry("README.md"),
            entry("src/a.rs"),
            entry("assets/logo.png"),
        ];

        assert_eq!(
            visible_file_paths(&files, false, &Default::default()),
            vec![
                "assets/logo.png",
                "src/a.rs",
                "src/z.rs",
                "README.md",
                "z.txt"
            ]
        );
    }

    #[test]
    fn visible_file_paths_skip_collapsed_dirs() {
        let files = vec![
            entry("src/a.rs"),
            entry("README.md"),
            entry("src/z.rs"),
            entry("z.txt"),
        ];
        let collapsed = HashSet::from([collapsed_key(false, "src")]);

        assert_eq!(
            visible_file_paths(&files, false, &collapsed),
            vec!["README.md", "z.txt"]
        );
    }
}
