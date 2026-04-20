//! Pure data structures for turning a flat `Vec<FileEntry>` into a nested
//! directory tree, plus the `collapsed_key` helper used when persisting which
//! subtrees the user has collapsed in the Git status sidebar.
//!
//! The rendering side (ratatui rows, click targets) lives with each panel
//! in `ui/*_panel.rs` so the tree stays decoupled from output format.

use super::FileEntry;
use std::collections::BTreeMap;

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
}
