use std::collections::BTreeMap;
use std::path::PathBuf;

fn prefs_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config").join("reef");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("git.prefs"))
}

fn read_all() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Some(path) = prefs_path() else {
        return map;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return map;
    };
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn write_all(map: &BTreeMap<String, String>) {
    let Some(path) = prefs_path() else {
        return;
    };
    let mut content = String::new();
    for (k, v) in map {
        content.push_str(k);
        content.push('=');
        content.push_str(v);
        content.push('\n');
    }
    let _ = std::fs::write(path, content);
}

fn set_key(key: &str, value: &str) {
    let mut map = read_all();
    map.insert(key.to_string(), value.to_string());
    write_all(&map);
}

pub fn load_tree_mode() -> bool {
    read_all().get("tree_mode").map(|v| v == "true").unwrap_or(false)
}

pub fn save_tree_mode(tree_mode: bool) {
    set_key("tree_mode", if tree_mode { "true" } else { "false" });
}

pub fn load_commit_diff_layout() -> &'static str {
    match read_all().get("commit_diff_layout").map(String::as_str) {
        Some("side_by_side") => "side_by_side",
        _ => "unified",
    }
}

pub fn save_commit_diff_layout(layout: &str) {
    set_key("commit_diff_layout", layout);
}

pub fn load_commit_diff_mode() -> &'static str {
    match read_all().get("commit_diff_mode").map(String::as_str) {
        Some("full_file") => "full_file",
        _ => "compact",
    }
}

pub fn save_commit_diff_mode(mode: &str) {
    set_key("commit_diff_mode", mode);
}

pub fn load_commit_files_tree_mode() -> bool {
    read_all()
        .get("commit_files_tree_mode")
        .map(|v| v == "true")
        .unwrap_or(false)
}

pub fn save_commit_files_tree_mode(tree_mode: bool) {
    set_key(
        "commit_files_tree_mode",
        if tree_mode { "true" } else { "false" },
    );
}
