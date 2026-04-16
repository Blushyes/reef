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
