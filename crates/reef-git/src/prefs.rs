use std::path::PathBuf;

fn prefs_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config").join("reef");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("git.prefs"))
}

pub fn load_tree_mode() -> bool {
    let Some(path) = prefs_path() else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    for line in content.lines() {
        if let Some(val) = line.strip_prefix("tree_mode=") {
            return val.trim() == "true";
        }
    }
    false
}

pub fn save_tree_mode(tree_mode: bool) {
    if let Some(path) = prefs_path() {
        let content = format!("tree_mode={}\n", tree_mode);
        let _ = std::fs::write(path, content);
    }
}
