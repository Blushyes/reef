use crate::git::FileEntry;
use reef_protocol::{Color, Span, StyledLine};
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

pub fn flatten(
    tree: &BTreeMap<String, Node>,
    is_staged: bool,
    collapsed: &HashSet<String>,
    out: &mut Vec<StyledLine>,
    file_renderer: &mut dyn FnMut(&FileEntry, usize, &mut Vec<StyledLine>),
) {
    walk(tree, 1, is_staged, collapsed, out, file_renderer);
}

fn walk(
    tree: &BTreeMap<String, Node>,
    depth: usize,
    is_staged: bool,
    collapsed: &HashSet<String>,
    out: &mut Vec<StyledLine>,
    file_renderer: &mut dyn FnMut(&FileEntry, usize, &mut Vec<StyledLine>),
) {
    let mut nodes: Vec<(&String, &Node)> = tree.iter().collect();
    nodes.sort_by(|a, b| {
        let a_dir = matches!(a.1, Node::Dir { .. });
        let b_dir = matches!(b.1, Node::Dir { .. });
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        }
    });

    for (name, node) in nodes {
        match node {
            Node::Dir { path, children } => {
                let key = collapsed_key(is_staged, path);
                let is_collapsed = collapsed.contains(&key);
                out.push(dir_row(name, path, is_staged, depth, is_collapsed));
                if !is_collapsed {
                    walk(children, depth + 1, is_staged, collapsed, out, file_renderer);
                }
            }
            Node::File(entry) => {
                file_renderer(entry, depth, out);
            }
        }
    }
}

pub fn collapsed_key(is_staged: bool, path: &str) -> String {
    format!("{}:{}", if is_staged { "s" } else { "u" }, path)
}

fn dir_row(name: &str, path: &str, is_staged: bool, depth: usize, is_collapsed: bool) -> StyledLine {
    let indent = "  ".repeat(depth);
    let arrow = if is_collapsed { "›" } else { "⌄" };
    StyledLine::new(vec![
        Span::new(indent),
        Span::new(format!("{} ", arrow)).fg(Color::named("darkGray")),
        Span::new(format!("{}/", name))
            .fg(Color::named("cyan"))
            .bold(),
    ])
    .on_click(
        "git.toggleDir",
        serde_json::json!({ "path": path, "staged": is_staged }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{FileEntry, FileStatus};
    use reef_protocol::StyledLine;

    fn entry(path: &str) -> FileEntry {
        FileEntry { path: path.to_string(), status: FileStatus::Modified }
    }

    // ── build() ──────────────────────────────────────────────────────────────

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
        let Node::Dir { children: b_map, .. } = tree.get("a").unwrap() else { panic!("expected Dir") };
        let Node::Dir { children: c_map, .. } = b_map.get("b").unwrap() else { panic!("expected Dir") };
        assert!(matches!(c_map.get("c.rs"), Some(Node::File(_))));
    }

    #[test]
    fn build_multiple_files_same_dir() {
        let tree = build(&[entry("a/x.rs"), entry("a/y.rs")]);
        assert_eq!(tree.len(), 1);
        let Node::Dir { children, .. } = tree.get("a").unwrap() else { panic!() };
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

    // ── collapsed_key() ──────────────────────────────────────────────────────

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

    // ── flatten() ────────────────────────────────────────────────────────────

    #[test]
    fn flatten_visits_all_flat_files() {
        let tree = build(&[entry("a.rs"), entry("b.rs"), entry("c.rs")]);
        let mut out = Vec::new();
        let mut count = 0usize;
        flatten(&tree, false, &HashSet::new(), &mut out, &mut |_, _, _| { count += 1; });
        assert_eq!(count, 3);
    }

    #[test]
    fn flatten_dir_row_appears_in_out() {
        let tree = build(&[entry("src/main.rs")]);
        let mut out = Vec::new();
        flatten(&tree, false, &HashSet::new(), &mut out, &mut |_, _, _| {});
        // The dir row for "src" is pushed to `out`
        assert!(!out.is_empty(), "dir row should appear in output");
    }

    #[test]
    fn flatten_collapsed_dir_skips_children() {
        let tree = build(&[entry("src/main.rs"), entry("src/lib.rs")]);
        let mut collapsed = HashSet::new();
        // The path stored in Node::Dir is "src" (no trailing slash)
        collapsed.insert(collapsed_key(false, "src"));
        let mut out: Vec<StyledLine> = Vec::new();
        let mut count = 0usize;
        flatten(&tree, false, &collapsed, &mut out, &mut |_, _, _| { count += 1; });
        assert_eq!(count, 0, "children of collapsed dir should be skipped");
        assert_eq!(out.len(), 1, "dir row itself should still appear");
    }

    #[test]
    fn flatten_expanded_dir_visits_children() {
        let tree = build(&[entry("src/main.rs")]);
        let mut out: Vec<StyledLine> = Vec::new();
        let mut count = 0usize;
        flatten(&tree, false, &HashSet::new(), &mut out, &mut |_, _, o| {
            count += 1;
            o.push(StyledLine::plain("file"));
        });
        assert_eq!(count, 1);
    }
}
